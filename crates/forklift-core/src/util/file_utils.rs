use std::collections::BTreeSet;
use std::fs::Metadata;
use std::ops::Add;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use file_id::FileId;
use regex::Regex;
use crate::enums::dir_entry_type::DirEntryType;
use crate::globals::{bay_root, forklift_root, FOLDER_NAME_GRAPH_ROOT, FOLDER_NAME_INVENTORY_ROOT, FOLDER_NAME_OBJECTS_ROOT};
use crate::util::byte_utils;

/// The number of characters in an object hash that are used for creating the folders.
/// The remaining characters are used for the file name.
///
/// A single 2-character level (256 folders) is enough: with 10 million objects that is
/// ~39k files per folder, which modern file systems handle comfortably (git uses the same
/// scheme at monorepo scale). Deeper nesting would cost extra directory inodes and path
/// lookups per object for no practical benefit.
///
/// Public so the pack layer can recognise these fan-out folders when it sweeps the loose
/// store (everything else under the object root, e.g. the `pack/` folder, is not one).
pub const OBJECT_HASH_FOLDER_PATH_CHARACTERS: usize = 2;

const FILENAME_IGNORE: &str = ".forkliftignore";

const IGNORE_FILE_COMMENT_PREFIX: &str = "#";
const IGNORE_FILE_CONTENT: &str = r#"# Forklift ignore file.
# This file is used to specify files and directories that should be ignored by Forklift.
# Every entry must be a valid regex pattern.
#
# Example - ignore a folder called "test":
# ^test\/?.*$
#
# Example - ignore all files with the extension ".log":
# \.log$
"#;

const DEFAULT_IGNORED_PATHS: [&str; 1] = ["^\\.forklift/?.*$"];

/// The path separator used in warehouse-internal paths (inventory keys, metadata entries,
/// object store paths). This is always `/`, on every platform: keys written on one platform
/// must parse identically on another. Note that `Path`/`PathBuf` values converted to strings
/// use the *native* separator (`\` on Windows), so native path strings must never be used as
/// warehouse keys directly — convert them through `WarehousePath` instead.
pub const PATH_SEPARATOR: &str = "/";
pub const PATH_SEPARATOR_CHAR: char = '/';

/// A prefix for the folder that contains the inventory files of the respective working directory.
/// E.g. for the `src` folder, the respective inventory folder would be `inv_src`.
/// This prefix is applied to make sure that folders in the working directory called
/// `data` or `metadata` do not conflict with the inventory data / metadata files.
pub const PREFIX_INVENTORY_FOLDER: &str = "inv_";

/// The name of the inventory data file.
pub const FILE_NAME_INVENTORY_DATA: &str = "data";

/// The name of the inventory metadata file.
pub const FILE_NAME_INVENTORY_METADATA: &str = "metadata";

/// Create a folder if it does not exist yet.
/// All folders in the given path will be created (if they don't exist already).
/// It is safe to call this function with a path that already exists,
/// no action will be taken in that case.
///
/// # Arguments
/// * `name` - The name of the folder to create.
///
/// # Returns
/// * `Ok(true)`    - If the folder was created.
/// * `Ok(false)`   - If the folder already existed.
/// * `Err(String)` - If an error occurred while creating (or checking) the folder.
pub fn create_folder_if_not_exists(path: &Path) -> Result<bool, String> {
    let does_exist = Path::new(path).try_exists()
        .map_err(|e| format!("Error while checking if folder \"{}\" exists: {}", path.to_string_lossy(), e))?;

    if !does_exist {
        std::fs::create_dir_all(path)
            .map_err(|e| format!("Error while creating folder \"{}\": {}", path.to_string_lossy(), e))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Resolve a shared `.forklift/<folder>` root, memoized in `memo` against the storage-root scope
/// fingerprint ([`globals::scope_fingerprint`]).
///
/// A hot read path resolves a root on every object access (for the pack-registry key, the
/// read-cache key, the graph-shard key), and each resolution reads the bay-context lock and
/// rebuilds the path. This returns a clone of the cached string while the scope is unchanged and
/// recomputes the instant it changes — so a server switching warehouses (or a bay entered
/// mid-process) is never served a stale root, while a walk within one scope pays the cost once.
fn memoized_root(
    memo: &'static std::thread::LocalKey<std::cell::RefCell<Option<((u64, u64), String)>>>,
    folder: &str,
) -> String {
    let fingerprint = crate::globals::scope_fingerprint();
    memo.with(|memo| {
        let mut memo = memo.borrow_mut();
        if let Some((cached_fingerprint, root)) = memo.as_ref() {
            if *cached_fingerprint == fingerprint {
                return root.clone();
            }
        }
        let root = forklift_root().to_string_lossy().into_owned().add(PATH_SEPARATOR).add(folder);
        *memo = Some((fingerprint, root.clone()));
        root
    })
}

/// Get the path to the "objects root" folder — memoized per scope (see [`memoized_root`]).
///
/// # Returns
/// * The path to the "objects root" folder.
pub fn get_path_objects_root() -> String {
    thread_local! {
        static MEMO: std::cell::RefCell<Option<((u64, u64), String)>> =
            const { std::cell::RefCell::new(None) };
    }
    memoized_root(&MEMO, FOLDER_NAME_OBJECTS_ROOT)
}

/// Get the path to the "inventory root" folder.
/// This folder is used for storing inventory files.
///
/// # Returns
/// * The path to the "inventory root" folder.
pub fn get_path_inventory_root() -> String {
    // The inventory is bay-local: each bay stages independently.
    bay_root().to_string_lossy().into_owned().add(PATH_SEPARATOR).add(FOLDER_NAME_INVENTORY_ROOT)
}

/// Get the path to the commit-graph root folder (the sharded, self-healing DAG cache, §B).
///
/// It lives next to `objects` under the shared forklift root — ancestry is warehouse-global,
/// so every bay reads the same graph — and is sharded by parcel-hash prefix underneath.
///
/// # Returns
/// * The path to the "graph root" folder.
pub fn get_path_graph_root() -> String {
    thread_local! {
        static MEMO: std::cell::RefCell<Option<((u64, u64), String)>> =
            const { std::cell::RefCell::new(None) };
    }
    memoized_root(&MEMO, FOLDER_NAME_GRAPH_ROOT)
}

/// Get the path and file name for an object.
///
/// # Arguments
/// * `hash` - The hash of the object.
///
/// # Returns
/// * The path to the folder where the object is stored (without trailing path separator).
/// The path is relative to the root folder of the warehouse
/// (so the path to the objects root folder is included).
/// * The file name of the object.
///
/// # Example
/// ```
/// use forklift_core::util::file_utils::{get_path_for_object, get_path_objects_root};
///
/// let (path, file_name) = get_path_for_object("9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc").unwrap();
///
/// // In this example we assume that the objects root folder
/// // is ".forklift/objects", which is the
/// // case at the time of writing this example.
/// assert_eq!(get_path_objects_root(), String::from(".forklift/objects"));
///
/// assert_eq!(path, String::from(".forklift/objects/90"));
///
/// assert_eq!(file_name, String::from("28a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc"));
/// ```
pub fn get_path_for_object(hash: &str) -> Result<(String, String), String> {
    // A corrupted or hand-entered hash must produce an error instead of a panic
    // (or a bogus path outside the object fan-out folders).
    if hash.len() <= OBJECT_HASH_FOLDER_PATH_CHARACTERS
        || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!("\"{}\" is not a valid object hash.", hash));
    }

    let folder_parts: Vec<String> = (&hash[0..OBJECT_HASH_FOLDER_PATH_CHARACTERS])
        .chars()
        .collect::<Vec<char>>()
        .chunks(2)
        .map(|c| c.iter().collect())
        .collect();

    let path = get_path_objects_root()
        .add(PATH_SEPARATOR)
        .add(folder_parts.join(PATH_SEPARATOR).as_str());

    Ok((path, hash[OBJECT_HASH_FOLDER_PATH_CHARACTERS..].to_string()))
}

/// Whether writes fsync for durability. Durable by default; set `FORKLIFT_FSYNC` to `0`, `off`,
/// `false`, or `no` to skip every fsync — a throughput escape hatch for bulk, disposable work
/// (large imports, test fixtures, CI) where a mid-run crash just means re-running the whole
/// operation. Read once and cached, because durability is a *process-wide* policy: the server head
/// serves many warehouses in one process, so it must not hang off a per-warehouse config lookup on
/// the write hot path.
pub fn fsync_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| parse_fsync_setting(std::env::var("FORKLIFT_FSYNC").ok().as_deref()))
}

/// Parse a `FORKLIFT_FSYNC` value: absent — or anything other than an explicit off token — means
/// durability stays on. Split out from [`fsync_enabled`] so it is testable without the process-wide
/// env read and its cache.
fn parse_fsync_setting(value: Option<&str>) -> bool {
    match value {
        Some(v) => !matches!(v.trim().to_ascii_lowercase().as_str(), "0" | "off" | "false" | "no"),
        None => true,
    }
}

/// fsync a directory so a create/rename/unlink inside it is durable across power loss, not merely a
/// process crash. Renaming a file into place makes its *contents* reachable, but the directory
/// entry recording the new name is itself only on disk once the directory is fsynced — without this
/// a post-crash directory could be missing an object, ref, or pack whose data was already synced.
///
/// A no-op when [`fsync_enabled`] is false, and on non-Unix targets, where a directory handle
/// cannot be opened for `sync_all` (NTFS gives the ordering this buys on other filesystems).
pub fn sync_dir(dir: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        if !fsync_enabled() {
            return Ok(());
        }
        std::fs::File::open(dir)
            .and_then(|handle| handle.sync_all())
            .map_err(|e| format!("Error while syncing directory \"{}\": {}", dir.to_string_lossy(), e))
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
        Ok(())
    }
}

