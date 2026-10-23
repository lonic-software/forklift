//! The held-heads record: remote heads of working pallets this warehouse lifted to or adopted.
//!
//! `scope-prune` counts as held by the remote only history it reaches from the remote's current
//! heads, so once another warehouse lifts past every head held here, this warehouse's history of
//! that pallet looks unlifted and is kept until a `lower`. This record nominates the earlier heads
//! the remote was seen to accept or serve, so a prune can ask the remote whether it still holds
//! them. An entry is only a candidate: a remote head can move backward (an `undo` on the served
//! root) and a pallet deleted and re-created under its name starts a new lineage, so a head seen
//! once is no proof that the remote still holds it, and nothing may be freed on an entry alone.
//!
//! The file lives in the shared forklift root beside the fetched-heads ledger, so every bay records
//! into one, and is a [`record_utils`] file of `<head> <pallet>` lines, oldest first. Per pallet it
//! keeps at most [`CAP_PER_PALLET`] heads: a new head replaces the recorded heads it descends from,
//! a head recorded again moves to the newest, and past the cap the oldest go. Writes are
//! serialized by the [`RefLock`](crate::util::lock_utils::RefLock) and replace the file
//! atomically, so a lock-free reader sees one whole version. A file that does not parse is an error, never an empty record.

use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::RefLock;
use crate::util::{file_utils, merge_utils, object_utils, pallet_utils, record_utils};

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME: &str = "held-heads";
const RECORD: &str = "held-heads";
const VERSION: u32 = 1;

/// The most heads kept for one pallet; past it, the oldest are dropped.
pub const CAP_PER_PALLET: usize = 16;

/// One recorded head.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeldHead {
    /// The parcel hash the remote accepted or served as the pallet's head.
    pub head: String,

    /// The working pallet, as its wire name.
    pub pallet: String,
}

fn record_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME)
}

/// Every recorded head, oldest first, or none when the file is absent. Lock-free.
pub fn read_held_heads() -> Result<Vec<HeldHead>, String> {
    let path = record_path();

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    record_utils::from_bytes(&bytes, RECORD, VERSION)
        .map_err(|e| format!("The file \"{}\" {}", path.to_string_lossy(), e.describe()))?
        .into_iter()
        .map(|line| match line.split_once(' ') {
            Some((head, pallet)) if object_utils::is_object_hash(head) && !pallet.is_empty() => Ok(HeldHead {
                head: head.to_string(),
                pallet: pallet.to_string(),
            }),
            _ => Err(format!(
                "The file \"{}\" holds an entry that is not a parcel hash and a pallet name: \"{}\".",
                path.to_string_lossy(), line.escape_debug()
            )),
        })
        .collect()
}

/// The recorded heads of `pallet` (its wire name), oldest first. Lock-free.
pub fn candidates(pallet: &str) -> Result<Vec<String>, String> {
    Ok(read_held_heads()?
        .into_iter()
        .filter(|entry| entry.pallet == pallet)
        .map(|entry| entry.head)
        .collect())
}

/// Record that the remote accepted or served `head` as the head of `pallet` (its wire name). A
/// meta pallet records nothing: prune's unlifted walk covers only user parcels.
///
/// Call it only once the remote has accepted the head (a lift's compare-and-set returned) or the
/// local ref has adopted it (a `lower`), never before a refusal check: an entry for a head the
/// remote never took is harmless to a prune, which asks the remote, but it is a false record. The
/// caller treats an error as a warning, because the head has landed either way and a lost entry
/// only leaves prune as conservative as it is without one.
///
/// Which recorded heads `head` descends from is decided before the lock is taken, since it walks
/// history; a head another writer adds meanwhile is kept, which only keeps one more candidate.
///
/// # Arguments
/// * `pallet` - The pallet, as its wire name.
/// * `head`   - The parcel hash.
pub fn record_held_head(pallet: &str, head: &str) -> Result<(), String> {
    if pallet.starts_with(pallet_utils::META_QUALIFIER) {
        return Ok(());
    }

    // An ancestry walk that cannot finish keeps the entry: it may still be the one the remote holds.
    let superseded: Vec<String> = candidates(pallet)?
        .into_iter()
        .filter(|recorded| recorded != head && matches!(merge_utils::is_ancestor(recorded, head), Ok(true)))
        .collect();

    let lock = RefLock::acquire()?;
    let mut entries = read_held_heads()?;
    let before = entries.clone();

    entries.retain(|entry| entry.pallet != pallet || (entry.head != head && !superseded.contains(&entry.head)));
    entries.push(HeldHead { head: head.to_string(), pallet: pallet.to_string() });

    let mut excess = entries.iter().filter(|entry| entry.pallet == pallet).count().saturating_sub(CAP_PER_PALLET);
    entries.retain(|entry| {
        if excess > 0 && entry.pallet == pallet {
            excess -= 1;
            return false;
        }
        true
    });

    if entries == before {
        return Ok(());
    }

    write_held_heads(&lock, &entries)
}

