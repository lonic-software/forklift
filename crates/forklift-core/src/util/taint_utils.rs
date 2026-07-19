//! The durable taint-record primitive (DESIGN.html §3.1.1): after a write's final names are
//! visible but the directory entries recording them (or their own bytes) could not be proven
//! durable, this module lets the failing write record exactly which final object paths are
//! suspect, so a later heal knows precisely what to redo instead of re-verifying a whole storage
//! root.
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
//! One taint file per failed batch, under `<root>/taint/`, named
//! `taint-<pid>-<nonce_hex>-<counter>` and created with an exclusive create (never a rename-over —
//! see [`record_taint`]'s doc comment for why). `<pid>` is this process's OS pid; `<nonce_hex>` is
//! a random value drawn once, lazily, per process (never per file — see
//! [`taint_filename_nonce`]); `<counter>` is a single process-global counter that only ever
//! increases for as long as this process lives, however many taint files it creates or deletes in
//! between (see [`create_taint_file`]). That combination makes a filename this process has ever
//! used **unique-forever, even after the file itself is later deleted**: the counter never resets,
//! so this process can never reissue a name it already handed out, and the nonce means a
//! *different* process — even one that crashed and was replaced by a new one reusing the exact
//! same `pid` — draws its own counter from an independent, practically-never-colliding space. This
//! is load-bearing, not cosmetic: a heal that deletes taint files by the snapshot it read (see
//! [`TaintState::files`]) is only actually safe if a name, once observed, can never later name a
//! *different* file on disk — otherwise a healer holding a stale snapshot of `taint-P-0` could end
//! up deleting a freshly re-recorded `taint-P-0` that has nothing to do with the one it read (the
//! ABA hole a naive "restart the counter at 0 every call" scheme leaves open).
//!
//! Content is the batch's final object paths, root-relative, one per line, followed by a
//! terminator line (see [`TAINT_TERMINATOR`]). A file whose bytes end with that exact terminator
//! line is complete; anything else — truncated by a crash mid-write, or simply absent — is
//! **torn**: its parseable (fully newline-terminated) prefix still contributes recorded paths,
//! since a real write only ever appends, but the file can no longer prove it named every path the
//! failing batch touched. [`read_taints`] unions every file under the directory and reports `torn`
//! if any one of them is.
//!
//! ## What this module does not do
//!
//! No restage logic and no entry chokepoint (see [`heal_utils`](crate::util::heal_utils) for
//! both), and no closure walk or CLI-visible recovery verb for the cases the automatic entry-heal
//! cannot resolve on its own — that is
//! [`recovery_utils`](crate::util::recovery_utils), built on top of
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted)'s refusal.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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

/// The process-global, never-reset source of the `<counter>` suffix in every taint filename this
/// process creates — see the module doc comment's format section for why "never reset, even
/// across a file's own deletion" (not just "unique among files that currently exist") is the
/// property that makes a snapshot-scoped delete safe. `fetch_add` is the only operation ever
/// performed on this counter, from [`create_taint_file`] alone; ordering among concurrent draws
/// within this process does not matter, only that no two draws ever return the same value, which
/// `Ordering::Relaxed` already guarantees for a plain monotonic counter.
static TAINT_FILENAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// This process's own random nonce component of every taint filename it creates — drawn once,
/// lazily, on first use, and reused for the rest of the process's life (never redrawn per file).
/// Defends against the `<pid>` half of a filename repeating across processes — a crash survivor
/// whose replacement process happens to reuse the exact same OS `pid` — see the module doc
/// comment's format section.
fn taint_filename_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // `RandomState::new()` is itself independently seeded per construction (the standard
        // library draws fresh OS entropy for it); folding in the pid and the current time on top
        // costs nothing and removes any dependence on exactly how that seeding is implemented.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        hasher.finish()
    })
}