/// Create `path` and write `content` to it, with no durability guarantee at all. Split out of
/// [`write_and_sync_file`] so a [`BulkStoreSession`]'s staged writes can share the exact same
/// creation/write path without paying for (or skipping past) its fsync.
fn create_and_write_file(path: &Path, content: &[u8]) -> Result<std::fs::File, String> {
    use std::io::Write;

    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("Error while writing file \"{}\": {}", path.to_string_lossy(), e))?;
    file.write_all(content)
        .map_err(|e| format!("Error while writing file \"{}\": {}", path.to_string_lossy(), e))?;
    Ok(file)
}

/// Write `content` to a fresh file at `path`, fsyncing its bytes before returning (unless
/// [`fsync_enabled`] is off). A following rename can then never publish a name whose contents never
/// reached disk — which, because object writers skip existing hashes, would otherwise be a torn
/// object that is never repaired.
fn write_and_sync_file(path: &Path, content: &[u8]) -> Result<(), String> {
    let file = create_and_write_file(path, content)?;
    if fsync_enabled() {
        file.sync_all()
            .map_err(|e| format!("Error while syncing file \"{}\": {}", path.to_string_lossy(), e))?;
    }
    Ok(())
}

/// Generate a fresh, process-unique temp-file path for `file_path`.
///
/// The temporary name must be unique per *write*, not just per process: two parallel tasks
/// writing the same path (e.g. storing identical object content) would otherwise share a
/// temporary file and race each other's rename. This counter is shared by every writer that
/// stages a temp file this way — [`write_file_atomically`] itself, [`BulkStoreSession`]'s
/// deferred writes (which call `write_file_atomically` directly), and [`WriteBatch::stage`] — so
/// no two ever collide even when several are staging concurrently.
/// `object_utils::store_object_stream` writes its own temp files off its own independent
/// counter — its `.stream.tmp` infix (vs. this function's plain `.tmp`) is deliberately
/// different so the two paths can never collide on the same temp name even if both counters
/// reach the same numeric value at the same moment (two independent counters offer no such
/// guarantee on their own).
fn temp_path_for(file_path: &Path) -> Result<PathBuf, String> {
    static TEMP_FILE_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let write_id = TEMP_FILE_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let file_name = file_path.file_name()
        .ok_or(format!("Cannot write to \"{}\": it has no file name.", file_path.to_string_lossy()))?
        .to_string_lossy();

    let mut temporary_file_path = PathBuf::from(file_path);
    temporary_file_path.set_file_name(format!("{}.tmp{}-{}", file_name, std::process::id(), write_id));

    Ok(temporary_file_path)
}

/// Write a file atomically: the content is written to a temporary file in the same folder first,
/// fsynced, then renamed into place, and finally the parent directory is fsynced. A crash mid-write
/// can therefore never leave a truncated file at the final path — the file either has its old
/// content or the new one — and after power loss the rename cannot resurrect empty/partial content
/// (see [`write_and_sync_file`] and [`sync_dir`]; both honour the `FORKLIFT_FSYNC` escape hatch).
///
/// # Arguments
/// * `file_path` - The path of the file to write.
/// * `content`   - The content to write.
///
/// # Returns
/// * `Ok(())`      - If the file was written successfully.
/// * `Err(String)` - If an error occurred while writing the file.
pub fn write_file_atomically(file_path: &Path, content: &[u8]) -> Result<(), String> {
    let temporary_file_path = temp_path_for(file_path)?;

    // While a bulk-store session is active, stage the write and hand its (temp, final) pair to
    // the session instead of fsyncing and renaming here — `BulkStoreSession::finish` runs the
    // durability barrier and every rename as one batch. The invariant this function otherwise
    // enforces per-write (a final name never exists before its bytes are durable) still holds:
    // the rename simply has not happened yet, so `file_path` stays invisible until it does.
    {
        let mut session = bulk_session_registry().lock().expect("bulk store session lock poisoned");
        if let Some(pending) = session.as_mut() {
            create_and_write_file(&temporary_file_path, content)?;
            pending.push((temporary_file_path, file_path.to_path_buf()));
            return Ok(());
        }
    }

    write_and_sync_file(&temporary_file_path, content)?;

    std::fs::rename(&temporary_file_path, file_path).map_err(|e|
        format!("Error while moving file into place at \"{}\": {}", file_path.to_string_lossy(), e)
    )?;

    // The rename is only durable once the directory entry recording the new name is on disk;
    // otherwise a power loss can undo it even though the file's bytes were already synced.
    match file_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => sync_dir(parent),
        _ => Ok(()),
    }
}

/// The process-global bulk-store session registry: `Some(pending)` while a session is active,
/// where `pending` is the recorded `(temp_path, final_path)` writes staged so far. Behind a
/// `Mutex` so [`write_file_atomically`] and [`BulkStoreSession`] can share it without either
/// side needing to pass a handle around.
fn bulk_session_registry() -> &'static std::sync::Mutex<Option<Vec<(PathBuf, PathBuf)>>> {
    static SESSION: std::sync::OnceLock<std::sync::Mutex<Option<Vec<(PathBuf, PathBuf)>>>> =
        std::sync::OnceLock::new();
    SESSION.get_or_init(|| std::sync::Mutex::new(None))
}

/// A scoped bulk-store session: batches the *publish*, not just the fsync, for a burst of loose
/// writes through [`write_file_atomically`] (only that function — `object_utils::store_object_stream`
/// and every other writer are untouched and behave exactly as before).
///
/// Every content-addressed writer in this store dedups by existence (skips writing a hash that is
/// already there), so a final name must never exist before its bytes are durable — a crash between
/// the two would leave a torn object that every future writer silently skips forever, permanent
/// silent poison (see [`write_and_sync_file`]). The per-object fsync that invariant normally costs
/// is what a bulk import spends most of its wall time on: on macOS, `File::sync_all` is
/// `F_FULLFSYNC`, a real device-cache flush (~5-15 ms), paid once per object. A session defers both
/// the durability wait and the rename for every write made while it is open to one barrier in
/// [`BulkStoreSession::finish`] — so the invariant is enforced once for the whole batch instead of
/// once per file, while still holding in every crash interleaving (see `finish`'s doc comment).
///
/// Exactly one session may be active per process (enforced by [`open`](BulkStoreSession::open)
/// through the `Mutex` in [`bulk_session_registry`]). This is deliberately not a concurrency
/// primitive: it exists for a single, sequential bulk writer (`import-git --no-compact`) and must
/// never be opened by anything with concurrent writers — in particular the server head, which
/// serves many warehouses and many requests from one process, must never open one.
///
/// The facility generalizes to any other bulk, sequential loose writer (an incremental batch
/// fetch landing many objects loose, say) — it is deliberately not wired into one yet, since
/// `import-git` is the only place that need has actually shown up so far.
pub struct BulkStoreSession {
    finished: bool,
}

impl BulkStoreSession {
    /// Open a bulk-store session. Fails if one is already active in this process (see the type's
    /// doc comment — only one may ever be open at a time).
    pub fn open() -> Result<BulkStoreSession, String> {
        let mut registry = bulk_session_registry().lock().expect("bulk store session lock poisoned");
        if registry.is_some() {
            return Err("A bulk store session is already active in this process.".to_string());
        }
        *registry = Some(Vec::new());
        Ok(BulkStoreSession { finished: false })
    }

    /// The barrier: fsync every staged write's bytes, then — only once every byte is durable —
    /// rename each into place and fsync the directories that changed.
    ///
    /// Runs in this exact order:
    /// 1. A *cheap* data-to-device fsync per staged temp file (`libc::fsync`): on Linux that
    ///    already is full durability; on macOS it only queues the write to the drive, without
    ///    flushing its cache; on Windows there is no cheaper variant, so this step falls back to
    ///    `File::sync_all` per file.
    /// 2. macOS only: one `F_FULLFSYNC` on any single staged file. The drive's write cache is
    ///    flushed device-wide, not per file, so one flush covers every write queued in step 1.
    /// 3. Only now: rename every temp file to its final name (metadata-only, so fast) and record
    ///    each distinct parent directory touched.
    /// 4. Fsync every touched parent directory, so the renames themselves survive power loss —
    ///    using the exact same cheap-then-shared-flush structure as steps 1-2, one level up: a
    ///    cheap per-directory `fsync` (Unix only — matching [`sync_dir`]'s own read-only open;
    ///    Windows has no directory handle to fsync at all, see `sync_dir`'s doc comment) queues
    ///    every touched directory's entry, then one more macOS-only `F_FULLFSYNC` (on any one of
    ///    them) flushes the device-wide write cache again — covering every directory queued in
    ///    this step, not just the file data from steps 1-2. On Linux the per-directory `fsync`
    ///    alone is already full durability (as in step 1), so no second flush runs there either.
    ///    A directory is fsynced *after* the renames into it (this step), never before — a
    ///    directory entry cannot be queued for durability before the change that creates it
    ///    exists on the filesystem.
    ///
    /// [`fsync_enabled`] gates steps 1, 2 and 4 exactly like every other writer's durability
    /// escape hatch — but the deferred-rename structure in step 3 always applies regardless,
    /// since that is what preserves the invariant, not the fsyncing.
    ///
    /// Crash analysis: before step 3 begins, only temp names exist on disk — invisible to every
    /// reader, cleaned up by the existing stale-temp patterns that already tolerate a crashed
    /// single-file write. During step 3, the durability barrier (steps 1-2) has already
    /// completed, so every temp file's bytes are durable *before* any rename runs — a crash
    /// partway through step 3 leaves some names published (durable bytes, correctly visible) and
    /// the rest as still-invisible temps. The invariant — a final name never exists before its
    /// bytes are durable — holds in every interleaving. Step 4 only ever makes an *already*
    /// kernel-visible rename power-loss durable too (the same "visible now, power-loss durable
    /// once fsynced" gap every single-write path already has between its own rename and its own
    /// `sync_dir` call); it never widens what a crash between steps 3 and 4 can lose, since that
    /// window is exactly the one `sync_dir` closes on every other path in this codebase, applied
    /// once per directory instead of once per write.
    ///
    /// A hard kill mid-barrier is covered by that crash analysis (nothing survives that isn't
    /// either durable-and-published or an invisible temp for the existing stale-temp patterns to
    /// find later). A *returned* error (disk full, a permission flip) is different: the process
    /// keeps running, and unlike the pack folder the loose store has no stale-temp sweeper, so a
    /// stranded temp would leak forever. `run_barrier` therefore cleans up every staged temp
    /// itself before propagating any error — see its doc comment — which is why `finished` is
    /// only set to `true` on success below: on the error path the registry slot and every temp
    /// are already gone, so `Drop`'s own abort pass (see below) just finds nothing left to do.
    pub fn finish(mut self) -> Result<(), String> {
        self.run_barrier()?;
        self.finished = true;
        Ok(())
    }

