//! The durable taint-record primitive: after a write's final names are visible but the
//! directory entries recording them (or their own bytes) could not be proven durable, this
//! module lets the failing write record exactly which final object paths are suspect, so a
//! later heal knows precisely what to redo instead of re-verifying a whole storage root.
//!
//! This module is the primitive only: recording a taint and reading one back, plus a
//! process-local in-memory gate that gives an *activated* process an immediate belt against
//! trusting existence under a root it just tainted. Nothing here decides *when* a taint should
//! fire (every post-rename directory-sync call site does, via `file_utils`'s
//! `sync_dir_or_taint`/`sync_result_or_taint`), and nothing here heals —
//! [`remove_taint_files`] and [`clear_gate`] are the primitives
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) clears a resolved
//! taint with, not a heal themselves; the restage logic and the entry chokepoint live there.
//!
//! ## Activation
//!
//! [`activate`] flips a process-global switch. Every public function in this module is a
//! documented no-op until it is called: [`record_taint`] writes nothing, [`gate_check`] never
//! refuses, [`read_taints`] always reads back empty. A process that has not activated this
//! machinery has no way to *heal* a taint either, so it must not be able to see or set one —
//! taking half the mechanism (recording or gating without ever healing) is strictly worse than
//! today's baseline (a permanent wedge, or a disk record nobody ever consumes), so activation is
//! all-or-nothing, never per-call. Nothing in this module ever calls [`activate`] itself.
//!
//! ## Format
//!
//! One taint file per failed batch, under `<root>/taint/`, named `taint-<pid>-<counter>` and
//! created with an exclusive create (never a rename-over — see [`record_taint`]'s doc comment
//! for why). Content is the batch's final object paths, root-relative, one per line, followed by
//! a terminator line (see [`TAINT_TERMINATOR`]). A file whose bytes end with that exact
//! terminator line is complete; anything else — truncated by a crash mid-write, or simply absent
//! — is **torn**: its parseable (fully newline-terminated) prefix still contributes recorded
//! paths, since a real write only ever appends, but the file can no longer prove it named every
//! path the failing batch touched. [`read_taints`] unions every file under the directory and
//! reports `torn` if any one of them is.
//!
//! ## What this module does not do
//!
//! No restage logic and no entry chokepoint (see
//! [`heal_utils`](crate::util::heal_utils) for both), and no CLI-visible recovery verb for the
//! cases the automatic entry-heal cannot resolve on its own (a later slice, built on top of
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted)'s refusal).

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use crate::globals::forklift_root;
use crate::util::file_utils;

/// The folder under a storage root that holds taint files.
const TAINT_DIR_NAME: &str = "taint";

/// The literal last line of a complete taint file. Its presence (as the file's exact suffix,
/// including the newline that follows it) is what distinguishes a fully-written taint file from
/// one truncated by a crash mid-write — see the module doc comment's format section.
const TAINT_TERMINATOR: &str = "END";

/// An arbitrary bound on how many same-process, same-`pid` taint files [`record_taint`] will
/// probe for before giving up. Thousands of live taint files under one root means something else
/// is badly wrong (a stuck writer looping, a `pid` reused pathologically often) — this turns
/// that into a loud error instead of an unbounded scan.
const TAINT_FILENAME_SANITY_CAP: u32 = 10_000;

/// A substring every [`gate_check`] refusal contains, so a caller can recognize "the gate is
/// standing" without string-matching the whole message.
pub const GATE_TAINT_MARKER: &str = "a durability taint is standing";

/// The process-global activation switch — see the module doc comment's activation section.
static ACTIVATED: AtomicBool = AtomicBool::new(false);

/// Activate the taint machinery for the rest of this process. See the module doc comment: this
/// is all-or-nothing, and nothing in this module calls it — a caller wires this in only once it
/// has also wired a heal, per the module doc comment's activation section.
pub fn activate() {
    ACTIVATED.store(true, Ordering::SeqCst);
}

