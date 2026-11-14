//! The served mark and the tombstones of a served root.
//!
//! A client that has seen a pallet's head on a remote may free its own copy of that history
//! (scope prune, a delete's containment check): the remote holds it. A ref removed on a served root
//! would break that promise if nothing kept the removed head, since gc frees what no root reaches.
//! So in a root that has been served, every ref removal leaves a tombstone: the pallet's wire name
//! and the head its ref held. The tombstone is a gc root, the handshake lists it, and the name
//! stays reserved for that history: re-creating it must descend from the tombstone's head and
//! acknowledge it ([`pallet_utils::update_pallet_head_in`](crate::util::pallet_utils::update_pallet_head_in)
//! states the rule). A tombstone stands until such a re-creation clears it, or until someone at the
//! served root's own disk drops it ([`drop_tombstone`]). Nothing expires one: a clone that pruned
//! against it would lose the only copy.
//!
//! Besides its named tombstones the list can hold *kept heads*: heads with no name, written by
//! [`repair_tombstones`](crate::util::tombstone_repair::repair_tombstones) for history a lost list
//! may have held when no record still names its pallet. A kept head is a root like a tombstone,
//! but the handshake does not list it, since its name would be a guess, and it reserves no name.
//! [`drop_kept_head`] releases one.
//!
//! Only a served root carries tombstones. `forklift-server` marks a root as served before it first
//! serves it ([`mark_served`]) and nothing clears the mark; a root never served writes none,
//! because no clone can have pruned against what nobody served. Two files witness a served root,
//! so that losing either one is told apart from a root never served ([`served_state`]): the mark,
//! and the tombstone list, which [`mark_served`] creates empty before it writes the mark. In a
//! served root a list that is missing or does not read is damage, refused with
//! [`RefusalCode::TombstonesDamaged`]; so is a mark that does not read, and a missing mark beside a
//! list that holds anything. Only losing both files at once reads as a root never served.
//!
//! Both files live in the shared forklift root, so every bay sees them, and both are
//! [`record_utils`] files: the mark has no entries, and the tombstone list one `<head> <pallet>`
//! entry per tombstone and one bare `<head>` entry per kept head. Every write of either is made
//! under the [`RefLock`], which orders it against every ref write.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::globals;
use crate::util::lock_utils::{RefLock, ServeLock};
use crate::util::{file_utils, object_utils, pallet_utils, record_utils, withholding};

/// The mark's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_SERVED: &str = "served";
pub(crate) const SERVED_RECORD: &str = "served";
pub(crate) const SERVED_VERSION: u32 = 1;

/// The tombstone list's file name in the shared forklift root, and its kind and version. Version 2
/// added kept heads; a list of any other version reads as damage, never as empty.
const FILE_NAME_TOMBSTONES: &str = "tombstones";
const TOMBSTONES_RECORD: &str = "tombstones";
pub(crate) const TOMBSTONES_VERSION: u32 = 2;

pub(crate) fn served_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_SERVED)
}

pub(crate) fn tombstones_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_TOMBSTONES)
}

/// The tombstones of a warehouse: each deleted pallet's wire name mapped to the head its ref held,
/// and the kept heads, which no name holds.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Tombstones {
    /// Each deleted pallet's wire name, mapped to the head its ref held.
    pub named: BTreeMap<String, String>,
    /// Heads kept with no name (see the module doc).
    pub kept: BTreeSet<String>,
}

impl Tombstones {
    /// Whether the list holds neither a tombstone nor a kept head.
    pub fn is_empty(&self) -> bool {
        self.named.is_empty() && self.kept.is_empty()
    }

    /// Every head the list keeps, named or not, each once.
    pub fn heads(&self) -> Vec<String> {
        self.named.values().chain(self.kept.iter()).cloned().collect::<BTreeSet<_>>().into_iter().collect()
    }
}