    /// Runs the four-step barrier described on [`finish`](BulkStoreSession::finish). On *any*
    /// error partway through, best-effort removes every staged temp before returning it — no
    /// staged write may survive a failed `finish`, since nothing else will ever clean it up.
    /// Harmless for an entry already renamed by the time the failure hit (its temp path is gone,
    /// so removing it again just fails `NotFound`, ignored) — that entry is durably published,
    /// which is a fine outcome for it, not a violation: the barrier had already made its bytes
    /// durable before any rename began.
    fn run_barrier(&mut self) -> Result<(), String> {
        let pending = bulk_session_registry().lock().expect("bulk store session lock poisoned")
            .take().unwrap_or_default();

        if pending.is_empty() {
            return Ok(());
        }

        if let Err(error) = run_write_barrier(&pending) {
            for (temp, _) in &pending {
                let _ = std::fs::remove_file(temp);
            }
            return Err(error);
        }

        Ok(())
    }
}

/// The four-step durability barrier shared by [`BulkStoreSession`] and [`WriteBatch`] — see
/// [`BulkStoreSession::finish`]'s doc comment for the exact steps and the crash analysis. Kept
/// as a free function (not tied to either type) so both share exactly one implementation of the
/// crash-safety-critical ordering instead of risking it drifting between two copies.
fn run_write_barrier(pending: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    if fsync_enabled() {
        for (temp, _) in pending {
            fsync_data(temp)?;
        }

        #[cfg(target_os = "macos")]
        macos_flush_device_cache(&pending[0].0)?;
    }

    let mut touched_parents: BTreeSet<PathBuf> = BTreeSet::new();
    for (temp, final_path) in pending {
        std::fs::rename(temp, final_path).map_err(|e| format!(
            "Error while moving file into place at \"{}\": {}", final_path.to_string_lossy(), e
        ))?;
        if let Some(parent) = final_path.parent() {
            if !parent.as_os_str().is_empty() {
                touched_parents.insert(parent.to_path_buf());
            }
        }
    }

    if fsync_enabled() {
        // `pending[0].1` (a final path) is guaranteed to exist now (every rename above already
        // ran) and is an ordinary file, safely opened write-mode by `macos_flush_device_cache` —
        // unlike a directory, which `OpenOptions::write(true).open` refuses with `EISDIR` on
        // macOS. Reusing it here (rather than any of `touched_parents`) is what lets the
        // directory half of the flush share `macos_flush_device_cache` unchanged.
        sync_touched_directories(&touched_parents, &pending[0].1)?;
    }

    Ok(())
}

/// Fsync every touched directory once the renames into it are done (step 4 of
/// [`run_write_barrier`] — see [`BulkStoreSession::finish`]'s doc comment for the full
/// crash-ordering analysis). Uses the same cheap-then-shared-flush structure as the file half of
/// the barrier (steps 1-2: [`fsync_data`] + one [`macos_flush_device_cache`]), one level up:
/// batches what would otherwise be one full (`F_FULLFSYNC`-class) directory fsync *per touched
/// directory* ([`sync_dir`], called individually) into a cheap `fsync` per directory plus one
/// shared device flush — the same aggregation [`run_write_barrier`] already applies to file data,
/// applied here to directory *entries* instead.
///
/// # Arguments
/// * `touched_parents` - Every distinct directory a rename just landed in.
/// * `flush_via`       - A regular file (a final, already-renamed path) to open write-mode for
///                       the macOS device flush — a directory cannot be opened write-mode
///                       (`EISDIR`), so the flush borrows any file guaranteed to already exist.
///
/// Unix only, exactly like [`sync_dir`]: on non-Unix targets there is no directory handle to
/// fsync at all (NTFS gives the ordering some other way — see `sync_dir`'s doc comment), so the
/// non-Unix half of this is a no-op, matching `sync_dir`'s own no-op there.
#[cfg(unix)]
fn sync_touched_directories(touched_parents: &BTreeSet<PathBuf>, flush_via: &Path) -> Result<(), String> {
    for parent in touched_parents {
        fsync_dir_data(parent)?;
    }

    #[cfg(target_os = "macos")]
    if !touched_parents.is_empty() {
        macos_flush_device_cache(flush_via)?;
    }

    Ok(())
}

#[cfg(not(unix))]
fn sync_touched_directories(_touched_parents: &BTreeSet<PathBuf>, _flush_via: &Path) -> Result<(), String> {
    Ok(())
}

/// Fsync one directory's pending entry changes (e.g. a rename into it) to the drive — the
/// directory counterpart of [`fsync_data`], used by [`sync_touched_directories`]. Opens
/// read-only, exactly like [`sync_dir`]'s own open: a directory cannot be opened write-mode on
/// this platform (`EISDIR`), and POSIX `fsync` works on a read-only descriptor regardless (see
/// [`open_for_sync`]'s doc comment for the same point about files, where a write-mode open is
/// chosen only for Windows' sake — moot here, since this function is Unix-only).
#[cfg(unix)]
fn fsync_dir_data(dir: &Path) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::File::open(dir)
        .map_err(|e| format!("Error while opening directory \"{}\" to sync it: {}", dir.to_string_lossy(), e))?;

    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(format!(
            "Error while syncing directory \"{}\": {}", dir.to_string_lossy(), std::io::Error::last_os_error()
        ));
    }

    Ok(())
}

/// A local, explicitly-scoped write batch — the same deferred-publish idea as
/// [`BulkStoreSession`] (see its doc comment for the full crash analysis, which applies here
/// unchanged), but passed by the caller instead of intercepted through the process-wide
/// registry, and safe to share (via `Arc`) across concurrent writer threads.
///
/// [`BulkStoreSession`] is documented as a single-sequential-writer primitive — intercepting
/// *every* [`write_file_atomically`] call in the process through one global registry slot is
/// exactly what makes it unsafe to open around a burst of writes that come from parallel
/// workers (an unrelated concurrent write elsewhere in the process would be silently swept into
/// the same batch). `WriteBatch` avoids that by construction: nothing is intercepted — a writer
/// must explicitly call [`stage`](WriteBatch::stage) on a specific instance, so only writes the
/// caller actually hands to *this* batch ever participate in it. Every `stage` call's own file
/// write happens before it ever touches the shared list, so concurrent callers only ever
/// contend on the cheap final push.
///
/// It exists for `stack`'s tree build, whose object writes run from `TaskExecutor`'s parallel
/// workers (see `model::task::tree_builder::tree_builder_context::ObjectSink`).
///
/// `finish` takes `&self`, not `self` by value (unlike `BulkStoreSession::finish`): a
/// `WriteBatch` is shared via `Arc` across parallel workers for the whole build, so there is no
/// single owner to consume. This is safe because `finish` (and `Drop`) both operate by taking
/// (`mem::take`/`drain`) whatever is currently staged — idempotent by construction: a successful
/// `finish` leaves `pending` empty, so a second `finish` call, or the `Drop` that eventually
/// runs, both find nothing left to do. A *failed* `finish` also leaves `pending` empty (every
/// staged temp is removed before the error is returned — see below), so nothing double-cleans up
/// there either.
///
/// The caller must not let anything that *depends on* a staged write's visibility (most
/// importantly: a ref pointing at a batched object) run until `finish` has returned `Ok` —
/// batching does not make the batch's renames atomic as a set (a crash mid-`finish` can still
/// leave some entries published and others not, exactly like `BulkStoreSession` — see its doc
/// comment), it only amortizes the fsyncs. `stack_utils::stack_parcel` relies on this: it never
/// batches the pallet-head ref write itself, and only calls `set_pallet_head` after the batch
/// covering every object the parcel references has already finished successfully.
pub struct WriteBatch {
    pending: std::sync::Mutex<Vec<(PathBuf, PathBuf)>>,
}

impl WriteBatch {
    /// Create a new, empty write batch.
    pub fn new() -> Self {
        Self { pending: std::sync::Mutex::new(Vec::new()) }
    }