/// Serializes every test — in this module or anywhere else in the crate — that touches
/// [`ACTIVATED`] or the gate map: both are process-global state (see the module doc comment's
/// activation section), so a test asserting the *unactivated* behavior would otherwise race a
/// concurrently running test that has already called [`activate`]. `pub(crate)` (rather than
/// nested inside this module's own `#[cfg(test)] mod tests`) so other modules' tests that also
/// need to observe the unactivated state (`file_utils`'s taint-wiring tests) serialize through
/// this exact same lock instead of a second, independent one that would not actually exclude
/// anything. Recovers from a poisoned lock (a prior test panicking while holding it) rather than
/// cascading a panic into every later test.
#[cfg(test)]
pub(crate) static ACTIVATION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Reset the process-global activation switch for a test that needs to observe the unactivated
/// state. Test-only: production code has exactly one direction, [`activate`]. `pub(crate)` for
/// the same cross-module reason as [`ACTIVATION_TEST_LOCK`].
#[cfg(test)]
pub(crate) fn reset_activation_for_test() {
    ACTIVATED.store(false, Ordering::SeqCst);
}

fn is_activated() -> bool {
    ACTIVATED.load(Ordering::SeqCst)
}

/// Record that a batch of final object paths was left visible without a proven-durable
/// directory entry (or proven-durable bytes), so a later heal knows exactly what to redo.
///
/// A no-op — `Ok(())`, nothing written, no gate set — unless [`activate`] has been called in
/// this process (see the module doc comment).
///
/// Resolves the storage root that owns `final_paths` via [`forklift_root`]. If any given path is
/// not actually under that root — the shape a bare-path unit test's paths take, having never
/// entered a storage-root scope at all — there is no warehouse whose trust needs revoking, so
/// this skips silently rather than erroring: best-effort root resolution is this module's
/// documented scope tolerance, not a failure. An empty `final_paths` is the same case (nothing to
/// taint) and is likewise a silent no-op.
///
/// **Ordering, load-bearing:** the in-memory gate (see [`gate_check`]) is set *before* the
/// durable write below is even attempted. The gate is cheap and cannot itself fail; the disk
/// write can. A process that observes the disk write fail must still refuse to trust existence
/// under this root for the rest of its life (or until a future heal clears it) — gating first
/// means that refusal holds even when the durable half of this call never lands.
///
/// **No re-entry, ever.** The taint's own write below never calls [`file_utils::write_file_atomically`]
/// or anything that could itself fail a post-rename directory sync and recurse back into this
/// module — it is its own small, self-contained routine (exclusive-create, write, fsync file,
/// fsync directory). On any failure of that routine, this returns the error and the routine
/// removes whatever partial file it created (see [`write_taint_file`]) — a *returned* error
/// therefore leaves no taint file behind. The one case that can still leave a torn file on disk
/// is a hard crash (power loss) *during* the write, which runs no cleanup code at all; that
/// residual — original failure, AND this write's own failure or a crash, AND a retry before any
/// heal — is exactly the double-failure window this design accepts and documents, never hidden
/// by pretending the write cannot fail.
///
/// # Returns
/// * `Ok(())`      - Not activated, nothing to resolve, or the taint was durably recorded.
/// * `Err(String)` - Activated, the root resolved, and the durable write itself failed. The gate
///                   for this root is set regardless (see the ordering note above).
pub fn record_taint(final_paths: &[&Path]) -> Result<(), String> {
    if !is_activated() || final_paths.is_empty() {
        return Ok(());
    }

    let root = forklift_root();
    let Some(relative_paths) = to_root_relative(&root, final_paths) else {
        return Ok(());
    };

    // Cheap and infallible: set first, so this process gates itself even if the write below
    // fails outright (see this function's doc comment).
    set_gate(&root);

    write_taint_file(&root, &relative_paths)
}

