//! The fetched-heads ledger and the fence list of a sparse warehouse.
//!
//! A history fetch in a sparse warehouse stops at every local ref head, on the understanding that
//! a ref's closure is complete at the current fetch scope. `expand` widens that scope, so it
//! re-walks every ref head under the widened scope before recording it. A closure can outlive the
//! command that fetched it without a ref, though: `lower` fetches the remote head and then refuses
//! as diverged, and the refusal's own next step palletizes that head through a write that fetches
//! nothing. If an `expand` ran in between, that head's newly in-scope content would never be
//! fetched. So every scoped fetch records its head here, under the
//! [`RefLock`](crate::util::lock_utils::RefLock), before it transfers anything, and `expand` walks
//! the recorded heads as well as the ref heads.
//!
//! A recorded head that no ref reaches and that the remote can no longer serve cannot be walked.
//! `expand` moves it to the fence list instead of failing, and the ref compare-and-set refuses a
//! fenced head as a new user-pallet head, until a scoped fetch of that head records it again.
//!
//! Both files live in the shared forklift root, so every bay sees the same ones, and both are
//! [`record_utils`] files: the ledger one `<head> <pallet>` entry per line, the fence list one
//! head per line. A full warehouse records nothing, so neither file exists there.

use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::RefLock;
use crate::util::{file_utils, object_utils, record_utils};

/// The ledger file's name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_LEDGER: &str = "fetched-heads";
const LEDGER_RECORD: &str = "fetched-heads";
const LEDGER_VERSION: u32 = 1;

/// The fence list's name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_FENCE: &str = "fenced-heads";
const FENCE_RECORD: &str = "fenced-heads";
const FENCE_VERSION: u32 = 1;

/// One scoped fetch the ledger recorded: the head it fetched, and the pallet it fetched it for.
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

fn fence_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_FENCE)
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

/// The fenced heads. Lock-free, like [`read_ledger`].
pub fn read_fence() -> Result<Vec<String>, String> {
    let path = fence_path();
    let heads = read_entries(&path, FENCE_RECORD, FENCE_VERSION)?;

    if let Some(bad) = heads.iter().find(|head| !object_utils::is_object_hash(head)) {
        return Err(format!(
            "The file \"{}\" holds an entry that is not a parcel hash: \"{}\".",
            path.to_string_lossy(), bad.escape_debug()
        ));
    }

    Ok(heads)
}

/// Replace the ledger. Only under the ref lock.
pub fn write_ledger(_lock: &RefLock, entries: &[LedgerEntry]) -> Result<(), String> {
    let lines = entries.iter().map(|entry| format!("{} {}", entry.head, entry.pallet));

    file_utils::write_file_atomically(&ledger_path(), &record_utils::to_bytes(LEDGER_RECORD, LEDGER_VERSION, lines))
}

/// Replace the fence list. Only under the ref lock.
pub fn write_fence(_lock: &RefLock, heads: &[String]) -> Result<(), String> {
    file_utils::write_file_atomically(&fence_path(), &record_utils::to_bytes(FENCE_RECORD, FENCE_VERSION, heads))
}

/// Record that a scoped fetch of `head` for `pallet` is about to run: add it to the ledger, unless
/// it is there already, and take it off the fence list, since this fetch is what makes it whole.
/// Only under the ref lock, in the same hold that read the fetch scope the fetch will use.
///
/// # Arguments
/// * `lock`   - The held ref lock.
/// * `head`   - The parcel hash about to be fetched.
/// * `pallet` - The pallet it is fetched for, as its wire name.
pub fn record_fetch(lock: &RefLock, head: &str, pallet: &str) -> Result<(), String> {
    let mut ledger = read_ledger()?;
    let entry = LedgerEntry { head: head.to_string(), pallet: pallet.to_string() };

    if !ledger.contains(&entry) {
        ledger.push(entry);
        write_ledger(lock, &ledger)?;
    }

    let mut fence = read_fence()?;

    if fence.iter().any(|fenced| fenced == head) {
        fence.retain(|fenced| fenced != head);
        write_fence(lock, &fence)?;
    }

    Ok(())
}

/// Whether `head` is on the fence list.
pub fn is_fenced(head: &str) -> Result<bool, String> {
    Ok(read_fence()?.iter().any(|fenced| fenced == head))
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
    fn a_recorded_fetch_is_listed_once_and_leaves_the_fence() {
        let _scratch = Scratch::new("record");
        let head = "a".repeat(64);
        let other = "b".repeat(64);

        assert!(read_ledger().unwrap().is_empty(), "an absent ledger is empty");

        let lock = RefLock::acquire().unwrap();
        write_fence(&lock, &[head.clone(), other.clone()]).unwrap();
        record_fetch(&lock, &head, "main").unwrap();
        record_fetch(&lock, &head, "main").unwrap();
        drop(lock);

        assert_eq!(read_ledger().unwrap(), vec![LedgerEntry { head: head.clone(), pallet: "main".to_string() }]);
        assert_eq!(read_fence().unwrap(), vec![other.clone()], "only the fetched head leaves the fence");
        assert!(!is_fenced(&head).unwrap());
        assert!(is_fenced(&other).unwrap());
    }

    #[test]
    fn a_malformed_ledger_entry_is_refused_not_skipped() {
        let _scratch = Scratch::new("malformed");
        std::fs::write(ledger_path(), record_utils::to_bytes(LEDGER_RECORD, LEDGER_VERSION, ["nothash main"])).unwrap();

        let error = read_ledger().unwrap_err();
        assert!(error.contains("not a parcel hash"), "{}", error);
    }
}
