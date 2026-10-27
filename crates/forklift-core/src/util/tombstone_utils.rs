//! The served mark and the tombstones of a served root.
//!
//! A client that has seen a pallet's head on a remote may free its own copy of that history
//! (scope-prune, a delete's containment check): the remote holds it. A ref removed on a served root
//! would break that promise if nothing kept the removed head, since gc frees what no root reaches.
//! So in a root that has been served, every ref removal leaves a tombstone: the pallet's wire name
//! and the head its ref held. The tombstone is a gc root, the handshake lists it, and the name
//! stays reserved for that history: re-creating it must descend from the tombstone's head and
//! acknowledge it ([`pallet_utils::update_pallet_head_in`](crate::util::pallet_utils::update_pallet_head_in)
//! states the rule). A tombstone stands until such a re-creation clears it, or until someone at the
//! served root's own disk drops it ([`drop_tombstone`]). Nothing expires one: a clone that pruned
//! against it would lose the only copy.
//!
//! Only a served root carries tombstones. `forklift-server` marks a root as served before it first
//! serves it ([`mark_served`]) and nothing clears the mark; a root never served writes none,
//! because no clone can have pruned against what nobody served. The mark is read fail-closed: one
//! that exists but cannot be read is an error, never "not served".
//!
//! Both files live in the shared forklift root, so every bay sees them, and both are
//! [`record_utils`] files: the mark has no entries, and the tombstone list one `<head> <pallet>`
//! entry per line. Every write of the list is made under the
//! [`RefLock`], which orders it against every ref write.

use std::collections::BTreeMap;
use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::{RefLock, ServeLock};
use crate::util::{file_utils, object_utils, pallet_utils, record_utils};

/// The mark's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_SERVED: &str = "served";
const SERVED_RECORD: &str = "served";
const SERVED_VERSION: u32 = 1;

/// The tombstone list's file name in the shared forklift root, and its kind and version.
const FILE_NAME_TOMBSTONES: &str = "tombstones";
const TOMBSTONES_RECORD: &str = "tombstones";
const TOMBSTONES_VERSION: u32 = 1;

fn served_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_SERVED)
}

fn tombstones_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_TOMBSTONES)
}

/// Read a record file's entries; `None` when the file is absent.
fn read_record(path: &PathBuf, kind: &str, version: u32) -> Result<Option<Vec<String>>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    record_utils::from_bytes(&bytes, kind, version)
        .map(Some)
        .map_err(|e| format!("The file \"{}\" {}", path.to_string_lossy(), e.describe()))
}

/// Whether the warehouse at the current storage root has been served.
///
/// # Returns
/// * `Ok(bool)`    - Whether the mark is there.
/// * `Err(String)` - The mark exists but could not be read, or is not a mark this build reads.
pub fn is_served() -> Result<bool, String> {
    read_record(&served_path(), SERVED_RECORD, SERVED_VERSION).map(|mark| mark.is_some())
}

/// Mark the warehouse at the current storage root as served, durably, unless it is already. Under
/// the [`RefLock`], so a ref removal in a bay reads the mark either before it is written (and the
/// server then serves a root without that ref) or after it (and the removal leaves a tombstone).
/// A root already marked is answered without the lock. Unless `wait`, the lock is tried, never
/// waited for: the server's multi-root registry calls this under its mutex, so a bay holding the
/// lock makes the first serve retry rather than stall every other warehouse.
///
/// # Returns
/// * `Ok(true)`    - The root is marked.
/// * `Ok(false)`   - Not `wait`, the root is unmarked and a bay holds the ref lock; nothing was
///                   written.
/// * `Err(String)` - The mark could not be read or written.
pub fn mark_served(wait: bool) -> Result<bool, String> {
    if is_served()? {
        return Ok(true);
    }

    let _lock = match wait {
        true => RefLock::acquire()?,
        false => match RefLock::try_acquire_in(&globals::forklift_root())? {
            Some(lock) => lock,
            None => return Ok(false),
        },
    };

    if is_served()? {
        return Ok(true);
    }

    file_utils::write_file_atomically(&served_path(), &record_utils::to_bytes(SERVED_RECORD, SERVED_VERSION, Vec::<String>::new()))?;

    Ok(true)
}