/// Replace the record. Only under the ref lock.
fn write_held_heads(_lock: &RefLock, entries: &[HeldHead]) -> Result<(), String> {
    let lines = entries.iter().map(|entry| format!("{} {}", entry.head, entry.pallet));

    file_utils::write_file_atomically(&record_path(), &record_utils::to_bytes(RECORD, VERSION, lines))
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
            let root = std::env::temp_dir().join(format!("forklift-held-heads-test-{}-{}", name, std::process::id()));
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

    fn hash(n: usize) -> String {
        format!("{:064x}", n)
    }

    #[test]
    fn heads_are_kept_per_pallet_once_each_and_a_meta_pallet_records_nothing() {
        let _scratch = Scratch::new("record");

        assert!(read_held_heads().unwrap().is_empty(), "an absent record is empty");

        record_held_head("main", &hash(1)).unwrap();
        record_held_head("feature/x", &hash(2)).unwrap();
        record_held_head("main", &hash(1)).unwrap();
        record_held_head("@manifest", &hash(3)).unwrap();
        record_held_head("@office", &hash(4)).unwrap();

        assert_eq!(read_held_heads().unwrap(), vec![
            HeldHead { head: hash(2), pallet: "feature/x".to_string() },
            HeldHead { head: hash(1), pallet: "main".to_string() },
        ], "recording a head again keeps one entry, moved to the newest");
        assert_eq!(candidates("main").unwrap(), vec![hash(1)]);
        assert_eq!(candidates("feature/x").unwrap(), vec![hash(2)]);
    }

    #[test]
    fn past_the_cap_the_oldest_head_of_that_pallet_goes_and_no_other_pallets() {
        let _scratch = Scratch::new("cap");

        record_held_head("other", &hash(1000)).unwrap();
        // The hashes name no stored parcel, so no ancestry walk finishes and none supersedes another.
        for n in 0..CAP_PER_PALLET + 2 {
            record_held_head("main", &hash(n)).unwrap();
        }

        let expected: Vec<String> = (2..CAP_PER_PALLET + 2).map(hash).collect();
        assert_eq!(candidates("main").unwrap(), expected, "the two oldest went, in order");
        assert_eq!(candidates("other").unwrap(), vec![hash(1000)], "another pallet's head is not dropped");
    }

    #[test]
    fn a_malformed_entry_or_an_unknown_version_is_refused_not_read_as_empty() {
        let _scratch = Scratch::new("malformed");

        std::fs::write(record_path(), record_utils::to_bytes(RECORD, VERSION, ["nothash main"])).unwrap();
        let error = read_held_heads().unwrap_err();
        assert!(error.contains("not a parcel hash"), "{}", error);
        let error = record_held_head("main", &hash(1)).unwrap_err();
        assert!(error.contains("not a parcel hash"), "a write over a malformed record refuses: {}", error);

        let line = format!("{} main", hash(1));
        std::fs::write(record_path(), record_utils::to_bytes(RECORD, VERSION + 1, [line.as_str()])).unwrap();
        assert!(read_held_heads().is_err(), "a newer version is not guessed at");
    }
}