/// What the two witness files say of the warehouse at the current storage root (see the module
/// doc). Every other combination is damage, refused by [`served_state`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServedState {
    /// Neither file is there: the root has never been served.
    Never,
    /// An empty list and no mark: a first serve that stopped between the two writes, or a served
    /// root that kept no tombstone and lost its mark. [`mark_served`] completes it; a ref removal
    /// is refused until then, since it could not tell whether to leave a tombstone.
    Unfinished,
    /// The mark is there, and so is a list that reads.
    Served,
}

/// The command that repairs the tombstones of the warehouse at the current storage root.
fn repair_command() -> String {
    // The storage root may be relative to where the command runs, and the remedy is run elsewhere.
    let root = std::path::absolute(globals::forklift_root()).unwrap_or_else(|_| globals::forklift_root());
    let warehouse = root.parent().unwrap_or(&root);

    format!("forklift-server tombstone repair --root {}", crate::util::franchise_utils::quote(&warehouse.to_string_lossy()))
}

/// The refusal for damaged tombstones, `detail` saying what is wrong.
pub(crate) fn damaged(detail: &str) -> String {
    CoreError::refusal(
        RefusalCode::TombstonesDamaged,
        format!(
            "{} Which deleted pallets' heads this served warehouse keeps cannot be told, so nothing \
            was changed, and nothing that frees history runs here until the list is repaired.",
            detail
        ),
        repair_command(),
    ).into()
}

/// Whether `error` is the refusal of damaged tombstones ([`damaged`]).
pub fn is_damage(error: &str) -> bool {
    crate::error::deframe(error).is_some_and(|(code, _, _)| code == RefusalCode::TombstonesDamaged.as_str())
}

/// Read a record file's entries; `None` when the file is absent.
fn read_record(path: &Path, kind: &str, version: u32) -> Result<Option<Vec<String>>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    record_utils::from_bytes(&bytes, kind, version)
        .map(Some)
        .map_err(|e| format!("The file \"{}\" {}", path.to_string_lossy(), e.describe()))
}

/// One entry of the tombstone list, as written.
pub(crate) enum Entry<'a> {
    /// `<head> <pallet>`.
    Named(&'a str, &'a str),
    /// A bare `<head>`.
    Kept(&'a str),
}

/// Parse one entry of the tombstone list, `None` when it is neither form.
pub(crate) fn parse_entry(line: &str) -> Option<Entry<'_>> {
    match line.split_once(' ') {
        Some((head, pallet)) if object_utils::is_object_hash(head) && pallet_utils::validate_pallet_name(pallet).is_ok() => {
            Some(Entry::Named(head, pallet))
        }
        None if object_utils::is_object_hash(line) => Some(Entry::Kept(line)),
        _ => None,
    }
}

/// Read the tombstone list, `None` when it is absent. The error is the plain reason it does not
/// read: the file cannot be read, is not a list of this version, holds an entry of neither form,
/// or names a pallet or keeps a head twice.
fn read_list() -> Result<Option<Tombstones>, String> {
    let path = tombstones_path();
    let Some(lines) = read_record(&path, TOMBSTONES_RECORD, TOMBSTONES_VERSION)? else {
        return Ok(None);
    };

    let mut tombstones = Tombstones::default();

    for line in lines {
        match parse_entry(&line) {
            // Two tombstones of one name would leave one head unrooted, so neither is chosen.
            Some(Entry::Named(head, pallet)) => if tombstones.named.insert(pallet.to_string(), head.to_string()).is_some() {
                return Err(format!("The file \"{}\" holds two tombstones of pallet \"{}\".", path.to_string_lossy(), pallet));
            },
            Some(Entry::Kept(head)) => if !tombstones.kept.insert(head.to_string()) {
                return Err(format!("The file \"{}\" keeps head {} twice.", path.to_string_lossy(), head));
            },
            None => return Err(format!(
                "The file \"{}\" holds an entry that is neither a parcel hash and a pallet name nor a \
                parcel hash alone: \"{}\".",
                path.to_string_lossy(), line.escape_debug()
            )),
        }
    }

    Ok(Some(tombstones))
}