/// Exclusive-create the next free `taint-<pid>-<nonce_hex>-<counter>` file under `taint_dir`:
/// `O_CREAT|O_EXCL` via [`std::fs::OpenOptions::create_new`]. `<counter>` is drawn fresh from
/// [`TAINT_FILENAME_COUNTER`] on every attempt — including every retry — never a locally-restarted
/// `0..CAP` index, so a name this process has ever handed out (even one whose file has since been
/// deleted) can never be handed out again: see the module doc comment's format section for why
/// that "unique-forever" property is what makes a snapshot-scoped delete
/// (`remove_taint_files`/`replace_taint_with_remainder`) safe. `EEXIST` on a given candidate (a
/// crash survivor from an earlier process that reused this exact `pid` and independently drew a
/// colliding nonce, or a same-process race on the freshly-drawn counter value some other way)
/// retries with the next freshly-drawn counter value rather than ever renaming over it — see
/// [`record_taint`]'s doc comment: a crash survivor structurally cannot be overwritten by exclusive
/// create, whatever its name.
fn create_taint_file(taint_dir: &Path) -> Result<(std::fs::File, PathBuf), String> {
    let pid = std::process::id();
    let nonce = taint_filename_nonce();

    for _ in 0..TAINT_FILENAME_SANITY_CAP {
        let counter = TAINT_FILENAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = taint_dir.join(format!("taint-{}-{:016x}-{}", pid, nonce, counter));
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

/// Lock-free mirror of [`gate_state`]'s cardinality — exactly the number of roots currently
/// gated. Exists only so [`gate_check`] can take a fast negative without taking the mutex on the
/// overwhelmingly common "no gate is standing anywhere" path (a bulk import/dedup calling this
/// from many threads per object checked would otherwise serialize entirely on that mutex for a
/// belt that is almost never tripped).
///
/// Kept exactly consistent with the set by construction, never independently: [`set_gate`] and
/// [`clear_gate`] adjust this *while still holding the same lock* as their `BTreeSet` mutation,
/// and only when that mutation actually changed membership (`insert`/`remove`'s `bool` return) —
/// never on every call, since both are idempotent (setting an already-set root, or clearing an
/// already-clear one, must not drift the count). No other code path touches this counter.
///
/// **Why this is safe as a fast negative, and only a negative:** the mutex over the `BTreeSet`
/// remains the sole source of truth for *membership* — this counter only ever answers "is the set
/// empty," and only the zero case is trusted without the lock. Zero is reached exactly when the
/// last `remove` that actually removed something ran (or before any `insert` ever has), which by
/// construction is exactly when the set itself is empty; so observing zero here means the
/// `BTreeSet` holds nothing for *any* root, and returning `Ok` without checking `root` in
/// particular is correct — there is nothing in the set it could possibly contain. Whenever the
/// count is nonzero, [`gate_check`] still takes the lock and runs the exact same per-root
/// membership test as before this optimization existed, so a standing gate for one root is never
/// missed and a check for an unrelated root is never falsely tripped — this counter never
/// participates in that decision, only in whether to skip straight to `Ok` first.
///
/// **Ordering:** `Relaxed` on every access. This establishes no happens-before relationship, but
/// none is needed beyond what already existed: `gate_check` already races a concurrent
/// `set_gate`/`clear_gate` under the mutex-only design (a taint set by another thread a moment
/// after this check reads may or may not be observed, exactly as today), so this fast path is not
/// asked to make a promise the code it replaces didn't already decline to make. What it must not
/// do — and does not — is report zero while the set is actually nonempty, and it cannot: the
/// increment/decrement happen inside the same critical section as the mutation they mirror, so
/// every observer of this counter sees a value consistent with *some* linearization point of the
/// set's history, never a stale "empty" left over from before an insert that already completed.
static GATE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Reset the process-global gate — both [`gate_state`]'s `BTreeSet` and its [`GATE_COUNT`] mirror
/// — to guaranteed-empty. Test-only: this module's tests share this same process-global state
/// (like [`ACTIVATED`]) across the whole test binary, and several existing tests intentionally
/// leave a gate standing for their own (uniquely-named) root when they finish, so a test that
/// needs to observe the true fast-path (`GATE_COUNT == 0`) cannot assume that starting from a
/// fresh process. Callers still take [`ACTIVATION_TEST_LOCK`] first, same as every other
/// gate-touching test.
#[cfg(test)]
fn reset_gate_for_test() {
    gate_state().lock().expect("taint gate lock poisoned").clear();
    GATE_COUNT.store(0, Ordering::Relaxed);
}

/// The one resolution [`record_taint`] (setting) and [`gate_check`] (checking) both key the gate
/// through, so the two sides can never drift apart into resolving "the same root" two different
/// ways. Identity today (both callers already hand in an already-resolved [`forklift_root`]
/// value), but named and shared so a future normalization need only change here.
fn gate_key(root: &Path) -> PathBuf {
    root.to_path_buf()
}

fn set_gate(root: &Path) {
    // Idempotent: setting an already-set root must not double-count. `insert`'s `bool` return
    // (true only when the key was actually new) is exactly the signal — increment inside the
    // same critical section as the mutation it mirrors, see [`GATE_COUNT`].
    let inserted = gate_state().lock().expect("taint gate lock poisoned").insert(gate_key(root));
    if inserted {
        GATE_COUNT.fetch_add(1, Ordering::Relaxed);
    }
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

    // Fast negative, lock-free: no gate is standing for *any* root, so none can be standing for
    // `root` in particular — skip the mutex entirely. See [`GATE_COUNT`] for why this is sound
    // as a negative-only shortcut and why `Relaxed` is the right ordering for it. When the count
    // is nonzero, fall through to the exact same locked membership test as before this
    // optimization existed — this branch never itself decides membership.
    if GATE_COUNT.load(Ordering::Relaxed) == 0 {
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
    // Idempotent: clearing an already-clear root must not underflow. `remove`'s `bool` return
    // (true only when a key was actually removed) is exactly the signal — decrement inside the
    // same critical section as the mutation it mirrors, see [`GATE_COUNT`].
    let removed = gate_state().lock().expect("taint gate lock poisoned").remove(&gate_key(root));
    if removed {
        GATE_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
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

    /// The exact absolute paths of every taint file this read actually consumed, captured at read
    /// time — this is the snapshot a later deletion must be scoped to, never re-derived by
    /// scanning the taint directory again at deletion time. Taint files are born concurrently, from
    /// any process, at any time (even a read-only command can self-heal a commit-graph shard and
    /// trip a taint); a heal that deletes "whatever the directory holds right now" instead of
    /// "exactly what I read" can delete a taint a sibling process recorded after this read, losing
    /// a real durability gap forever. [`remove_taint_files`] and
    /// [`replace_taint_with_remainder`] take this set as their deletion scope for exactly that
    /// reason — see their own doc comments.
    pub files: Vec<PathBuf>,
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
        state.files.push(path);
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

/// Delete every path in `files`, tolerating each one already being gone (`NotFound` — a sibling
/// healer racing against the same snapshot legitimately won, which is success here, not error),
/// then fsync `root`'s taint directory so the removals are themselves durable (tolerating the
/// directory itself being absent too — the sync is best-effort durability, not a correctness
/// requirement once every file is already confirmed gone). Never re-scans the directory: `files`
/// — always a caller's own [`TaintState::files`] snapshot — is the *entire* deletion scope, by
/// construction. Shared by [`remove_taint_files`] and [`replace_taint_with_remainder`].
fn delete_snapshot_and_sync(root: &Path, files: &[PathBuf]) -> Result<(), String> {
    for path in files {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Error while removing taint file \"{}\": {}", path.to_string_lossy(), e)),
        }
    }

    let taint_dir = root.join(TAINT_DIR_NAME);
    if !taint_dir.exists() {
        return Ok(());
    }

    file_utils::sync_dir(&taint_dir)
}

/// Delete exactly the taint files named in `files` — a snapshot a caller captured earlier via
/// [`read_taints`]'s [`TaintState::files`], never re-derived by scanning the taint directory again
/// at deletion time — then fsync the directory so the removals are themselves durable.
///
/// **Why snapshot-scoped, not a rescan-and-delete-everything:** taint files are born concurrently,
/// from any process, at any time (even a read-only command can self-heal a commit-graph shard and
/// trip a taint) — the entry-heal that eventually calls this runs lock-free, before the warehouse
/// lock. A version of this function that instead re-read the directory at deletion time would
/// delete every file it found there, including one recorded by a *different* process after this
/// heal's caller snapshotted its work — silently losing a real durability gap forever. Deleting
/// exactly `files` makes that impossible: a taint recorded after the snapshot was taken is not in
/// `files`, so it is never touched here, however concurrently it was born.
///
/// `pub(crate)` — the primitive
/// [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) clears taints with
/// once it has durably restaged every recorded path (see that function's doc comment). Not gated
/// by activation: unlike recording or checking, deleting is only ever reached through a heal an
/// activated process itself drives, so gating here would be redundant, not protective.
///
/// # Returns
/// * `Ok(())`      - Every path in `files` was removed (or already absent — see
///                   [`delete_snapshot_and_sync`]), and the directory sync succeeded.
/// * `Err(String)` - A file in `files` could not be removed for a reason other than already being
///                   gone, or the directory fsync failed.
pub(crate) fn remove_taint_files(root: &Path, files: &[PathBuf]) -> Result<(), String> {
    delete_snapshot_and_sync(root, files)
}

/// Replace exactly the taint files named in `old_files` — a snapshot a caller captured earlier via
/// [`read_taints`]'s [`TaintState::files`] — with a single new one recording exactly `remainder`:
/// the partial-clear primitive the recovery verb ([`recovery_utils`](crate::util::recovery_utils))
/// uses when some recorded paths resolved (restaged, or vanished-and-unreferenced) on a heal
/// attempt and others did not. The taint must afterwards record exactly the unresolved remainder,
/// never the original full set (which would re-report already-resolved paths forever) and never
/// nothing (which would silently drop the paths still genuinely in doubt).
///
/// **Snapshot-scoped, same as [`remove_taint_files`] and for the same reason:** `old_files` is
/// deleted exactly as given, never by re-scanning the taint directory at deletion time — a taint
/// recorded by a concurrent process after `old_files` was snapshotted (this closure walk can run
/// for minutes) must never be swept just because it happened to be sitting in the directory when
/// this function got around to deleting things.
///
/// Crash-safe by construction, the same way [`record_taint`] itself is: the replacement is durably
/// written *first*, via the exact same exclusive-create + fsync + terminator routine
/// [`record_taint`]'s own write uses ([`write_taint_file`] — reused directly, not reimplemented),
/// and only once that succeeds are `old_files` removed. There is therefore never a window where the
/// remainder is unrecorded on disk: a crash before the new file's write completes leaves the
/// original (larger, still-correct, if now stale-in-places) taint set standing; a crash after
/// leaves exactly the new, smaller set (plus, until the next heal's snapshot-scoped cleanup runs,
/// whatever of `old_files` survived that crash). The freshly-written remainder file can never
/// itself appear in `old_files` — the snapshot predates it (it was captured before this call even
/// started), and [`create_taint_file`]'s unique-forever names guarantee no future name can ever
/// collide with a past one — so writing before deleting is safe by construction, not by luck of
/// ordering.
///
/// `remainder` empty is the full-clear case and is delegated to [`remove_taint_files`] directly —
/// no empty taint file is ever written for "nothing left to record."
///
/// Does not touch the in-memory gate ([`gate_check`]) either way: clearing it is the caller's call
/// once it knows whether the *whole* taint (not just this replacement step) is resolved — see
/// [`remove_taint_files`]'s doc comment for the same division of responsibility.
///
/// # Returns
/// * `Ok(())`      - The taint directory now records exactly `remainder` (or is empty/absent, if
///                   `remainder` was empty), and `old_files` are gone (or were already gone — see
///                   [`delete_snapshot_and_sync`]).
/// * `Err(String)` - The new file could not be durably written (`old_files` are left completely
///                   untouched — the original, larger set still stands, never partially deleted
///                   before a replacement is durable), or a file in `old_files` could not be
///                   removed for a reason other than already being gone.
pub(crate) fn replace_taint_with_remainder(
    root: &Path,
    remainder: &BTreeSet<PathBuf>,
    old_files: &[PathBuf],
) -> Result<(), String> {
    if remainder.is_empty() {
        return remove_taint_files(root, old_files);
    }

    write_taint_file(root, remainder)?;

    delete_snapshot_and_sync(root, old_files)
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

    /// The exact taint filename `read_taints`/`record_taint` most recently created under
    /// `taint_dir` — used by tests that need to reason about the concrete `taint-<pid>-<nonce_hex>-
    /// <counter>` shape without hardcoding a counter value, since [`TAINT_FILENAME_COUNTER`] is a
    /// single process-global counter shared (and already advanced) by every other test in this
    /// binary that has recorded a taint before this one ran.
    fn only_taint_filename(taint_dir: &Path) -> String {
        let names: Vec<String> = std::fs::read_dir(taint_dir).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "expected exactly one taint file under {:?}, found {:?}", taint_dir, names);
        names.into_iter().next().unwrap()
    }

    #[test]
    fn a_preexisting_next_candidate_filename_forces_a_retry_and_both_survive() {
        // Pins the O_CREAT|O_EXCL crash-survivor guarantee (memo test 9's shape): a taint file
        // already occupying the exact name a fresh write would generate must never be
        // overwritten — the write takes the next counter suffix instead, and reading unions both.
        let _serial = lock_activation();
        activate();

        let root = scratch("o-excl-survivor");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(TAINT_DIR_NAME);

        // Learn this process's actual next filename shape by recording for real first — the
        // process-global counter's absolute value depends on how many taint files earlier tests in
        // this same test binary have already created, so this test cannot assume it starts at 0.
        let first_object = forklift.join("objects").join("11").join("22334455");
        record_taint(&[first_object.as_path()]).unwrap();
        let first_name = only_taint_filename(&taint_dir);

        // "taint-<pid>-<nonce_hex>-<counter>": split off the counter to learn the exact value this
        // process just drew, so the very next draw (a single global, monotonic counter) can be
        // preempted deterministically.
        let (prefix, counter_str) = first_name.rsplit_once('-').expect("well-formed taint filename");
        let counter: u64 = counter_str.parse().expect("counter suffix must be numeric");

        let survivor_path = taint_dir.join(format!("{}-{}", prefix, counter + 1));
        let survivor_content = "objects/aa/survivor\nEND\n";
        std::fs::write(&survivor_path, survivor_content).unwrap();

        let second_object = forklift.join("objects").join("22").join("33445566");
        record_taint(&[second_object.as_path()]).unwrap();

        // The pre-existing file must be untouched, byte for byte.
        assert_eq!(std::fs::read_to_string(&survivor_path).unwrap(), survivor_content);

        // The fresh write must have skipped the occupied counter and landed at the next one.
        let next_path = taint_dir.join(format!("{}-{}", prefix, counter + 2));
        assert!(next_path.exists(), "the next counter value must be used when the immediate next candidate exists");

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn);
        assert!(state.recorded.contains(&PathBuf::from("objects/aa/survivor")),
            "the survivor's own recorded path must still be read back");
        assert!(state.recorded.contains(&PathBuf::from("objects/22/33445566")),
            "the fresh write's recorded path must be read back alongside the survivor");
    }

    #[test]
    fn a_deleted_taint_filename_is_never_reissued_by_the_same_process() {
        // Pins Part 1's load-bearing property: a filename this process has already used must never
        // be handed out again, even after the file is deleted — the ABA hole a naive "restart the
        // counter at 0 every call" scheme leaves open. Reverting the process-global monotonic
        // counter back to a per-call `0..CAP` local index makes this go red: with the taint
        // directory emptied by the deletion below, the very next call would again probe counter 0
        // first (and succeed immediately, since nothing occupies it anymore), reproducing the
        // exact first filename.
        let _serial = lock_activation();
        activate();

        let root = scratch("unique-forever");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(TAINT_DIR_NAME);

        let object_a = forklift.join("objects").join("aa").join("bb001122");
        record_taint(&[object_a.as_path()]).unwrap();
        let first_name = only_taint_filename(&taint_dir);
        std::fs::remove_file(taint_dir.join(&first_name)).unwrap();

        let object_b = forklift.join("objects").join("cc").join("dd003344");
        record_taint(&[object_b.as_path()]).unwrap();
        let second_name = only_taint_filename(&taint_dir);

        assert_ne!(first_name, second_name,
            "a filename this process has already used must never be reissued, even after deletion");
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
    fn remove_taint_files_clears_every_snapshotted_file_so_a_fresh_read_sees_nothing() {
        // Pins the future-heal primitive in isolation: after removal, a read sees the fully
        // empty, non-torn state again — the same state an untainted root reads as.
        let _serial = lock_activation();
        activate();

        let root = scratch("remove-taints");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("88").join("99001122");

        record_taint(&[object_path.as_path()]).unwrap();
        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.is_empty());

        remove_taint_files(&forklift, &state.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(state.recorded.is_empty() && !state.torn,
            "every snapshotted taint file must be gone after a clean removal");
    }

    #[test]
    fn remove_taint_files_deletes_only_the_snapshot_sparing_a_concurrently_recorded_taint() {
        // Pins Part 3: the core bug this whole fix exists for. `remove_taint_files` must delete
        // exactly the snapshot a caller read earlier, never whatever the directory happens to hold
        // at deletion time. Interleaving: activate -> record T1 -> read_taints (snapshot) ->
        // record F2 (a *different* process's taint, born after the snapshot) -> remove using the
        // stale snapshot. Reverting Part 3 (rescanning the directory at deletion time instead of
        // deleting exactly `files`) deletes F2 too, going red.
        let _serial = lock_activation();
        activate();

        let root = scratch("snapshot-scoped-remove");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(TAINT_DIR_NAME);

        let object_t1 = forklift.join("objects").join("11").join("aa112233");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1, "sanity: exactly T1's file was read");

        // A second taint recorded *after* the snapshot was taken — standing in for a sibling
        // process's heal-triggering write racing this one.
        let object_f2 = forklift.join("objects").join("22").join("bb223344");
        record_taint(&[object_f2.as_path()]).unwrap();

        remove_taint_files(&forklift, &snapshot.files).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(&taint_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(remaining.len(), 1, "exactly the concurrently-recorded taint must survive, found {:?}",
            remaining.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/11/aa112233")),
            "T1, named in the snapshot, must be gone after the snapshot-scoped removal");
        assert!(state.recorded.contains(&PathBuf::from("objects/22/bb223344")),
            "F2, recorded after the snapshot, must survive the removal completely untouched");
    }

    #[test]
    fn remove_taint_files_tolerates_a_snapshotted_file_already_gone() {
        // Pins the ENOENT-tolerance half of Part 3: a sibling healer racing the exact same
        // snapshot may already have removed a file by the time this call gets to it — that is a
        // legitimate race won, not an error. Reverting the `NotFound` tolerance (making any
        // `remove_file` error propagate) turns this red.
        let _serial = lock_activation();
        activate();

        let root = scratch("enoent-tolerant-remove");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_path = forklift.join("objects").join("33").join("cc334455");
        record_taint(&[object_path.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        // Out-of-band removal, standing in for a sibling healer that already won the race on this
        // exact file.
        std::fs::remove_file(&snapshot.files[0]).unwrap();

        let _guard = file_utils::SyncDirFaultGuard::recording();
        let result = remove_taint_files(&forklift, &snapshot.files);
        assert!(result.is_ok(), "a file already gone from the snapshot must not be an error, got {:?}", result);
        assert!(file_utils::sync_dir_attempts().contains(&forklift.join(TAINT_DIR_NAME)),
            "the taint directory must still be synced even when every snapshotted file was already gone");
    }

    #[test]
    fn replace_taint_with_remainder_is_snapshot_scoped_and_spares_a_concurrent_record() {
        // Pins Part 4: the same snapshot-scoped-delete discipline as `remove_taint_files`, but for
        // the partial-clear primitive, whose real caller (the closure walk in `recovery_utils`) can
        // run for minutes between snapshotting and deleting. Reverting Part 4 (rescanning the
        // directory for `old_files` instead of using exactly the given snapshot) deletes the
        // concurrently-recorded F2 too, going red.
        let _serial = lock_activation();
        activate();

        let root = scratch("snapshot-scoped-replace");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("44").join("dd445566");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        let object_f2 = forklift.join("objects").join("55").join("ee556677");
        record_taint(&[object_f2.as_path()]).unwrap();

        let remainder: BTreeSet<PathBuf> = [PathBuf::from("objects/99/still-dangling")].into_iter().collect();
        replace_taint_with_remainder(&forklift, &remainder, &snapshot.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/44/dd445566")),
            "T1, named in the snapshot, must be superseded by the replacement");
        assert!(state.recorded.contains(&PathBuf::from("objects/99/still-dangling")),
            "the new remainder must be recorded");
        assert!(state.recorded.contains(&PathBuf::from("objects/55/ee556677")),
            "F2, recorded after the snapshot, must survive the replacement completely untouched");
    }

    #[test]
    fn replace_taint_with_remainder_empty_remainder_delegates_to_snapshot_scoped_removal() {
        // The empty-`remainder` branch must delegate to the same snapshot-scoped
        // `remove_taint_files`, not a rescan-and-delete-everything shortcut — otherwise a
        // concurrently recorded taint would be lost via this branch even after Part 3/4 fixed the
        // other two call shapes.
        let _serial = lock_activation();
        activate();

        let root = scratch("replace-empty-remainder");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("66").join("ff667788");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        let object_f2 = forklift.join("objects").join("77").join("00778899");
        record_taint(&[object_f2.as_path()]).unwrap();

        replace_taint_with_remainder(&forklift, &BTreeSet::new(), &snapshot.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/66/ff667788")), "T1 must be gone");
        assert!(state.recorded.contains(&PathBuf::from("objects/77/00778899")),
            "F2, recorded after the snapshot, must survive an empty-remainder replacement untouched");
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

    // --- GATE_COUNT fast-path tests --------------------------------------------------------
    //
    // These pin the `gate_check` fast negative added on top of the mutex-only gate: a lock-free
    // `GATE_COUNT` that must stay in exact lockstep with `gate_state`'s `BTreeSet` cardinality.
    // Each test uses `reset_gate_for_test` first since the gate is process-global state shared
    // with every other test in this file (several of which deliberately leave a gate standing),
    // then asserts `GATE_COUNT` directly (not just `gate_check`'s return) where that is the only
    // way to redden a miscount — an over-count is invisible to `gate_check`'s return value,
    // because a nonzero count just falls through to the same exact locked membership test as
    // before this optimization, so it can never itself produce a wrong answer, only waste a lock
    // acquisition. Only an *under*-count (reaching zero while a gate genuinely still stands) can
    // corrupt `gate_check`'s answer, so several of these tests assert the count itself to catch
    // over-count bugs that a same-root `gate_check` call alone would never reveal.

    #[test]
    fn gate_check_takes_the_fast_path_when_no_gate_is_standing() {
        // Item 1: nothing ever gated -> the count is zero and the fast path alone answers Ok.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-none");
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0);
        assert!(gate_check(&root).is_ok(), "an unset root must pass with no gate ever standing");
    }

    #[test]
    fn gate_check_finds_a_standing_gate_for_its_own_root() {
        // Item 2: a nonzero count still takes the lock and finds the exact root that was set.
        // Reddens if the fast-path condition is inverted (e.g. `== 0` flipped to `!= 0`) or if
        // `set_gate` fails to increment on a genuine insert.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-self");
        set_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1, "a fresh set_gate must increment the count exactly once");
        assert!(gate_check(&root).is_err(), "a standing gate for this exact root must still trip");
    }

    #[test]
    fn gate_check_is_unaffected_by_a_standing_gate_on_a_different_root() {
        // Item 3: a nonzero count does not make the fast path (or a sloppy slow path) over-trigger
        // for a root that was never gated — the locked membership test is still exact per-root.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root_a = scratch("gate-count-diff-a");
        let root_b = scratch("gate-count-diff-b");
        set_gate(&root_a);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1);

        assert!(gate_check(&root_b).is_ok(), "a disjoint root must pass even while another root is gated");
        assert!(gate_check(&root_a).is_err(), "the gated root itself must still trip");
    }

    #[test]
    fn clearing_the_only_standing_gate_restores_the_fast_path() {
        // Item 4: set then clear -> the count returns to zero and the fast path answers Ok again.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-clear");
        set_gate(&root);
        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0, "clearing the only standing gate must bring the count back to zero");
        assert!(gate_check(&root).is_ok(), "clearing the only standing gate must restore a passing check");
    }

    #[test]
    fn a_duplicate_set_gate_does_not_inflate_the_count() {
        // Item 5 (double-set half): set_gate is idempotent, so setting an already-set root a
        // second time must not increment again. Reddens if someone implements the increment as
        // "on every set_gate call" rather than gated on `BTreeSet::insert`'s `true` return — that
        // bug leaves the count stuck at 1 after the single real `clear_gate` below, which this
        // test catches by asserting the count directly (gate_check(root) alone would still read
        // Ok in that buggy case, via the slow path finding the root genuinely absent — it would
        // not redden).
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-double-set");
        set_gate(&root);
        set_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1,
            "a duplicate set_gate on an already-set root must not inflate the count");

        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0,
            "the single real clear must bring the count back to zero, proving the duplicate set never incremented it");
        assert!(gate_check(&root).is_ok());
    }

    #[test]
    fn a_duplicate_clear_gate_does_not_underflow_the_count() {
        // Item 5 (double-clear mirror): clear_gate is idempotent, so clearing an already-clear (or
        // never-set) root must never decrement. Reddens if the decrement is not gated on
        // `BTreeSet::remove`'s `true` return — that bug underflows the `AtomicUsize` (wraps to a
        // huge nonzero value under `fetch_sub`, since atomics wrap rather than panic even in
        // debug builds), which this test catches directly via the count assertion.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-double-clear");
        clear_gate(&root);
        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0, "clearing an unset root must never decrement the count");
        assert!(gate_check(&root).is_ok());
    }

    #[test]
    fn clearing_an_unset_root_does_not_erode_a_different_standing_gate() {
        // The dangerous shape of an ungated decrement: clearing a root that was *never* set must
        // not erode the count contributed by a *different*, genuinely standing gate down to zero
        // — that would send `gate_check` for the still-tainted root down the fast path to a false
        // `Ok`. This is the one miscount class that a same-root `gate_check` call cannot catch on
        // its own (see this section's header comment), so it is asserted on both the count and
        // the still-tainted root's `gate_check` result.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root_a = scratch("gate-count-erosion-a");
        let root_b_never_set = scratch("gate-count-erosion-b");
        set_gate(&root_a);
        clear_gate(&root_b_never_set);

        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1,
            "clearing a root that was never gated must not erode a different root's standing gate");
        assert!(gate_check(&root_a).is_err(), "the genuinely standing gate must still trip, not be skipped via the fast path");
    }
}