/// Strip `root` off every path in `final_paths`, returning `None` (rather than a partial result)
/// if any single one is not actually under `root` — see [`record_taint`]'s doc comment on why an
/// all-or-nothing skip, not a partial taint, is the right behavior for an unresolvable root.
fn to_root_relative(root: &Path, final_paths: &[&Path]) -> Option<BTreeSet<PathBuf>> {
    final_paths.iter()
        .map(|path| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .collect()
}

/// The taint's own write routine — deliberately not [`file_utils::write_file_atomically`] (see
/// [`record_taint`]'s doc comment on no re-entry). Exclusive-creates a fresh file under
/// `<root>/taint/`, writes every recorded path plus the terminator, fsyncs the file's own bytes,
/// then fsyncs the directory. On any failure past file creation, best-effort removes the file it
/// created before returning the error — a *returned* error (as opposed to a crash, which runs no
/// cleanup at all) therefore never leaves a taint file behind.
fn write_taint_file(root: &Path, relative_paths: &BTreeSet<PathBuf>) -> Result<(), String> {
    let taint_dir = root.join(TAINT_DIR_NAME);
    file_utils::create_folder_if_not_exists(&taint_dir)?;

    let (mut handle, candidate) = create_taint_file(&taint_dir)?;

    let mut content = String::new();
    for path in relative_paths {
        content.push_str(&path.to_string_lossy());
        content.push('\n');
    }
    content.push_str(TAINT_TERMINATOR);
    content.push('\n');

    if let Err(e) = handle.write_all(content.as_bytes()) {
        let _ = std::fs::remove_file(&candidate);
        return Err(format!("Error while writing taint file \"{}\": {}", candidate.to_string_lossy(), e));
    }

    if let Err(e) = sync_taint_file(&handle, &candidate) {
        let _ = std::fs::remove_file(&candidate);
        return Err(e);
    }

    // The directory counterpart of the fsync above: the taint file's own bytes durable is not
    // enough if its directory entry (the name itself) is not. Reuses `file_utils::sync_dir`
    // unchanged — a plain fsync-of-a-directory-handle helper, never itself a barrier that could
    // fail a rename and recurse back into tainting.
    if let Err(e) = file_utils::sync_dir(&taint_dir) {
        let _ = std::fs::remove_file(&candidate);
        return Err(e);
    }

    Ok(())
}

/// Exclusive-create the next free `taint-<pid>-<counter>` file under `taint_dir`: `O_CREAT|O_EXCL`
/// via [`std::fs::OpenOptions::create_new`], retrying with the next integer suffix on `EEXIST`
/// (a crash survivor from an earlier process with the same `pid`, or a concurrent unlocked
/// writer) rather than ever renaming over it — see [`record_taint`]'s doc comment: a crash
/// survivor structurally cannot be overwritten by exclusive create, whatever its name.
fn create_taint_file(taint_dir: &Path) -> Result<(std::fs::File, PathBuf), String> {
    let pid = std::process::id();

    for counter in 0..TAINT_FILENAME_SANITY_CAP {
        let candidate = taint_dir.join(format!("taint-{}-{}", pid, counter));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(handle) => return Ok((handle, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!(
                "Error while creating taint file \"{}\": {}", candidate.to_string_lossy(), e
            )),
        }
    }

    Err(format!(
        "Could not find a free taint file name under \"{}\" after {} attempts.",
        taint_dir.to_string_lossy(), TAINT_FILENAME_SANITY_CAP
    ))
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection for [`sync_taint_file`], mirroring `file_utils`'s
    /// `DirSyncFaultGuard`: records every taint file path it is asked to sync, in call order,
    /// and can be armed to fail for paths containing a given substring instead of touching the
    /// filesystem. Thread-local for the same reason as its `file_utils` counterpart — a taint
    /// write always runs entirely on its caller's thread.
    static TAINT_WRITE_FAULT: std::cell::RefCell<TaintFaultState> =
        const { std::cell::RefCell::new(TaintFaultState { attempted: Vec::new(), fail_needle: None }) };
}

#[cfg(test)]
struct TaintFaultState {
    attempted: Vec<PathBuf>,
    fail_needle: Option<String>,
}

/// RAII scope for [`TAINT_WRITE_FAULT`]: both construction and `Drop` reset this thread's state,
/// so neither a stale guard from a previous test on a reused thread, nor this guard's own
/// arming once the test that created it is done, can bleed into another test.
#[cfg(test)]
struct TaintFaultGuard;

#[cfg(test)]
impl TaintFaultGuard {
    /// Record every taint file [`sync_taint_file`] is asked to sync; fail none of them.
    fn recording() -> Self {
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState { attempted: Vec::new(), fail_needle: None });
        TaintFaultGuard
    }

    /// Record every taint file, and fail (with a distinctive error, no filesystem access) any
    /// whose path contains `needle`.
    fn failing(needle: &str) -> Self {
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState {
            attempted: Vec::new(),
            fail_needle: Some(needle.to_string()),
        });
        TaintFaultGuard
    }
}