/// Read both witness files of the warehouse at the current storage root.
///
/// # Returns
/// * `Ok((ServedState, Tombstones))` - What they say, and the list (empty unless served).
/// * `Err(String)`                   - A [`RefusalCode::TombstonesDamaged`] refusal: the mark does
///                                     not read, the list does not read, a served root has no list,
///                                     or a list that holds anything has no mark beside it.
fn load() -> Result<(ServedState, Tombstones), String> {
    let mark = read_record(&served_path(), SERVED_RECORD, SERVED_VERSION).map_err(|e| damaged(&e))?;
    let list = read_list().map_err(|e| damaged(&e))?;

    match (mark, list) {
        (Some(_), Some(tombstones)) => Ok((ServedState::Served, tombstones)),
        (Some(_), None) => Err(damaged(&format!(
            "This warehouse has been served, but its tombstone list \"{}\" is missing.",
            tombstones_path().to_string_lossy()
        ))),
        (None, None) => Ok((ServedState::Never, Tombstones::default())),
        (None, Some(tombstones)) if tombstones.is_empty() => Ok((ServedState::Unfinished, tombstones)),
        (None, Some(_)) => Err(damaged(&format!(
            "The tombstone list \"{}\" holds tombstones, but the served mark \"{}\" beside it is missing.",
            tombstones_path().to_string_lossy(), served_path().to_string_lossy()
        ))),
    }
}

/// What the witness files say of the warehouse at the current storage root; see [`load`] for the
/// damage refused.
pub fn served_state() -> Result<ServedState, String> {
    load().map(|(state, _)| state)
}

/// Whether the warehouse at the current storage root has been served: [`ServedState::Served`]. An
/// unfinished first serve is not, since nothing was served before the mark.
///
/// # Returns
/// * `Ok(bool)`    - Whether the mark is there.
/// * `Err(String)` - The damage [`load`] refuses.
pub fn is_served() -> Result<bool, String> {
    Ok(served_state()? == ServedState::Served)
}

/// Whether a ref removal in the warehouse at the current storage root must leave a tombstone: the
/// root is served. An unfinished first serve is refused, with the damage code, since the removal
/// cannot tell whether a clone was served the head it removes.
pub(crate) fn served_for_removal() -> Result<bool, String> {
    match served_state()? {
        ServedState::Served => Ok(true),
        ServedState::Never => Ok(false),
        ServedState::Unfinished => Err(damaged(&format!(
            "The served mark \"{}\" is missing beside an empty tombstone list: a first serve did not \
            finish, or the mark was lost.",
            served_path().to_string_lossy()
        ))),
    }
}

/// Mark the warehouse at the current storage root as served, durably, unless it is already: first
/// an empty record of withheld content and an empty tombstone list, only when there are none, then
/// the mark, once the record of withheld content reads. An unfinished first serve is
/// completed by writing the mark. Under the [`RefLock`], so a ref removal in a bay reads the mark
/// either before it is written (and the server then serves a root without that ref) or after it
/// (and the removal leaves a tombstone). A root already marked is answered without the lock.
/// Unless `wait`, the lock is tried, never waited for: the server's multi-root registry calls this
/// under its mutex, so a bay holding the lock makes the first serve retry rather than stall every
/// other warehouse.
///
/// # Returns
/// * `Ok(true)`    - The root is marked.
/// * `Ok(false)`   - Not `wait`, the root is unmarked and a bay holds the ref lock; nothing was
///                   written.
/// * `Err(String)` - The damage [`load`] refuses, the record of withheld content's
///                   ([`withholding::read_withheld`]), or a file could not be written.
pub fn mark_served(wait: bool) -> Result<bool, String> {
    if is_served()? {
        return Ok(true);
    }

    let lock = match wait {
        true => RefLock::acquire()?,
        false => match RefLock::try_acquire_in(&globals::forklift_root())? {
            Some(lock) => lock,
            None => return Ok(false),
        },
    };

    match served_state()? {
        ServedState::Served => return Ok(true),
        // Every write of the list is made under the ref lock, so it is still absent here. The
        // record of withheld content comes first: a served root without one has lost it.
        ServedState::Never => {
            withholding::create_if_absent(&lock)?;
            write_tombstones(&lock, &Tombstones::default())?
        }
        ServedState::Unfinished => {}
    }

    // Never served over a record that does not read, or one lost after an unfinished first serve.
    withholding::read_withheld()?;

    write_mark(&lock)?;

    Ok(true)
}

