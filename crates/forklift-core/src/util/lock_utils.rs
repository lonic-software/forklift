use std::io::Write;
use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root};

/// The name of the warehouse lock file (inside the forklift root folder).
const FILE_NAME_LOCK: &str = "lock";

/// The name of the shared object-store lock file (inside the forklift root folder).
const FILE_NAME_STORE_LOCK: &str = "store.lock";

/// The name of the serve lock file (inside the forklift root folder).
const FILE_NAME_SERVE_LOCK: &str = "serve.lock";

/// An exclusive lock on the warehouse, held for the duration of a mutating command.
///
/// This is the staging-area atomicity story (see the design document): commands that
/// mutate the inventory, the object store bookkeeping or pallet refs take this lock, so
/// that e.g. `stack` can never read a staging area that another forklift process is
/// halfway through rewriting. Read-only commands (`peek`, `stocktake`) do not take it —
/// at worst they report an in-flight intermediate state.
///
/// The lock is a lock *file* created with `create_new` (atomic on every platform); it is
/// removed when the guard is dropped. If the process is killed hard, the file stays
/// behind and the next command reports it — with the owning PID — so the user can remove
/// it once they verified the process is gone.
pub struct WarehouseLock {
    path: PathBuf,
}

impl WarehouseLock {
    /// Acquire the warehouse lock. The current directory must be the warehouse root
    /// (see `warehouse_utils::enter_warehouse`).
    ///
    /// # Returns
    /// * `Ok(WarehouseLock)` - The acquired lock (released when dropped).
    /// * `Err(String)`       - If another process holds the lock, or the lock file could
    ///                         not be created.
    pub fn acquire() -> Result<WarehouseLock, String> {
        // The lock is bay-local: each bay serializes its own mutations independently (ref
        // updates touch shared refs, but those are short and the CAS is the guard).
        //
        // Store an *absolute* path: `bay add` changes the working directory mid-command
        // (to materialize the new bay), and the lock must still release at the file it
        // created — a relative path would resolve against the new cwd on drop.
        let path = absolute(bay_root().join(FILE_NAME_LOCK));
        acquire_lock_file(&path, "warehouse")?;
        Ok(WarehouseLock { path })
    }
}

impl Drop for WarehouseLock {
    fn drop(&mut self) {
        release_lock_file(&self.path);
    }
}

/// An exclusive lock on the **shared object store** — the objects, packs and commit-graph that
/// live at the warehouse root and are shared by every bay, as opposed to the bay-local
/// [`WarehouseLock`].
///
/// `compact` (and, later, `gc`/repack) take it so two bays — or two processes — cannot run
/// destructive store maintenance against the same loose/pack set at once and race each other's
/// deletions. It is deliberately a *distinct* lock from the bay lock: an ordinary mutating command
/// holds only its bay lock and never contends with a compaction running elsewhere on the store,
/// while two compactions serialize even across bays that each hold their own bay lock.
///
/// Like the bay lock it is a lock *file* created with `create_new` (atomic on every platform) and
/// removed on drop; a hard-killed process leaves it behind for the next command to report by PID.
pub struct StoreLock {
    path: PathBuf,
}

impl StoreLock {
    /// Acquire the shared object-store lock. Errors immediately (does not block) if another
    /// process or bay already holds it — the caller decides whether that is fatal (an explicit
    /// `compact`) or a reason to skip (auto-maintenance, which already ignores compaction errors).
    pub fn acquire() -> Result<StoreLock, String> {
        // Shared across bays: the store lives at `forklift_root`, not `bay_root`. Absolute for the
        // same reason as the bay lock — the working directory can change before the guard drops.
        let path = absolute(forklift_root().join(FILE_NAME_STORE_LOCK));
        acquire_lock_file(&path, "object store")?;
        Ok(StoreLock { path })
    }
}

impl Drop for StoreLock {
    fn drop(&mut self) {
        release_lock_file(&self.path);
    }
}