/// The tombstones of the warehouse at the current storage root: each removed pallet's wire name,
/// mapped to the head its ref held. Empty when there are none, as in every root never served.
/// Lock-free: the file is replaced atomically, so a reader sees one whole version; a caller that
/// decides a write from what it read reads again under the lock.
///
/// # Returns
/// * `Ok(BTreeMap<String, String>)` - The tombstones.
/// * `Err(String)`                  - The list exists but could not be read, holds an entry that is
///                                    not a parcel hash and a pallet name, or names a pallet twice.
pub fn read_tombstones() -> Result<BTreeMap<String, String>, String> {
    let path = tombstones_path();
    let mut tombstones = BTreeMap::new();

    for line in read_record(&path, TOMBSTONES_RECORD, TOMBSTONES_VERSION)?.unwrap_or_default() {
        match line.split_once(' ') {
            Some((head, pallet)) if object_utils::is_object_hash(head) && pallet_utils::validate_pallet_name(pallet).is_ok() => {
                // Two tombstones of one name would leave one head unrooted, so neither is chosen.
                if tombstones.insert(pallet.to_string(), head.to_string()).is_some() {
                    return Err(format!(
                        "The file \"{}\" holds two tombstones of pallet \"{}\".",
                        path.to_string_lossy(), pallet
                    ));
                }
            }
            _ => return Err(format!(
                "The file \"{}\" holds an entry that is not a parcel hash and a pallet name: \"{}\".",
                path.to_string_lossy(), line.escape_debug()
            )),
        }
    }

    Ok(tombstones)
}

/// The heads every tombstone holds, which gc, compaction and prune keep (see [`read_tombstones`]).
pub fn tombstone_heads() -> Result<Vec<String>, String> {
    Ok(read_tombstones()?.into_values().collect())
}

/// Replace the tombstone list, durably. Only under the ref lock.
pub(crate) fn write_tombstones(_lock: &RefLock, tombstones: &BTreeMap<String, String>) -> Result<(), String> {
    let lines = tombstones.iter().map(|(pallet, head)| format!("{} {}", head, pallet));

    file_utils::write_file_atomically(&tombstones_path(), &record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, lines))
}

/// Drop the tombstone of pallet `wire` in the warehouse at the current storage root: an explicit,
/// destructive act at the served root's own disk. The name is free for unrelated history from then
/// on, and the history the tombstone held is collectable unless something else reaches it. Takes
/// the serve lock, as gc does, so it refuses while a server serves the root, then rewrites the list
/// under the ref lock, so a removal in a bay meanwhile keeps its own tombstone.
///
/// # Returns
/// * `Ok(String)`  - The head the dropped tombstone held.
/// * `Err(String)` - The root is being served, no tombstone has that name, or the list could not
///                   be read or written.
pub fn drop_tombstone(wire: &str) -> Result<String, String> {
    let _serve_lock = ServeLock::acquire()
        .map_err(|e| format!("Refusing to drop a tombstone: {}", e))?;
    let lock = RefLock::acquire()?;

    let mut tombstones = read_tombstones()?;

    let Some(head) = tombstones.remove(wire) else {
        return Err(format!("No deleted pallet \"{}\" has a tombstone in this warehouse.", wire));
    };

    write_tombstones(&lock, &tombstones)?;

    Ok(head)
}