/// Write the served mark, durably. Only under the ref lock.
pub(crate) fn write_mark(_lock: &RefLock) -> Result<(), String> {
    file_utils::write_file_atomically(&served_path(), &record_utils::to_bytes(SERVED_RECORD, SERVED_VERSION, Vec::<String>::new()))
}

/// The tombstones of the warehouse at the current storage root. Empty in a root never served, or
/// one whose first serve did not finish. Lock-free: each file is replaced atomically, so a reader
/// sees one whole version; a caller that decides a write from what it read reads again under the
/// lock.
///
/// # Returns
/// * `Ok(Tombstones)` - The tombstones and kept heads.
/// * `Err(String)`    - The damage [`load`] refuses.
pub fn read_tombstones() -> Result<Tombstones, String> {
    load().map(|(_, tombstones)| tombstones)
}

/// The heads every tombstone and kept head holds, which gc, compaction and prune keep (see
/// [`read_tombstones`]).
pub fn tombstone_heads() -> Result<Vec<String>, String> {
    Ok(read_tombstones()?.heads())
}

/// Replace the tombstone list, durably. Only under the ref lock.
pub(crate) fn write_tombstones(_lock: &RefLock, tombstones: &Tombstones) -> Result<(), String> {
    let named = tombstones.named.iter().map(|(pallet, head)| format!("{} {}", head, pallet));
    let lines: Vec<String> = named.chain(tombstones.kept.iter().cloned()).collect();

    file_utils::write_file_atomically(&tombstones_path(), &record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, lines))
}

/// Take the serve lock, then the ref lock, for a rewrite of the list at the served root's own disk.
/// The serve lock is the one gc takes, so the rewrite refuses while a server serves the root; the
/// ref lock keeps a removal in a bay meanwhile from losing its own tombstone.
fn rewrite_locks(what: &str) -> Result<(ServeLock, RefLock), String> {
    let serve_lock = ServeLock::acquire().map_err(|e| format!("Refusing to {}: {}", what, e))?;

    Ok((serve_lock, RefLock::acquire()?))
}

/// Drop the tombstone of pallet `wire` in the warehouse at the current storage root: an explicit,
/// destructive act at the served root's own disk. The name is free for unrelated history from then
/// on, and the history the tombstone held is collectable unless something else reaches it.
/// Refused while a server serves the root ([`rewrite_locks`]).
///
/// # Returns
/// * `Ok(String)`  - The head the dropped tombstone held.
/// * `Err(String)` - The root is being served, no tombstone has that name, or the list could not
///                   be read or written.
pub fn drop_tombstone(wire: &str) -> Result<String, String> {
    let (_serve_lock, lock) = rewrite_locks("drop a tombstone")?;

    let mut tombstones = read_tombstones()?;

    let Some(head) = tombstones.named.remove(wire) else {
        return Err(format!("No deleted pallet \"{}\" has a tombstone in this warehouse.", wire));
    };

    write_tombstones(&lock, &tombstones)?;

    Ok(head)
}

/// Release kept head `head` in the warehouse at the current storage root, as [`drop_tombstone`]
/// drops a tombstone: the history it held is collectable from then on unless something else
/// reaches it.
///
/// # Returns
/// * `Ok(())`      - The head is no longer kept.
/// * `Err(String)` - The root is being served, the list keeps no such head, or it could not be
///                   read or written.
pub fn drop_kept_head(head: &str) -> Result<(), String> {
    let (_serve_lock, lock) = rewrite_locks("drop a kept head")?;

    let mut tombstones = read_tombstones()?;

    if !tombstones.kept.remove(head) {
        return Err(format!("The tombstone list of this warehouse keeps no head {} without a name.", head));
    }

    write_tombstones(&lock, &tombstones)
}