    /// Stage a write: create and write a fresh temp file now (no fsync — that is the whole
    /// point of batching), and record the `(temp, final)` pair to publish in [`finish`](Self::finish).
    /// `file_path` stays exactly as invisible as it was before this call until `finish` runs —
    /// the atomic-visibility rule (a final name never exists before its bytes are durable)
    /// still holds, the rename has simply not happened yet.
    ///
    /// Safe to call from multiple threads at once: each call's write happens before it ever
    /// touches the shared list, so concurrent callers never contend on anything but the final,
    /// pointer-cheap push.
    ///
    /// # Returns
    /// * `Ok(())`      - If the write was staged.
    /// * `Err(String)` - If the temp file could not be created or written.
    pub fn stage(&self, file_path: &Path, content: &[u8]) -> Result<(), String> {
        let temp_path = temp_path_for(file_path)?;
        create_and_write_file(&temp_path, content)?;

        self.pending.lock().expect("write batch lock poisoned").push((temp_path, file_path.to_path_buf()));

        Ok(())
    }

    /// The barrier: fsync every staged write's bytes, then — only once every byte is durable —
    /// rename each into place and fsync the directories that changed. See
    /// [`BulkStoreSession::finish`]'s doc comment for the exact four steps and the full crash
    /// analysis (shared implementation, [`run_write_barrier`]): every conclusion there — no
    /// torn object survives any crash interleaving, an in-flight error leaves no stranded temp —
    /// applies here unchanged.
    ///
    /// # Returns
    /// * `Ok(())`      - Every staged write is durable and visible at its final path.
    /// * `Err(String)` - A write or rename failed; every staged temp for this batch was
    ///                   best-effort removed before returning (nothing survives to leak).
    pub fn finish(&self) -> Result<(), String> {
        let pending = std::mem::take(&mut *self.pending.lock().expect("write batch lock poisoned"));

        if pending.is_empty() {
            return Ok(());
        }

        if let Err(error) = run_write_barrier(&pending) {
            for (temp, _) in &pending {
                let _ = std::fs::remove_file(temp);
            }
            return Err(error);
        }

        Ok(())
    }
}

impl Default for WriteBatch {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for WriteBatch {
    /// Whatever is still staged when a `WriteBatch` is dropped was never published — a
    /// successful or failed [`finish`](WriteBatch::finish) both empty `pending` on every path
    /// (see their doc comments), so this only ever finds work to do when `finish` was never
    /// called at all (an early `?` return elsewhere). Best-effort removes those temps so that
    /// case leaks nothing either — the durability invariant holds trivially, since nothing
    /// staged here was ever published.
    fn drop(&mut self) {
        if let Ok(mut pending) = self.pending.lock() {
            for (temp, _) in pending.drain(..) {
                let _ = std::fs::remove_file(&temp);
            }
        }
    }
}

/// Stage an object write into a [`WriteBatch`] instead of writing (and fsyncing) it immediately.
/// See [`write_object_to_file`] for the immediate form and [`WriteBatch`] for why the deferred
/// form exists.
///
/// # Arguments
/// * `path`      - The path to the folder where the object should be stored.
/// * `file_name` - The name of the file where the object should be stored.
/// * `content`   - The content of the object (should be compressed).
/// * `batch`     - The batch to stage the write into.
///
/// # Returns
/// * `Ok(())`      - If the write was staged.
/// * `Err(String)` - If an error occurred while staging the write.
pub fn write_object_to_file_deferred(path: &Path, file_name: &str, content: Vec<u8>,
                                     batch: &WriteBatch) -> Result<(), String> {
    let mut file_path = PathBuf::from(path);
    file_path.push(file_name);

    create_folder_if_not_exists(path)?;

    batch.stage(&file_path, &content)
}

impl Drop for BulkStoreSession {
    /// Dropping without calling [`finish`](BulkStoreSession::finish) aborts: no staged write was
    /// ever fsynced or renamed, so best-effort removing the temp files publishes nothing — the
    /// durability invariant holds trivially, since there is nothing to un-publish. This also runs
    /// (as a harmless no-op) after a *failed* `finish`: `run_barrier` already removed every temp
    /// and cleared the registry itself before returning its error, so there is nothing left here
    /// to find.
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        if let Ok(mut registry) = bulk_session_registry().lock() {
            if let Some(pending) = registry.take() {
                for (temp, _) in pending {
                    let _ = std::fs::remove_file(&temp);
                }
            }
        }
    }
}

/// Open `path` for a sync/flush call that follows. A *write* handle, not a read-only one:
/// Windows refuses `FlushFileBuffers` on a handle without write access, while POSIX fsyncs any
/// descriptor — so a read-only open works on macOS/Linux and fails on Windows only (see the same
/// constraint on `pack_utils::sync_file`). Opening for write here keeps every platform's staged
/// temp file syncable through one code path.
fn open_for_sync(path: &Path) -> Result<std::fs::File, String> {
    std::fs::OpenOptions::new().write(true).open(path)
        .map_err(|e| format!("Error while opening \"{}\" to sync it: {}", path.to_string_lossy(), e))
}

/// Fsync one file's data to the drive — the *cheap* half of the durability barrier compared to
/// `File::sync_all`. `libc::fsync` only queues the write to the drive: on Linux that already is
/// full durability (there is no cheaper-vs-complete distinction there), so this is the entire
/// per-file cost. On macOS it is cheaper than `sync_all`'s `F_FULLFSYNC` but leaves the drive's
/// write cache unflushed — which is why [`BulkStoreSession::finish`] still runs one
/// `F_FULLFSYNC` afterwards (see [`macos_flush_device_cache`]) to cover the whole batch.
#[cfg(unix)]
fn fsync_data(path: &Path) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let file = open_for_sync(path)?;
    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(format!(
            "Error while syncing \"{}\": {}", path.to_string_lossy(), std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Windows has no cheaper variant than a full flush, so the per-file step of the barrier falls
/// back to `File::sync_all` here — the batch still amortizes the rename and directory syncs
/// (steps 3-4 of [`BulkStoreSession::finish`]), just not this one.
#[cfg(windows)]
fn fsync_data(path: &Path) -> Result<(), String> {
    open_for_sync(path)?.sync_all()
        .map_err(|e| format!("Error while syncing \"{}\": {}", path.to_string_lossy(), e))
}

/// Flush the drive's write cache once via `fcntl(F_FULLFSYNC)` — macOS's actual device-cache
/// flush, which [`fsync_data`] deliberately does not pay per file. The flush is drive-wide, not
/// file-specific, so running it on any one of the batch's files covers every write already
/// queued there by [`fsync_data`].
#[cfg(target_os = "macos")]
fn macos_flush_device_cache(path: &Path) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    let file = open_for_sync(path)?;
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_FULLFSYNC) } == -1 {
        return Err(format!(
            "Error while flushing the device cache via \"{}\": {}",
            path.to_string_lossy(), std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

/// Write an object to a file (atomically, see [`write_file_atomically`]): object writes are
/// skipped when the hash already exists, so a truncated object would never be repaired.
///
/// # Arguments
/// * `path`      - The path to the folder where the object should be stored.
/// * `file_name` - The name of the file where the object should be stored.
/// * `content`   - The content of the object (should be compressed).
///
/// # Returns
/// * `Ok(())`      - If the object was written to the file successfully.
/// * `Err(String)` - If an error occurred while writing the object to the file.
pub fn write_object_to_file(path: &Path, file_name: &str, content: Vec<u8>) -> Result<(), String> {
    let mut file_path = PathBuf::from(path);
    file_path.push(file_name);

    create_folder_if_not_exists(path)?;

    write_file_atomically(&file_path, &content)
}

/// Retrieve the decompressed bytes of the object with the given hash, through a bounded,
/// content-addressed read cache.
///
/// The cache is what makes reconstruction-heavy reads (`blame`, `export`, cross-revision
/// `diff`) viable: those resolve the same objects — and the same delta *bases* — over and over
/// as they walk history, so without a cache each reconstruction re-walks its whole chain. Since
/// an object's hash *is* its content (immutable), a cached value is always valid — no invalidation,
/// even across a `compact` that relocates the bytes. It is bounded by a byte budget and keyed
/// by the warehouse's object root too, so a server hosting several warehouses never serves one
/// warehouse's bytes for another's request.
///
/// # Arguments
/// * `hash` - The hash of the object to retrieve.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The decompressed bytes of the object.
/// * `Err(String)` - The error message.
///
/// This owns-the-bytes form is for callers that keep the object (a network response body, a
/// bundle sink, a value stored in a struct). Callers that only *borrow* the bytes to parse them
/// (`object_utils::load_tree`/`load_blob`, the pack delta-base reads) should use
/// [`retrieve_object_by_hash_shared`], which hands back the cached `Arc` so a hit is a pointer
/// clone and the one cached allocation is shared instead of copied.
pub fn retrieve_object_by_hash(hash: &str) -> Result<Vec<u8>, String> {
    // The single copy an owned caller needs happens here, *outside* the cache lock — the
    // critical section is only the pointer-sized `Arc` clone inside `retrieve_object_by_hash_shared`.
    Ok(retrieve_object_by_hash_shared(hash)?.as_ref().clone())
}

/// Retrieve the decompressed bytes of the object with the given hash as a shared
/// [`std::sync::Arc`], through the same content-addressed read cache as [`retrieve_object_by_hash`].
///
/// A cache hit clones an `Arc` (a pointer bump) under the lock rather than copying the bytes, so
/// the critical section is pointer-sized regardless of object size — the lever that keeps a
/// read-bound parallel loop from serializing on the cache mutex. The caller then borrows the
/// bytes (`&*arc`) to parse them, sharing the one cached allocation. A caller that needs owned
/// bytes uses [`retrieve_object_by_hash`], which clones once at that boundary (off the lock).
///
/// The returned `Arc` is safe to hold across a storage-scope switch: an object is addressed by
/// (and verified against) its hash, so its bytes are the same bytes in every warehouse that
/// holds it — the cache key isolates *presence* per warehouse, never the content.
pub fn retrieve_object_by_hash_shared(hash: &str) -> Result<std::sync::Arc<Vec<u8>>, String> {
    if let Some(bytes) = read_cache_get(hash) {
        return Ok(bytes);
    }

    // Wrap the freshly read bytes in the `Arc` once and share that same allocation with the
    // cache — the caller and the cached entry point at one buffer, never two.
    let bytes = std::sync::Arc::new(read_object_uncached(hash)?);
    read_cache_put(hash, std::sync::Arc::clone(&bytes));
    Ok(bytes)
}

/// Retrieve an object's bytes **without** consulting or populating the read cache.
///
/// The cache pays for itself on reconstruction-heavy walks that re-read the same objects and
/// delta *bases* (`blame`/`export`/`diff` over trees and blobs). It is pure overhead, though,
/// for objects read once and never delta-reconstructed — parcels, which are stored full and
/// which a full-history `history` walk reads exactly once. Reading those through this bypass
/// skips the per-read cache-key allocation and the cache churn (inserting tens of thousands of
/// single-use entries), and leaves the cache budget for the trees and blobs that reuse it.
pub fn retrieve_object_by_hash_uncached(hash: &str) -> Result<Vec<u8>, String> {
    read_object_uncached(hash)
}

/// Read an object's decompressed bytes straight from the store, without consulting the read
/// cache. The uncached body of [`retrieve_object_by_hash`].
///
/// Packs are consulted *first*: locating an object in a pack is a syscall-free binary search of
/// the resident index, so once a warehouse is compacted (the common case at scale — most objects
/// are packed) a read is served without the guaranteed-to-fail loose `open` it used to pay on
/// every packed object. The loose store is the fallback for an object written but not yet packed.
fn read_object_uncached(hash: &str) -> Result<Vec<u8>, String> {
    // Packs first (free index lookup, then one positional read on a cached handle).
    // Pack reads are content-verified inside `pack_utils::resolve_record`.
    if let Some(bytes) = crate::util::pack_utils::retrieve_from_packs(hash)? {
        return Ok(bytes);
    }

    // Loose fallback: a freshly written object not yet swept into a pack.
    let (path, file_name) = get_path_for_object(hash)?;
    let file_path = path.add(PATH_SEPARATOR).add(&file_name);

    let compressed = match std::fs::read(&file_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Neither the cached packs nor a loose file holds it. In a long-running process (a
            // live server) the pack registry can predate an external `compact` that moved this
            // object into a new pack and swept its loose source — so reload the registry once and
            // retry the packs before concluding the object is gone (reload-on-miss).
            if let Some(bytes) = crate::util::pack_utils::retrieve_from_packs_reloading(hash)? {
                return Ok(bytes);
            }
            return Err(format!("Error while reading object from file \"{}\": {}", file_path, error));
        }
        Err(error) => return Err(format!("Error while reading object from file \"{}\": {}", file_path, error)),
    };

    let bytes = zstd::stream::decode_all(compressed.as_slice())
        .map_err(|e| format!("Error while decompressing object: {}", e))?;

    // Same content-addressing guarantee the pack path enforces: a corrupt loose file fails the
    // read rather than silently returning wrong bytes.
    crate::util::object_utils::verify_object_bytes(hash, &bytes)?;

    Ok(bytes)
}

/// The read cache's byte budget. When the live generation reaches it, it is retired to the
/// second generation and a fresh one starts — an approximate LRU that never exceeds ~2× this.
const READ_CACHE_BUDGET: usize = 128 * 1024 * 1024;

/// Objects larger than this are not cached (one huge object must not evict the whole working
/// set of small trees and blobs a walk actually reuses).
const READ_CACHE_MAX_ENTRY: usize = READ_CACHE_BUDGET / 8;

/// A bounded content-addressed object cache (two generations for approximate LRU). Entries are
/// `Arc`-shared, so a hit clones a pointer under the lock and the caller shares that one
/// allocation — an owned-bytes caller copies out afterwards, off the lock (see
/// [`retrieve_object_by_hash`] vs [`retrieve_object_by_hash_shared`]).
struct ReadCache {
    live: std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>,
    old: std::collections::HashMap<String, std::sync::Arc<Vec<u8>>>,
    live_bytes: usize,
}

fn read_cache() -> &'static std::sync::Mutex<ReadCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<ReadCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(ReadCache {
        live: std::collections::HashMap::new(),
        old: std::collections::HashMap::new(),
        live_bytes: 0,
    }))
}