#[cfg(test)]
impl Drop for TaintFaultGuard {
    fn drop(&mut self) {
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState { attempted: Vec::new(), fail_needle: None });
    }
}

/// The taint files [`sync_taint_file`] has been asked to sync on this thread since the current
/// [`TaintFaultGuard`] was armed, in call order.
#[cfg(test)]
fn taint_write_attempts() -> Vec<PathBuf> {
    TAINT_WRITE_FAULT.with(|f| f.borrow().attempted.clone())
}

/// Fsync the taint file's own bytes — `File::sync_all` is `F_FULLFSYNC` on macOS (a real
/// device-cache flush), so this alone is sufficient device durability for the taint file itself,
/// with no separate device-flush call needed (unlike the cheap `libc::fsync` used elsewhere in
/// this store's batched barrier, which is why *that* path needs one). Honors [`file_utils::fsync_enabled`]
/// like every other durability step in this store.
fn sync_taint_file(handle: &std::fs::File, path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if let Some(injected) = TAINT_WRITE_FAULT.with(|f| {
        let mut f = f.borrow_mut();
        f.attempted.push(path.to_path_buf());
        f.fail_needle.as_deref()
            .filter(|needle| path.to_string_lossy().contains(needle))
            .map(|_| format!("injected taint-write failure for \"{}\"", path.to_string_lossy()))
    }) {
        return Err(injected);
    }

    if file_utils::fsync_enabled() {
        handle.sync_all()
            .map_err(|e| format!("Error while syncing taint file \"{}\": {}", path.to_string_lossy(), e))?;
    }

    Ok(())
}

/// The process-local per-root gate — see [`gate_check`]'s doc comment.
fn gate_state() -> &'static Mutex<BTreeSet<PathBuf>> {
    static GATE: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// The one resolution [`record_taint`] (setting) and [`gate_check`] (checking) both key the gate
/// through, so the two sides can never drift apart into resolving "the same root" two different
/// ways. Identity today (both callers already hand in an already-resolved [`forklift_root`]
/// value), but named and shared so a future normalization need only change here.
fn gate_key(root: &Path) -> PathBuf {
    root.to_path_buf()
}

fn set_gate(root: &Path) {
    gate_state().lock().expect("taint gate lock poisoned").insert(gate_key(root));
}

/// Check whether `root` is gated: `Err` while a taint [`record_taint`] set for this root is
/// standing, `Ok(())` otherwise. A no-op — always `Ok(())` — unless [`activate`] has been called
/// in this process (see the module doc comment).
///
/// This is the process-local belt, not the durable record — it only sees taints this same
/// process set, never a sibling process's. The returned error always contains
/// [`GATE_TAINT_MARKER`], so a caller can recognize it without matching the whole message.
///
/// # Returns
/// * `Ok(())`      - Not activated, or no taint is standing for `root`.
/// * `Err(String)` - Activated and a taint is standing for `root`.
pub fn gate_check(root: &Path) -> Result<(), String> {
    if !is_activated() {
        return Ok(());
    }

    if gate_state().lock().expect("taint gate lock poisoned").contains(&gate_key(root)) {
        return Err(format!(
            "{} under \"{}\"; existence cannot be trusted here until it is healed.",
            GATE_TAINT_MARKER, root.to_string_lossy()
        ));
    }

    Ok(())
}