/// Bring deleted pallet `wire` back at the head its tombstone holds, in the warehouse at the
/// current storage root (`forklift pallet restore`): the creation acknowledges the tombstone
/// and clears it. Refused, as undo's restore is, while a bay stands on the name out of step with
/// that head, or when the head's history no longer passes the warehouse's trust check or is no
/// longer complete.
///
/// # Returns
/// * `Ok(String)`  - The head the pallet is back at.
/// * `Err(String)` - The refusal or failure; nothing was written, unless the write failed after
///                   landing and the name reads back at the head.
pub fn restore_pallet(wire: &str) -> Result<String, String> {
    use crate::util::journal_utils;
    use crate::util::pallet_utils::{self, RefMoved, RefUpdateError};

    pallet_utils::validate_pallet_name(wire)?;

    // Only a served root's tombstones bind its ref writes, so only those are restored.
    if !is_served()? {
        return Err(format!(
            "This warehouse has never been served, so it keeps no tombstones: pallet \"{}\" cannot be \
            restored from one. A pallet deleted here is brought back with \"forklift undo\".",
            wire
        ));
    }

    let Some(head) = read_tombstones()?.named.remove(wire) else {
        return Err(format!("No deleted pallet \"{}\" has a tombstone in this warehouse.", wire));
    };

    let closing = "Nothing was restored.";
    let rerun = format!("forklift pallet restore {}", wire);
    journal_utils::refuse_a_bay_out_of_step(wire, &head, "restore", &rerun, closing)?;
    let preconditions = journal_utils::trust_preconditions(wire, &head, closing)?;

    match pallet_utils::recreate_pallet_head(wire, &head, &preconditions, &head) {
        Ok(()) => Ok(head),
        // A creation only reports a move when the ref exists.
        Err(RefUpdateError::Moved(RefMoved { found: Some(found), .. })) => Err(format!(
            "Pallet \"{}\" exists again, at {}; it is not overwritten. {}",
            wire, found, closing
        )),
        Err(RefUpdateError::Incomplete { missing, .. }) => Err(format!(
            "The history of deleted pallet \"{}\" ({}) is no longer complete in this warehouse: {} {}",
            wire, head, missing, closing
        )),
        Err(RefUpdateError::Failed(error)) => match pallet_utils::get_pallet_head(wire) {
            Ok(Some(found)) if found == head => Ok(head),
            _ => Err(error),
        },
        Err(error) => Err(error.into()),
    }
}