/// The cache key: the object hash qualified by the warehouse's object root, so a server hosting
/// several warehouses keeps them isolated (identical content shares a hash regardless, but a
/// warehouse must never be served an object it does not itself hold).
///
/// `pub(crate)` so `object_utils`'s parsed-tree cache can key itself identically — the two
/// caches must never disagree about which warehouse a hash belongs to.
pub(crate) fn read_cache_key(hash: &str) -> String {
    format!("{}\u{0}{}", get_path_objects_root(), hash)
}

fn read_cache_get(hash: &str) -> Option<std::sync::Arc<Vec<u8>>> {
    let key = read_cache_key(hash);
    let mut cache = read_cache().lock().expect("the read cache lock is poisoned");

    if let Some(bytes) = cache.live.get(&key) {
        // Clone the `Arc`, not the bytes: the critical section is a pointer bump, not a memcpy.
        return Some(std::sync::Arc::clone(bytes));
    }

    // A hit in the older generation is promoted to the live one (so it survives the next retire).
    if let Some(bytes) = cache.old.remove(&key) {
        let out = std::sync::Arc::clone(&bytes);
        cache.live_bytes += bytes.len();
        cache.live.insert(key, bytes);
        retire_if_full(&mut cache);
        return Some(out);
    }

    None
}

fn read_cache_put(hash: &str, bytes: std::sync::Arc<Vec<u8>>) {
    if bytes.len() > READ_CACHE_MAX_ENTRY {
        return;
    }

    let key = read_cache_key(hash);
    let mut cache = read_cache().lock().expect("the read cache lock is poisoned");

    if cache.live.contains_key(&key) {
        return;
    }

    // Store the caller's `Arc` directly — the fetched allocation is shared, never re-copied.
    cache.live_bytes += bytes.len();
    cache.live.insert(key, bytes);
    retire_if_full(&mut cache);
}

/// Retire the live generation to the old one (dropping the previous old generation) once it
/// fills — bounding the cache to ~2× the budget with O(1) eviction.
fn retire_if_full(cache: &mut ReadCache) {
    if cache.live_bytes >= READ_CACHE_BUDGET {
        cache.old = std::mem::take(&mut cache.live);
        cache.live_bytes = 0;
    }
}

/// Retrieve the bytes of the inventory data file for the given warehouse path key.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory to retrieve the inventory for
///           (see `WarehousePath::as_key`).
///
/// # Returns
/// * `Ok(Vec<u8>)` - The bytes of the inventory data file.
/// * `Err(String)` - If the inventory does not exist, or an error occurred while reading it.
pub fn retrieve_inventory_by_key(key: &str) -> Result<(PathBuf, Vec<u8>), String> {
    let (path, bytes_opt) = retrieve_inventory_or_none_by_key(key)?;
    let bytes = bytes_opt.ok_or(format!(
        "Inventory file not found for folder \"{}\".",
        if key.is_empty() { "./" } else { key }
    ))?;

    Ok((path, bytes))
}

/// Retrieve the bytes of the inventory file associated with the given warehouse path key,
/// or return `None`, if no inventory exists for the given key.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory to retrieve the inventory for.
///
/// # Returns
/// * `Ok((PathBuf, Some(Vec<u8>)))` - If the inventory file was found:
///    * `PathBuf`       - The path of the inventory file.
///    * `Some(Vec<u8>)` - The contents of the inventory file.
/// * `Ok((PathBuf, None))` - If the inventory file was not found:
///    * `PathBuf` - The path where the inventory file should have been.
/// * `Err(String)` - The error message if the inventory file exists, but there was an error while
/// reading it.
pub fn retrieve_inventory_or_none_by_key(key: &str) -> Result<(PathBuf, Option<Vec<u8>>), String> {
    let file_path = get_inventory_data_path_for_key(key);

    if !file_path.exists() {
        return Ok((file_path, None));
    }

    std::fs::read(&file_path).map_err(|e|
        format!("Error while reading inventory from file \"{}\": {}", file_path.to_string_lossy(), e)
    ).map(|bytes| (file_path, Some(bytes)))
}

/// Retrieve the contents of the inventory metadata file (i.e. the paths of existing inventory
/// files), if it exists.
///
/// # Returns
/// * `Ok((PathBuf, Some(BTreeSet<String>)))` - If the inventory metadata was found:
///    * `PathBuf`                - The path of the inventory metadata file.
///    * `Some(BTreeSet<String>)` - The paths of inventory files in a `BTreeSet`.
/// * `Ok((PathBuf, None))` - If the inventory metadata file does not exist:
///    * `PathBuf` - The path where the inventory metadata file should have been.
/// * `Err(String)` - The error message if the inventory metadata file exists, but there was an
/// error while reading it.
pub fn retrieve_inventory_metadata_or_none() -> Result<(PathBuf, Option<BTreeSet<String>>), String> {
    let mut metadata_store_path = PathBuf::from(get_path_inventory_root());
    metadata_store_path.push(FILE_NAME_INVENTORY_METADATA);

    if !metadata_store_path.exists() {
        return Ok((metadata_store_path, None));
    }

    let metadata_bytes = std::fs::read(&metadata_store_path)
        .map_err(|e| format!("Error while reading inventory metadata from file \"{}\": {}", metadata_store_path.to_string_lossy(), e))?;
    let mut metadata: BTreeSet<String> = BTreeSet::new();

    let mut cursor = 0usize;

    while let Some((line, bytes_read)) = byte_utils::read_line(cursor, &metadata_bytes) {
        cursor += bytes_read;
        let path = String::from_utf8(line).map_err(|e| format!("Error while parsing inventory metadata line as UTF-8: {}", e))?;
        metadata.insert(path);
    }

    Ok((metadata_store_path, Some(metadata)))
}

