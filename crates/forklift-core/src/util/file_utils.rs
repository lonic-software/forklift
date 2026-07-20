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
use crate::util::taint_utils;

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

/// Recover the object hash a loose object's final on-disk path encodes — the exact inverse of
/// [`get_path_for_object`]. Used by [`heal_utils`](crate::util::heal_utils) to tell a loose
/// object apart from every other kind of path a taint can record (a pack data/index file, an
/// inventory shard): only a loose object's path has this exact `<2 hex chars>/<rest>` shape, so
/// `Some` here is also the signal that a content-hash re-check is possible (and required) before
/// restaging — see [`heal_utils::restage_object`](crate::util::heal_utils::restage_object).
///
/// # Returns
/// * `Some(String)` - The hash, if `path`'s last two components have the `<2 hex chars>/<rest>`
///                     fan-out shape [`get_path_for_object`] produces.
/// * `None`         - `path` does not have that shape (not an object path, or corrupt).
pub fn hash_from_object_path(path: &Path) -> Option<String> {
    let file_name = path.file_name()?.to_str()?;
    let folder_name = path.parent()?.file_name()?.to_str()?;

    if folder_name.len() != OBJECT_HASH_FOLDER_PATH_CHARACTERS {
        return None;
    }

    let candidate = format!("{}{}", folder_name, file_name);

    candidate.bytes().all(|b| b.is_ascii_hexdigit()).then_some(candidate)
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

/// Process-wide count of durability barriers actually paid (DESIGN.html §5.0 D item 10) — one per
/// completed immediate write ([`write_file_atomically`], outside an active bulk session) or one
/// per completed [`run_write_barrier`] call (the shared implementation behind both
/// [`WriteBatch::finish`] and [`BulkStoreSession::finish`]), incremented exactly once per barrier
/// no matter how many files/objects it covers. Gated by [`fsync_enabled`], like every other step
/// of the barrier it counts (see both increment sites): with fsync off there is no durability
/// wait to amortize, so counting a "barrier" there would count work that never happened. Not a
/// performance feature: a cheap, always-on observability hook — see [`barrier_count`] — that lets
/// the batching work's tests (`crates/forklift/tests/crash_consistency.rs`'s
/// `load_pays_a_constant_number_of_barriers_regardless_of_changed_file_count`, and a maintainer
/// running `FORKLIFT_DEBUG_BARRIER_COUNT=1`) prove a burst of N writes actually collapsed to a
/// constant number of barriers, not just that the resulting state happens to be correct.
static BARRIER_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current value of the process-wide durability-barrier counter — see [`BARRIER_COUNT`].
pub fn barrier_count() -> u64 {
    BARRIER_COUNT.load(std::sync::atomic::Ordering::Relaxed)
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

        #[cfg(test)]
        if let Some(injected) = SYNC_DIR_FAULT.with(|f| {
            let mut f = f.borrow_mut();
            f.attempted.push(dir.to_path_buf());
            f.fail_needle.as_deref()
                .filter(|needle| dir.to_string_lossy().contains(needle))
                .map(|_| format!("injected directory-sync failure for \"{}\"", dir.to_string_lossy()))
        }) {
            return Err(injected);
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

#[cfg(test)]
thread_local! {
    /// Test-only fault injection for [`sync_dir`] itself (the single-directory case — the
    /// immediate write path's own sync and the pack write's own sync both call it directly,
    /// unlike the batched barrier's aggregated [`sync_touched_directories`], which has its own
    /// separate rig, [`DIR_SYNC_FAULT`]). Same shape as that rig: records every directory path
    /// it is asked to sync, in call order, and can be armed to fail for paths containing a given
    /// substring instead of touching the filesystem. `pub(crate)`-visible guard/accessor below so
    /// `pack_utils`'s tests (a different module, same crate, same `--cfg test` unit-test binary)
    /// can drive it too.
    static SYNC_DIR_FAULT: std::cell::RefCell<DirSyncFaultState> =
        const { std::cell::RefCell::new(DirSyncFaultState { attempted: Vec::new(), fail_needle: None }) };
}

/// RAII scope for [`SYNC_DIR_FAULT`]: both construction and `Drop` reset this thread's state, so
/// neither a stale guard from a previous test on a reused thread, nor this guard's own arming
/// once the test that created it is done, can bleed into another test. `pub(crate)` for the same
/// cross-module reason as [`SYNC_DIR_FAULT`] itself.
#[cfg(test)]
pub(crate) struct SyncDirFaultGuard;

#[cfg(test)]
impl SyncDirFaultGuard {
    /// Record every directory [`sync_dir`] is asked to sync; fail none of them.
    pub(crate) fn recording() -> Self {
        SYNC_DIR_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState { attempted: Vec::new(), fail_needle: None });
        SyncDirFaultGuard
    }

    /// Record every directory, and fail (with a distinctive error, no filesystem access) any
    /// whose path contains `needle`.
    pub(crate) fn failing(needle: &str) -> Self {
        SYNC_DIR_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState {
            attempted: Vec::new(),
            fail_needle: Some(needle.to_string()),
        });
        SyncDirFaultGuard
    }
}

#[cfg(test)]
impl Drop for SyncDirFaultGuard {
    fn drop(&mut self) {
        SYNC_DIR_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState { attempted: Vec::new(), fail_needle: None });
    }
}

/// The directories [`sync_dir`] has been asked to sync on this thread since the current
/// [`SyncDirFaultGuard`] was armed, in call order.
#[cfg(test)]
pub(crate) fn sync_dir_attempts() -> Vec<PathBuf> {
    SYNC_DIR_FAULT.with(|f| f.borrow().attempted.clone())
}

/// The single routine every post-rename directory-sync failure point in this store reports
/// through: given the already-attempted `Result` of syncing the directory (or directories) a
/// rename just landed in, and the exact FINAL object paths — never a temp, never a parent, the
/// schema [`taint_utils::record_taint`] expects — whose durability that sync was supposed to
/// prove:
///
/// - On failure: records a taint for `final_paths` and returns the sync error, with the taint
///   write's own failure (if any) appended, never substituted — the same append-never-substitute
///   discipline [`run_write_barrier`]'s rename-failure path already uses for its own combined
///   error (see [`taint_after_sync_failure`]).
/// - On success: the durable-taint re-check — refuses if a taint happens to already be standing
///   for the storage root that owns `final_paths`, so a caller only reports `Ok` once both the
///   sync and the re-check have passed (see [`taint_recheck`]). Every recorder that durably
///   records a reference off the back of this call (a pallet head, a parked-list entry) must
///   write that reference only after this returns `Ok`.
///
/// Both halves are no-ops beyond returning `sync_result` unchanged on failure, or always
/// `Ok(())` on success — unless [`taint_utils::activate`] has been called in this process: both
/// [`taint_utils::record_taint`] and [`taint_utils::read_taints`] (which the re-check goes
/// through) gate themselves on activation, so an unactivated process sees no behavior change
/// from this routine's existence at all.
fn sync_result_or_taint(sync_result: Result<(), String>, final_paths: &[&Path]) -> Result<(), String> {
    match sync_result {
        Ok(()) => taint_recheck(final_paths),
        Err(sync_error) => Err(taint_after_sync_failure(sync_error, final_paths)),
    }
}

/// Convenience form of [`sync_result_or_taint`] for the common single-directory case: syncs
/// `dir` and folds the result through the same taint-or-recheck decision. `pub(crate)` so the
/// pack write's own directory sync (`pack_utils`) can share it; the immediate write path below
/// uses it directly.
pub(crate) fn sync_dir_or_taint(dir: &Path, final_paths: &[&Path]) -> Result<(), String> {
    sync_result_or_taint(sync_dir(dir), final_paths)
}

/// Record a taint for `final_paths` after a directory sync just failed, folding the taint
/// write's own failure (if any) into the returned message — appended after the original sync
/// error, never in its place, so a caller always sees the failure that actually blocked its
/// write first. A no-op recording (the original error returns unchanged) unless
/// [`taint_utils::activate`] has been called in this process — see
/// [`taint_utils::record_taint`]'s own doc comment.
fn taint_after_sync_failure(sync_error: String, final_paths: &[&Path]) -> String {
    match taint_utils::record_taint(final_paths) {
        Ok(()) => sync_error,
        Err(taint_error) => format!(
            "{} (additionally, failed to record a durability taint for the affected object(s): {})",
            sync_error, taint_error
        ),
    }
}

/// Process-global count of currently-armed [`SelfTripExemptionGuard`]s. A *counter*, not a bool,
/// so nested arming (e.g. a future caller inside an already-armed scope) composes instead of one
/// `Drop` clearing a still-needed exemption early. Consulted only by [`taint_recheck`]'s early
/// check; see that function and the guard's own doc comment for what arming it does and does not
/// make safe.
static SELF_TRIP_EXEMPTION_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The no-self-trip exemption for heal's own refetch writes (extending `heal_utils`'s own
/// "no-self-trip rule", see that module's doc comment, to the second heal-owned write path that
/// was never brought under it when it was added — `forklift heal`'s remote refetch, which stores
/// through the ordinary [`write_file_atomically`]/[`sync_dir_or_taint`] path while the very taint
/// it is trying to resolve is still standing). While at least one guard is held, [`taint_recheck`]
/// returns `Ok(())` immediately instead of reading the durable taint files — see that function.
///
/// Construct with [`SelfTripExemptionGuard::new`] and hold it for the guard's entire lifetime;
/// `Drop` decrements the count automatically.
///
/// **Must be process-global, not thread-local.** A refetch's stores do not all run on the thread
/// that armed this guard: `fetch_loose_objects`'s `JoinSet` tasks run on tokio worker threads, and
/// `import_bundle_bytes` runs on whatever thread the async runtime happens to poll it on. A
/// thread-local guard would not be visible to those threads, reproducing exactly the scheduling-
/// dependent bug this exemption exists to remove. This assumes the CLI's tokio runtime is
/// multithreaded, which is the default — a load-bearing assumption for anyone who later touches
/// the runtime configuration.
///
/// **Single-writer-per-process precondition.** The counter is a bare count with no notion of
/// *which* call armed it — deliberately, since that bareness is what lets it reach the worker
/// threads above. That same bareness means that while armed, **every** store in this process —
/// not just heal's — is exempted from the standing-taint refusal, because `taint_recheck` cannot
/// tell one caller's store from another's. This is sound only because heal is the sole writer in
/// the process for the guard's lifetime: no other operation in the same process may store objects
/// while a heal-owned guard is armed. The shipped CLI satisfies this by construction (one command
/// per process, so the armed window contains only heal's own stores).
///
/// Why the exemption stays sound under that precondition (see DESIGN.html §3.1.1 for the taint
/// mechanism this reasons about): the exemption is process-local and purely in-memory, while the
/// taint itself stays on disk for the whole armed window — a sibling process's own `taint_recheck`
/// call still reads the standing durable taint files exactly as before, so nothing about this
/// guard changes what a *different* process observes. And within this process, heal never records
/// a durable reference off the strength of an exempted store's `Ok`: heal's own recovery verdict
/// — which hashes are reported resolved and dropped from the taint's remainder — comes from a
/// separate, unconditional presence recheck run after the fetch attempt, never from the store
/// call's own return value, so the exemption changes which objects land on disk but not how
/// "recovered" is decided.
///
/// If a future long-lived process ever runs heal concurrently with another store-issuing
/// operation in the same process (for example a multi-tenant server holding one warehouse's heal
/// open while a different request stores into another), the precondition above stops holding by
/// construction, and this bare process-global counter would wrongly exempt that other operation's
/// stores too. Before any such process shape ships, this exemption must be re-scoped from
/// process-global to heal-invocation-scoped — e.g. a token threaded explicitly through the store
/// calls one heal invocation makes, or a task-local propagated into the tasks it spawns — rather
/// than a bare process-wide counter.
///
/// If `audit` (the other chokepoint-exempt verb) ever grows a repair-fetch of its own, it must
/// arm this same guard rather than invent a parallel exemption.
pub(crate) struct SelfTripExemptionGuard;

impl SelfTripExemptionGuard {
    /// Arm the exemption. Forward-looking tripwire, not present-tense enforcement: in the shipped
    /// CLI this guard has exactly one call site, reached once per heal invocation, so nested or
    /// concurrent arming cannot occur today and this assertion is dead code by construction. Its
    /// value is catching the moment a second arming site is introduced — precisely the change that
    /// would silently invalidate the single-writer-per-process precondition above. It catches only
    /// nested/concurrent *arming*; it cannot catch, and does not attempt to catch, a non-heal
    /// writer simply observing a nonzero counter from someone else's already-open guard (see this
    /// type's own doc comment).
    pub(crate) fn new() -> Self {
        let previous_count = SELF_TRIP_EXEMPTION_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        debug_assert!(previous_count == 0, "SelfTripExemptionGuard armed while already armed");
        SelfTripExemptionGuard
    }
}