/// An exclusive lock marking a warehouse root as **served by a live server** (or held for the
/// duration of a serve-exclusive maintenance command).
///
/// The server head acquires it once at startup — for the single served root, or per warehouse in
/// multi-warehouse mode — and holds it for the whole process lifetime; `gc` acquires it for its
/// duration. That makes the two mutually exclusive on a given root: `gc` can never sweep a live
/// server's in-flight objects (a lift slower than the grace period would otherwise lose its
/// staged objects and then fail its ref update), and a second server accidentally started on the
/// same root is refused up front instead of silently breaking the first server's in-process
/// ref-update CAS. `bundle` deliberately does *not* take it — it never deletes an object,
/// writes atomically, and a stale bundle is self-healing, so it is safe against a live server.
///
/// Distinct from [`StoreLock`], which serializes destructive store *maintenance* against itself
/// (compaction vs compaction) — a server never compacts, so it never takes that lock; this one is
/// the serve-vs-maintenance gate. Like the other locks it is a lock *file* created with
/// `create_new` (atomic on every platform) and removed on drop; a hard-killed holder leaves it
/// behind for the next command to report by PID (a crashed server thus blocks `gc`/`bundle` until
/// the operator removes it, which is the safe default — better than sweeping a root that might
/// still be served).
pub struct ServeLock {
    path: PathBuf,
}

impl ServeLock {
    /// Acquire the serve lock at the current storage root. Errors immediately (does not block) if
    /// another process already holds it — a live server on this root, or a `gc`/`bundle` already
    /// running against it.
    pub fn acquire() -> Result<ServeLock, String> {
        // Lives at `forklift_root` (the shared store root), like the store lock: the server and
        // `gc`/`bundle` all enter the same storage-root scope, so they agree on the path. Absolute
        // for the same reason as the other locks — the working directory can change before drop.
        let path = absolute(forklift_root().join(FILE_NAME_SERVE_LOCK));
        acquire_lock_file(&path, "warehouse root")?;
        Ok(ServeLock { path })
    }
}

impl Drop for ServeLock {
    fn drop(&mut self) {
        release_lock_file(&self.path);
    }
}

/// Create an exclusive lock file at `path`, atomically (`create_new`, atomic on every platform).
/// `subject` names what is being locked, for the contention message. Shared by every lock scope.
fn acquire_lock_file(path: &Path, subject: &str) -> Result<(), String> {
    match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
        Ok(mut file) => {
            // The PID is informational only (it helps the user identify a stale lock);
            // failing to write it must not fail the command.
            let _ = writeln!(file, "{}", std::process::id());
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            let owner = std::fs::read_to_string(path).unwrap_or_default();
            let owner = owner.trim();
            let owner_info = if owner.is_empty() {
                String::new()
            } else {
                format!(" (held by process {})", owner)
            };

            Err(format!(
                "The {} is locked by another forklift process{}. If that process \
                is no longer running, remove \"{}\" and try again.",
                subject,
                owner_info,
                path.to_string_lossy()
            ))
        }
        Err(e) => Err(format!(
            "Error while creating the lock file \"{}\": {}",
            path.to_string_lossy(),
            e
        )),
    }
}

/// Release a lock file. Nothing sensible can be done about a failed removal here; the next
/// command will report the leftover lock file with the owning process's PID.
fn release_lock_file(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Resolve a path against the current directory when it is relative, so it stays valid if
/// the working directory later changes.
fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map(|cwd| cwd.join(&path)).unwrap_or(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    #[test]
    fn the_store_lock_is_exclusive_and_releases_on_drop() {
        let temp = std::env::temp_dir().join(format!("forklift-storelock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        // The shared lock lives in `forklift_root` (the `.forklift` folder), which must exist for
        // the atomic `create_new`.
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let first = StoreLock::acquire().expect("the first acquire succeeds");
        assert!(
            StoreLock::acquire().is_err(),
            "a second store lock must be refused while the first is held",
        );
        drop(first);
        StoreLock::acquire().expect("after the first is dropped the lock is free again");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn the_serve_lock_is_exclusive_releases_on_drop_and_is_distinct_from_the_store_lock() {
        let temp = std::env::temp_dir().join(format!("forklift-servelock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let first = ServeLock::acquire().expect("the first serve-lock acquire succeeds");
        assert!(
            ServeLock::acquire().is_err(),
            "a second serve lock must be refused while the first is held (a live server, or a \
             gc/bundle already running, blocks the other)",
        );
        // The serve lock and the store lock are different files: a compaction and a running server
        // are independent concerns and must be able to coexist.
        let store = StoreLock::acquire()
            .expect("the store lock is a distinct file — holding the serve lock must not block it");
        drop(store);

        drop(first);
        ServeLock::acquire().expect("after the first is dropped the serve lock is free again");

        std::fs::remove_dir_all(&temp).ok();
    }
}