/// Get the path of the inventory folder associated with the given warehouse path key.
///
/// The warehouse root maps to a folder named after the inventory folder prefix, and *every*
/// path component below it is prefixed as well, so entries in the working directory can never
/// collide with the inventory data / metadata files. E.g. for the key `src/data`, the folder is
/// `.forklift/inventory/inv_/inv_src/inv_data`, which cannot collide with the data *file* of
/// `src` (`.forklift/inventory/inv_/inv_src/data`).
///
/// Nesting the folders like this also means that the inventory folder of a directory contains
/// the inventory folders of all of its subdirectories, so removing a directory's inventory
/// removes the inventories of its subdirectories as well.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory (see `WarehousePath::as_key`).
///
/// # Returns
/// * `PathBuf` - The path of the inventory folder.
pub fn get_inventory_folder_for_key(key: &str) -> PathBuf {
    let mut folder = PathBuf::from(get_path_inventory_root());
    folder.push(PREFIX_INVENTORY_FOLDER);

    if !key.is_empty() {
        for component in key.split(PATH_SEPARATOR_CHAR) {
            folder.push(format!("{}{}", PREFIX_INVENTORY_FOLDER, component));
        }
    }

    folder
}

/// Get the path of the inventory data file associated with the given warehouse path key.
/// Note that this function only calculates the path; it does not check whether the file exists.
///
/// # Arguments
/// * `key` - The warehouse path key of the directory (see `WarehousePath::as_key`).
///
/// # Returns
/// * `PathBuf` - The path of the inventory data file.
pub fn get_inventory_data_path_for_key(key: &str) -> PathBuf {
    let mut file_path = get_inventory_folder_for_key(key);
    file_path.push(FILE_NAME_INVENTORY_DATA);

    file_path
}

/// Check if an object with the given hash exists.
///
/// # Arguments
/// * `hash` - The hash of the object to check.
///
/// # Returns
/// * `Ok(true)`    - If the object exists.
/// * `Ok(false)`   - If the object does not exist.
/// * `Err(String)` - If an error occurred while checking if the object exists.
pub fn does_object_exist(hash: &str) -> Result<bool, String> {
    // Packs first: a resident-index lookup is syscall-free, so a packed object needs no stat.
    if crate::util::pack_utils::is_in_packs(hash)? {
        return Ok(true);
    }

    // Otherwise it may be loose (written but not yet packed).
    let (path, file_name) = get_path_for_object(hash)?;
    let file_path = path.add(PATH_SEPARATOR).add(&file_name);

    std::fs::exists(&file_path)
        .map_err(|e| format!("Error while checking if object exists: {}", e))
}

/// Get the UTF-8 encoded name of a file or directory.
///
/// # Arguments
/// * `item` - The file or directory to get the name for.
///
/// # Returns
/// * `Ok(String)`  - The name of the file or directory.
/// * `Err(String)` - If an error occurred while converting the name to UTF-8.
pub fn get_name_for_file_or_directory(item: &std::fs::DirEntry) -> Result<String, String> {
    item.file_name().into_string()
        .map_err(|_| "Error while converting name to UTF-8".to_string())
}

/// Check if a directory entry is executable.
///
/// # Arguments
/// * `metadata` - The metadata of the dir entry.
///
/// # Returns
/// * `Ok(true)`    - If the directory entry is executable.
/// * `Ok(false)`   - If the directory entry is not executable.
/// * `Err(String)` - If an error occurred while checking if the directory entry is executable.
#[cfg(unix)]
pub fn is_dir_entry_executable(metadata: &Metadata) -> bool {
    metadata.permissions().mode() & 0o111 != 0
}

/// Check if a directory entry is executable.
///
/// # Arguments
/// * `metadata` - The metadata of the dir entry.
///
/// # Returns
/// * `true`  - If the directory entry is executable.
/// * `false` - If the directory entry is not executable.
// We don't need to track UNIX executable files in windows. Treat all files as not executable
// on windows. Make sure to ignore this flag on windows even when detecting changes based on
// file metadata.
#[cfg(windows)]
pub fn is_dir_entry_executable(_metadata: &Metadata) -> bool {
    false
}

/// Read the content of a directory.
///
/// # Arguments
/// * `path` - The path to the directory.
///
/// # Returns
/// * `Ok(std::fs::ReadDir)` - The content of the directory (as a list of directory entries).
/// * `Err(String)`          - If an error occurred while reading the directory.
pub fn read_directory(path: &PathBuf) -> Result<std::fs::ReadDir, String> {
    std::fs::read_dir(path).map_err(|e| format!(
        "Error while reading directory \"{}\": {}",
        path.to_str().unwrap_or(""),
        e
    ))
}

/// Get the name of the directory or file from the given path.
///
/// # Arguments
/// * `path` - The path to the directory or file.
///
/// # Returns
/// * `Ok(Some(String))`  - The name of the directory or file, if it has one.
/// * `Ok(None)`          - If the directory or file does not have a name.
/// * `Err(String)`       - If an error occurred while getting the name of the directory or file.
pub fn get_filename_from_path(path: &Path) -> Result<Option<String>, String> {
    let file_name = path.file_name();

    if let Some(name) = file_name {
        return name.to_str().map_or_else(
            || Err("Error while converting file name to UTF-8.".to_string()),
            |s| Ok(Some(s.to_string()))
        );
    }

    Ok(None)
}

/// Try to convert a path to a UTF-8 string.
///
/// # Arguments
/// * `path` - The path to convert to a string.
///
/// # Returns
/// * `Ok(String)`  - The path as a string.
/// * `Err(String)` - If an error occurred while converting the path to a string.
pub fn path_to_string(path: &Path) -> Result<String, String> {
    path.to_str().ok_or(
        "Error while converting path to string.".to_string()
    ).map(|s| s.to_string())
}

/// Get the type of a directory entry.
/// Note that the given metadata must come from [`get_symlink_metadata_for_path`]
/// (i.e. `lstat` semantics), otherwise symbolic links are never detected.
///
/// # Arguments
/// * `metadata` - The metadata of the directory entry.
///
/// # Returns
/// * `Ok(DirEntryType)` - The type of the directory entry.
/// * `Err(String)`      - If an error occurred while getting the type of the directory entry.
pub fn get_type_of_dir_entry(metadata: &Metadata) -> DirEntryType {
    let is_executable = is_dir_entry_executable(metadata);
    let file_type = metadata.file_type();

    if file_type.is_symlink() {
        DirEntryType::SymbolicLink
    } else if file_type.is_dir() {
        DirEntryType::Tree
    } else if is_executable {
        DirEntryType::Executable
    } else {
        DirEntryType::Normal
    }
}

/// Get the modification timestamp of the metadata of a file.
/// This always returns `0` on Windows, as Windows does not have an alternative to "ctime".
///
/// # Arguments
/// * `file_metadata` - The metadata of the file.
///
/// # Returns
/// * `u64` - The modification timestamp of the metadata. Always `0` on Windows.
#[cfg(unix)]
pub fn get_metadata_modification_timestamp_for_file(file_metadata: &Metadata) -> u64 {
    // A ctime before 1970 (or a filesystem reporting a bogus negative value) must not wrap
    // into a huge u64, as that would break metadata-based change detection.
    file_metadata.ctime().max(0) as u64
}

/// Get the modification timestamp of the metadata of a file.
/// Windows does not have an alternative to "ctime", so the content modification timestamp
/// is reused (see the documentation of `InventoryItem::metadata_change_timestamp`).
///
/// # Arguments
/// * `file_metadata` - The metadata of the file.
///
/// # Returns
/// * `u64` - The modification timestamp of the metadata.
#[cfg(windows)]
pub fn get_metadata_modification_timestamp_for_file(file_metadata: &Metadata) -> u64 {
    get_content_modification_timestamp_for_file(file_metadata).unwrap_or(0)
}

/// Get the modification timestamp of the content of a file.
///
/// # Arguments
/// * `file_metadata` - The metadata of the file.
///
/// # Returns
/// * `Ok(u64)`     - The modification timestamp of the content.
/// * `Err(String)` - If an error occurred while processing the file metadata.
pub fn get_content_modification_timestamp_for_file(file_metadata: &Metadata) -> Result<u64, String> {
    file_metadata.modified()
        .map_or_else(
            |err| Err(format!("Error while getting creation time for file: {}", err)),
            |time| time.duration_since(std::time::SystemTime::UNIX_EPOCH).map_err(|err|
                format!("Error while getting creation time for file: {}", err)
            )
        ).map(|time| time.as_secs())
}

/// Get the file ID for a file.
/// On windows, we use the low resolution file ID.
///
/// # Arguments
/// * `path` - The path to the file.
///
/// # Returns
/// * `Ok(FileId)`  - The file ID for the file.
/// * `Err(String)` - If an error occurred while getting the file ID.
#[cfg(unix)]
pub fn get_file_id_for_file(path: &Path) -> Result<FileId, String> {
    file_id::get_file_id(path).map_err(|e|
        format!("Error while getting file ID for file: {}", e)
    )
}