/// Clear the gate for `root`, if one is standing. `pub(crate)` — this is the primitive
/// [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) clears the in-memory
/// belt with once it has durably resolved the taint on disk.
pub(crate) fn clear_gate(root: &Path) {
    gate_state().lock().expect("taint gate lock poisoned").remove(&gate_key(root));
}

/// The union of every taint file recorded under a storage root.
#[derive(Debug, Default)]
pub struct TaintState {
    /// Every root-relative final object path recorded across every taint file under the root,
    /// unioned. Populated even when [`torn`](Self::torn) is set — a torn file's parseable prefix
    /// still contributes (see the module doc comment's format section).
    pub recorded: BTreeSet<PathBuf>,

    /// Whether at least one taint file under the root was missing its terminator — truncated by
    /// a crash mid-write, or otherwise unreadable as complete. A torn taint's scope is unknown:
    /// [`recorded`](Self::recorded) is a lower bound, never a proof of the full set the failing
    /// batch touched.
    pub torn: bool,
}

/// Read and union every taint file under `root`'s taint directory. A no-op — always the empty,
/// non-torn state — unless [`activate`] has been called in this process (see the module doc
/// comment): an unactivated process has no heal wired, so it must not act on taint state even to
/// the extent of reading it back.
///
/// An absent or empty taint directory reads as the empty, non-torn state — the overwhelmingly
/// common case.
///
/// # Returns
/// * `Ok(state)`   - Not activated (state is empty), or the directory was read successfully
///                   (possibly empty).
/// * `Err(String)` - Activated, and the taint directory exists but could not be read (a taint
///                   file could not be opened, or its bytes could not be read).
pub fn read_taints(root: &Path) -> Result<TaintState, String> {
    if !is_activated() {
        return Ok(TaintState::default());
    }

    let taint_dir = root.join(TAINT_DIR_NAME);
    if !taint_dir.exists() {
        return Ok(TaintState::default());
    }

    let mut state = TaintState::default();

    let entries = std::fs::read_dir(&taint_dir)
        .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let bytes = std::fs::read(&path)
            .map_err(|e| format!("Error while reading taint file \"{}\": {}", path.to_string_lossy(), e))?;

        let (file_recorded, file_torn) = parse_taint_content(&bytes);
        state.recorded.extend(file_recorded);
        state.torn |= file_torn;
    }

    Ok(state)
}

/// Parse one taint file's bytes into its recorded root-relative paths and whether it is torn —
/// see the module doc comment's format section.
///
/// Bytes that are not valid UTF-8 at all are treated as maximally torn: nothing about them can
/// be trusted as a path, so the recorded set contributes nothing and `torn` is set.
fn parse_taint_content(bytes: &[u8]) -> (BTreeSet<PathBuf>, bool) {
    let Ok(text) = std::str::from_utf8(bytes) else {
        return (BTreeSet::new(), true);
    };

    let terminator_line = format!("{}\n", TAINT_TERMINATOR);

    if let Some(body) = text.strip_suffix(&terminator_line) {
        let recorded = body.lines().filter(|line| !line.is_empty()).map(PathBuf::from).collect();
        return (recorded, false);
    }

    // No terminator as the file's exact suffix: torn. Only fully newline-terminated lines are
    // trusted as recorded paths — a trailing fragment with no closing `\n` is exactly the debris
    // a crash mid-write-of-a-line would leave, and is dropped rather than parsed as a path.
    let mut recorded = BTreeSet::new();
    let mut rest = text;
    while let Some(newline_at) = rest.find('\n') {
        let line = &rest[..newline_at];
        if !line.is_empty() && line != TAINT_TERMINATOR {
            recorded.insert(PathBuf::from(line));
        }
        rest = &rest[newline_at + 1..];
    }

    (recorded, true)
}

