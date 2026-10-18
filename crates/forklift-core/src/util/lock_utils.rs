use std::io::Write;
use std::path::{Path, PathBuf};
use crate::globals::{bay_root, forklift_root};

/// The name of the warehouse lock file (inside the forklift root folder).
const FILE_NAME_LOCK: &str = "lock";

/// The name of the shared object-store lock file (inside the forklift root folder).
const FILE_NAME_STORE_LOCK: &str = "store.lock";

/// The name of the serve lock file (inside the forklift root folder).
const FILE_NAME_SERVE_LOCK: &str = "serve.lock";

/// The name of the store-access lock sidecar (inside the forklift root folder). Never deleted.
const FILE_NAME_STORE_ACCESS_LOCK: &str = "store-access.lock";

/// An exclusive lock on the warehouse, held for the duration of a mutating command.
///
/// This is the staging-area atomicity story (see the design document): commands that
/// mutate the inventory, the object store bookkeeping or pallet refs take this lock, so
/// that e.g. `stack` can never read a staging area that another forklift process is
/// halfway through rewriting. Read-only commands (`peek`, `stocktake`) do not take it —
/// at worst they report an in-flight intermediate state. Every process that fills a bay's
/// working directory holds that bay's lock, which is what lets [`WarehouseLock::acquire`] clear
/// the bay's assembly folder.
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
    /// (see `warehouse_utils::enter_warehouse`). Once it is held, everything in the bay's
    /// chunked-file assembly folder is removed as a leftover, so a process takes a bay's lock
    /// before it assembles anything there.
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

        // Holding the lock is what makes anything in the bay's assembly folder a leftover.
        crate::util::shift_utils::clear_assembly_leftovers();
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
/// never takes it, while two compactions serialize even across bays that each hold their own bay
/// lock. It orders compaction against compaction only; ordering a full repack against the store's
/// writers is [`StoreAccessLock`]'s job.
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
/// duration, and so does a full repack (`compact --all`, FORK-149), which drops every packed object
/// outside the live set with no grace period at all. That makes them mutually exclusive on a given
/// root: neither can sweep a live server's in-flight objects (a lift slower than the grace period —
/// or any lift, for a repack — would otherwise lose its staged objects and then fail its ref
/// update), and a second server accidentally started on the
/// same root is refused up front instead of silently breaking the first server's in-process
/// ref-update CAS. `bundle` deliberately does *not* take it — it never deletes an object,
/// writes atomically, and a stale bundle is self-healing, so it is safe against a live server.
///
/// Distinct from [`StoreLock`], which serializes destructive store *maintenance* against itself
/// (compaction vs compaction) — a server never compacts, so it never takes that lock; this one is
/// the serve-vs-maintenance gate. It is not what orders a deleter against the objects a server
/// writes: a served warehouse also holds [`StoreAccessLock`] shared, which every deleter's
/// exclusive try refuses beside, scope-prune included. Like the other locks it is a lock *file* created with
/// `create_new` (atomic on every platform) and removed on drop; a hard-killed holder leaves it
/// behind for the next command to report by PID (a crashed server thus blocks `gc` and a repack until
/// the operator removes it, which is the safe default — better than sweeping a root that might
/// still be served).
pub struct ServeLock {
    path: PathBuf,
}

