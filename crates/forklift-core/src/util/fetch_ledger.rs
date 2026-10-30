//! The fetched-heads ledger of a sparse warehouse.
//!
//! A history fetch in a sparse warehouse stops at every local ref head, on the understanding that
//! a ref's closure is complete at the current fetch scope. `expand` widens that scope, so it
//! re-walks every ref head under the widened scope before recording it. A closure can outlive the
//! command that fetched it without a ref, though: `lower` fetches the remote head and then refuses
//! as diverged, and the refusal's own next step palletizes that head through a write that fetches
//! nothing. If an `expand` ran in between, that head's newly in-scope content would never be
//! fetched. So every scoped fetch records its head here, under the
//! [`RefLock`](crate::util::lock_utils::RefLock), before it transfers anything, and `expand` walks
//! the recorded heads as well as the ref heads. A ref write that leaves a head no longer reached
//! from its ref records that head too, since an `undo` can write it back without fetching.
//!
//! A recorded head that no ref reaches and that the remote can no longer serve cannot be walked;
//! `expand` drops it rather than failing. Nothing here guards against such a head becoming a ref:
//! the ref compare-and-set verifies a new head's closure at the current fetch scope, so a head that
//! is not complete there is refused whatever fetched it.
//!
//! The file lives in the shared forklift root, so every bay sees the same one, and it is a
//! [`record_utils`] file of one `<head> <pallet>` entry per line. A full warehouse records nothing,
//! so the file does not exist there.

use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::RefLock;
use crate::util::{file_utils, object_utils, origin_move, record_utils};

/// The ledger file's name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_LEDGER: &str = "fetched-heads";
const LEDGER_RECORD: &str = "fetched-heads";
const LEDGER_VERSION: u32 = 1;

/// One head the ledger recorded: the head, and the pallet it was fetched for or left by. The
/// pallet is kept for diagnosis; the ledger holds each head once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LedgerEntry {
    /// The parcel hash the fetch walked from.
    pub head: String,

    /// The pallet the fetch was for, as its wire name.
    pub pallet: String,
}

fn ledger_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_LEDGER)
}

/// Read a record file's entries, or none when the file is absent.
fn read_entries(path: &PathBuf, kind: &str, version: u32) -> Result<Vec<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    record_utils::from_bytes(&bytes, kind, version)
        .map_err(|e| format!("The file \"{}\" {}", path.to_string_lossy(), e.describe()))
}

/// The heads the ledger holds, in the order they were recorded. Lock-free: the files are replaced
/// atomically, so a reader sees one whole version; a caller that decides a write from what it read
/// reads again under the lock.
pub fn read_ledger() -> Result<Vec<LedgerEntry>, String> {
    let path = ledger_path();

    read_entries(&path, LEDGER_RECORD, LEDGER_VERSION)?
        .into_iter()
        .map(|line| match line.split_once(' ') {
            Some((head, pallet)) if object_utils::is_object_hash(head) && !pallet.is_empty() => Ok(LedgerEntry {
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

/// Replace the ledger. Only under the ref lock.
pub fn write_ledger(lock: &RefLock, entries: &[LedgerEntry]) -> Result<(), String> {
    origin_move::refuse_if_pending(lock)?;

    let lines = entries.iter().map(|entry| format!("{} {}", entry.head, entry.pallet));

    file_utils::write_file_atomically(&ledger_path(), &record_utils::to_bytes(LEDGER_RECORD, LEDGER_VERSION, lines))
}

/// Add `head` to the ledger, unless it is there already: a scoped fetch about to run, in the same
/// hold that read the fetch scope it will use, or a head a ref write is leaving. Only under the
/// ref lock.
///
/// # Arguments
/// * `lock`   - The held ref lock.
/// * `head`   - The parcel hash.
/// * `pallet` - The pallet it is fetched for or left by, as its wire name.
pub fn record_fetch(lock: &RefLock, head: &str, pallet: &str) -> Result<(), String> {
    let mut ledger = read_ledger()?;

    if !ledger.iter().any(|entry| entry.head == head) {
        ledger.push(LedgerEntry { head: head.to_string(), pallet: pallet.to_string() });
        write_ledger(lock, &ledger)?;
    }

    Ok(())
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
            let root = std::env::temp_dir().join(format!("forklift-ledger-test-{}-{}", name, std::process::id()));
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
    fn a_recorded_head_is_listed_once() {
        let _scratch = Scratch::new("record");
        let head = "a".repeat(64);

        assert!(read_ledger().unwrap().is_empty(), "an absent ledger is empty");

        let lock = RefLock::acquire().unwrap();
        record_fetch(&lock, &head, "main").unwrap();
        record_fetch(&lock, &head, "side").unwrap();
        drop(lock);

        assert_eq!(read_ledger().unwrap(), vec![LedgerEntry { head: head.clone(), pallet: "main".to_string() }]);
    }

    #[test]
    fn a_malformed_ledger_entry_is_refused_not_skipped() {
        let _scratch = Scratch::new("malformed");
        std::fs::write(ledger_path(), record_utils::to_bytes(LEDGER_RECORD, LEDGER_VERSION, ["nothash main"])).unwrap();

        let error = read_ledger().unwrap_err();
        assert!(error.contains("not a parcel hash"), "{}", error);
    }
}