/// Delete every taint file under `root`'s taint directory, then fsync the directory so the
/// removals are themselves durable. `pub(crate)` — the primitive
/// [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) clears taints with
/// once it has durably restaged every recorded path (see that function's doc comment). Not gated
/// by activation: unlike recording or checking, deleting is only ever reached through a heal an
/// activated process itself drives, so gating here would be redundant, not protective.
///
/// # Returns
/// * `Ok(())`      - The taint directory was absent, empty, or successfully cleared.
/// * `Err(String)` - A taint file could not be removed, or the directory fsync failed.
pub(crate) fn remove_taint_files(root: &Path) -> Result<(), String> {
    let taint_dir = root.join(TAINT_DIR_NAME);
    if !taint_dir.exists() {
        return Ok(());
    }

    let entries = std::fs::read_dir(&taint_dir)
        .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;
        let path = entry.path();
        if path.is_file() {
            std::fs::remove_file(&path)
                .map_err(|e| format!("Error while removing taint file \"{}\": {}", path.to_string_lossy(), e))?;
        }
    }

    file_utils::sync_dir(&taint_dir)
}

/// The path of any one regular taint file under `root`'s taint directory — used by
/// [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) as the write-mode-
/// openable regular file its post-restage macOS device flush needs (a directory cannot be opened
/// write-mode; see `file_utils::macos_flush_device_cache`'s doc comment). Which file is returned
/// is unspecified when more than one exists — the flush is drive-wide, so any one of them serves
/// equally.
///
/// # Returns
/// * `Ok(Some(path))` - A taint file exists; `path` is one of them (unspecified which).
/// * `Ok(None)`       - The taint directory is absent or holds no regular file.
/// * `Err(String)`    - The taint directory exists but could not be read.
pub(crate) fn any_taint_file_path(root: &Path) -> Result<Option<PathBuf>, String> {
    let taint_dir = root.join(TAINT_DIR_NAME);
    if !taint_dir.exists() {
        return Ok(None);
    }

    let entries = std::fs::read_dir(&taint_dir)
        .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;
        let path = entry.path();
        if path.is_file() {
            return Ok(Some(path));
        }
    }

    Ok(None)
}