/// Refuse to name a new unborn pallet `wire` when the name has a tombstone. A pallet created at a
/// head meets the tombstone in its ref write; an unborn one writes no ref until its first stack,
/// which the tombstone would refuse only after the work was done.
///
/// # Returns
/// * `Ok(())`      - The name has no tombstone.
/// * `Err(String)` - The refusal, naming the deleted head and the way out, or why the tombstones
///                   could not be read.
pub fn refuse_a_tombstoned_name(wire: &str) -> Result<(), String> {
    // Bound as a ref write is: only a served root's tombstones count.
    if !is_served()? {
        return Ok(());
    }

    match read_tombstones()?.named.get(wire) {
        Some(head) => Err(format!(
            "Pallet \"{}\" was deleted in this served warehouse, at {}, and the name is kept for it. \
            Choose another name, or bring the pallet back with \"forklift pallet restore {}\". \
            Nothing was created.",
            wire, head, wire
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    pub(crate) struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        pub(crate) fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-tombstone-test-{}-{}", name, std::process::id()));
            let _ = file_utils::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = file_utils::remove_dir_all(&self.root);
        }
    }

    fn named(pairs: &[(&str, &str)]) -> Tombstones {
        Tombstones {
            named: pairs.iter().map(|(name, head)| (name.to_string(), head.to_string())).collect(),
            kept: BTreeSet::new(),
        }
    }

    /// Mark the scratch root served and write `tombstones` as its list.
    fn served_with(tombstones: &Tombstones) {
        assert!(mark_served(false).unwrap());
        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, tombstones).unwrap();
    }

    fn write_list(lines: &[&str]) {
        std::fs::write(tombstones_path(), record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, lines.iter().copied())).unwrap();
    }

    fn assert_damaged(error: &str, needle: &str) {
        let (code, human, next_step) = crate::error::deframe(error).unwrap_or_else(|| panic!("not a coded refusal: {}", error));
        assert_eq!(code, "tombstones_damaged", "{}", error);
        assert!(human.contains(needle), "{}", human);
        assert!(next_step.starts_with("forklift-server tombstone repair --root "), "{}", next_step);
    }

    #[test]
    fn the_mark_is_written_once_and_read_fail_closed() {
        let _scratch = Scratch::new("mark");

        assert!(!is_served().unwrap(), "a root never served carries no mark");
        assert!(mark_served(false).unwrap());
        assert!(mark_served(false).unwrap());
        assert!(is_served().unwrap(), "the mark stands");

        std::fs::write(served_path(), b"something else\n").unwrap();
        assert_damaged(&is_served().unwrap_err(), FILE_NAME_SERVED);
    }

    #[test]
    fn a_first_mark_creates_a_missing_list_and_never_overwrites_one() {
        let _scratch = Scratch::new("mark-list");

        assert!(!tombstones_path().exists());
        assert!(mark_served(false).unwrap());
        assert_eq!(read_record(&tombstones_path(), TOMBSTONES_RECORD, TOMBSTONES_VERSION).unwrap(), Some(Vec::new()), "the mark brings an empty list");

        // A list already there, beside a lost mark, is never overwritten: it is damage.
        let head = "a".repeat(64);
        crate::util::file_utils::remove_file(served_path()).unwrap();
        write_list(&[&format!("{} gone", head)]);
        assert_damaged(&mark_served(false).unwrap_err(), "the served mark");
        assert_eq!(read_record(&tombstones_path(), TOMBSTONES_RECORD, TOMBSTONES_VERSION).unwrap(), Some(vec![format!("{} gone", head)]));
    }

    #[test]
    fn a_torn_first_serve_is_completed_by_the_next_mark() {
        let _scratch = Scratch::new("torn");

        // A first serve writes the record of withheld content before the list.
        withholding::create_if_absent(&RefLock::acquire().unwrap()).unwrap();
        write_list(&[]);
        assert_eq!(served_state().unwrap(), ServedState::Unfinished);
        assert!(!is_served().unwrap(), "nothing was served before the mark");
        assert_damaged(&served_for_removal().unwrap_err(), "did not finish");

        assert!(mark_served(false).unwrap());
        assert_eq!(served_state().unwrap(), ServedState::Served);
        assert!(served_for_removal().unwrap());
    }

    // Revert: write the record of withheld content after the list, or not at all → the first
    // serve leaves a served root with none, read as nothing withheld; or skip the read before the
    // mark → a root is marked served over a lost record.
    #[test]
    fn the_first_serve_writes_the_withheld_record_first_and_never_marks_over_a_lost_one() {
        let _scratch = Scratch::new("withheld-witness");

        assert!(withholding::read_withheld().unwrap().is_empty(), "a root never served reads none as nothing withheld");
        assert!(mark_served(true).unwrap());
        assert!(globals::forklift_root().join("withheld").is_file(), "the first serve writes the record");
        assert!(withholding::read_withheld().unwrap().is_empty());

        file_utils::remove_file(globals::forklift_root().join("withheld")).unwrap();
        assert!(withholding::is_damage(&withholding::read_withheld().unwrap_err()), "lost from a served root");
        assert!(mark_served(true).unwrap(), "a marked root is answered without reading the record");
        assert!(!globals::forklift_root().join("withheld").exists(), "never written anew over a loss");

        // A torn first serve that lost its record is not completed over the loss.
        file_utils::remove_file(served_path()).unwrap();
        assert_eq!(served_state().unwrap(), ServedState::Unfinished);
        assert!(withholding::is_damage(&mark_served(true).unwrap_err()));
        assert_eq!(served_state().unwrap(), ServedState::Unfinished, "no mark written");
    }

    #[test]
    fn a_lost_mark_beside_tombstones_is_damage() {
        let _scratch = Scratch::new("lost-mark");

        served_with(&named(&[("gone", &"a".repeat(64))]));
        crate::util::file_utils::remove_file(served_path()).unwrap();

        for error in [is_served().unwrap_err(), served_for_removal().unwrap_err(), read_tombstones().unwrap_err()] {
            assert_damaged(&error, "the served mark");
        }
    }

    /// Revert: the old note, which said a newer forklift may have written the list and named no
    /// repair → red.
    #[test]
    fn a_tolerant_root_walk_over_damaged_tombstones_notes_the_repair() {
        use crate::util::{bay_utils::BayReadPolicy, office_utils::TrustPinReadPolicy, recovery_utils};
        let _scratch = Scratch::new("tolerant-walk");
        served_with(&Tombstones::default());
        std::fs::write(tombstones_path(), b"\xff\xfe not a list").unwrap();

        let refused = recovery_utils::gc_root_sources(&[], BayReadPolicy::FailClosed, TrustPinReadPolicy::FailClosed).err().unwrap_or_default();
        assert!(is_damage(&refused), "gc and prune refuse: {}", refused);

        let tolerated = recovery_utils::gc_root_sources(&[], BayReadPolicy::Tolerate, TrustPinReadPolicy::Tolerate).unwrap();
        assert!(
            tolerated.degraded.iter().any(|note| note.contains("forklift-server tombstone repair --root") && note.contains("nothing was freed")),
            "{:?}", tolerated.degraded
        );
    }

    #[test]
    fn a_served_root_without_a_list_is_damage() {
        let _scratch = Scratch::new("lost-list");

        assert!(mark_served(false).unwrap());
        crate::util::file_utils::remove_file(tombstones_path()).unwrap();

        assert_damaged(&read_tombstones().unwrap_err(), "is missing");
        assert_damaged(&tombstone_heads().unwrap_err(), "is missing");
    }

    #[test]
    fn a_first_mark_under_a_held_ref_lock_is_refused_not_waited_for() {
        let _scratch = Scratch::new("busy");

        let lock = RefLock::acquire().unwrap();
        assert!(!mark_served(false).unwrap(), "a bay holds the ref lock, so the mark must not wait");
        assert!(!is_served().unwrap(), "nothing is written while the lock is held");
        assert!(!tombstones_path().exists(), "nor the list");

        drop(lock);
        assert!(mark_served(false).unwrap());

        let _lock = RefLock::acquire().unwrap();
        assert!(mark_served(false).unwrap(), "a marked root is answered without the lock");
    }

    #[test]
    fn a_waiting_first_mark_waits_for_the_ref_lock() {
        let _scratch = Scratch::new("wait");

        // The bay's hold is taken on its own thread: a ref lock stays on the thread that took it.
        let root = crate::globals::current_scope_root().unwrap();
        let (locked, held) = std::sync::mpsc::channel();
        let releaser = std::thread::spawn(move || {
            let _scope = crate::globals::StorageRootScope::enter(&root);
            let _lock = RefLock::acquire().unwrap();
            locked.send(()).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
        });

        held.recv().unwrap();
        assert!(mark_served(true).unwrap(), "a waiting mark is written once the bay lets go");
        assert!(is_served().unwrap());
        releaser.join().unwrap();
    }

    #[test]
    fn a_tombstoned_name_is_refused_for_a_new_unborn_pallet_only_in_a_served_root() {
        let _scratch = Scratch::new("reserved");
        let head = "a".repeat(64);

        refuse_a_tombstoned_name("gone").unwrap();

        served_with(&named(&[("gone", &head)]));
        let refused = refuse_a_tombstoned_name("gone").unwrap_err();
        assert!(refused.contains(&head) && refused.contains("forklift pallet restore gone"), "{}", refused);
        refuse_a_tombstoned_name("other").unwrap();

        // A kept head reserves no name.
        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, &Tombstones { named: BTreeMap::new(), kept: BTreeSet::from([head]) }).unwrap();
        drop(lock);
        refuse_a_tombstoned_name("gone").unwrap();
    }

    #[test]
    fn a_restore_in_a_root_never_served_is_refused() {
        let _scratch = Scratch::new("restore-unserved");

        let refused = restore_pallet("gone").unwrap_err();
        assert!(refused.contains("never been served"), "{}", refused);
        assert_eq!(crate::util::pallet_utils::get_pallet_head("gone").unwrap(), None);
    }

    #[test]
    fn tombstones_round_trip_and_a_malformed_entry_is_refused() {
        let _scratch = Scratch::new("list");
        let (head, kept) = ("a".repeat(64), "c".repeat(64));

        assert!(read_tombstones().unwrap().is_empty(), "a root never served has none");

        let written = Tombstones {
            named: BTreeMap::from([("feature/x".to_string(), head.clone())]),
            kept: BTreeSet::from([kept.clone()]),
        };
        served_with(&written);

        assert_eq!(read_tombstones().unwrap(), written);
        assert_eq!(tombstone_heads().unwrap(), vec![head.clone(), kept.clone()], "kept heads are kept too");

        write_list(&["nothash main"]);
        assert_damaged(&read_tombstones().unwrap_err(), "neither a parcel hash and a pallet name");

        let bad_name = format!("{} bad name", head);
        write_list(&[bad_name.as_str()]);
        assert_damaged(&read_tombstones().unwrap_err(), "neither a parcel hash and a pallet name");

        let (first, second) = (format!("{} main", head), format!("{} main", "b".repeat(64)));
        write_list(&[first.as_str(), second.as_str()]);
        assert_damaged(&read_tombstones().unwrap_err(), "two tombstones of pallet \"main\"");

        write_list(&[kept.as_str(), kept.as_str()]);
        assert_damaged(&read_tombstones().unwrap_err(), "twice");

        std::fs::write(tombstones_path(), b"\xff\xfe not text").unwrap();
        assert_damaged(&read_tombstones().unwrap_err(), FILE_NAME_TOMBSTONES);

        // A list of the version before kept heads is not read as this one.
        std::fs::write(tombstones_path(), record_utils::to_bytes(TOMBSTONES_RECORD, 1, [first.as_str()])).unwrap();
        assert_damaged(&read_tombstones().unwrap_err(), FILE_NAME_TOMBSTONES);
    }

    #[test]
    fn a_drop_removes_only_its_tombstone_and_refuses_an_unknown_name() {
        let _scratch = Scratch::new("drop");
        let (a, b) = ("a".repeat(64), "b".repeat(64));

        served_with(&named(&[("x", &a), ("y", &b)]));

        assert_eq!(drop_tombstone("x").unwrap(), a);
        assert_eq!(read_tombstones().unwrap(), named(&[("y", &b)]));

        let error = drop_tombstone("x").unwrap_err();
        assert!(error.contains("No deleted pallet \"x\""), "{}", error);
    }

    #[test]
    fn a_kept_head_is_dropped_by_its_head() {
        let _scratch = Scratch::new("drop-kept");
        let (a, b) = ("a".repeat(64), "b".repeat(64));

        served_with(&Tombstones { named: BTreeMap::from([("x".to_string(), a.clone())]), kept: BTreeSet::from([b.clone()]) });

        let error = drop_kept_head(&a).unwrap_err();
        assert!(error.contains("keeps no head"), "a tombstone's head is not a kept head: {}", error);

        drop_kept_head(&b).unwrap();
        assert_eq!(read_tombstones().unwrap(), named(&[("x", &a)]), "only the kept head went");
        assert!(drop_kept_head(&b).unwrap_err().contains("keeps no head"));
    }

    #[test]
    fn a_drop_refuses_while_the_root_is_served() {
        let _scratch = Scratch::new("drop-served");
        served_with(&Tombstones { named: BTreeMap::from([("x".to_string(), "a".repeat(64))]), kept: BTreeSet::from(["b".repeat(64)]) });

        let _server = ServeLock::acquire().unwrap();
        let error = drop_tombstone("x").unwrap_err();
        assert!(error.starts_with("Refusing to drop a tombstone"), "{}", error);
        let error = drop_kept_head(&"b".repeat(64)).unwrap_err();
        assert!(error.starts_with("Refusing to drop a kept head"), "{}", error);
        assert_eq!(read_tombstones().unwrap().heads().len(), 2, "nothing was dropped");
    }
}