impl Drop for SelfTripExemptionGuard {
    fn drop(&mut self) {
        SELF_TRIP_EXEMPTION_COUNT.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The durable-taint re-check on a directory sync's success path: refuse if a taint is already
/// standing for the storage root that owns `final_paths` — closing the window between a failed
/// sync elsewhere (this process's own earlier failure, or a sibling process's, since this reads
/// the durable taint files rather than only the in-memory gate [`taint_utils::gate_check`]
/// covers) and this call's own success. A recorder must not durably reference anything this call
/// covered until it has returned `Ok`.
///
/// Skipped early, before any taint file is read, while a [`SelfTripExemptionGuard`] is armed
/// anywhere in this process — see that type's doc comment for what makes this sound (and the
/// precondition it depends on).
///
/// Skipped silently — the same scope tolerance [`taint_utils::record_taint`] documents — when no
/// storage root resolves, or `final_paths` are not actually under one (the shape a bare-path
/// unit test's paths take, having never entered a storage-root scope at all): there is no
/// warehouse whose taint state this call could sensibly consult. A no-op — always `Ok(())` —
/// unless [`taint_utils::activate`] has been called in this process: [`taint_utils::read_taints`]
/// gates itself on activation, so this inherits that without a separate check, and in the
/// overwhelmingly common case (no taint directory at all) it costs one `stat`.
fn taint_recheck(final_paths: &[&Path]) -> Result<(), String> {
    if SELF_TRIP_EXEMPTION_COUNT.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        return Ok(());
    }

    let Some(root) = taint_utils::resolve_root_for(final_paths) else {
        return Ok(());
    };

    let state = taint_utils::read_taints(&root)?;
    if state.recorded.is_empty() && !state.torn {
        return Ok(());
    }

    Err(taint_utils::gate_standing_message(&root))
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
/// guarantee on their own). `pub(crate)` so [`heal_utils`](crate::util::heal_utils)'s restage
/// primitive shares the exact same uniqueness discipline for its own fresh temp name, rather
/// than reimplementing it — restaging never routes through `write_file_atomically` itself (see
/// that module's no-self-trip rule), but there is no reason its temp-naming should differ.
pub(crate) fn temp_path_for(file_path: &Path) -> Result<PathBuf, String> {
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
/// * `Ok(())`      - The file was written, renamed into place, and its directory entry's own
///                   durability was proven — including the taint re-check below, if
///                   `taint_utils::activate` has been called in this process (see
///                   `sync_dir_or_taint`); an unactivated process sees no change here.
/// * `Err(String)` - Either the write/rename itself failed (nothing is visible at `file_path`),
///                   or the rename succeeded but the following directory sync failed — `file_path`
///                   is then visible with its directory entry's durability unproven, and (once
///                   activated) that fact is recorded as a taint; or the directory sync itself
///                   succeeded but a taint was already standing for this root, in which case
///                   `file_path`'s directory entry was never re-attempted and the caller must not
///                   trust it as durable.
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
    // otherwise a power loss can undo it even though the file's bytes were already synced. A
    // sync failure here taints `file_path`; a sync success is re-checked against any taint
    // already standing for this root before this function may report `Ok` — see
    // `sync_dir_or_taint`.
    match file_path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => sync_dir_or_taint(parent, &[file_path])?,
        _ => (),
    };

    // Counted only once every step above — including the taint re-check — has actually
    // succeeded — see `BARRIER_COUNT`'s doc comment ("one per *completed* barrier"). A caller
    // that hit the `?` above already got the error; nothing durable was left half-finished for
    // this counter to over-report. Gated by `fsync_enabled` like the fsync/directory-sync steps
    // above: with fsync off, this rename paid no durability wait at all, so it is not the
    // barrier the counter exists to measure.
    if fsync_enabled() {
        BARRIER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    Ok(())
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
    /// reader, eventually reclaimed by `gc`'s ordinary reachability sweep (see below) the same
    /// way any crashed single-file write's stranded temp already is. During step 3, the
    /// durability barrier (steps 1-2) has already
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
    /// either durable-and-published or an invisible temp `gc` can reclaim once it ages past the
    /// grace period — see below). A *returned* error (disk full, a permission flip) is different:
    /// the process keeps running, so `run_barrier` cleans up every staged temp itself before
    /// propagating any error — see its doc comment — which is why `finished` is only set to
    /// `true` on success below: on the error path the registry slot and every temp are already
    /// gone, so `Drop`'s own abort pass (see below) just finds nothing left to do.
    ///
    /// A rename failure partway through step 3 gets one more thing done before it returns: every
    /// directory step 3 already touched gets a best-effort attempt at step 4's own work, run early
    /// over the prefix that exists so far, *before* the error propagates — see
    /// [`run_write_barrier`]. Without that, an entry renamed before the failure is visible but not
    /// durable; a later retry would see it via `does_object_exist` and skip restaging it, and a
    /// subsequent power loss could then drop its directory entry entirely — a durable reference to
    /// an object that no longer exists. This closes that exposure only when the early sync itself
    /// succeeds. It does not in two other cases: the early sync can itself fail (`run_write_barrier`
    /// then returns the combined error, naming both the rename and the sync failure), or every
    /// rename in step 3 can succeed and it is step 4's own trailing sync that fails instead. In
    /// both, some directories end up only *attempted*, not proven durable — not even necessarily
    /// the ones that come before the first per-directory failure: `fsync_dir_data`'s own success on
    /// macOS only queues the write to the drive, and the shared device-cache flush that actually
    /// confers durability runs afterward and has its own error suppressed once an earlier
    /// per-directory error is already recorded (see [`sync_touched_directories`]'s doc comment).
    /// Both of those two remaining cases now record a durability taint for exactly the final paths
    /// left in that unproven state, and a directory sync that *succeeds* is re-checked against any
    /// taint already standing for this root before this function may return `Ok` — see
    /// `sync_result_or_taint` — though both halves stay dormant (no taint file, no re-check, no
    /// behavior change at all) unless `taint_utils::activate` has been called in this process.
    ///

    /// Unlike the *pack* folder, the loose store has no sweeper *dedicated* to `.tmp` names
    /// (`pack_utils` has one, for its own `.compact-*.tmp` staging files — see
    /// `remove_stale_temp_files`) — but a loose-store temp left behind by a hard kill (rather
    /// than a returned error, which is handled above) is not actually unreclaimed: `gc_utils`'s
    /// ordinary reachability sweep (`collect_garbage`) does not pattern-match on `.tmp` at all,
    /// it simply treats *any* non-`.sig`, unreferenced file sitting in an object fan-out folder
    /// as garbage once it ages past the mtime grace period — a stranded temp qualifies with no
    /// special-casing needed (see `gc_utils`'s own
    /// `gc_sweeps_a_stranded_write_batch_temp_past_the_grace_period` test). This is the same
    /// reclaiming path every other crashed single-file write already relied on before batching
    /// existed — batching only widens *how much* can be
    /// staged in one still-unfinished barrier, not *whether* an abandoned temp is ever reclaimed.
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
            discard_staged_temps(&pending);
            return Err(error);
        }

        Ok(())
    }
}

/// The four-step durability barrier shared by [`BulkStoreSession`] and [`WriteBatch`] — see
/// [`BulkStoreSession::finish`]'s doc comment for the exact steps and the crash analysis. Kept
/// as a free function (not tied to either type) so both share exactly one implementation of the
/// crash-safety-critical ordering instead of risking it drifting between two copies — including
/// the rename-failure fix below, which now applies to both callers identically.
fn run_write_barrier(pending: &[(PathBuf, PathBuf)]) -> Result<(), String> {
    if fsync_enabled() {
        for (temp, _) in pending {
            fsync_data(temp)?;
        }

        #[cfg(target_os = "macos")]
        macos_flush_device_cache(&pending[0].0)?;
    }

    let mut touched_parents: BTreeSet<PathBuf> = BTreeSet::new();
    let mut first_renamed: Option<&Path> = None;
    // Every final path successfully renamed so far, in rename order — the visible prefix a
    // rename failure below must taint (never the whole batch: entries after the failure never
    // became visible at all).
    let mut renamed_finals: Vec<&Path> = Vec::new();
    for (temp, final_path) in pending {
        if let Err(e) = std::fs::rename(temp, final_path) {
            let rename_error = format!(
                "Error while moving file into place at \"{}\": {}", final_path.to_string_lossy(), e
            );

            // Everything renamed above is already visible but, without this, its directory
            // entries would not yet be durable — a later retry sees these names via
            // `does_object_exist` and skips restaging them, so a power loss between here and the
            // next successful barrier could drop the dentry for an object no code path will ever
            // write again. `sync_touched_directories` attempts every directory in
            // `touched_parents` regardless of an earlier one's failure (see its own doc comment),
            // so this reaches the whole touched prefix, not just as much of it as succeeds before
            // the first per-directory failure. Skipped when nothing renamed yet (`touched_parents`
            // empty — the first rename itself is what failed, so there is nothing to sync and no
            // already-existing file to borrow for the macOS device flush).
            if fsync_enabled() && !touched_parents.is_empty() {
                if let Some(flush_via) = first_renamed {
                    if let Err(sync_error) = sync_touched_directories(&touched_parents, flush_via) {
                        // Taints exactly `renamed_finals` — the visible prefix — never the
                        // entries after the failure, which never became visible. Appends (never
                        // substitutes) the taint write's own failure, if any — see
                        // `taint_after_sync_failure`.
                        let sync_error = taint_after_sync_failure(sync_error, &renamed_finals);
                        return Err(format!(
                            "{} (additionally, failed to sync the directories of entries already \
                            renamed before this failure: {})", rename_error, sync_error
                        ));
                    }
                }
            }

            return Err(rename_error);
        }

        if first_renamed.is_none() {
            first_renamed = Some(final_path.as_path());
        }
        renamed_finals.push(final_path.as_path());
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
        // directory half of the flush share `macos_flush_device_cache` unchanged. Unlike the
        // early sync above, a failure here has nothing earlier to fall back to reporting — every
        // rename in this batch already succeeded, so this is the batch's only remaining step and
        // its error propagates directly. Like the early sync, every directory in
        // `touched_parents` is attempted here regardless of an earlier one's failure (see
        // `sync_touched_directories`'s own doc comment) — a caller catching this error still had
        // every reachable directory *attempted*, not just a prefix (only the first failure is
        // reported, so a later directory may also have failed its own fsync).
        //
        // Every rename above ran, so on failure this taints the batch's full set of final paths
        // (`renamed_finals` now covers every entry in `pending`); on success it re-checks for a
        // standing taint before this function may return `Ok` — see `sync_result_or_taint`.
        sync_result_or_taint(sync_touched_directories(&touched_parents, &pending[0].1), &renamed_finals)?;
    }

    // One barrier no matter how many files `pending` covers — see `BARRIER_COUNT`'s doc comment.
    // Counted only here, after every step above — including the taint re-check just above — has
    // actually succeeded (every failure path above already returned past this point): both
    // callers only ever reach this function with a non-empty `pending` (each checks first), so
    // this never double-counts an empty `finish()` that had nothing staged, and never counts a
    // barrier that failed partway. Gated by `fsync_enabled`, exactly like steps 1-2 and 4 above:
    // with fsync off, the rename loop (step 3) is the only work that actually ran, and it paid no
    // durability wait to amortize.
    if fsync_enabled() {
        BARRIER_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
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
/// Best-effort across the whole set: every directory in `touched_parents` is attempted even after
/// an earlier one fails — a caller catching the returned error still needs as much of the batch
/// made durable as the filesystem will allow, not just a prefix ending at the first failure. Only
/// the *first* failure is returned (a `Result<(), String>` has room for exactly one); the macOS
/// device flush is still attempted once at least one directory was, but its own error only
/// surfaces when no per-directory fsync already failed.
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
    let mut first_error: Option<String> = None;
    for parent in touched_parents {
        if let Err(e) = fsync_dir_data(parent) {
            first_error.get_or_insert(e);
        }
    }

    #[cfg(target_os = "macos")]
    if !touched_parents.is_empty() {
        if let Err(e) = macos_flush_device_cache(flush_via) {
            first_error.get_or_insert(e);
        }
    }

    match first_error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(not(unix))]
fn sync_touched_directories(_touched_parents: &BTreeSet<PathBuf>, _flush_via: &Path) -> Result<(), String> {
    Ok(())
}

/// Process-wide count of successful per-directory `fsync()` calls [`fsync_dir_data`] actually made
/// — the directory-sync counterpart of [`BARRIER_COUNT`], incremented once per directory whose
/// entry changes were handed to the drive's queue (not once per [`sync_touched_directories`] call,
/// which may sync several directories in one barrier). That is a weaker claim than "durable": on
/// macOS `fsync_dir_data`'s own `fsync` only queues the write, and the device-cache flush that
/// actually confers durability ([`macos_flush_device_cache`], shared across the whole batch) runs
/// afterward and separately — a flush failure does not un-increment this counter. Declared
/// unconditionally so [`dir_sync_count`] needs no `cfg` at any call site; on non-Unix targets
/// `fsync_dir_data` does not exist (there is no directory handle to fsync there — see
/// [`sync_dir`]'s doc comment for the same point), so the counter simply never advances on those
/// targets and the accessor always reads back whatever it started at.
///
/// Not a performance feature: a cheap, always-on observability hook that lets a test prove a
/// barrier actually reached the filesystem for a given directory, rather than only checking the
/// resulting file state — which plain, unsynced `rename` would produce identically.
static DIR_SYNC_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The current value of the process-wide directory-fsync counter — see [`DIR_SYNC_COUNT`].
pub fn dir_sync_count() -> u64 {
    DIR_SYNC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection for [`fsync_dir_data`]: records every directory path it is
    /// asked to sync, in call order, and can be armed to fail for paths containing a given
    /// substring instead of touching the filesystem. Thread-local because unit tests run
    /// concurrently on separate threads while a given `WriteBatch::finish`/`run_write_barrier`
    /// call always runs entirely on its caller's thread — one test's arming can never be observed
    /// by a concurrently running test on another thread. `DirSyncFaultGuard`'s construct-and-Drop
    /// reset (see its own doc comment) is what additionally isolates tests from each other when
    /// they instead run sequentially (`--test-threads=1`) and reuse the same thread.
    static DIR_SYNC_FAULT: std::cell::RefCell<DirSyncFaultState> =
        const { std::cell::RefCell::new(DirSyncFaultState { attempted: Vec::new(), fail_needle: None }) };
}

#[cfg(test)]
struct DirSyncFaultState {
    attempted: Vec<PathBuf>,
    fail_needle: Option<String>,
}

/// RAII scope for [`DIR_SYNC_FAULT`]: both construction and `Drop` reset this thread's state, so
/// neither a stale guard from a previous test on a reused thread, nor this guard's own arming
/// once the test that created it is done, can bleed into another test.
#[cfg(test)]
pub(crate) struct DirSyncFaultGuard;

#[cfg(test)]
impl DirSyncFaultGuard {
    /// Record every directory `fsync_dir_data` is asked to sync; fail none of them.
    pub(crate) fn recording() -> Self {
        DIR_SYNC_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState { attempted: Vec::new(), fail_needle: None });
        DirSyncFaultGuard
    }

    /// Record every directory, and fail (with a distinctive error, no filesystem access) any
    /// whose path contains `needle`.
    fn failing(needle: &str) -> Self {
        DIR_SYNC_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState {
            attempted: Vec::new(),
            fail_needle: Some(needle.to_string()),
        });
        DirSyncFaultGuard
    }
}