/// The path of the taint directory under a given warehouse root, independent of any active
/// process-wide storage-root scope — for a caller that already knows the target warehouse's root
/// directly rather than through the ambient scope [`record_taint`]/[`read_taints`] resolve
/// against (a test driving `forklift` as a spawned subprocess, whose own working directory has
/// nothing to do with the warehouse being exercised — the same need `load_guard_utils::
/// marker_path_under` serves for the incomplete-load marker). Assumes a plain, non-bay warehouse
/// layout (`<root>/.forklift/taint/...`), which is all such a caller ever drives.
pub fn taint_dir_path_under(warehouse_root: &Path) -> PathBuf {
    warehouse_root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join(TAINT_DIR_NAME)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    /// This module's own tests share [`super::ACTIVATION_TEST_LOCK`] (via the glob import above)
    /// with every other module's activation-sensitive tests — see its doc comment.
    fn lock_activation() -> std::sync::MutexGuard<'static, ()> {
        ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forklift-taint-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn record_then_read_round_trips_the_exact_root_relative_set() {
        // Pins the round trip end to end: what is recorded is exactly what comes back, and a
        // freshly (and completely) written taint never reads as torn.
        let _serial = lock_activation();
        activate();

        let root = scratch("round-trip");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let a = forklift.join("objects").join("ab").join("cdef1234567890");
        let b = forklift.join("objects").join("cd").join("00112233445566");
        record_taint(&[a.as_path(), b.as_path()]).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn, "a freshly recorded taint must not read as torn");

        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("objects/ab/cdef1234567890"),
            PathBuf::from("objects/cd/00112233445566"),
        ].into_iter().collect();
        assert_eq!(state.recorded, expected);
    }

    #[test]
    fn a_preexisting_first_candidate_filename_forces_the_next_suffix_and_both_survive() {
        // Pins the O_CREAT|O_EXCL crash-survivor guarantee (memo test 9's shape): a taint file
        // already occupying the exact name a fresh write would generate must never be
        // overwritten — the write takes the next counter suffix instead, and reading unions both.
        let _serial = lock_activation();
        activate();

        let root = scratch("o-excl-survivor");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(TAINT_DIR_NAME);
        std::fs::create_dir_all(&taint_dir).unwrap();

        let pid = std::process::id();
        let survivor_path = taint_dir.join(format!("taint-{}-0", pid));
        let survivor_content = "objects/aa/survivor\nEND\n";
        std::fs::write(&survivor_path, survivor_content).unwrap();

        let object_path = forklift.join("objects").join("11").join("22334455");
        record_taint(&[object_path.as_path()]).unwrap();

        // The pre-existing file must be untouched, byte for byte.
        assert_eq!(std::fs::read_to_string(&survivor_path).unwrap(), survivor_content);

        // The fresh write must have landed at the next suffix, not clobbered the survivor.
        let next_path = taint_dir.join(format!("taint-{}-1", pid));
        assert!(next_path.exists(), "the next counter suffix must be used when the first candidate exists");

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn);
        assert!(state.recorded.contains(&PathBuf::from("objects/aa/survivor")),
            "the survivor's own recorded path must still be read back");
        assert!(state.recorded.contains(&PathBuf::from("objects/11/22334455")),
            "the fresh write's recorded path must be read back alongside the survivor");
    }

    #[test]
    fn a_taint_file_missing_its_terminator_reads_as_torn_but_keeps_its_parseable_prefix() {
        // Pins the torn-file contract: a file a crash cut off before its terminator reads as
        // torn, but every complete line before the cut point still counts as recorded.
        let _serial = lock_activation();
        activate();

        let root = scratch("torn");
        let forklift = root.join(".forklift");
        let taint_dir = forklift.join(TAINT_DIR_NAME);
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/ab/cdef\n").unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(state.torn, "a taint file without its terminator must read as torn");
        assert!(state.recorded.contains(&PathBuf::from("objects/ab/cdef")),
            "the parseable prefix must still be recorded even though the file is torn");
    }

    #[test]
    fn activation_gates_every_public_entry_point_until_it_is_called() {
        // Pins the all-or-nothing activation contract: before `activate()`, every public entry
        // point is a true no-op (no file written, no gate ever trips, reads see nothing); after
        // it, the exact same calls behave for real.
        let _serial = lock_activation();
        reset_activation_for_test();

        let root = scratch("activation");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("22").join("33445566");

        record_taint(&[object_path.as_path()]).unwrap();
        assert!(!forklift.join(TAINT_DIR_NAME).exists(),
            "an unactivated process must create no taint directory at all");
        assert!(gate_check(&forklift).is_ok(), "the gate must never trip in an unactivated process");
        assert!(read_taints(&forklift).unwrap().recorded.is_empty(),
            "an unactivated read must see nothing recorded");

        activate();

        record_taint(&[object_path.as_path()]).unwrap();
        let gate_error = gate_check(&forklift).expect_err("once activated, a recorded taint must trip the gate");
        assert!(gate_error.contains(GATE_TAINT_MARKER),
            "a gate refusal must contain the machine-recognizable marker, got {:?}", gate_error);
        assert!(!read_taints(&forklift).unwrap().recorded.is_empty(),
            "once activated, the write must actually be readable");

        clear_gate(&forklift);
        assert!(gate_check(&forklift).is_ok(), "clearing the gate must restore a passing check");
    }

    #[test]
    fn gate_key_resolves_identically_for_the_setter_and_the_checker() {
        // Pins gate-key pinning: the same root, however independently it is built by the
        // checking side, must resolve to the same key the setting side used — and a disjoint
        // root must never be affected by an unrelated one's taint.
        let _serial = lock_activation();
        activate();

        let root_a = scratch("gate-key-a");
        let root_b = scratch("gate-key-b");

        let _scope_a = StorageRootScope::enter(&root_a);
        let forklift_a = forklift_root();
        let object_path = forklift_a.join("objects").join("44").join("55667788");
        record_taint(&[object_path.as_path()]).unwrap();

        // Built independently of the scope machinery, but denoting the exact same root.
        let same_root_independently_built = root_a.join(".forklift");
        assert!(gate_check(&same_root_independently_built).is_err(),
            "the gate must trip when checked against the same resolved root, however it was built");

        {
            let _scope_b = StorageRootScope::enter(&root_b);
            let forklift_b = forklift_root();
            assert!(gate_check(&forklift_b).is_ok(), "a disjoint root's gate must be unaffected");
        }

        clear_gate(&forklift_a);
        assert!(gate_check(&forklift_a).is_ok(), "clearing must restore the exact root it was asked to clear");
    }

    #[test]
    fn a_fault_during_the_taints_own_write_leaves_no_taint_file_but_still_gates() {
        // Pins the no-re-entry / double-failure-residual contract: a *returned* failure from the
        // taint's own write leaves no taint file behind (never a rename-over, never partial
        // debris from a controlled failure — only a real crash can do that, see the torn test),
        // fires exactly once (no recursion back into tainting), and the gate — set before the
        // write was even attempted — still stands afterward.
        let _serial = lock_activation();
        activate();

        let root = scratch("fault");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("66").join("77889900");

        let fault = TaintFaultGuard::failing("taint-");
        let result = record_taint(&[object_path.as_path()]);
        assert!(result.is_err(), "an injected failure during the taint's own write must surface as an error");
        assert_eq!(taint_write_attempts().len(), 1,
            "the fault must have fired exactly once — no recursion back into tainting");
        drop(fault);

        let taint_dir = forklift.join(TAINT_DIR_NAME);
        let leftovers: Vec<_> = std::fs::read_dir(&taint_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftovers.is_empty(), "a failed taint write must leave no taint file behind, found {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        assert!(gate_check(&forklift).is_err(),
            "the process must gate itself even though the durable write failed (ordering: gate before write)");
    }

    #[test]
    fn record_taint_skips_silently_when_a_path_is_not_under_the_resolved_root() {
        // Pins the scope-tolerance clause: a path with no relation to the resolved storage root
        // (the shape a bare-path unit test's own paths take) is tolerated, not errored, and
        // nothing is written for it.
        let _serial = lock_activation();
        activate();

        let root = scratch("unresolvable");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let unrelated = std::env::temp_dir()
            .join(format!("forklift-taint-unrelated-{}", std::process::id()));

        let result = record_taint(&[unrelated.as_path()]);
        assert!(result.is_ok(), "an unresolvable root must be tolerated, not errored");
        assert!(!forklift.join(TAINT_DIR_NAME).exists(),
            "nothing may be written when a path cannot be resolved against the storage root");
    }

    #[test]
    fn remove_taint_files_clears_every_file_so_a_fresh_read_sees_nothing() {
        // Pins the future-heal primitive in isolation: after removal, a read sees the fully
        // empty, non-torn state again — the same state an untainted root reads as.
        let _serial = lock_activation();
        activate();

        let root = scratch("remove-taints");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("88").join("99001122");

        record_taint(&[object_path.as_path()]).unwrap();
        assert!(!read_taints(&forklift).unwrap().recorded.is_empty());

        remove_taint_files(&forklift).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(state.recorded.is_empty() && !state.torn,
            "every taint file must be gone after a clean removal");
    }

    #[test]
    fn taint_fault_guard_recording_mode_observes_without_failing() {
        // Pins the fault guard's own "recording" mode (used by stage-2 tests that want to assert
        // a taint write happened without injecting a failure): the write still succeeds, and the
        // attempted path is still observable.
        let _serial = lock_activation();
        activate();

        let root = scratch("fault-recording");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("99").join("00112233");

        let _fault = TaintFaultGuard::recording();
        record_taint(&[object_path.as_path()]).unwrap();
        assert_eq!(taint_write_attempts().len(), 1, "a successful write must still be observed");
    }
}