/// Get the file ID for a file.
/// On windows, we use the low resolution file ID.
///
/// # Arguments
/// * `path` - The path to the file.
///
/// # Returns
/// * `Ok(FileId)`  - The file ID for the file.
/// * `Err(String)` - If an error occurred while getting the file ID.
#[cfg(windows)]
pub fn get_file_id_for_file(path: &Path) -> Result<FileId, String> {
    file_id::get_low_res_file_id(path).map_err(|e|
        format!("Error while getting file ID for file: {}", e)
    )
}

/// Get the owners of a file (user ID and group ID).
/// This always returns `(0, 0)` on Windows, as Windows does not have user or group IDs.
///
/// # Arguments
/// * `metadata` - The metadata of the given file.
///
/// # Returns
/// * `(u64, u64)` - The user ID and group ID of the file owner.
#[cfg(unix)]
pub fn get_owners_for_file(metadata: &Metadata) -> (u64, u64) {
    let user_id = metadata.uid();
    let group_id = metadata.gid();

    (user_id as u64, group_id as u64)
}

/// Get the owners of a file (user ID and group ID).
/// This always returns `(0, 0)` on Windows, as Windows does not have user or group IDs.
///
/// # Arguments
/// * `metadata` - The metadata of the given file.
///
/// # Returns
/// * `(u64, u64)` - The user ID and group ID of the file owner.
#[cfg(windows)]
pub fn get_owners_for_file(_metadata: &Metadata) -> (u64, u64) {
    (0, 0)
}

/// Create the `.forkliftignore` file (with default content) if it does not exist yet.
///
/// # Returns
/// * `Ok(true)`    - If the ignore file was created.
/// * `Ok(false)`   - If the ignore file already existed.
/// * `Err(String)` - If an error occurred while creating the ignore file.
pub fn create_ignore_file_if_not_exists() -> Result<bool, String> {
    let ignore_file_path = crate::globals::warehouse_root().join(FILENAME_IGNORE);
    let mut created_ignore_file = false;

    if !ignore_file_path.exists() {
        std::fs::write(&ignore_file_path, IGNORE_FILE_CONTENT)
            .map_err(|e| format!("Error while creating ignore file: {}", e))?;

        created_ignore_file = true;
    }

    Ok(created_ignore_file)
}

/// Get regex patterns for paths that should be ignored by Forklift.
///
/// # Returns
/// * Ok(Vec<Regex>) - The regex patterns for ignored paths.
/// * Err(String)    - If an error occurred while reading the ignore file.
pub fn get_ignored_paths() -> Result<Vec<Regex>, String> {
    let mut ignored_paths = get_default_ignored_paths()?;
    let ignore_file_path = crate::globals::warehouse_root().join(FILENAME_IGNORE);

    if !ignore_file_path.exists() {
        return Ok(ignored_paths);
    }

    let ignore_file = std::fs::read_to_string(ignore_file_path)
        .map_err(|e| format!("Error while reading ignore file: {}", e))?;

    for line in ignore_file.lines() {
        // Skip empty lines and comments
        if line.is_empty() || line.starts_with(IGNORE_FILE_COMMENT_PREFIX) {
            continue;
        }

        let regex = get_regex_for_pattern(line)?;

        ignored_paths.push(regex);
    }

    Ok(ignored_paths)
}

/// Check if a path should be ignored by Forklift.
///
/// # Arguments
/// * `path`          - The path to check.
/// * `ignored_paths` - The regex patterns for ignored paths.
///
/// # Returns
/// * `true`  - If the path should be ignored.
/// * `false` - If the path should not be ignored.
pub fn is_path_ignored(path: &str, ignored_paths: &Vec<Regex>) -> bool {
    ignored_paths.iter().any(|r| r.is_match(path))
}

/// Get the metadata of the file or directory at the given path.
///
/// # Arguments
/// * `path` - The path.
///
/// # Returns
/// * `Ok(Metadata)` - The metadata.
/// * `Err(String)`  - The error message if there was an error while retrieving the metadata.
pub fn get_metadata_for_path(path: &Path) -> Result<Metadata, String> {
    std::fs::metadata(path)
        .map_err(|e| format!("Error while getting metadata for path: {}", e))
}

/// Get the metadata of the file, directory or symbolic link at the given path,
/// without following symbolic links (i.e. `lstat` semantics).
///
/// This must be used when walking the working directory: following symbolic links would make
/// symlinks undetectable, would recurse into symlinked directories (looping forever on symlink
/// cycles), and would fail on dangling symlinks.
///
/// # Arguments
/// * `path` - The path.
///
/// # Returns
/// * `Ok(Metadata)` - The metadata.
/// * `Err(String)`  - The error message if there was an error while retrieving the metadata.
pub fn get_symlink_metadata_for_path(path: &Path) -> Result<Metadata, String> {
    std::fs::symlink_metadata(path)
        .map_err(|e| format!("Error while getting metadata for path: {}", e))
}

/// Check if a path is a directory. Symbolic links are not followed, so a symbolic link
/// pointing to a directory is not considered a directory (it is tracked as a symlink entry).
///
/// # Arguments
/// * `path` - The path to check.
///
/// # Returns
/// * `true`  - If the path is a directory.
/// * `false` - If the path is a file.
pub fn is_directory(path: &Path) -> Result<bool, String> {
    get_symlink_metadata_for_path(path).map(|m| m.is_dir())
}

/// Get the path of the parent folder of the given file.
///
/// # Arguments
/// * `file_path` - The path of the file.
///
/// # Returns
/// * `Ok(&Path)`   - The path of the parent folder.
/// * `Err(String)` - The error message, if there was an error while retrieving the path of the
/// parent folder.
pub fn get_parent_folder_of_file(file_path: &str) -> Result<&Path, String> {
    Path::new(file_path).parent().ok_or("Error while getting parent folder of file.".to_string())
}

/// Get the regex patterns for paths that should be ignored by Forklift by default.
///
/// # Returns
/// * Vec<Regex> - The regex patterns for ignored paths.
fn get_default_ignored_paths() -> Result<Vec<Regex>, String> {
    DEFAULT_IGNORED_PATHS.iter()
        .map(|p| get_regex_for_pattern(p))
        .collect()
}