#[cfg(test)]
impl Drop for DirSyncFaultGuard {
    fn drop(&mut self) {
        DIR_SYNC_FAULT.with(|f| *f.borrow_mut() = DirSyncFaultState { attempted: Vec::new(), fail_needle: None });
    }
}

/// The directories [`fsync_dir_data`] has been asked to sync on this thread since the current
/// [`DirSyncFaultGuard`] was armed, in call order.
#[cfg(test)]
pub(crate) fn dir_sync_attempts() -> Vec<PathBuf> {
    DIR_SYNC_FAULT.with(|f| f.borrow().attempted.clone())
}

/// Fsync one directory's pending entry changes (e.g. a rename into it) to the drive — the
/// directory counterpart of [`fsync_data`], used by [`sync_touched_directories`]. Opens
/// read-only, exactly like [`sync_dir`]'s own open: a directory cannot be opened write-mode on
/// this platform (`EISDIR`), and POSIX `fsync` works on a read-only descriptor regardless (see
/// [`open_for_sync`]'s doc comment for the same point about files, where a write-mode open is
/// chosen only for Windows' sake — moot here, since this function is Unix-only).
///
/// `pub(crate)` so [`heal_utils`](crate::util::heal_utils)'s entry-heal can fsync a batch of
/// restaged objects' distinct parent directories directly — the same raw primitive
/// [`sync_touched_directories`] uses, never the taint-aware [`sync_dir_or_taint`] wrapper (whose
/// own success re-check would refuse while the very taint the heal is resolving still stands).
#[cfg(unix)]
pub(crate) fn fsync_dir_data(dir: &Path) -> Result<(), String> {
    use std::os::unix::io::AsRawFd;

    #[cfg(test)]
    if let Some(injected) = DIR_SYNC_FAULT.with(|f| {
        let mut f = f.borrow_mut();
        f.attempted.push(dir.to_path_buf());
        f.fail_needle.as_deref()
            .filter(|needle| dir.to_string_lossy().contains(needle))
            .map(|_| format!("injected directory-sync failure for \"{}\"", dir.to_string_lossy()))
    }) {
        return Err(injected);
    }

    let file = std::fs::File::open(dir)
        .map_err(|e| format!("Error while opening directory \"{}\" to sync it: {}", dir.to_string_lossy(), e))?;

    if unsafe { libc::fsync(file.as_raw_fd()) } != 0 {
        return Err(format!(
            "Error while syncing directory \"{}\": {}", dir.to_string_lossy(), std::io::Error::last_os_error()
        ));
    }

    DIR_SYNC_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Best-effort removes every staged temp still referenced by `pending` — the cleanup a failed or
/// aborted batch needs so nothing it staged survives to be discovered later. Shared by
/// [`BulkStoreSession`]'s and [`WriteBatch`]'s failure and abort paths so the invariant ("a batch
/// that will never publish leaves no temp behind") has exactly one implementation instead of
/// several copies that could drift.
///
/// Ignores per-file errors: an entry already consumed by its own rename (see
/// [`run_write_barrier`]) simply fails removal with `NotFound`, which is not a problem here —
/// that entry is already renamed into place (visible; durability is the barrier's concern, not
/// this function's), not leaked.
fn discard_staged_temps(pending: &[(PathBuf, PathBuf)]) {
    for (temp, _) in pending {
        let _ = std::fs::remove_file(temp);
    }
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
/// It exists for `stack`'s and `park`'s tree build, whose object writes run from `TaskExecutor`'s
/// parallel workers (see `model::task::tree_builder::tree_builder_context::TreeBuilderContext`).
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
/// batching does not make the batch's renames atomic as a set, it only amortizes the fsyncs.
/// A crash mid-`finish` can leave some entries published and others not (exactly like
/// `BulkStoreSession` — see its doc comment), and a `finish` that *returns* an error has a
/// similar shape, though what is durable afterward depends on exactly which step failed — see
/// `finish`'s `# Returns` for the breakdown. In every case no record is kept of which entries
/// these are, so the caller must treat the whole batch as unpublished regardless.
/// `stack_utils::stack_parcel` relies on the `Ok`-gating: it never
/// batches the pallet-head ref write itself, and only calls `set_pallet_head` after the batch
/// covering every object the parcel references has already finished successfully.
pub struct WriteBatch {
    state: std::sync::Mutex<WriteBatchState>,
}

/// The mutable state behind one lock, so [`WriteBatch::reserve_final_path`] and every `stage`
/// call see (and update) the reservation set and the pending list atomically together — see
/// `reserve_final_path`'s doc comment for why that atomicity is what makes it a real dedupe
/// guard, not just a racy best-effort hint.
#[derive(Default)]
struct WriteBatchState {
    pending: Vec<(PathBuf, PathBuf)>,

    /// Every final path staged (or reserved) into this batch so far — see
    /// [`WriteBatch::reserve_final_path`].
    final_paths: std::collections::HashSet<PathBuf>,

    /// Final paths that won their [`WriteBatch::reserve_final_path`] call but have not (yet, or
    /// ever) been staged — exactly the leak set [`WriteBatch::finish`] refuses to publish
    /// through. A winning reservation inserts here; `stage`/`stage_with_mtime` removes here (a
    /// no-op if the path was never reserved — i.e. staged directly, without going through
    /// `reserve_final_path` first). Maintained incrementally rather than recomputed from
    /// `final_paths` and `pending` on every `finish` call, so a successful finish — the common
    /// case — costs nothing proportional to the batch's size to check.
    reserved_not_staged: std::collections::HashSet<PathBuf>,
}

impl WriteBatch {
    /// Create a new, empty write batch.
    pub fn new() -> Self {
        Self { state: std::sync::Mutex::new(WriteBatchState::default()) }
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
    /// If the write itself fails after the temp file was created, the temp is best-effort removed
    /// before the error returns: this path never reaches `pending`, so neither a later `finish`'s
    /// [`discard_staged_temps`] nor `Drop`'s would ever see it to clean it up — and, unlike a
    /// stranded temp inside an object fan-out folder (which `gc`'s reachability sweep eventually
    /// reclaims regardless, see [`BulkStoreSession::finish`]'s doc comment), a temp staged into
    /// some other directory (an inventory folder, via [`stage_with_mtime`](Self::stage_with_mtime))
    /// has no *dedicated* sweeper of its own — it survives until whatever incidentally clears that
    /// directory wholesale (e.g. `inventory_utils::replace_subtree_inventories`'s `remove_dir_all`
    /// of the whole staging folder), not on any schedule this crate guarantees.
    ///
    /// # Returns
    /// * `Ok(())`      - If the write was staged.
    /// * `Err(String)` - If the temp file could not be created or written.
    pub fn stage(&self, file_path: &Path, content: &[u8]) -> Result<(), String> {
        let temp_path = temp_path_for(file_path)?;
        if let Err(e) = create_and_write_file(&temp_path, content) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }

        let mut state = self.state.lock().expect("write batch lock poisoned");
        state.final_paths.insert(file_path.to_path_buf());
        state.reserved_not_staged.remove(file_path);
        state.pending.push((temp_path, file_path.to_path_buf()));

        Ok(())
    }

    /// Like [`stage`](Self::stage), but sets the staged temp file's modification time to
    /// `mtime` — through the same write handle the content was just written with, never a
    /// reopen-by-path (the project's Windows fsync convention applies here too: a fresh
    /// `File::open` on Windows can fail to observe the write that just happened without an
    /// intervening flush) — before it is queued for the barrier.
    ///
    /// This exists for a rewrite that must be invisible, timing-wise, to a later mtime-based
    /// staleness check (`inventory_utils`'s rollup-clear maintenance: see
    /// `stage_rollup_clear`'s doc comment). Setting the mtime on the *temp* file, before the
    /// barrier's fsync and rename, means the final path never has an observable window where its
    /// mtime is wrong — `rename` does not itself change a file's mtime, so whatever the temp
    /// file's mtime was at rename time is exactly what the final path carries. A caller that
    /// instead set the mtime on the final path *after* `finish()` would leave a real crash
    /// window: a crash between the rename becoming durable and the mtime fix-up would durably
    /// publish the wrong (advanced) mtime.
    ///
    /// A failure after the temp file was created — from the write itself or from `set_modified` —
    /// best-effort removes the temp before the error returns, for the same reason [`stage`](Self::stage)
    /// does (see its doc comment): this path never reaches `pending`, so neither `finish` nor `Drop`
    /// will ever see it, and no dedicated sweeper exists for it either.
    ///
    /// # Returns
    /// * `Ok(())`      - The write was staged, with its temp file's mtime already set.
    /// * `Err(String)` - If the temp file could not be created, written, or have its
    ///                   modification time set.
    pub fn stage_with_mtime(&self,
                            file_path: &Path,
                            content: &[u8],
                            mtime: std::time::SystemTime) -> Result<(), String> {
        let temp_path = temp_path_for(file_path)?;
        let file = match create_and_write_file(&temp_path, content) {
            Ok(file) => file,
            Err(e) => {
                let _ = std::fs::remove_file(&temp_path);
                return Err(e);
            }
        };

        if let Err(e) = file.set_modified(mtime) {
            let _ = std::fs::remove_file(&temp_path);
            return Err(format!(
                "Error while setting the modification time of \"{}\": {}", temp_path.to_string_lossy(), e
            ));
        }

        let mut state = self.state.lock().expect("write batch lock poisoned");
        state.final_paths.insert(file_path.to_path_buf());
        state.reserved_not_staged.remove(file_path);
        state.pending.push((temp_path, file_path.to_path_buf()));

        Ok(())
    }

    /// Atomically reserve `final_path` for staging in this batch: `true` exactly once for a
    /// given path (the caller that gets it "wins" and must go on to actually stage it, e.g. via
    /// [`write_object_to_file_deferred`]), `false` for every other call — concurrent or later —
    /// with the same path, which must *not* stage it again.
    ///
    /// This is the dedupe [`crate::model::object::loose_object::LooseObject::store_deferred`]
    /// needs: [`does_object_exist`] alone only sees
    /// packs and already-*renamed* final paths, never a write staged earlier in the very same
    /// batch (its final name does not exist yet — that is the whole point of batching). Without
    /// this, every repeated occurrence of the same content hash in one batched walk would
    /// independently decide "not on disk yet" and stage its own full compressed temp — for
    /// heavily duplicated content (many copies of the same vendored asset, say) that is a
    /// multiplier on both wall time and peak disk usage that dedupe-by-existence is supposed to
    /// prevent entirely.
    ///
    /// Checked and inserted under one lock acquisition (the same lock `stage`/`stage_with_mtime`
    /// use) so two threads reserving the same path at the same instant can never both "win" —
    /// unlike a separate check-then-stage, which would leave a real (if narrow) race window.
    /// Reserving a path does not itself stage anything; the winning caller is still responsible
    /// for calling `stage`/`stage_with_mtime` afterward (which records the same path into
    /// `final_paths` again — a harmless re-insert — and clears it from the leak set below).
    ///
    /// A win also adds `final_path` to `reserved_not_staged` — the set
    /// [`finish`](Self::finish) checks to catch a winner that never followed through.
    pub fn reserve_final_path(&self, final_path: &Path) -> bool {
        let mut state = self.state.lock().expect("write batch lock poisoned");
        let won = state.final_paths.insert(final_path.to_path_buf());
        if won {
            state.reserved_not_staged.insert(final_path.to_path_buf());
        }
        won
    }

    /// The barrier: fsync every staged write's bytes, then — only once every byte is durable —
    /// rename each into place and fsync the directories that changed. See
    /// [`BulkStoreSession::finish`]'s doc comment for the exact four steps and the full crash
    /// analysis (shared implementation, [`run_write_barrier`]): every conclusion there — no
    /// torn object survives any crash interleaving, an in-flight error leaves no stranded temp —
    /// applies here unchanged.
    ///
    /// Clears the reservation set together with the pending list (both live behind the same
    /// lock) — a batch is drained here either way, so nothing about a prior round should still
    /// influence [`reserve_final_path`] if this instance is ever reused afterward. Nothing is
    /// lost by that: once a write is durable (this call returned `Ok`), [`does_object_exist`]
    /// sees it directly, and a caller reusing a `WriteBatch` for a later round already treats
    /// each `finish` as its own barrier.
    ///
    /// Must only be called once every producer that may
    /// [`reserve_final_path`](Self::reserve_final_path) or [`stage`](Self::stage) into this batch
    /// for this round has actually finished running — the leak check below cannot distinguish a
    /// producer that is merely between winning a reservation and completing its stage from one
    /// that failed outright, so calling `finish` while producers are still in flight can misread
    /// a live, in-progress write as a leak and discard an otherwise-healthy batch. A caller that
    /// fans producers out across tasks or threads must actually wait for every one of them to
    /// complete before calling `finish` — an abort that only signals cancellation, without
    /// waiting for the signal to be observed and every producer to stop, does not satisfy this:
    /// a producer still running between a won reservation and its stage is indistinguishable from
    /// one that failed outright.
    ///
    /// Refuses to run the barrier at all if some path was
    /// [`reserve_final_path`](Self::reserve_final_path)'d but never actually staged: the winner
    /// of a reservation is trusted to go on and stage it, but a fallible step between winning
    /// and staging (in [`crate::model::object::loose_object::LooseObject::store_deferred`]:
    /// `compress()`, or the write itself) can fail after the reservation is already recorded.
    /// Every other caller for that same path already saw `reserve_final_path` return `false`
    /// and, reading that as "someone else is staging this," staged nothing of its own — so
    /// without this check `finish` would happily publish a batch whose `pending` is silently
    /// missing an entry the reservation set promised existed, and a caller downstream (a shard
    /// naming that path's hash) would end up referencing a blob that was never written.
    /// Deliberately *not* fixed by releasing the reservation on the failing caller's error path
    /// instead: a concurrent second occurrence can lose the race (and commit to "already staged,
    /// nothing to do") *before* the first occurrence's failure ever releases anything, so
    /// releasing alone cannot undo a decision another caller already made. Checking here, once,
    /// under the precondition above, catches the mismatch regardless of timing — without
    /// weakening `reserve_final_path`'s one-winner guarantee or reintroducing the redundant
    /// compress-and-stage race it exists to prevent.
    ///
    /// The check itself is `reserved_not_staged`, maintained incrementally rather than
    /// recomputed here: every winning `reserve_final_path` call adds to it, every
    /// `stage`/`stage_with_mtime` call removes from it, so by the time `finish` runs it already
    /// holds exactly the promises nothing fulfilled — an O(1) empty check on the common,
    /// successful path, not a diff over the whole batch recomputed on every call.
    ///
    /// # Returns
    /// * `Ok(())`      - Every staged write is durable and visible at its final path, and (once
    ///                   [`taint_utils::activate`] has been called in this process) no durability
    ///                   taint was found standing for this batch's storage root on the re-check
    ///                   that runs right before this returns — see [`sync_result_or_taint`]. An
    ///                   unactivated process skips that re-check entirely.
    /// * `Err(String)` - A reserved path was never staged, or a temp-fsync, rename, or
    ///                   directory-sync step of the barrier failed. Either way the batch is dead:
    ///                   every remaining staged temp was best-effort removed before returning, so
    ///                   nothing staged here survives to be published later — but what is already
    ///                   on disk depends on which step failed. A leaked reservation is caught
    ///                   before the barrier ever runs, so nothing in this batch was renamed. A
    ///                   temp-fsync failure means the same: no rename has happened yet, so no
    ///                   final path is visible. A rename failure means every entry renamed before
    ///                   it is visible, and — when [`fsync_enabled`] (skipped, like every other
    ///                   sync step, when it is off) — its directory got a best-effort attempt at
    ///                   being fsynced before this error returned; an attempt, not a proof, since
    ///                   that attempt can itself also fail (see [`run_write_barrier`]). A failure
    ///                   in the barrier's own trailing directory sync (every rename already
    ///                   succeeded) means every entry is visible, but directory durability is only
    ///                   best-effort attempted, not proven. In every case there is no per-path
    ///                   record of which case applies — the caller must
    ///                   treat the whole batch as unpublished. A failed `finish` can also leave
    ///                   names visible whose directory entries are not proven durable (the
    ///                   trailing-sync case above, and the rename-failure case's own visible
    ///                   prefix); a retry that dedupes via [`does_object_exist`] will see such a
    ///                   name and treat it as already stored. Callers whose retry correctness
    ///                   depends on visible-implies-durable must not rely on that after a failed
    ///                   `finish` unless [`taint_utils::activate`] has been called: activated, both
    ///                   of those directory-sync failures record a durability taint for exactly the
    ///                   affected final paths (see [`taint_after_sync_failure`]), which
    ///                   [`does_object_exist`]'s own gate then refuses to answer past rather than
    ///                   silently trusting. In an activated process a later
    ///                   [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted)
    ///                   call (run automatically at the CLI's storage-scope entry) restages exactly
    ///                   those paths and clears the taint once every one of them is proven durable
    ///                   again.
    pub fn finish(&self) -> Result<(), String> {
        let (pending, leaked) = {
            let mut state = self.state.lock().expect("write batch lock poisoned");
            let pending = std::mem::take(&mut state.pending);
            let leaked = std::mem::take(&mut state.reserved_not_staged);
            state.final_paths.clear();
            (pending, leaked)
        };

        if !leaked.is_empty() {
            discard_staged_temps(&pending);
            // Sorted so the message is deterministic — `reserved_not_staged` itself iterates in
            // hash order.
            let mut leaked: Vec<PathBuf> = leaked.into_iter().collect();
            leaked.sort();
            let message = format!(
                "Error while finishing a write batch: {} final path(s) were reserved but never \
                staged — a fallible step between reservation and staging must have failed (any \
                concurrent or later caller for the same path would have read the reservation as \
                already handled and staged nothing of its own either); refusing to publish this \
                batch: {}",
                leaked.len(),
                leaked.iter().map(|p| p.to_string_lossy()).collect::<Vec<_>>().join(", "),
            );
            return Err(message);
        }

        if pending.is_empty() {
            return Ok(());
        }

        if let Err(error) = run_write_barrier(&pending) {
            discard_staged_temps(&pending);
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
    /// (see their doc comments), so this only ever finds work to do when something staged was
    /// never followed by a `finish` call that saw it: either `finish` was never called at all (an
    /// early `?` return elsewhere), or the batch was reused for a later round (see the type-level
    /// doc comment) whose writes were staged after the last `finish` and never got one of their
    /// own before the drop. Best-effort removes those temps so that case leaks nothing either —
    /// the durability invariant holds trivially, since nothing staged here was ever published.
    fn drop(&mut self) {
        if let Ok(mut state) = self.state.lock() {
            discard_staged_temps(&std::mem::take(&mut state.pending));
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
                discard_staged_temps(&pending);
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
///
/// `pub(crate)` so [`heal_utils`](crate::util::heal_utils)'s entry-heal can share this exact
/// primitive for its own post-restage flush (via a taint file, or a small anchor file for a
/// directory on a different volume — see that module's doc comment) instead of duplicating the
/// raw `fcntl` call.
#[cfg(target_os = "macos")]
pub(crate) fn macos_flush_device_cache(path: &Path) -> Result<(), String> {
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

/// The classified outcome of [`read_object_classified`] — the one shape every taint recheck (see
/// `recovery_utils`/`heal_utils`) is built from, so a recheck can never disagree with what an
/// ordinary read of the same hash would actually do.
pub(crate) enum StoreReadOutcome {
    /// The ordinary read succeeds; the bytes hash to the address.
    Verified(Vec<u8>),
    /// Neither packs nor loose hold it, even after the read path's own reload-on-miss retry.
    Absent,
    /// Something is at the address, but the ordinary read fails on it — this carries the exact
    /// error the read path would surface for it. Decisive, not an unknown: a subsequent ordinary
    /// read of this hash *will* fail the same way.
    Unverifiable(String),
}

/// The classifying read core (DESIGN.html §3.1.1): the single predicate [`read_object_uncached`]
/// and every durability-taint recheck are both expressed through, so a recheck can never drift
/// from what an ordinary read of the same hash would actually do — see [`read_object_uncached`],
/// which is written as a plain match over this function's result rather than a parallel
/// implementation. A prior recheck predicate that merely *resembled* the read path (rather than
/// being it) was found, more than once, to disagree with it on an edge case — this function is
/// the structural fix: there is only one read implementation now.
///
/// Mirrors [`read_object_uncached`]'s own shape branch for branch (packs first, then loose, with
/// the same reload-on-miss retry) — see that function's doc comment for why packs are consulted
/// first.
///
/// **There is no pack→loose fall-through.** Once a hash is confirmed a pack member
/// ([`pack_utils::is_in_packs`]), a failure to *resolve* that record
/// ([`pack_utils::retrieve_from_packs`] returning `Err`) is [`StoreReadOutcome::Unverifiable`],
/// never a reason to go check the loose store instead. This looks like it could be an omission —
/// "the pack copy is bad, isn't the loose copy worth a try?" — but it is deliberate and has
/// already been "fixed" the wrong way once: an object that lives in a pack has no independent
/// loose copy in the common (compacted) case, so falling through would either find nothing (a
/// wasted stat) or, worse, find a stale/unrelated loose file at that path and clear a taint on an
/// object the ordinary read path can never actually serve — a false clear, strictly worse than
/// leaving the taint standing. [`read_object_uncached`] has never had this fall-through either
/// (see its own doc comment); this function must not grow one on its behalf.
///
/// # Returns
/// * `Ok(StoreReadOutcome)` - See [`StoreReadOutcome`].
/// * `Err(String)`          - The store itself could not be consulted at all (the pack registry
///                            failed to load, or the loose read hit an I/O error that is neither
///                            `NotFound` nor content-shaped) — an environment failure, unknown
///                            rather than decisive: it says nothing about whether a subsequent
///                            ordinary read would succeed or fail.
pub(crate) fn read_object_classified(hash: &str) -> Result<StoreReadOutcome, String> {
    // Packs first (mirrors `read_object_uncached`): a resident-index membership check is
    // syscall-free, so it is the environment channel (`?`) here — its only `Err` is a registry
    // load failure, never anything about this particular hash.
    if crate::util::pack_utils::is_in_packs(hash)? {
        match crate::util::pack_utils::retrieve_from_packs(hash) {
            Ok(Some(bytes)) => return Ok(StoreReadOutcome::Verified(bytes)),
            // Membership was confirmed a moment ago but the resolve now comes back empty — only
            // reachable via a registry reload racing between the two calls (`compact` invalidating
            // the cache mid-check). Not the pack→loose fall-through the doc comment above
            // forbids: nothing here failed to resolve a *located* record, so falling into the
            // ordinary loose branch below is the same "try the other place, once" reload-on-miss
            // discipline `read_object_uncached` already applies on a loose miss.
            Ok(None) => {}
            // A located record that fails to resolve (bad decompression, an unreconstructable
            // delta chain, a hash mismatch) is decisive, not an unknown — a subsequent ordinary
            // read of this hash will fail the exact same way, since it is the exact same call.
            Err(e) => return Ok(StoreReadOutcome::Unverifiable(e)),
        }
    }

    // Loose fallback: a freshly written object not yet swept into a pack, or (the `Ok(None)`
    // race above) a pack membership check whose confirmed-a-moment-ago record already moved on.
    let (path, file_name) = get_path_for_object(hash)?;
    let file_path = path.add(PATH_SEPARATOR).add(&file_name);

    let compressed = match std::fs::read(&file_path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            // Neither the cached packs nor a loose file holds it. In a long-running process (a
            // live server) the pack registry can predate an external `compact` that moved this
            // object into a new pack and swept its loose source — so reload the registry once and
            // retry the packs before concluding the object is gone (reload-on-miss).
            return Ok(match crate::util::pack_utils::retrieve_from_packs_reloading(hash) {
                Ok(Some(bytes)) => StoreReadOutcome::Verified(bytes),
                Ok(None) => StoreReadOutcome::Absent,
                Err(e) => StoreReadOutcome::Unverifiable(e),
            });
        }
        Err(error) => return Err(format!("Error while reading object from file \"{}\": {}", file_path, error)),
    };

    let bytes = match zstd::stream::decode_all(compressed.as_slice()) {
        Ok(bytes) => bytes,
        Err(e) => return Ok(StoreReadOutcome::Unverifiable(format!("Error while decompressing object: {}", e))),
    };

    // Same content-addressing guarantee the pack path enforces: a corrupt loose file fails the
    // read rather than silently returning wrong bytes.
    Ok(match crate::util::object_utils::verify_object_bytes(hash, &bytes) {
        Ok(()) => StoreReadOutcome::Verified(bytes),
        Err(e) => StoreReadOutcome::Unverifiable(e),
    })
}

/// Read an object's decompressed bytes straight from the store, without consulting the read
/// cache. The uncached body of [`retrieve_object_by_hash`].
///
/// Packs are consulted *first*: locating an object in a pack is a syscall-free binary search of
/// the resident index, so once a warehouse is compacted (the common case at scale — most objects
/// are packed) a read is served without the guaranteed-to-fail loose `open` it used to pay on
/// every packed object. The loose store is the fallback for an object written but not yet packed.
///
/// A plain match over [`read_object_classified`] — not a parallel implementation of its own —
/// which is what makes disagreement between an ordinary read and a taint recheck built on the
/// same core structurally impossible (DESIGN.html §3.1.1).
fn read_object_uncached(hash: &str) -> Result<Vec<u8>, String> {
    match read_object_classified(hash)? {
        StoreReadOutcome::Verified(bytes) => Ok(bytes),
        StoreReadOutcome::Unverifiable(e) => Err(e),
        StoreReadOutcome::Absent => {
            // The same NotFound-shaped message the read path has always produced for this case —
            // reconstructed here (rather than carried on the `Absent` variant, which is bare by
            // design) since nothing about the message depends on which of the two `Absent`
            // sources in `read_object_classified` produced it.
            let (path, file_name) = get_path_for_object(hash)?;
            let file_path = path.add(PATH_SEPARATOR).add(&file_name);
            Err(format!(
                "Error while reading object from file \"{}\": {}",
                file_path, std::io::Error::from(std::io::ErrorKind::NotFound)
            ))
        }
    }
}

/// The read cache's byte budget. When the live generation reaches it, it is retired to the
/// second generation and a fresh one starts — an approximate LRU that never exceeds ~2× this.
const READ_CACHE_BUDGET: usize = 128 * 1024 * 1024;

/// Objects larger than this are not cached (one huge object must not evict the whole working
/// set of small trees and blobs a walk actually reuses).
const READ_CACHE_MAX_ENTRY: usize = READ_CACHE_BUDGET / 8;

/// A bounded content-addressed object cache. Entries are `Arc`-shared, so a hit clones a pointer
/// under the lock and the caller shares that one allocation — an owned-bytes caller copies out
/// afterwards, off the lock (see [`retrieve_object_by_hash`] vs [`retrieve_object_by_hash_shared`]).
/// See [`super::two_gen_cache::TwoGenCache`] for the eviction/bounding shape (shared with
/// `object_utils`'s parsed-tree cache).
fn read_cache() -> &'static super::two_gen_cache::TwoGenCache<Vec<u8>> {
    static CACHE: std::sync::OnceLock<super::two_gen_cache::TwoGenCache<Vec<u8>>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| super::two_gen_cache::TwoGenCache::new(READ_CACHE_BUDGET, READ_CACHE_MAX_ENTRY))
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
    read_cache().get(&read_cache_key(hash))
}

fn read_cache_put(hash: &str, bytes: std::sync::Arc<Vec<u8>>) {
    // Weighed by its own length; store the caller's `Arc` directly — the fetched allocation is
    // shared, never re-copied.
    let weight = bytes.len();
    read_cache().put(&read_cache_key(hash), bytes, weight);
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
/// A pack-registry hit answers `true` with **no** taint-gate check at all, deliberately: the
/// registry is process-local memory, reloaded from disk only at process startup (and on a
/// reload-on-miss — see [`read_object_uncached`]), so a crash that could have lost a pack's
/// dentry also clears whatever this process's registry remembers about it. There is no stale
/// in-memory "yes" here that could outlive the crash it would need to survive, so the taint gate
/// (which exists precisely to catch such a stale "yes") has nothing to add on that path.
///
/// Only past that shortcut — a loose-path answer — does this consult
/// [`taint_utils::gate_check`]: the process-local, per-root belt that trips while a taint *this
/// process* has recorded (or seen recorded) for the root is still standing. That is
/// intentionally the weakest of the taint design's layers — it does not see a sibling process's
/// taint — the stronger, disk-backed checks are the CLI's storage-scope entry-heal
/// ([`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted), run once per
/// command before this function is ever reachable), plus every write path's own re-check on its
/// sync's success path (see [`sync_result_or_taint`]) before it may report `Ok`. A no-op unless
/// [`taint_utils::activate`] has been called in this process.
///
/// # Arguments
/// * `hash` - The hash of the object to check.
///
/// # Returns
/// * `Ok(true)`    - If the object exists.
/// * `Ok(false)`   - If the object does not exist.
/// * `Err(String)` - If an error occurred while checking if the object exists, or (loose-path
///                   only, and only once activated) a durability taint is standing for this
///                   root — never a false "does not exist" in that case.
pub fn does_object_exist(hash: &str) -> Result<bool, String> {
    // Packs first: a resident-index lookup is syscall-free, so a packed object needs no stat —
    // and, as this function's own doc comment explains, no taint-gate check either.
    if crate::util::pack_utils::is_in_packs(hash)? {
        return Ok(true);
    }

    // A standing taint means a loose-path "yes" cannot be trusted until it is healed — refuse
    // rather than silently answering as if nothing had happened.
    taint_utils::gate_check(&forklift_root())?;

    // Otherwise it may be loose (written but not yet packed).
    let (path, file_name) = get_path_for_object(hash)?;
    let file_path = path.add(PATH_SEPARATOR).add(&file_name);

    std::fs::exists(&file_path)
        .map_err(|e| format!("Error while checking if object exists: {}", e))
}

/// The gate-free sibling of [`does_object_exist`]: checks a hash's raw presence (packed, or a
/// loose dentry) without ever consulting [`taint_utils::gate_check`].
///
/// Every ordinary caller must go through the gated form — trusting a bare "yes" for dedupe *is*
/// the soundness question a standing taint puts in doubt. The one sanctioned exception is the
/// durability-taint recovery walk itself (`heal_utils`/`recovery_utils`): it runs precisely while
/// a taint may be standing under this root, so a gate-consulting presence check would refuse on
/// its very first call and the walk could never complete — the very thing it exists to resolve.
/// It is safe here because the walk never trusts this answer as proof of durability or records a
/// reference off the back of it; it only decides whether one *already-known-suspect* hash (the
/// taint's own recorded vanished path, or a hash a vanished pack's surviving index once claimed)
/// is genuinely absent — worth chasing through the closure walk — or was a false alarm (present
/// after all, e.g. it was repacked since the taint fired). That is presence-as-a-fact for a
/// read-only walk, not existence-as-proof-of-durability for a write path — the distinction the
/// gate exists to police.
///
/// # Returns
/// * `Ok(true)`    - The object is present (packed or loose).
/// * `Ok(false)`   - Neither a pack nor the loose fan-out path holds it.
/// * `Err(String)` - The pack registry or the loose path could not be checked.
pub fn raw_object_present(hash: &str) -> Result<bool, String> {
    if crate::util::pack_utils::is_in_packs(hash)? {
        return Ok(true);
    }

    let (path, file_name) = get_path_for_object(hash)?;
    let file_path = path.add(PATH_SEPARATOR).add(&file_name);

    std::fs::exists(&file_path).map_err(|e| format!("Error while checking if object exists: {}", e))
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
    fn hash_from_object_path_round_trips_with_get_path_for_object() {
        // The inverse `hash_from_object_path` exists to recover: a taint records final paths, and
        // the heal needs the hash a loose object's own path claims in order to verify it — a wrong
        // answer here (a hash that does not actually match the path, or a path that should decode
        // but returns `None`) would either skip a verification that should have run or wrongly
        // reject a genuine object path.
        for hash in ["a".repeat(64), "0123456789abcdef".repeat(4), "f".repeat(65), "1".repeat(3)] {
            let (folder, file_name) = get_path_for_object(&hash).unwrap();
            let mut final_path = PathBuf::from(&folder);
            final_path.push(&file_name);

            assert_eq!(hash_from_object_path(&final_path), Some(hash.clone()),
                "must recover the exact hash \"{hash}\" that produced this path");
        }
    }

    #[test]
    fn hash_from_object_path_rejects_a_shape_get_path_for_object_never_produces() {
        // Any path that does not have the fan-out shape (a 2-hex-char folder, then the rest of
        // the hash) must decode to `None`, not silently produce a garbage hash that could then
        // coincidentally (or maliciously) match a real item's recorded hash — this is exactly the
        // signal `heal_utils::restage_object` uses to skip hash verification for a non-loose-object
        // recorded path (a pack file, an inventory shard), so a false `Some` there would wrongly
        // demand a hash match from content that was never hash-addressed in the first place.
        let cases: Vec<PathBuf> = vec![
            PathBuf::from("/some/warehouse/.forklift/objects/ab/cdef"), // well-formed control case, see below
            PathBuf::from("/some/warehouse/.forklift/objects/a/bcdef"), // 1-char folder, not 2
            PathBuf::from("/some/warehouse/.forklift/objects/abc/def"), // 3-char folder, not 2
            PathBuf::from("/some/warehouse/.forklift/objects/zz/cdef"), // non-hex folder
            PathBuf::from("/some/warehouse/.forklift/objects/ab/cdeg"), // non-hex filename
            PathBuf::from("no-parent-at-all"),
            PathBuf::from("/"),
        ];

        // The control case (a genuinely well-formed path) must still decode correctly — proves
        // the other cases are rejected for their specific shape defect, not by some overly broad
        // check that rejects everything.
        assert_eq!(hash_from_object_path(&cases[0]), Some("abcdef".to_string()));

        for path in &cases[1..] {
            assert_eq!(hash_from_object_path(path), None,
                "must reject {path:?} instead of returning a garbage hash");
        }
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
    #[cfg(unix)]
    fn sync_dir_fault_guard_recording_mode_observes_without_failing() {
        // Pins `SyncDirFaultGuard`'s own "recording" mode (mirrors `DirSyncFaultGuard`'s and
        // `taint_utils::TaintFaultGuard`'s equivalents): the sync still succeeds, and the
        // attempted path is still observable.
        let temp = std::env::temp_dir()
            .join(format!("forklift-syncdir-fault-recording-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let _guard = SyncDirFaultGuard::recording();
        assert!(sync_dir(&temp).is_ok(), "a successful sync must still succeed while only recording");
        assert_eq!(sync_dir_attempts(), vec![temp.clone()], "the attempted path must still be observed");

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
    fn reserve_final_path_wins_exactly_once_then_loses_forever() {
        // The dedupe primitive `LooseObject::store_deferred` needs: the first caller to reserve
        // a given final path gets `true` (it owns
        // staging that path); every later call for the same path — even after the winner has
        // gone on to actually stage it — gets `false`.
        let batch = WriteBatch::new();
        let target = Path::new("/some/warehouse/.forklift/objects/ab/cdef");

        assert!(batch.reserve_final_path(target), "the first reservation must win");
        assert!(!batch.reserve_final_path(target), "a second reservation for the same path must lose");
        assert!(!batch.reserve_final_path(target), "losing is permanent, not just once");

        // An unrelated path is entirely unaffected.
        let other = Path::new("/some/warehouse/.forklift/objects/ab/0000");
        assert!(batch.reserve_final_path(other), "a different path must still win its own reservation");
    }

    #[test]
    fn reserve_final_path_is_race_free_under_concurrent_callers() {
        // The atomicity claim: many threads racing to reserve the *same* path must produce
        // exactly one winner, never zero and never more than one — a separate check-then-insert
        // (rather than one lock covering both) could let two racers both observe "not reserved
        // yet" and both proceed to stage (and compress) redundant work.
        let batch = WriteBatch::new();
        let target = Path::new("/some/warehouse/.forklift/objects/cd/ef01");
        let target_ref: &Path = target;
        let batch_ref: &WriteBatch = &batch;

        let wins: Vec<bool> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..200)
                .map(|_| scope.spawn(move || batch_ref.reserve_final_path(target_ref)))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });

        assert_eq!(wins.iter().filter(|w| **w).count(), 1,
            "exactly one of 200 concurrent reservations for the same path must win");
    }

    #[test]
    fn a_staged_write_also_counts_as_reserved() {
        // `stage`/`stage_with_mtime` populate the same reservation set `reserve_final_path`
        // checks — a caller that stages a path directly (not through `reserve_final_path` first)
        // still blocks a later reservation attempt for that same path.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-reserve-stage-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target = temp.join("object");
        let batch = WriteBatch::new();
        batch.stage(&target, b"content").unwrap();

        assert!(!batch.reserve_final_path(&target),
            "a path already staged directly must read as already reserved");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn finish_clears_the_reservation_set_so_a_reused_batch_starts_fresh() {
        // `finish` drains `pending` and `final_paths` together (same doc comment) — a `WriteBatch`
        // reused for a second round after a successful `finish` must not have the first round's
        // reservations linger and block the second round's otherwise-unrelated write to the same
        // path (e.g. a rewrite of the same file in a later, independent batch).
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-reserve-reuse-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let target = temp.join("object");
        let batch = WriteBatch::new();

        batch.stage(&target, b"round one").unwrap();
        batch.finish().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"round one");

        // A fresh reservation for the same path in the *same* batch instance, after `finish`,
        // must win again — the first round is over and published.
        assert!(batch.reserve_final_path(&target),
            "a path must be reservable again after the batch that staged it has finished");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn finish_refuses_a_batch_with_a_leaked_reservation_and_discards_its_staged_temps() {
        // The reservation set is a promise list: a path reserved but never staged means some
        // producer failed between winning the reservation and staging, while every other
        // producer for that path already stood down. Publishing the rest anyway would let a
        // downstream caller durably name a blob that was never written — `finish` must refuse
        // the whole batch instead, and (like every failed finish) discard what was staged.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-leak-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let leaked = temp.join("promised-but-never-staged");
        let staged = temp.join("staged-fine");
        let batch = WriteBatch::new();
        assert!(batch.reserve_final_path(&leaked));
        batch.stage(&staged, b"complete content").unwrap();

        let error = batch.finish().unwrap_err();
        assert!(error.contains("promised-but-never-staged"),
            "the error must name the leaked path, got: {}", error);

        // The leak is caught before the barrier: nothing was renamed, and the staged temp is
        // gone — the batch is discarded wholesale, not held for selective publishing.
        assert!(!staged.exists(), "a leaked reservation must keep the whole batch unpublished");
        let leftovers: Vec<_> = std::fs::read_dir(&temp).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftovers.is_empty(), "a failed finish must remove every staged temp, found {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn finish_names_exactly_the_leaked_paths_in_sorted_order() {
        // The error message is the diagnostic half of the leak contract: it must name precisely
        // the reserved-but-never-staged paths (a caller invalidates decisions depending on them
        // before re-staging), in sorted order so the report is deterministic, and never a path
        // that was actually staged.
        //
        // Eight leaked paths, not two: the leak set is a `HashSet`, so its iteration order is
        // randomized per process — with only two entries, an implementation that forgot to sort
        // would still produce a message in the "right" order about half the time. Enough entries
        // make that false pass negligible: about 1 in 8! (~1/40320) even under a uniform-random
        // order, and `HashSet` iteration order is not adversarial.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-leak-detail-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let names = ["zzz", "yyy", "xxx", "www", "vvv", "uuu", "ttt", "sss"];
        let leaked: Vec<_> = names.iter().map(|n| temp.join(format!("{}-leaked", n))).collect();
        let staged = temp.join("mmm-staged");

        let batch = WriteBatch::new();
        for path in &leaked {
            assert!(batch.reserve_final_path(path));
        }
        batch.stage(&staged, b"content").unwrap();

        let error = batch.finish().unwrap_err();

        let mut expected_order: Vec<String> =
            leaked.iter().map(|p| p.to_string_lossy().to_string()).collect();
        expected_order.sort();
        let positions: Vec<usize> = expected_order.iter()
            .map(|p| error.find(p.as_str())
                .unwrap_or_else(|| panic!("the error must name every leaked path, missing {}: {}", p, error)))
            .collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]),
            "leaked paths must appear in the message in sorted order, got: {}", error);

        assert!(!error.contains("mmm-staged"),
            "a path that was actually staged must not be reported as missing");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_leaked_reservation_fails_finish_even_when_nothing_was_staged() {
        // The empty-`pending` early return must not outrank the leak check: a batch whose only
        // producer failed before staging anything still made a promise it cannot keep.
        let batch = WriteBatch::new();
        assert!(batch.reserve_final_path(Path::new("/warehouse/.forklift/objects/ab/cdef")));

        assert!(batch.finish().is_err(),
            "a reservation with nothing staged at all must still refuse to finish");

        // The failure drained the reservation set with everything else, so the batch is clean
        // for a full retry: a second finish (with nothing re-staged) is an ordinary empty Ok.
        batch.finish().unwrap();
    }

    #[test]
    fn a_failed_finish_leaves_the_batch_ready_for_a_full_restage_retry() {
        // Recovery under the one contract there is: after any failed finish nothing staged
        // survives, so a retry re-stages *everything* from source — which requires the failure
        // to have drained the reservation set too, or the retry's own `reserve_final_path`
        // would read the dead round's reservations as "already handled" and stand down again.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-retry-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let flaky = temp.join("failed-first-round");
        let steady = temp.join("staged-both-rounds");
        let batch = WriteBatch::new();
        assert!(batch.reserve_final_path(&flaky));
        batch.stage(&steady, b"round one").unwrap();
        batch.finish().unwrap_err();

        assert!(batch.reserve_final_path(&flaky),
            "a retry must be able to win the reservation the failed round left behind");
        batch.stage(&flaky, b"second round, staged this time").unwrap();
        batch.stage(&steady, b"round two").unwrap();
        batch.finish().unwrap();

        assert_eq!(std::fs::read(&flaky).unwrap(), b"second round, staged this time");
        assert_eq!(std::fs::read(&steady).unwrap(), b"round two");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_rename_failure_partway_discards_the_batch_but_earlier_renames_stay_visible() {
        // The honest half of the failure contract: the rename loop stops at the first failure,
        // so an entry renamed *before* it is already visible — a failed finish means "treat the
        // whole batch as unpublished," not "nothing became visible." What it does promise: every
        // remaining temp is removed, nothing staged survives to be published later, and (see
        // `run_write_barrier`'s fix for the data-loss window this closes) the directory of every
        // entry renamed before the failure is best-effort fsynced before this call returns —
        // not just visible, but no less durable than the same crash point would leave it.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-partway-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let first = temp.join("renamed-before-the-failure");
        let blocked = temp.join("rename-target-blocked");
        let batch = WriteBatch::new();
        batch.stage(&first, b"landed").unwrap();
        batch.stage(&blocked, b"never lands").unwrap();

        // Sabotage the second rename: a non-empty directory at its final path makes the rename
        // fail on every platform, after the first entry's rename already succeeded.
        std::fs::create_dir_all(blocked.join("occupant")).unwrap();

        let error = batch.finish().unwrap_err();
        assert!(error.contains("rename-target-blocked"),
            "the error must name the failing rename, got: {}", error);

        assert_eq!(std::fs::read(&first).unwrap(), b"landed",
            "an entry renamed before the failure stays visible — the set is not atomic");
        assert!(blocked.is_dir(), "the blocked entry must not have been published");

        // Every temp is gone: the visible-first entry's was consumed by its rename, the blocked
        // entry's was removed by the failure path.
        let temps: Vec<_> = std::fs::read_dir(&temp).unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(temps.is_empty(), "a failed finish must remove every remaining temp, found {:?}",
            temps.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_batch_finish_syncs_every_touched_directory_across_a_multi_directory_batch() {
        // `run_write_barrier`'s directory-sync step (`sync_touched_directories`) fsyncs every
        // *distinct* touched parent, not just the first — exercised here transitively: staging
        // writes into three separate directories and confirming `finish` still publishes every
        // one of them (not just the ones sharing a directory with the last-written file, which is
        // the case a bug that only synced one directory would miss). Content/visibility alone
        // cannot tell a real fsync from a bug that dropped it (plain `rename` produces the same
        // file state either way), so on Unix (where `fsync_dir_data` exists — see its own doc
        // comment) this also asserts against its attempt log and `DIR_SYNC_COUNT` that every
        // directory was actually handed to the kernel, not just renamed into.
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

        #[cfg(unix)]
        let _guard = DirSyncFaultGuard::recording();
        #[cfg(unix)]
        let count_before = dir_sync_count();

        batch.finish().unwrap();

        for (target, content) in &targets {
            assert!(target.exists(), "finish must publish every directory's staged write");
            assert_eq!(std::fs::read_to_string(target).unwrap(), *content);
        }

        #[cfg(unix)]
        {
            let attempts = dir_sync_attempts();
            for dir in &dirs {
                assert!(attempts.contains(dir),
                    "every touched directory must actually be fsynced, missing {:?} from attempts {:?}",
                    dir, attempts);
            }
            assert!(dir_sync_count() >= count_before + dirs.len() as u64,
                "the directory-fsync counter must advance by at least one per distinct directory");
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    // The rest of this module exercises `run_write_barrier`'s directory-sync steps directly
    // through `fsync_dir_data`'s test-only fault injection (`DirSyncFaultGuard`,
    // `dir_sync_attempts`, `dir_sync_count`) — see their doc comments. All Unix-only: that
    // injection point does not exist on non-Unix targets (`fsync_dir_data` doesn't either).
    // Nothing in this binary ever sets `FORKLIFT_FSYNC` (unlike the separate
    // `write_batch_fsync_off` integration test binary, a different process), and `fsync_enabled`
    // defaults to on when the variable is absent — so every `fsync_dir_data` call below is live
    // rather than vacuously skipped, unless the process was launched with `FORKLIFT_FSYNC` already
    // set to an off value in its inherited environment (a caller's problem, not this binary's).

    #[test]
    #[cfg(unix)]
    fn a_rename_failure_syncs_the_directories_already_touched_before_returning() {
        // The headline fix this module exists to pin down: on a rename failure partway through
        // the barrier, every directory a rename has *already* landed in is fsynced before the
        // error returns (`run_write_barrier`'s early-sync block) — without it, an entry renamed
        // just before the failure would be visible but not durable, and a later retry would skip
        // restaging it via `does_object_exist` (see `finish`'s `# Returns`). Two directories, not
        // one: `landed` and `blocked` live in separate parents so `touched_parents` has the
        // multi-entry shape the early sync actually has to handle in practice — the proof that the
        // sync itself ran (rather than the file just happening to already be on disk) is the
        // attempt log below, not the directory layout.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-earlysync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let dir_a = temp.join("dir-a");
        let dir_b = temp.join("dir-b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let landed = dir_a.join("landed");
        let blocked = dir_b.join("blocked");

        let batch = WriteBatch::new();
        batch.stage(&landed, b"landed").unwrap();
        batch.stage(&blocked, b"never lands").unwrap();

        // Sabotage the second rename exactly like the existing partway test: a non-empty
        // directory already at the final path fails `rename` on every platform.
        std::fs::create_dir_all(blocked.join("occupant")).unwrap();

        let _guard = DirSyncFaultGuard::recording();

        let error = batch.finish().unwrap_err();
        assert!(error.contains("blocked"), "the error must name the failing rename, got: {}", error);

        // The attempt log is thread-local and per-guard, so — unlike `dir_sync_count`, a
        // process-global counter any concurrently running test could also advance — this proves
        // specifically that *this* test's `dir_a` was fsynced, not just that some fsync happened
        // somewhere in the process.
        let attempts = dir_sync_attempts();
        assert!(attempts.contains(&dir_a),
            "the directory of the entry already renamed before the failure must be fsynced \
            before finish returns, attempts: {:?}", attempts);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_batch_finish_still_syncs_every_directory_after_one_fails() {
        // `sync_touched_directories` must be best-effort across the whole set (its doc comment,
        // and the two call sites in `run_write_barrier`): a directory whose own fsync fails must
        // not stop the others from being attempted. Three directories with the middle one armed
        // to fail proves the loop does not stop at the first failure — a `?`-short-circuit would
        // leave the last directory never attempted at all.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-besteffort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let dirs: Vec<_> = ["alpha", "beta", "gamma"].iter().map(|name| temp.join(name)).collect();
        for dir in &dirs {
            std::fs::create_dir_all(dir).unwrap();
        }

        let batch = WriteBatch::new();
        let targets: Vec<_> = dirs.iter().map(|dir| dir.join("object")).collect();
        for target in &targets {
            batch.stage(target, b"content").unwrap();
        }

        // Every rename below succeeds — only the middle directory's *fsync* is sabotaged, so this
        // exercises the barrier's trailing sync (step 4), not the rename loop.
        let _guard = DirSyncFaultGuard::failing("beta");

        let error = batch.finish().unwrap_err();
        assert!(error.contains("injected directory-sync failure"),
            "the error must surface the injected fsync failure, got: {}", error);

        let attempts = dir_sync_attempts();
        for dir in &dirs {
            assert!(attempts.contains(dir),
                "every touched directory must be attempted even after another one failed, \
                missing {:?} from attempts {:?}", dir, attempts);
        }

        // The renames themselves already succeeded (only the directory fsync was sabotaged), so
        // every entry is visible despite the failed `finish` — matching `finish`'s `# Returns`
        // for the trailing-sync-failure case.
        for target in &targets {
            assert!(target.exists(), "a trailing directory-sync failure must not un-rename anything");
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_rename_failure_leads_the_error_even_when_the_early_sync_also_fails() {
        // Precedence: when a rename fails *and* the resulting early directory-sync also fails,
        // the rename error must lead the combined message (with the sync error appended), never
        // the other way around — `run_write_barrier`'s format string bakes this in
        // (`rename_error` first, `sync_error` parenthetically after), and a caller matching on
        // error text must see the rename failure it actually needs to react to up front.
        let temp = std::env::temp_dir().join(format!("forklift-writebatch-precedence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let dir_a = temp.join("dir-a");
        let dir_b = temp.join("dir-b");
        std::fs::create_dir_all(&dir_a).unwrap();
        std::fs::create_dir_all(&dir_b).unwrap();

        let landed = dir_a.join("landed");
        let blocked = dir_b.join("blocked");

        let batch = WriteBatch::new();
        batch.stage(&landed, b"landed").unwrap();
        batch.stage(&blocked, b"never lands").unwrap();
        std::fs::create_dir_all(blocked.join("occupant")).unwrap();

        // Arm the early sync itself to fail for the one directory it will actually attempt.
        let _guard = DirSyncFaultGuard::failing("dir-a");

        let error = batch.finish().unwrap_err();

        let rename_pos = error.find("Error while moving file into place")
            .unwrap_or_else(|| panic!("the rename error must be present, got: {}", error));
        let sync_pos = error.find("injected directory-sync failure")
            .unwrap_or_else(|| panic!("the appended sync error must be present, got: {}", error));
        assert!(rename_pos < sync_pos,
            "the rename error must lead the message, with the sync error appended after it, got: {}", error);
        assert_eq!(rename_pos, 0, "the rename error must be at the very start of the message, got: {}", error);

        std::fs::remove_dir_all(&temp).ok();
    }

    // Durable-taint wiring tests: `sync_result_or_taint`/`sync_dir_or_taint`/`taint_after_sync_failure`/
    // `taint_recheck` at every fire site, plus the `does_object_exist` gate. Every test that needs
    // to observe activation (on OR deliberately off) shares `taint_utils::ACTIVATION_TEST_LOCK` —
    // the same lock `taint_utils`'s own tests use — via `lock_taint_activation` below, following
    // `taint_utils`'s own tests' pattern: `ACTIVATED` is process-global, so two tests toggling it
    // concurrently would race without a shared lock. A test whose paths never enter a `StorageRootScope` is
    // unaffected by activation regardless (see `record_taint`'s scope-tolerance doc comment) —
    // which is why every *other* test in this module, none of which touch this lock, stays safe
    // even if some other test elsewhere in this binary has already activated the process.

    fn lock_taint_activation() -> std::sync::MutexGuard<'static, ()> {
        taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    #[cfg(unix)]
    fn write_batch_trailing_sync_failure_taints_the_batchs_final_paths_once_activated() {
        // Fire-site a(i): every rename in the batch ran, but the barrier's own trailing directory
        // sync then failed. Reverting the `sync_result_or_taint` wiring in `run_write_barrier`'s
        // trailing-sync step back to a bare `sync_touched_directories(...)?` call kills this test:
        // no taint file would exist to read back.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-trailing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let alpha = forklift.join("objects").join("aa").join("alpha-object");
        let beta = forklift.join("objects").join("bb").join("beta-object");
        std::fs::create_dir_all(alpha.parent().unwrap()).unwrap();
        std::fs::create_dir_all(beta.parent().unwrap()).unwrap();

        let batch = WriteBatch::new();
        batch.stage(&alpha, b"alpha content").unwrap();
        batch.stage(&beta, b"beta content").unwrap();

        let _guard = DirSyncFaultGuard::failing("bb");
        let error = batch.finish().unwrap_err();
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        // Both renames landed — only the trailing fsync failed — so both are visible but unproven.
        assert!(alpha.exists() && beta.exists());

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.torn, "a freshly recorded taint must not read as torn");
        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("objects/aa/alpha-object"),
            PathBuf::from("objects/bb/beta-object"),
        ].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "the taint must record exactly the batch's final paths, root-relative");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn a_batched_trailing_sync_failure_is_healed_by_restaging_both_final_paths() {
        // The entry-heal, driven end to end from a real `WriteBatch` trailing-sync failure (the
        // batched fire-site class, distinct from `heal_utils`'s own tests which mostly drive the
        // immediate path and hand-planted taint files). Mutation: skip the restage in
        // `heal_utils::heal_if_tainted` (fsync the parent directories without rewriting the
        // dentries first) — this test still catches it via the inode check, since the taint would
        // still clear (the fsync itself succeeds) but neither file's dentry would actually change.
        //
        // Both entries are genuine loose objects (real hash, real zstd-compressed content) rather
        // than mnemonic non-hex filenames ("alpha-object"/"beta-object", this test's own shape
        // before this slice): `heal_if_tainted` now gates its restage on content-addressability
        // (I1/I2, `heal_utils`'s module doc comment) *before* attempting anything, so a taint over
        // a path `hash_from_object_path` does not recognize would escalate instead of healing —
        // this test needs the common, content-addressed case that must still heal inline.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-heal-batched-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let alpha_content = b"a genuine loose alpha object for the batched restage-heal test";
        let beta_content = b"a genuine loose beta object for the batched restage-heal test";
        let alpha_hash = blake3::hash(alpha_content).to_hex().to_string();
        let beta_hash = blake3::hash(beta_content).to_hex().to_string();
        let alpha_compressed = zstd::encode_all(&alpha_content[..], 0).unwrap();
        let beta_compressed = zstd::encode_all(&beta_content[..], 0).unwrap();

        let (alpha_folder, alpha_file) = get_path_for_object(&alpha_hash).unwrap();
        let (beta_folder, beta_file) = get_path_for_object(&beta_hash).unwrap();
        std::fs::create_dir_all(&alpha_folder).unwrap();
        std::fs::create_dir_all(&beta_folder).unwrap();
        let alpha = PathBuf::from(&alpha_folder).join(&alpha_file);
        let beta = PathBuf::from(&beta_folder).join(&beta_file);

        let batch = WriteBatch::new();
        batch.stage(&alpha, &alpha_compressed).unwrap();
        batch.stage(&beta, &beta_compressed).unwrap();

        let inode_alpha_before = std::fs::metadata(&alpha).map(|m| m.ino()).ok();
        let inode_beta_before = std::fs::metadata(&beta).map(|m| m.ino()).ok();
        assert!(inode_alpha_before.is_none() && inode_beta_before.is_none(),
            "neither final path may exist before the batch runs");

        {
            // The needle is beta's own real shard prefix (derived from its real hash, not a
            // hardcoded folder name) — guaranteed to match beta's directory regardless of what
            // either hash actually is.
            let _guard = DirSyncFaultGuard::failing(&beta_hash[..2]);
            batch.finish().unwrap_err();
        }

        assert!(alpha.exists() && beta.exists(), "both renames landed before the trailing sync failed");
        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty());

        let inode_alpha_before = std::fs::metadata(&alpha).unwrap().ino();
        let inode_beta_before = std::fs::metadata(&beta).unwrap().ino();
        let dir_syncs_before = dir_sync_count();

        crate::util::heal_utils::heal_if_tainted().expect("a clean batched restage must heal");

        assert!(dir_sync_count() >= dir_syncs_before + 2,
            "both distinct parent directories must be fsynced by the heal");

        assert_ne!(std::fs::metadata(&alpha).unwrap().ino(), inode_alpha_before,
            "mutation check: alpha's dentry must actually be rewritten, not just fsynced");
        assert_ne!(std::fs::metadata(&beta).unwrap().ino(), inode_beta_before,
            "mutation check: beta's dentry must actually be rewritten, not just fsynced");

        let restaged_alpha = std::fs::read(&alpha).unwrap();
        assert_eq!(zstd::stream::decode_all(restaged_alpha.as_slice()).unwrap(), alpha_content);
        let restaged_beta = std::fs::read(&beta).unwrap();
        assert_eq!(zstd::stream::decode_all(restaged_beta.as_slice()).unwrap(), beta_content);

        assert!(taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint files must be gone once every recorded path healed");
        assert!(taint_utils::gate_check(&forklift).is_ok(), "the gate must be cleared");

        // The command proceeds: a fresh write under the now-healed root succeeds cleanly.
        let after_heal_dir = forklift.join("objects").join("cc");
        std::fs::create_dir_all(&after_heal_dir).unwrap();
        assert!(write_file_atomically(&after_heal_dir.join("after-heal"), b"ok").is_ok());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_batch_early_sync_failure_after_a_rename_failure_taints_only_the_visible_prefix() {
        // Fire-site a(ii): a rename failure whose own early sync (over the renames that already
        // landed) also fails. Only the prefix that actually became visible before the failure may
        // be tainted, never the entry whose rename never ran. Reverting `renamed_finals` back to
        // tainting the *whole* `pending` set (or dropping the `taint_after_sync_failure` call
        // entirely) kills this test either way — the first by making `blocked-object` appear in
        // `recorded`, the second by leaving `recorded` empty.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-earlysync-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let landed = forklift.join("objects").join("aa").join("landed-object");
        let blocked = forklift.join("objects").join("bb").join("blocked-object");
        std::fs::create_dir_all(landed.parent().unwrap()).unwrap();
        std::fs::create_dir_all(blocked.parent().unwrap()).unwrap();

        let batch = WriteBatch::new();
        batch.stage(&landed, b"landed").unwrap();
        batch.stage(&blocked, b"never lands").unwrap();

        // Sabotage the second rename: a non-empty directory at its final path fails `rename` on
        // every platform, after the first entry's rename already succeeded.
        std::fs::create_dir_all(blocked.join("occupant")).unwrap();

        // Arm the early sync itself to fail for the one directory it will actually attempt
        // ("aa" — `landed` is already renamed by the time the second rename fails).
        let _guard = DirSyncFaultGuard::failing("aa");

        let error = batch.finish().unwrap_err();
        assert!(error.contains("blocked-object"), "got: {}", error);

        let state = taint_utils::read_taints(&forklift).unwrap();
        let expected: BTreeSet<PathBuf> = [PathBuf::from("objects/aa/landed-object")].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "only the visible prefix may be tainted, never an entry that was never renamed");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn write_file_atomically_dir_sync_failure_taints_the_single_final_path_once_activated() {
        // Fire-site b: `write_file_atomically`'s own immediate-path directory sync. Reverting the
        // `sync_dir_or_taint` wiring back to a bare `sync_dir(parent)?` call kills this test.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-immediate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let target = forklift.join("objects").join("cc").join("immediate-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        let _guard = SyncDirFaultGuard::failing("cc");
        let error = write_file_atomically(&target, b"content").unwrap_err();
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        assert!(target.exists(), "the rename must have landed before the directory sync failed");

        let state = taint_utils::read_taints(&forklift).unwrap();
        let expected: BTreeSet<PathBuf> = [PathBuf::from("objects/cc/immediate-object")].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "the taint must record exactly the single final path, root-relative");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_batch_finish_refuses_when_a_taint_is_already_standing_once_activated() {
        // The success-path re-check: a taint file already standing for the root (simulating a
        // sibling write's earlier failure, or a crash survivor) must fail a batch whose own sync
        // succeeds cleanly. Reverting the `taint_recheck` call inside
        // `sync_result_or_taint` (i.e. returning `Ok(())` unconditionally on sync success) kills
        // this test.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-recheck-batch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        // Hand-write a complete taint file under this root, mirroring `taint_utils`'s own format
        // (a crash-survivor / sibling-process taint neither this batch nor this process caused).
        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let target = forklift.join("objects").join("dd").join("fresh-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        let batch = WriteBatch::new();
        batch.stage(&target, b"content").unwrap();
        let error = batch.finish().unwrap_err();
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER),
            "the refusal must be machine-recognizable, got: {}", error);

        // The write itself (rename + directory sync) already succeeded — only the re-check
        // refuses to let the caller report `Ok`.
        assert!(target.exists(), "the write itself must have landed; only the re-check refuses");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn write_file_atomically_refuses_when_a_taint_is_already_standing_once_activated() {
        // Same shape as the `WriteBatch` re-check test above, but for the immediate path — every
        // fire site must inherit the same re-check, not just the batched one (see
        // `sync_dir_or_taint`'s doc comment).
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir()
            .join(format!("forklift-taint-recheck-immediate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let target = forklift.join("objects").join("ee").join("fresh-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        let error = write_file_atomically(&target, b"content").unwrap_err();
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER), "got: {}", error);
        assert!(target.exists(), "the write itself must have landed; only the re-check refuses");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn self_trip_exemption_guard_suppresses_the_standing_taint_refusal_only_while_armed() {
        // Pins the `SelfTripExemptionGuard` mechanism itself (the mechanism, not process-global
        // scope across threads — see the remote.rs integration test for that): (a) unguarded, a
        // taint standing for the root refuses the write via `taint_recheck`; (b) with the guard
        // held, the identical write succeeds; (c) once the guard drops, the identical write
        // refuses again — not a permanent bypass.
        //
        // Mutation: drop the early-return in `taint_recheck` (or arm the guard somewhere that
        // does not actually reach `taint_recheck`) kills step (b) — the guarded write would
        // refuse just like the unguarded ones. Mutation: make the guard's `Drop` a no-op kills
        // step (c) — the post-drop write would wrongly keep succeeding.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-exemption-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let target = forklift.join("objects").join("gg").join("fresh-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        // (a) Unguarded: refuses.
        let error = write_file_atomically(&target, b"content").unwrap_err();
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER),
            "an unguarded write must refuse while a taint stands, got: {}", error);

        // (b) Guarded: succeeds.
        {
            let _guard = SelfTripExemptionGuard::new();
            write_file_atomically(&target, b"content").expect(
                "a write made while the exemption guard is armed must succeed despite the standing taint");
        }

        // (c) Guard dropped: refuses again — not a permanent bypass.
        let error = write_file_atomically(&target, b"content").unwrap_err();
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER),
            "once the guard drops, the identical write must refuse again, got: {}", error);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn self_trip_exemption_guard_is_visible_from_a_second_os_thread_it_never_armed() {
        // Pins the design's most-defended choice: `SELF_TRIP_EXEMPTION_COUNT` must be a
        // process-global `AtomicUsize`, not a `thread_local!`. Production needs this because a
        // refetch's stores do not all run on the thread that armed the guard — `JoinSet` tasks and
        // wherever the async runtime happens to poll a future both land on worker threads the
        // arming call never touched (see `SelfTripExemptionGuard`'s own doc comment). Test 2 above
        // arms, writes, and drops all on one thread, so it passes identically under a
        // `thread_local!` implementation; it cannot tell the two designs apart. This test can:
        // it arms the guard on the main thread, then does the write from a second, explicitly
        // constructed `std::thread::spawn` thread — the same call shape a `JoinSet` task or a
        // polled future uses in production, just driven by construction instead of by scheduler
        // luck. Deliberately not a yield/retry loop waiting for thread ids to diverge: that
        // mechanism can *hang* rather than merely flake, under a `current_thread`-flavor test
        // runtime or a `worker_threads = 1` CI pin (tokio's LIFO fast-path can keep a
        // freshly-spawned task on the very worker that spawned it, with no bound on the wait).
        //
        // Mutation: this is the test that reddens if `SELF_TRIP_EXEMPTION_COUNT` is changed from
        // an `AtomicUsize` to a `thread_local!` — the child thread's runtime would then see an
        // unarmed (zero) counter and the write below would wrongly refuse.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-exemption-cross-thread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let target = forklift.join("objects").join("hh").join("fresh-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        // Armed on the main thread. Held across the spawn/join below so it is still armed for
        // the entire lifetime of the child thread's own write.
        let _guard = SelfTripExemptionGuard::new();

        let child_temp = temp.clone();
        let child_target = target.clone();
        let child_result = std::thread::spawn(move || {
            // `StorageRootScope` is thread-local by design (unlike the exemption counter this
            // test pins) — the child thread must enter its own scope onto the same root so
            // `taint_recheck`'s `resolve_root_for` resolves the same taint directory the main
            // thread just populated, or the write would trivially succeed by skipping the taint
            // check entirely rather than by being exempted from it.
            let _child_scope = StorageRootScope::enter(&child_temp);

            // `new_current_thread`, not the default multithreaded builder: this reproduces the
            // exact call shape production uses (a sync store call driven from inside a polled
            // async task on a worker thread), on a runtime this test constructs and controls
            // rather than one whose scheduling could vary.
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            runtime.block_on(async { write_file_atomically(&child_target, b"content") })
        })
        .join()
        .expect("the child thread must not panic");

        child_result.expect(
            "a write from a different OS thread must still be exempted while the guard \
             armed on the main thread is held — the exemption counter must be process-global");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn does_object_exist_refuses_a_loose_check_while_tainted_but_still_answers_pack_hits() {
        // The existence gate's documented ordering: a pack-registry hit answers with no gate
        // check at all (the registry is process-local memory that a crash also clears); only the
        // loose-path fallback consults the gate. Reverting the `taint_utils::gate_check` call in
        // `does_object_exist` kills the loose-refusal half of this test; moving the gate check
        // *before* the pack-registry lookup would kill the pack-hit half.
        let _serial = lock_taint_activation();
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-taint-gate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        // A packed object, packed *before* the gate ever trips.
        let packed_content = vec![0x22u8; 500];
        let packed_hash = blake3::hash(&packed_content).to_hex().to_string();
        let compressed = zstd::encode_all(packed_content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&packed_hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
        crate::util::pack_utils::compact(false, false).unwrap();

        // A loose object, written *after* compaction so it stays loose.
        let loose_content = vec![0x33u8; 500];
        let loose_hash = blake3::hash(&loose_content).to_hex().to_string();
        let compressed = zstd::encode_all(loose_content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&loose_hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();

        assert!(does_object_exist(&loose_hash).unwrap(), "sanity: unblocked, the loose object answers true");

        // Trip the in-memory gate for this root the same way a failing write would.
        let unrelated = forklift.join("objects").join("ff").join("unrelated");
        taint_utils::record_taint(&[unrelated.as_path()]).unwrap();

        let error = does_object_exist(&loose_hash).unwrap_err();
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER),
            "a loose-path check must refuse while the gate is standing, got: {}", error);

        assert!(does_object_exist(&packed_hash).unwrap(),
            "a pack-registry hit must still answer `true` regardless of the gate");

        // `test_clear_gate`, not the production `resolve_taints`, deliberately: this isolates
        // `does_object_exist`'s gate consultation from taint-file state — the taint file the
        // `record_taint` call above wrote for `unrelated` is left standing on disk on purpose.
        taint_utils::test_clear_gate(&forklift);
        assert!(does_object_exist(&loose_hash).unwrap(), "clearing the gate must restore a normal answer");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    #[cfg(unix)]
    fn taint_wiring_is_a_no_op_end_to_end_when_never_activated() {
        // The activation test for the wiring layer itself: with `taint_utils::activate` never
        // called (forced back to the unactivated state under the shared lock, in case an earlier
        // test elsewhere in this binary already activated the process), every fire site above —
        // and the existence gate — must behave exactly as it did before this feature existed: no
        // taint directory ever created, no re-check ever performed, no gate ever trips. This is
        // the regression proof for the wiring layer: every one of `record_taint`/`read_taints`/
        // `gate_check` already gates itself on activation (`taint_utils`'s own tests pin that);
        // this test pins that the *wiring* added here never bypasses that gating or activates the
        // machinery itself.
        let _serial = lock_taint_activation();
        taint_utils::reset_activation_for_test();

        let temp = std::env::temp_dir().join(format!("forklift-taint-baseline-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = forklift_root();

        // A real directory-sync failure, batched — must fail the batch but leave no taint trace.
        let alpha = forklift.join("objects").join("aa").join("alpha-object");
        let beta = forklift.join("objects").join("bb").join("beta-object");
        std::fs::create_dir_all(alpha.parent().unwrap()).unwrap();
        std::fs::create_dir_all(beta.parent().unwrap()).unwrap();

        let batch = WriteBatch::new();
        batch.stage(&alpha, b"alpha content").unwrap();
        batch.stage(&beta, b"beta content").unwrap();

        {
            let _guard = DirSyncFaultGuard::failing("bb");
            let error = batch.finish().unwrap_err();
            assert!(error.contains("injected directory-sync failure"), "got: {}", error);
        }

        assert!(!forklift.join("taint").exists(),
            "an unactivated process must never create a taint directory, even after a real \
            directory-sync failure");

        // The existence gate must never trip.
        let content = vec![0x44u8; 500];
        let hash = blake3::hash(&content).to_hex().to_string();
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
        let (folder, file_name) = get_path_for_object(&hash).unwrap();
        write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
        assert!(does_object_exist(&hash).unwrap(), "an unactivated gate check must never refuse");

        // And a hand-written (pre-existing) taint file must not be honored by the re-check either.
        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-1-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let target = forklift.join("objects").join("cc").join("fresh-object");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        write_file_atomically(&target, b"content").unwrap();
        assert!(target.exists(), "an unactivated re-check must never refuse a clean sync");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Falsifying test (DESIGN.html §3.1.1): [`read_object_classified`]'s outcome class must agree
    /// with [`retrieve_object_by_hash_uncached`]'s (the ordinary read path's) success/failure
    /// across every shape the store can hand back — a genuinely verified object (loose or packed),
    /// a present-but-unreadable one (loose content that does not hash-verify; a packed delta whose
    /// base was never stored), and one genuinely absent everywhere. This is the structural
    /// guarantee the whole module doc comment on [`read_object_classified`] rests on: since
    /// [`read_object_uncached`] is written as a plain match over this function's own result, the
    /// two can never disagree *by construction* — this test pins that contract so a future edit
    /// that reintroduces two independent implementations (rather than one core plus a match) is
    /// caught here rather than trusted on faith. Mutation: reorder the branches in either
    /// `read_object_classified` or `read_object_uncached` (e.g. check loose before packs in one but
    /// not the other) → at least one case's classified/ordinary pairing disagrees → red.
    #[test]
    fn read_object_classified_agrees_with_the_ordinary_read_across_every_shape() {
        use crate::util::pack_utils::TransportPackBuilder;

        #[derive(Debug, Clone, Copy)]
        enum Shape { VerifiedLoose, CorruptLoose, PackedGood, PackedUnreconstructable, Absent }

        for shape in [
            Shape::VerifiedLoose, Shape::CorruptLoose, Shape::PackedGood,
            Shape::PackedUnreconstructable, Shape::Absent,
        ] {
            let temp = std::env::temp_dir()
                .join(format!("forklift-classified-agreement-{:?}-{}", shape, std::process::id()));
            let _ = std::fs::remove_dir_all(&temp);
            std::fs::create_dir_all(&temp).unwrap();
            let _scope = StorageRootScope::enter(&temp);

            let hash = match shape {
                Shape::VerifiedLoose => {
                    let content = b"a genuinely verified loose object";
                    let hash = blake3::hash(content).to_hex().to_string();
                    let (folder, file_name) = get_path_for_object(&hash).unwrap();
                    std::fs::create_dir_all(&folder).unwrap();
                    let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();
                    write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
                    hash
                }
                Shape::CorruptLoose => {
                    let hash = "1".repeat(64);
                    let mismatched = zstd::encode_all(
                        b"these bytes do not hash to the claimed address".as_slice(), 0,
                    ).unwrap();
                    let (folder, file_name) = get_path_for_object(&hash).unwrap();
                    write_object_to_file(Path::new(&folder), &file_name, mismatched).unwrap();
                    hash
                }
                Shape::PackedGood => {
                    let content = b"a genuinely verified packed object";
                    let hash = blake3::hash(content).to_hex().to_string();
                    let mut builder = TransportPackBuilder::new(&crate::util::pack_utils::pack_folder()).unwrap();
                    builder.append_full(&hash, content.as_slice()).unwrap();
                    builder.finish().unwrap();
                    hash
                }
                Shape::PackedUnreconstructable => {
                    let hash = "2".repeat(64);
                    let never_stored_base = "3".repeat(64);
                    let mut builder = TransportPackBuilder::new(&crate::util::pack_utils::pack_folder()).unwrap();
                    builder.append_delta(&hash, &never_stored_base, 10, b"not a real delta payload").unwrap();
                    builder.finish().unwrap();
                    hash
                }
                Shape::Absent => "4".repeat(64),
            };

            let classified = read_object_classified(&hash)
                .unwrap_or_else(|e| panic!("[{:?}] the store itself must be consultable: {}", shape, e));
            let ordinary = retrieve_object_by_hash_uncached(&hash);

            match shape {
                Shape::VerifiedLoose | Shape::PackedGood => {
                    assert!(matches!(classified, StoreReadOutcome::Verified(_)),
                        "[{:?}] expected Verified", shape);
                    assert!(ordinary.is_ok(),
                        "[{:?}] classified Verified but the ordinary read failed: {:?}", shape, ordinary);
                }
                Shape::CorruptLoose | Shape::PackedUnreconstructable => {
                    assert!(matches!(classified, StoreReadOutcome::Unverifiable(_)),
                        "[{:?}] expected Unverifiable, got a different classification", shape);
                    assert!(ordinary.is_err(),
                        "[{:?}] classified Unverifiable but the ordinary read succeeded", shape);
                }
                Shape::Absent => {
                    assert!(matches!(classified, StoreReadOutcome::Absent),
                        "[{:?}] expected Absent, got a different classification", shape);
                    assert!(ordinary.is_err(),
                        "[{:?}] classified Absent but the ordinary read succeeded", shape);
                }
            }

            std::fs::remove_dir_all(&temp).ok();
        }
    }
}