impl ServeLock {
    /// Acquire the serve lock at the current storage root. Errors immediately (does not block) if
    /// another process already holds it — a live server on this root, or a `gc` or full repack
    /// already running against it.
    pub fn acquire() -> Result<ServeLock, String> {
        // Lives at `forklift_root` (the shared store root), like the store lock: the server, `gc`
        // and a repack all enter the same storage-root scope, so they agree on the path. Absolute
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

/// A store-wide **reader/writer** lock ordering every writer of the shared object store against
/// every operation that deletes objects on a liveness or scope decision. This is the shared hold:
/// writers hold it together; [`StoreAccessExclusive`] is the deleters' side.
///
/// A writer's dedup ("this object is present, skip the write") says nothing about whether the
/// object is *live*: unreachable garbage stays present until a deleter removes it. A deleter
/// computes its keep-set once and then deletes everything outside it — a full repack
/// (`pack_utils::compact` with `all`), the loose sweep (`gc_utils::collect_garbage`) and
/// scope-prune. Without an order between the two, a writer in another bay can dedup against an
/// object the deleter has already excluded, publish a root that references it, and be left with
/// a dangling reference when the deletion lands. Holding this lock shared across the writer's
/// whole decide-then-publish window, and exclusive from before the keep-set is computed until the
/// deletions end, orders the two.
///
/// Who holds which side:
/// * **shared, waiting** — every CLI command that takes the warehouse lock, except `compact` and
///   `scope-prune`, which host deleters (a process's own shared hold refuses its own exclusive
///   try). Taken right after the bay lock, so the order is always bay lock first and no cycle is
///   possible; released before auto-maintenance, which may repack.
/// * **shared, try** — a server, for every warehouse it serves, for the handle's lifetime; and
///   incremental compaction (explicit or automatic), which moves loose objects into a pack and
///   deletes the loose copies, so it must not interleave with a deleter's loose deletions.
/// * **exclusive, try** — the deleters, at the deleter itself so no caller can forget it.
///
/// Unlike [`StoreLock`] and [`ServeLock`], this is a *kernel* lock (`File::lock_shared` /
/// `File::try_lock`: `flock` on Unix, `LockFileEx` on Windows) on a sidecar that is never
/// deleted, as [`ConfigFileLock`] is. A hard-killed holder leaves nothing stale, because the
/// kernel drops the lock with the process. That is what makes it safe for a writer to *wait*:
/// a wait can only end, never hang on a dead holder. Deleters never wait on anything a writer
/// holds, so the waits cannot deadlock. The sidecar is never read or written, so Windows'
/// mandatory locking blocks nothing else.
pub struct StoreAccessLock {
    // Held only to keep the lock: closing the handle releases it.
    _file: std::fs::File,
}

impl StoreAccessLock {
    /// Take the shared hold at the current storage root, waiting while a deleter holds it
    /// exclusively. Tries first; only when that would block is `on_wait` called — once, before
    /// the wait — so the caller can say why it is waiting (core never prints).
    ///
    /// # Returns
    /// * `Ok(StoreAccessLock)` - The shared hold (released when dropped).
    /// * `Err(String)`         - The sidecar could not be opened or locked.
    pub fn shared(on_wait: impl FnOnce()) -> Result<StoreAccessLock, String> {
        let (file, path) = open_store_access_sidecar()?;

        match file.try_lock_shared() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => {
                on_wait();
                file.lock_shared().map_err(|e| lock_error(&path, &e))?;
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(lock_error(&path, &e)),
        }

        Ok(StoreAccessLock { _file: file })
    }

    /// Take the shared hold at the current storage root without waiting: refused while a deleter
    /// holds it exclusively. For the server, which must not block a runtime thread, and for
    /// incremental compaction, which is maintenance and is retried later rather than waiting.
    ///
    /// # Returns
    /// * `Ok(StoreAccessLock)` - The shared hold (released when dropped).
    /// * `Err(String)`         - A deleter is running on this store, or the sidecar could not be
    ///                           opened or locked.
    pub fn try_shared() -> Result<StoreAccessLock, String> {
        let (file, path) = open_store_access_sidecar()?;

        match file.try_lock_shared() {
            Ok(()) => Ok(StoreAccessLock { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "Object-store maintenance (a full repack, gc or scope-prune) is running on the \
                warehouse at \"{}\". Try again once it finishes.",
                warehouse_root_of(&path)
            )),
            Err(std::fs::TryLockError::Error(e)) => Err(lock_error(&path, &e)),
        }
    }

    /// Take the exclusive hold at the current storage root, for an operation that deletes objects
    /// on a liveness or scope decision. Never waits: refused while any other holder exists, shared
    /// or exclusive — including a shared hold this same process took on another open file.
    ///
    /// # Returns
    /// * `Ok(StoreAccessExclusive)` - The exclusive hold (released when dropped).
    /// * `Err(String)`              - Another forklift process is using the store, or the sidecar
    ///                                could not be opened or locked.
    pub fn try_exclusive() -> Result<StoreAccessExclusive, String> {
        let (file, path) = open_store_access_sidecar()?;

        match file.try_lock() {
            Ok(()) => Ok(StoreAccessExclusive { _file: file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(format!(
                "Another forklift process is using the object store of the warehouse at \"{}\" \
                (a command in another bay, a server serving it, or other maintenance). Try again \
                once it finishes.",
                warehouse_root_of(&path)
            )),
            Err(std::fs::TryLockError::Error(e)) => Err(lock_error(&path, &e)),
        }
    }
}

/// The exclusive side of [`StoreAccessLock`], held by a deleter from before its keep-set is
/// computed until its deletions end. Functions that delete on a keep-set take a reference to it,
/// so the type proves the caller holds the guard.
pub struct StoreAccessExclusive {
    // Held only to keep the lock: closing the handle releases it.
    _file: std::fs::File,
}

/// `OPEN_MAX` from macOS's `<sys/syslimits.h>`, which the `libc` crate does not export: the first
/// limit tried when macOS refuses the hard limit (an unlimited one).
#[cfg(target_os = "macos")]
const MACOS_OPEN_MAX: libc::rlim_t = 10240;

/// Raise this process's soft limit on open files toward its hard limit. A server keeps one
/// store-access hold — one open file — per warehouse it serves, for as long as it serves it, so in
/// multi-warehouse mode the count grows with the warehouses served, and the default soft limit
/// (256 on macOS) is far below what the hard limit allows. Tries the hard limit, then searches for
/// the largest limit below it the kernel accepts (on macOS, whose kernel also caps the limit at
/// `kern.maxfilesperproc`, trying `OPEN_MAX` first when it is above the current limit); never
/// lowers the limit. Best-effort: a limit that cannot be read or raised is left as it is. A no-op
/// off Unix, which has no such per-process soft limit.
pub fn raise_open_file_limit() {
    #[cfg(unix)]
    {
        let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        // SAFETY: `getrlimit` writes into the `rlimit` it is given and reads nothing else.
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) } != 0 {
            return;
        }

        let hard = limit.rlim_max;
        #[cfg(target_os = "macos")]
        let fallback = Some(MACOS_OPEN_MAX);
        #[cfg(not(target_os = "macos"))]
        let fallback = None;

        largest_accepted_limit(limit.rlim_cur, hard, fallback, |target| {
            let raised = libc::rlimit { rlim_cur: target, rlim_max: hard };
            // SAFETY: `setrlimit` only reads the `rlimit` it is given.
            unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 }
        });
    }
}

/// The search behind [`raise_open_file_limit`], apart from the kernel so it can be tested against
/// any ceiling: `accept` sets a soft limit and says whether the kernel took it. Tries `hard`, then
/// `fallback` (capped at `hard`), then searches between the largest accepted limit and the
/// smallest refused one. Only limits above `current` are ever tried, so the limit never drops.
///
/// # Returns
/// The limit in force at the end: the largest one accepted, or `current` if none was.
#[cfg(unix)]
fn largest_accepted_limit(current: libc::rlim_t,
                          hard: libc::rlim_t,
                          fallback: Option<libc::rlim_t>,
                          mut accept: impl FnMut(libc::rlim_t) -> bool) -> libc::rlim_t {
    let mut try_raise = |target: libc::rlim_t| target > current && accept(target);
    if try_raise(hard) {
        return hard;
    }

    // The largest limit known to be accepted, and the smallest known to be refused.
    let (mut accepted, mut refused) = (current, hard);
    if let Some(start) = fallback.map(|start| start.min(hard)).filter(|&start| start > current) {
        if try_raise(start) {
            accepted = start;
        } else {
            refused = start;
        }
    }
    while refused > accepted && refused - accepted > 1 {
        let middle = accepted + (refused - accepted) / 2;
        if try_raise(middle) {
            accepted = middle;
        } else {
            refused = middle;
        }
    }

    accepted
}

/// Open (creating if needed) the store-access sidecar at the current storage root, at an absolute
/// path. The forklift root folder must exist.
fn open_store_access_sidecar() -> Result<(std::fs::File, PathBuf), String> {
    let path = absolute(forklift_root().join(FILE_NAME_STORE_ACCESS_LOCK));
    let file = open_lock_sidecar(&path)?;

    Ok((file, path))
}

/// Open (creating if needed) the sidecar of a kernel lock. A sidecar is never deleted, so it
/// outlives the process that created it, and on a shared warehouse it may belong to a user whose
/// file mode leaves it read-only to us. A kernel lock needs only a read handle, so a refused
/// read-write open falls back to a read-only one rather than locking that user out.
fn open_lock_sidecar(path: &Path) -> Result<std::fs::File, String> {
    let opened = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(path);

    match opened {
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied && path.is_file() =>
            std::fs::File::open(path),
        opened => opened,
    }.map_err(|e| format!("Error while opening the lock file \"{}\": {}", path.to_string_lossy(), e))
}

fn lock_error(path: &Path, error: &std::io::Error) -> String {
    format!("Error while locking \"{}\": {}", path.to_string_lossy(), error)
}

/// The warehouse root a sidecar in the forklift root folder belongs to, for messages.
fn warehouse_root_of(sidecar: &Path) -> String {
    sidecar.parent().and_then(Path::parent).unwrap_or(sidecar).to_string_lossy().into_owned()
}

/// An exclusive lock serializing every edit of one **configuration file**, held for one
/// load → edit → write and no longer.
///
/// Unlike the lock files above, this is a *kernel* lock (`File::lock`: `flock` on Unix,
/// `LockFileEx` on Windows) on a sidecar `<config file>.lock` that is never deleted. A second
/// writer **waits** instead of failing: config edits are short, and the cost of refusing is high
/// (operator identity is minted on first use by any command). And a hard-killed holder leaves
/// nothing stale, because the kernel drops the lock with the process. A `create_new` lock file
/// in the home directory, left behind by a crash, would refuse identity minting for every
/// warehouse on the machine until someone deleted it by hand.
///
/// The lock is on a sidecar, never the config file itself: a write replaces the config by rename,
/// so a lock on the old file would exclude nothing, and on Windows the lock may be mandatory and
/// would then block the readers, which deliberately take no lock.
///
/// A thread holds at most one config lock at a time and runs no other config operation while
/// holding it, so two config locks can never deadlock.
pub struct ConfigFileLock {
    // Held only to keep the lock: closing the handle releases it.
    _file: std::fs::File,
}

impl ConfigFileLock {
    /// Acquire the edit lock for the configuration file at `config_path`, waiting while another
    /// thread or process holds it. The folder of `config_path` must exist.
    ///
    /// # Returns
    /// * `Ok(ConfigFileLock)` - The acquired lock (released when dropped).
    /// * `Err(String)`        - The sidecar could not be opened or locked.
    pub fn acquire(config_path: &Path) -> Result<ConfigFileLock, String> {
        let path = config_lock_path(config_path)?;

        let file = open_lock_sidecar(&path)?;
        file.lock().map_err(|e| lock_error(&path, &e))?;

        Ok(ConfigFileLock { _file: file })
    }
}

/// The sidecar lock file of a configuration file: `<config file>.lock`, beside it.
fn config_lock_path(config_path: &Path) -> Result<PathBuf, String> {
    let file_name = config_path.file_name()
        .ok_or_else(|| format!("Cannot lock \"{}\": it has no file name.", config_path.to_string_lossy()))?;

    let mut lock_name = file_name.to_os_string();
    lock_name.push(".lock");

    Ok(config_path.with_file_name(lock_name))
}

/// How many times `acquire_lock_file` retries a transient permission-denied create, and the
/// delay between attempts (see the function doc for what this absorbs).
const LOCK_CREATE_MAX_ATTEMPTS: u32 = 6;
const LOCK_CREATE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

/// Create an exclusive lock file at `path`, atomically (`create_new`, atomic on every platform).
/// `subject` names what is being locked, for the contention message. Shared by every lock scope.
///
/// Retries a narrow, Windows-only failure a few times before giving up: NTFS can leave a
/// just-deleted file in a brief "pending delete" state — e.g. while an antivirus scanner still
/// holds a handle open on it — during which `CREATE_NEW` against the same path reports
/// access-denied rather than a clean create. POSIX `unlink` has no such state (the directory
/// entry is gone immediately, regardless of any other open handle), so this path is never taken
/// on Unix. The retry is bounded (at most ~120ms total) and scoped to exactly this error kind —
/// the moment a create instead reports `AlreadyExists`, that is the genuine "another process
/// holds it" signal and returns immediately, no retry.
fn acquire_lock_file(path: &Path, subject: &str) -> Result<(), String> {
    let mut attempts = 0;

    loop {
        attempts += 1;

        match std::fs::OpenOptions::new().write(true).create_new(true).open(path) {
            Ok(mut file) => {
                // The PID is informational only (it helps the user identify a stale lock);
                // failing to write it must not fail the command.
                let _ = writeln!(file, "{}", std::process::id());
                return Ok(());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let owner = std::fs::read_to_string(path).unwrap_or_default();
                let owner = owner.trim();
                let owner_info = if owner.is_empty() {
                    String::new()
                } else {
                    format!(" (held by process {})", owner)
                };

                return Err(format!(
                    "The {} is locked by another forklift process{}. If that process \
                    is no longer running, remove \"{}\" and try again.",
                    subject,
                    owner_info,
                    path.to_string_lossy()
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied
                && attempts < LOCK_CREATE_MAX_ATTEMPTS => {
                std::thread::sleep(LOCK_CREATE_RETRY_DELAY);
            }
            Err(e) => return Err(format!(
                "Error while creating the lock file \"{}\": {}",
                path.to_string_lossy(),
                e
            )),
        }
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

    /// Writers share the store; a deleter's exclusive try is refused beside any of them; a
    /// writer's try is refused beside a deleter, and its waiting acquire announces the wait once
    /// and is granted when the deleter releases. Separate open files stand in for separate
    /// processes: the kernel lock is per open file.
    #[test]
    fn store_access_is_shared_among_writers_and_exclusive_against_deleters() {
        let temp = std::env::temp_dir().join(format!("forklift-storeaccess-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let first = StoreAccessLock::shared(|| panic!("an uncontended shared hold must not wait"))
            .expect("the first writer");
        let second = StoreAccessLock::try_shared().expect("a second writer shares the store");
        let error = match StoreAccessLock::try_exclusive() {
            Ok(_) => panic!("a deleter must be refused while a writer holds the store"),
            Err(error) => error,
        };
        assert!(error.contains("using the object store"), "the refusal names the contention: {}", error);
        drop(first);
        assert!(StoreAccessLock::try_exclusive().is_err(), "one remaining writer still refuses a deleter");
        drop(second);

        let deleter = StoreAccessLock::try_exclusive().expect("a deleter runs once no writer holds the store");
        assert!(StoreAccessLock::try_shared().is_err(), "a server's try is refused while a deleter runs");
        assert!(StoreAccessLock::try_exclusive().is_err(), "two deleters never run together");

        let (waiting_tx, waiting_rx) = std::sync::mpsc::channel();
        let root = temp.clone();
        let writer = std::thread::spawn(move || {
            let _scope = StorageRootScope::enter(&root);
            StoreAccessLock::shared(move || waiting_tx.send(()).unwrap()).map(|_| ())
        });
        waiting_rx.recv_timeout(std::time::Duration::from_secs(10))
            .expect("a writer blocked by a deleter announces its wait");
        drop(deleter);
        writer.join().unwrap().expect("the waiting writer is granted once the deleter releases");

        assert!(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join(FILE_NAME_STORE_ACCESS_LOCK).is_file(),
            "the sidecar stays: it is never deleted");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// The open-file search against a fake kernel: it lands on the largest limit the ceiling
    /// allows, from below or above the fallback, and never tries a limit at or under the current
    /// one — including when the current limit is already above the fallback, where a search that
    /// started from the fallback would run downward.
    #[cfg(unix)]
    #[test]
    fn the_open_file_search_finds_the_largest_accepted_limit_and_never_lowers() {
        let search = |current, hard, fallback, ceiling| {
            let mut tried = Vec::new();
            let mut limit = current;
            let final_limit = largest_accepted_limit(current, hard, fallback, |target| {
                tried.push(target);
                let ok = target <= ceiling;
                if ok { limit = target; }
                ok
            });
            assert!(tried.iter().all(|&target| target > current), "tried a limit at or under the current one: {:?}", tried);
            assert_eq!(final_limit, limit, "the result is the limit in force");
            final_limit
        };
        let infinity = libc::RLIM_INFINITY;

        assert_eq!(search(256, 65536, None, 65536), 65536, "the hard limit, when accepted");
        assert_eq!(search(256, infinity, Some(10240), 8000), 8000, "a ceiling under the fallback");
        assert_eq!(search(256, infinity, Some(10240), 245760), 245760, "a ceiling over the fallback");
        assert_eq!(search(1024, 4096, None, 3000), 3000, "a ceiling under a finite hard limit");
        assert_eq!(search(1_048_576, infinity, Some(10240), 245760), 1_048_576,
            "a current limit above everything the kernel accepts stays");
        assert_eq!(search(256, 256, Some(10240), infinity), 256, "nothing to raise");
    }

    /// A sidecar another user created can be read-only to this one. Taking either side of the lock
    /// needs only a read handle, so a read-only sidecar must not lock this user out of the store.
    /// The read-only permission stands in for another owner's file mode.
    #[test]
    fn a_read_only_store_access_sidecar_still_locks_both_ways() {
        let temp = std::env::temp_dir().join(format!("forklift-storeaccess-ro-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let sidecar = temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join(FILE_NAME_STORE_ACCESS_LOCK);
        std::fs::write(&sidecar, b"").unwrap();
        let mut permissions = std::fs::metadata(&sidecar).unwrap().permissions();
        permissions.set_readonly(true);
        std::fs::set_permissions(&sidecar, permissions.clone()).unwrap();

        let writer = StoreAccessLock::try_shared();
        let writer_ok = writer.is_ok();
        drop(writer);
        let deleter = StoreAccessLock::try_exclusive();
        let deleter_ok = deleter.is_ok();
        drop(deleter);

        #[allow(clippy::permissions_set_readonly_false)]
        permissions.set_readonly(false);
        std::fs::set_permissions(&sidecar, permissions).unwrap();
        std::fs::remove_dir_all(&temp).ok();

        assert!(writer_ok, "a writer takes the shared side through a read-only sidecar");
        assert!(deleter_ok, "a deleter takes the exclusive side through a read-only sidecar");
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