/// Bring deleted pallet `wire` back at the head its tombstone holds, in the warehouse at the
/// current storage root (`forklift palletize --restore`): the creation acknowledges the tombstone
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

    let Some(head) = read_tombstones()?.remove(wire) else {
        return Err(format!("No deleted pallet \"{}\" has a tombstone in this warehouse.", wire));
    };

    let closing = "Nothing was restored.";
    let rerun = format!("forklift palletize --restore {}", wire);
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
        Err(error) => Err(error.to_string()),
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

    match read_tombstones()?.get(wire) {
        Some(head) => Err(format!(
            "Pallet \"{}\" was deleted in this served warehouse, at {}, and the name is kept for it. \
            Choose another name, or bring the pallet back with \"forklift palletize --restore {}\". \
            Nothing was created.",
            wire, head, wire
        )),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-tombstone-test-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn the_mark_is_written_once_and_read_fail_closed() {
        let _scratch = Scratch::new("mark");

        assert!(!is_served().unwrap(), "a root never served carries no mark");
        assert!(mark_served(false).unwrap());
        assert!(mark_served(false).unwrap());
        assert!(is_served().unwrap(), "the mark stands");

        std::fs::write(served_path(), b"something else\n").unwrap();
        let error = is_served().unwrap_err();
        assert!(error.contains(FILE_NAME_SERVED), "an unreadable mark is an error, never \"not served\": {}", error);
    }

    #[test]
    fn a_first_mark_under_a_held_ref_lock_is_refused_not_waited_for() {
        let _scratch = Scratch::new("busy");

        let lock = RefLock::acquire().unwrap();
        assert!(!mark_served(false).unwrap(), "a bay holds the ref lock, so the mark must not wait");
        assert!(!is_served().unwrap(), "nothing is written while the lock is held");

        drop(lock);
        assert!(mark_served(false).unwrap());

        let _lock = RefLock::acquire().unwrap();
        assert!(mark_served(false).unwrap(), "a marked root is answered without the lock");
    }

    #[test]
    fn a_waiting_first_mark_waits_for_the_ref_lock() {
        let _scratch = Scratch::new("wait");

        let lock = RefLock::acquire().unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(lock);
        });

        assert!(mark_served(true).unwrap(), "a waiting mark is written once the bay lets go");
        assert!(is_served().unwrap());
        releaser.join().unwrap();
    }

    #[test]
    fn a_tombstoned_name_is_refused_for_a_new_unborn_pallet_only_in_a_served_root() {
        let _scratch = Scratch::new("reserved");
        let head = "a".repeat(64);

        refuse_a_tombstoned_name("gone").unwrap();

        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, &BTreeMap::from([("gone".to_string(), head.clone())])).unwrap();
        drop(lock);
        refuse_a_tombstoned_name("gone").unwrap();

        assert!(mark_served(false).unwrap());
        let refused = refuse_a_tombstoned_name("gone").unwrap_err();
        assert!(refused.contains(&head) && refused.contains("forklift palletize --restore gone"), "{}", refused);
        refuse_a_tombstoned_name("other").unwrap();
    }

    #[test]
    fn tombstones_round_trip_and_a_malformed_entry_is_refused() {
        let _scratch = Scratch::new("list");
        let head = "a".repeat(64);

        assert!(read_tombstones().unwrap().is_empty(), "an absent list is empty");

        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, &BTreeMap::from([("feature/x".to_string(), head.clone())])).unwrap();
        drop(lock);

        assert_eq!(read_tombstones().unwrap(), BTreeMap::from([("feature/x".to_string(), head.clone())]));
        assert_eq!(tombstone_heads().unwrap(), vec![head.clone()]);

        std::fs::write(tombstones_path(), record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, ["nothash main"])).unwrap();
        let error = read_tombstones().unwrap_err();
        assert!(error.contains("not a parcel hash"), "{}", error);

        let bad_name = format!("{} bad name", head);
        std::fs::write(tombstones_path(), record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, [bad_name.as_str()])).unwrap();
        let error = read_tombstones().unwrap_err();
        assert!(error.contains("not a parcel hash and a pallet name"), "{}", error);

        let (first, second) = (format!("{} main", head), format!("{} main", "b".repeat(64)));
        std::fs::write(tombstones_path(), record_utils::to_bytes(TOMBSTONES_RECORD, TOMBSTONES_VERSION, [first.as_str(), second.as_str()])).unwrap();
        let error = read_tombstones().unwrap_err();
        assert!(error.contains("two tombstones of pallet \"main\""), "a duplicate is refused, never collapsed: {}", error);
    }

    #[test]
    fn a_drop_removes_only_its_tombstone_and_refuses_an_unknown_name() {
        let _scratch = Scratch::new("drop");
        let (a, b) = ("a".repeat(64), "b".repeat(64));

        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, &BTreeMap::from([("x".to_string(), a.clone()), ("y".to_string(), b.clone())])).unwrap();
        drop(lock);

        assert_eq!(drop_tombstone("x").unwrap(), a);
        assert_eq!(read_tombstones().unwrap(), BTreeMap::from([("y".to_string(), b)]));

        let error = drop_tombstone("x").unwrap_err();
        assert!(error.contains("No deleted pallet \"x\""), "{}", error);
    }

    #[test]
    fn a_drop_refuses_while_the_root_is_served() {
        let _scratch = Scratch::new("drop-served");
        let lock = RefLock::acquire().unwrap();
        write_tombstones(&lock, &BTreeMap::from([("x".to_string(), "a".repeat(64))])).unwrap();
        drop(lock);

        let _server = ServeLock::acquire().unwrap();
        let error = drop_tombstone("x").unwrap_err();
        assert!(error.starts_with("Refusing to drop a tombstone"), "{}", error);
        assert_eq!(read_tombstones().unwrap().len(), 1, "nothing was dropped");
    }
}