/// Get a regex for a pattern.
///
/// # Arguments
/// * `pattern` - The pattern to create a regex for.
///
/// # Returns
/// * Ok(Regex)    - The regex for the pattern.
/// * Err(String)  - If an error occurred while parsing the pattern.
fn get_regex_for_pattern(pattern: &str) -> Result<Regex, String> {
    Regex::new(pattern)
        .map_err(|e| format!("Error while parsing regex pattern: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    #[test]
    fn a_corrupt_loose_object_fails_the_read_instead_of_returning_wrong_bytes() {
        // The loose half of the content-hash verification guarantee: a loose file whose bytes decompress cleanly but do not hash to
        // the address they are stored under (a torn or tampered file). The read must error rather
        // than silently hand back the wrong content.
        let temp = std::env::temp_dir().join(format!("forklift-loose-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Store B's compressed bytes at A's object path — a valid zstd blob, wrong content.
        let content_a = vec![3u8; 4000];
        let content_b = vec![4u8; 4000];
        let hash_a = blake3::hash(&content_a).to_hex().to_string();

        let compressed_b = zstd::encode_all(content_b.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&hash_a).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed_b).unwrap();

        let result = retrieve_object_by_hash(&hash_a);
        assert!(result.is_err(), "a loose file that hashes wrong must fail the read, got {:?}", result);
        assert!(result.unwrap_err().contains("corrupt"), "the error should name the corruption");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_healthy_loose_object_reads_back() {
        // The companion to the corruption test: a well-formed loose object round-trips through
        // the now-verifying read path unchanged.
        let temp = std::env::temp_dir().join(format!("forklift-loose-ok-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = vec![5u8; 4000];
        let hash = blake3::hash(&content).to_hex().to_string();

        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();

        assert_eq!(retrieve_object_by_hash(&hash).unwrap(), content);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn fsync_setting_is_on_unless_explicitly_disabled() {
        // Durability is the default: absent, blank, or any unrecognised value keeps fsync on.
        assert!(parse_fsync_setting(None), "absent means durable");
        assert!(parse_fsync_setting(Some("1")), "1 means durable");
        assert!(parse_fsync_setting(Some("on")), "on means durable");
        assert!(parse_fsync_setting(Some("yes")), "yes means durable");
        assert!(parse_fsync_setting(Some("anything")), "an unknown value stays durable");

        // Only the explicit off tokens (case/space-insensitive) disable it.
        for off in ["0", "off", "false", "no", " OFF ", "False"] {
            assert!(!parse_fsync_setting(Some(off)), "{off:?} must disable fsync");
        }
    }

    #[test]
    fn sync_dir_succeeds_on_a_real_directory() {
        // The durable-rename helper must accept an existing directory (its no-op-on-Windows path
        // returns Ok too, so this holds on every target).
        let temp = std::env::temp_dir().join(format!("forklift-syncdir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        std::fs::write(temp.join("entry"), b"x").unwrap();

        assert!(sync_dir(&temp).is_ok(), "fsync of an existing directory should succeed");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn an_interrupted_write_never_becomes_a_readable_object() {
        // The durability contract's other half: objects are addressed by their hash and the
        // atomic write stages through a `hash.tmp…` sibling, so a crash *between* the temp write
        // and the rename leaves only that temp file — never a truncated file at the object's real
        // path. A reader keys on the hash, so that debris must be invisible: the object does not
        // exist, and a genuine object at the same address still reads back cleanly alongside it.
        let temp = std::env::temp_dir().join(format!("forklift-interrupted-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = vec![7u8; 4000];
        let hash = blake3::hash(&content).to_hex().to_string();
        let (folder, file_name) = get_path_for_object(&hash).unwrap();
        create_folder_if_not_exists(Path::new(&folder)).unwrap();

        // Simulate a crashed write: only the temporary file exists, the real object never landed.
        let debris = Path::new(&folder).join(format!("{}.tmp99999-0", file_name));
        std::fs::write(&debris, b"half-written, never renamed").unwrap();

        assert!(!does_object_exist(&hash).unwrap(), "temp debris must not read as an object");
        assert!(retrieve_object_by_hash(&hash).is_err(), "a never-renamed object must not be readable");

        // Now the real object lands; the leftover temp must not have disturbed it.
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
        assert_eq!(retrieve_object_by_hash(&hash).unwrap(), content);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn atomic_write_lands_the_content_and_leaves_no_temp_file() {
        // The rewritten (now fsyncing) atomic write must still publish exactly the target file with
        // the intended bytes and consume its temporary — a crash-window regression guard.
        let temp = std::env::temp_dir().join(format!("forklift-atomic-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target = temp.join("nested").join("value");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        write_file_atomically(&target, b"durable bytes").unwrap();

        assert_eq!(std::fs::read(&target).unwrap(), b"durable bytes");
        // Overwrite in place — the old content must be fully replaced, still atomically.
        write_file_atomically(&target, b"second").unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"second");

        // No `.tmp*` sibling is left behind once the rename has consumed it.
        let leftovers: Vec<_> = std::fs::read_dir(target.parent().unwrap()).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "no temporary file should survive a successful write");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_cached_object_is_shared_by_arc_not_recopied() {
        // P1: the whole point of the shared read is that a hit hands back the *same* allocation.
        let temp = std::env::temp_dir().join(format!("forklift-arc-share-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = vec![9u8; 4000];
        let hash = blake3::hash(&content).to_hex().to_string();
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();

        // First read caches the bytes; the second is a hit that must return the same `Arc`
        // (a pointer clone), not a fresh copy.
        let first = retrieve_object_by_hash_shared(&hash).unwrap();
        let second = retrieve_object_by_hash_shared(&hash).unwrap();
        assert_eq!(*first, content);
        assert!(std::sync::Arc::ptr_eq(&first, &second),
                "a cache hit must share the one cached allocation, not copy it");

        // The owned-bytes wrapper still yields correct, independent bytes (copied off the lock).
        assert_eq!(retrieve_object_by_hash(&hash).unwrap(), content);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn concurrent_readers_of_one_object_all_get_correct_bytes() {
        // P1: the pointer-sized critical section must stay correct under contention — many
        // threads hammering the same cached object all see the exact bytes, never a torn read.
        let temp = std::env::temp_dir().join(format!("forklift-arc-conc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = vec![0xABu8; 8000];
        let hash = blake3::hash(&content).to_hex().to_string();
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();

        let temp_ref: &Path = &temp;
        let hash_ref: &str = &hash;
        let content_ref: &Vec<u8> = &content;
        std::thread::scope(|scope| {
            for _ in 0..16 {
                scope.spawn(move || {
                    // Storage-root scopes are thread-local, so each worker re-enters it (the read
                    // cache is keyed by the resolved object root).
                    let _s = StorageRootScope::enter(temp_ref);
                    for _ in 0..200 {
                        let bytes = retrieve_object_by_hash_shared(hash_ref).unwrap();
                        assert_eq!(*bytes, *content_ref);
                    }
                });
            }
        });

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_cached_object_is_not_served_across_a_scope_switch() {
        // The multi-warehouse guard: an `Arc` cached for warehouse A must never be handed to
        // warehouse B. The cache key carries the object root, so B (which does not hold the
        // object) fails the read rather than being served A's bytes — a held `Arc` cannot leak
        // across a scope switch.
        let temp_a = std::env::temp_dir().join(format!("forklift-scope-a-{}", std::process::id()));
        let temp_b = std::env::temp_dir().join(format!("forklift-scope-b-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_a);
        let _ = std::fs::remove_dir_all(&temp_b);
        std::fs::create_dir_all(&temp_a).unwrap();
        std::fs::create_dir_all(&temp_b).unwrap();

        let content = vec![0x5Au8; 4000];
        let hash = blake3::hash(&content).to_hex().to_string();

        {
            let _a = StorageRootScope::enter(&temp_a);
            let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
            let (folder, file_name) = get_path_for_object(&hash).unwrap();
            write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
            // Cache it under A.
            assert_eq!(retrieve_object_by_hash(&hash).unwrap(), content);
        }

        {
            let _b = StorageRootScope::enter(&temp_b);
            assert!(retrieve_object_by_hash_shared(&hash).is_err(),
                    "warehouse B must not be served warehouse A's cached object");
        }

        std::fs::remove_dir_all(&temp_a).ok();
        std::fs::remove_dir_all(&temp_b).ok();
    }

    // `WriteBatch` tests (modeled on `tests/bulk_store_session.rs`'s coverage of the same
    // durability barrier). Unlike `BulkStoreSession`, `WriteBatch` is not a process-global
    // registry — each instance is its own, independently owned batch — so these are ordinary
    // unit tests here rather than needing their own integration-test binary: nothing about one
    // test's `WriteBatch` can intercept another's write.

    #[test]
    fn write_batch_finish_is_idempotent() {
        // `finish` takes `&self` (unlike `BulkStoreSession::finish`, which consumes `self`) so it
        // can be shared via `Arc` across parallel workers — its doc comment promises a second
        // call is a harmless no-op, since the first `finish` already took every staged write out
        // of `pending`. This is also what makes `Drop` safe to run unconditionally afterwards.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-idempotent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target = temp.join("staged-object");
        let batch = WriteBatch::new();
        batch.stage(&target, b"batch content").unwrap();

        batch.finish().unwrap();
        assert!(target.exists(), "the first finish must publish the staged write");
        assert_eq!(std::fs::read(&target).unwrap(), b"batch content");

        // A second `finish` on the same batch must succeed and change nothing further.
        batch.finish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"batch content",
            "a second finish must not disturb the already-published content");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_batch_dropped_without_finish_removes_every_staged_temp_and_publishes_nothing() {
        // Abort semantics, exactly like `BulkStoreSession`: a batch dropped without `finish` must
        // remove its staged temp files and must never let the final name come into existence.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target_a = temp.join("never-published-a");
        let target_b = temp.join("never-published-b");
        {
            let batch = WriteBatch::new();
            batch.stage(&target_a, b"should vanish a").unwrap();
            batch.stage(&target_b, b"should vanish b").unwrap();
            assert!(!target_a.exists() && !target_b.exists());
            // `batch` drops here without `finish` being called.
        }

        assert!(!target_a.exists() && !target_b.exists(),
            "a dropped batch must never publish its staged writes");

        let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftovers.is_empty(), "a dropped batch must remove every staged temp, found {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_batch_concurrent_stage_of_the_same_final_path_ends_with_the_file_intact_and_no_leftover_temps() {
        // The duplicate-hash race `stage`'s doc comment allows for: several parallel tree-build
        // workers can legitimately build the *same* content-addressed object (e.g. two identical
        // empty-looking directories) and all stage a write to the same final path before any of
        // them is renamed. `finish` must still end with the final file present and correct, and
        // — since every staged temp is individually consumed by its own rename, even when several
        // renames target the same destination — with none of the (many) temp files left behind.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-duprace-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target = temp.join("duplicate-object");
        let content: &[u8] = b"identical content every racer writes";
        let batch = WriteBatch::new();

        let target_ref: &Path = &target;
        let batch_ref: &WriteBatch = &batch;
        std::thread::scope(|scope| {
            for _ in 0..200 {
                scope.spawn(move || {
                    batch_ref.stage(target_ref, content).unwrap();
                });
            }
        });

        // Every racer staged into the same target, and none may have been visible yet.
        assert!(!target.exists(), "nothing may be visible before finish");

        batch.finish().unwrap();

        assert!(target.exists(), "finish must publish the contested target");
        assert_eq!(std::fs::read(&target).unwrap(), content);

        let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path() != target)
            .collect();
        assert!(leftovers.is_empty(),
            "every staged temp (200 racers) must be consumed by its own rename, found {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_batch_finish_syncs_every_touched_directory_across_a_multi_directory_batch() {
        // `run_write_barrier`'s directory-sync step (`sync_touched_directories`) fsyncs every
        // *distinct* touched parent, not just the first — exercised here transitively: staging
        // writes into three separate directories and confirming `finish` still publishes every
        // one of them (not just the ones sharing a directory with the last-written file, which is
        // the case a bug that only synced one directory would miss).
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-multidir-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let dirs: Vec<_> = ["alpha", "beta", "gamma"].iter().map(|name| temp.join(name)).collect();
        for dir in &dirs {
            std::fs::create_dir_all(dir).unwrap();
        }

        let batch = WriteBatch::new();
        let targets: Vec<_> = dirs.iter().enumerate()
            .map(|(i, dir)| {
                let target = dir.join("object");
                let content = format!("content for directory {}", i);
                batch.stage(&target, content.as_bytes()).unwrap();
                (target, content)
            })
            .collect();

        for (target, _) in &targets {
            assert!(!target.exists(), "nothing may be visible before finish");
        }

        batch.finish().unwrap();

        for (target, content) in &targets {
            assert!(target.exists(), "finish must publish every directory's staged write");
            assert_eq!(std::fs::read_to_string(target).unwrap(), *content);
        }

        std::fs::remove_dir_all(&temp).ok();
    }
}
