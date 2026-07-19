//! The durable-taint entry-heal (DESIGN.html §3.1.1): the automatic, common-case repair that
//! runs once at every storage-scope entry and either clears a standing taint (see `taint_utils`)
//! or refuses with a machine-coded error naming exactly what it could not resolve.
//!
//! ## Scope
//!
//! This module owns three things: [`restage_object`], the per-path redo-the-write primitive,
//! [`heal_if_tainted`], the automatic chokepoint that drives it over every path a taint
//! recorded, and — `pub(crate)`, for [`recovery_utils`](crate::util::recovery_utils) to build on
//! — [`RestageAttempt`]/[`attempt_restage_all`]/[`finish_clean_heal`], the same restage-and-
//! categorize machinery factored out so the `forklift heal` recovery verb can drive it directly
//! instead of only reading `heal_if_tainted`'s refusal string. This module does **not** own the
//! closure walk over durable ref sources or any remedy (refetch/restage-from-worktree/abandon)
//! for a case it cannot auto-heal — that is `recovery_utils`, built on top of what this module
//! exposes. When entry-heal meets a case it cannot resolve itself (a vanished object, one that
//! fails to read back, one whose content no longer matches its own hash, or a torn taint
//! record), it refuses loudly with [`RefusalCode::DurabilityTaint`] and leaves the taint
//! standing exactly as it found it — `forklift heal`, built on top of this refusal, is what
//! gives it remedies; this module's own automatic pass ends at the refusal.
//!
//! ## The no-self-trip rule
//!
//! [`restage_object`]'s own write (see [`rewrite_dentry`]) and [`heal_if_tainted`]'s own
//! post-restage directory sync never call [`file_utils::write_file_atomically`],
//! [`file_utils::sync_dir_or_taint`], or anything else that runs the taint helper's success
//! re-check (`taint_recheck`, in `file_utils`) — that re-check exists precisely to refuse while a
//! taint is standing, and the taint this module is in the middle of healing is standing by
//! definition until the moment this function clears it. Every write here goes through the same
//! raw primitives `taint_utils::record_taint`'s own write uses: a fresh temp name
//! ([`file_utils::temp_path_for`]), a plain write + `sync_all`, a plain `rename`, and — once every
//! recorded path is confirmed restaged — a raw per-directory fsync
//! ([`file_utils::fsync_dir_data`]) plus (macOS only) one raw device flush
//! ([`file_utils::macos_flush_device_cache`]). None of those consult or set a taint.
//!
//! ## Soundness
//!
//! Restaging never trusts that the recorded path's *existing* bytes were ever durably flushed —
//! it reads them back (verifying the content hash for a loose object; see [`RestageOutcome`]),
//! then writes them fresh to a brand-new temp file, fsyncs that, and renames it over the same
//! final name. Every dirty page in that sequence is new. Combined with the read-back (which turns
//! "the bytes are still there" from an assumption into a verified fact, and would fail loudly if
//! they were not), this is what makes the heal's success meaningful regardless of whether the
//! original write path's own pre-rename fsync actually reached the drive.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::globals::forklift_root;
use crate::util::{file_utils, object_utils, taint_utils};

#[cfg(target_os = "macos")]
use std::os::unix::fs::MetadataExt;

/// The stable `durability_taint` refusal code, re-exported from the typed [`RefusalCode`] — the
/// same convention `load_guard_utils::CODE_INCOMPLETE_LOAD` and its siblings use.
pub const CODE_DURABILITY_TAINT: &str = RefusalCode::DurabilityTaint.as_str();

/// The recovery step every `durability_taint` refusal names — one shared constant so the message
/// (and the generated docs built from it) and the actual `forklift heal` verb
/// ([`recovery_utils`](crate::util::recovery_utils)) never drift apart on the command name.
pub const DURABILITY_TAINT_NEXT_STEP: &str =
    "Run \"forklift heal\" to see what still needs attention and resolve it.";

/// The outcome of attempting to restage one recorded final path — see [`restage_object`].
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RestageOutcome {
    /// The recorded path was present, its content (where verifiable) checked out, and its dentry
    /// was freshly rewritten (new temp, fsynced, renamed over the same final name).
    Restaged,

    /// The recorded path no longer exists (`ENOENT` on the read back) — not an error, a verdict:
    /// a later recovery pass resolves whether that is expected (the object is reachable from
    /// nowhere durable) or a real loss.
    Vanished,

    /// The recorded path exists but its bytes could not be read back — an OS-level (`EIO`-class)
    /// failure on the read itself, distinct from the content simply being wrong (see
    /// [`HashMismatch`](Self::HashMismatch)).
    Unreadable(String),

    /// The recorded path is a loose object (its path has the fan-out shape
    /// [`file_utils::hash_from_object_path`] recognizes) whose read-back content does not address
    /// to the hash its own path encodes — either because the bytes are corrupt, or because they
    /// will not even decompress (folded into this verdict rather than [`Unreadable`](Self::Unreadable):
    /// both mean the content cannot be trusted, and only a decompression failure is not, strictly,
    /// a failure to *read*).
    HashMismatch,
}

/// Restage one recorded final path: read it back, verify what can be verified, and — only on a
/// verified-good read — rewrite a fresh dentry over the same final name. See the module doc
/// comment's no-self-trip rule and soundness section.
///
/// `relative_path` is root-relative, exactly as [`taint_utils::read_taints`] returns it; `root` is
/// the storage root the taint was recorded under (`root.join(relative_path)` is the absolute final
/// path).
///
/// For a loose object (`relative_path` has the shape [`file_utils::hash_from_object_path`]
/// recognizes) the read-back bytes are decompressed and content-hash-verified against the hash the
/// path itself encodes — the same check [`crate::util::object_utils::store_object_bytes`] runs on
/// the way in, run again here on the way out. For anything else this taint schema can record (a
/// pack data/index file, an inventory shard file) no such check is possible — those are not
/// content-addressed, so this restages their bytes exactly as read, verbatim, with no claim about
/// their correctness beyond "these are the bytes that were already on disk." In both cases the
/// rewrite re-establishes the dentry's own durability; it is never what proves a pack's or shard's
/// content valid — that is the job of their own validation layers (pack index verification,
/// inventory rebuild/audit), unaffected by and independent of this restage.
///
/// # Returns
/// * `Ok(RestageOutcome)` - A verdict about the recorded path's content — see [`RestageOutcome`].
///                          Every variant except [`Restaged`](RestageOutcome::Restaged) means
///                          nothing was written; the caller decides what that verdict means for
///                          the taint as a whole.
/// * `Err(String)`        - The path was read back and (for a loose object) verified clean, but
///                          the restage *write* itself (creating/writing/syncing the fresh temp,
///                          or the final rename) failed — an operational failure of the heal's own
///                          machinery, not a verdict about the recorded path's content. Distinct
///                          from every [`RestageOutcome`] variant on purpose: a caller must not
///                          fold "the heal itself misfired" into a content verdict a later
///                          recovery pass would otherwise act on.
pub(crate) fn restage_object(root: &Path, relative_path: &Path) -> Result<RestageOutcome, String> {
    let final_path = root.join(relative_path);

    let bytes = match std::fs::read(&final_path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(RestageOutcome::Vanished),
        Err(e) => return Ok(RestageOutcome::Unreadable(format!(
            "Error while reading \"{}\" back to restage it: {}", final_path.to_string_lossy(), e
        ))),
    };

    if let Some(expected_hash) = file_utils::hash_from_object_path(&final_path) {
        match zstd::stream::decode_all(bytes.as_slice()) {
            Ok(decompressed) => {
                if object_utils::hash_object_bytes(&decompressed) != expected_hash {
                    return Ok(RestageOutcome::HashMismatch);
                }
            }
            Err(_) => return Ok(RestageOutcome::HashMismatch),
        }
    }

    rewrite_dentry(&final_path, &bytes)?;
    Ok(RestageOutcome::Restaged)
}

/// Rewrite `final_path` fresh with `content`: a new temp name in the same directory (see
/// [`file_utils::temp_path_for`]), written, fsynced (unless [`file_utils::fsync_enabled`] is
/// off), then renamed over the exact final name. Never calls
/// [`file_utils::write_file_atomically`] — see the module doc comment's no-self-trip rule. Every
/// dirty page this produces is new: nothing here trusts or reuses any byte the failed write
/// (or anything before it) may have already flushed.
///
/// On any failure, best-effort removes the temp file it created before returning the error — a
/// failed restage leaves no half-written temp behind for a later heal attempt to trip over.
fn rewrite_dentry(final_path: &Path, content: &[u8]) -> Result<(), String> {
    let temp_path = file_utils::temp_path_for(final_path)?;

    let write_result = write_and_sync_temp(&temp_path, content);
    if let Err(e) = write_result {
        let _ = std::fs::remove_file(&temp_path);
        return Err(e);
    }

    std::fs::rename(&temp_path, final_path).map_err(|e| {
        let _ = std::fs::remove_file(&temp_path);
        format!("Error while restaging \"{}\": {}", final_path.to_string_lossy(), e)
    })
}

/// Create, write, and (unless [`file_utils::fsync_enabled`] is off) fsync `temp_path` — the raw
/// primitive [`rewrite_dentry`] builds its fresh temp file from. No cleanup on failure; the caller
/// owns removing a partial temp (see [`rewrite_dentry`]).
fn write_and_sync_temp(temp_path: &Path, content: &[u8]) -> Result<(), String> {
    let mut file = std::fs::File::create(temp_path).map_err(|e| format!(
        "Error while creating a restage temp file \"{}\": {}", temp_path.to_string_lossy(), e
    ))?;

    file.write_all(content).map_err(|e| format!(
        "Error while writing a restage temp file \"{}\": {}", temp_path.to_string_lossy(), e
    ))?;

    if file_utils::fsync_enabled() {
        file.sync_all().map_err(|e| format!(
            "Error while syncing a restage temp file \"{}\": {}", temp_path.to_string_lossy(), e
        ))?;
    }

    Ok(())
}

/// The storage-scope entry-heal: run once per command, before any code path may durably record a
/// reference off the back of an existence check (see `taint_utils`'s module doc comment on the
/// trust-gating invariant). Resolves [`forklift_root`] and checks for a standing taint — absent or
/// empty is the overwhelmingly common case, and costs exactly the one `stat`
/// [`taint_utils::read_taints`] already documents.
///
/// A no-op — `Ok(())`, nothing read, nothing written — unless [`taint_utils::activate`] has been
/// called in this process (inherited from [`taint_utils::read_taints`]'s own activation gate): an
/// unactivated process has no taint to see in the first place.
///
/// When a taint is standing:
/// - **Torn** (`state.torn`, see `taint_utils`'s module doc comment): its recorded scope is a
///   lower bound, not the full set the failing batch touched — restaging only what is recorded
///   could clear the taint while an un-recorded path is still unproven. Refuses immediately,
///   before touching anything; the taint is left exactly as found. Unknown scope is never
///   auto-healed — that is the heavier recovery path's job.
/// - **Otherwise**, every recorded path is restaged (see [`restage_object`]), best-effort across
///   the whole set (every path is attempted regardless of an earlier one's verdict, mirroring
///   `sync_touched_directories`'s own best-effort discipline — a caller learns everything this
///   attempt found, not just the first problem). If every single one restaged cleanly, their
///   distinct parent directories are fsynced and (macOS only) the device cache is flushed once
///   (see [`sync_restaged_parents`]); only then are the taint files removed and the in-memory gate
///   cleared. If *any* path came back [`Vanished`](RestageOutcome::Vanished),
///   [`Unreadable`](RestageOutcome::Unreadable), or [`HashMismatch`](RestageOutcome::HashMismatch),
///   or failed to restage due to an operational error, the whole heal refuses — the taint stands,
///   nothing is cleared, and the refusal lists every affected path under its own verdict. A
///   partial restage (some paths rewritten, others not) is safe to leave as-is: restaging is
///   idempotent, so the next heal attempt simply repeats the already-restaged ones too.
///
/// # Returns
/// * `Ok(())`         - Not activated, no taint was standing, or every recorded path restaged and
///                      the result was made durable — the taint is now cleared.
/// * `Err(CoreError)` - A [`RefusalCode::DurabilityTaint`] refusal: the taint could not be
///                      resolved automatically (torn, or one or more paths in an unhealable
///                      state), or the taint directory itself could not be read. The taint is left
///                      standing in every case.
pub fn heal_if_tainted() -> Result<(), CoreError> {
    let root = forklift_root();
    let state = taint_utils::read_taints(&root).map_err(|e| read_failure_refusal(&root, &e))?;

    if state.torn {
        return Err(torn_refusal(&root));
    }
    if state.recorded.is_empty() {
        return Ok(());
    }

    let attempt = attempt_restage_all(&root, &state.recorded);

    if !attempt.all_clean() {
        return Err(unhealable_refusal(
            &root, &attempt.vanished, &attempt.unreadable, &attempt.hash_mismatch, &attempt.restage_failed,
        ));
    }

    finish_clean_heal(&root, &attempt.restaged, &state.files).map_err(|e| sync_failure_refusal(&root, &e))
}

/// The outcome of restaging every path in a recorded taint set — the shared core of
/// [`heal_if_tainted`]'s automatic entry-heal and the recovery verb's deeper analysis
/// ([`recovery_utils`](crate::util::recovery_utils)), which needs the categorized breakdown
/// `heal_if_tainted` itself only turns straight into a refusal message. Every recorded path is
/// attempted regardless of an earlier one's verdict (best-effort, mirroring
/// `sync_touched_directories`'s own discipline) — a caller learns everything this attempt found,
/// not just the first problem.
#[derive(Default)]
pub(crate) struct RestageAttempt {
    /// Present, verified (where verifiable), and freshly rewritten.
    pub(crate) restaged: BTreeSet<PathBuf>,
    /// Absent on read-back (`ENOENT`) — see [`RestageOutcome::Vanished`].
    pub(crate) vanished: BTreeSet<PathBuf>,
    /// Present but the read-back itself failed — see [`RestageOutcome::Unreadable`], paired with
    /// the read error.
    pub(crate) unreadable: Vec<(PathBuf, String)>,
    /// A loose object whose read-back content does not address to its own path's hash — see
    /// [`RestageOutcome::HashMismatch`].
    pub(crate) hash_mismatch: BTreeSet<PathBuf>,
    /// The content read back and verified clean, but the restage *write* itself failed — an
    /// operational failure of the heal's own machinery, paired with the write error.
    pub(crate) restage_failed: Vec<(PathBuf, String)>,
}

impl RestageAttempt {
    /// Whether every recorded path restaged cleanly — the condition under which
    /// [`heal_if_tainted`] may proceed to [`finish_clean_heal`].
    pub(crate) fn all_clean(&self) -> bool {
        self.vanished.is_empty() && self.unreadable.is_empty()
            && self.hash_mismatch.is_empty() && self.restage_failed.is_empty()
    }
}

/// Attempt [`restage_object`] on every path in `recorded`, categorizing each verdict — see
/// [`RestageAttempt`].
pub(crate) fn attempt_restage_all(root: &Path, recorded: &BTreeSet<PathBuf>) -> RestageAttempt {
    let mut attempt = RestageAttempt::default();

    for relative in recorded {
        match restage_object(root, relative) {
            Ok(RestageOutcome::Restaged) => { attempt.restaged.insert(relative.clone()); }
            Ok(RestageOutcome::Vanished) => { attempt.vanished.insert(relative.clone()); }
            Ok(RestageOutcome::Unreadable(e)) => { attempt.unreadable.push((relative.clone(), e)); }
            Ok(RestageOutcome::HashMismatch) => { attempt.hash_mismatch.insert(relative.clone()); }
            Err(e) => { attempt.restage_failed.push((relative.clone(), e)); }
        }
    }

    attempt
}

/// The common tail of a fully successful heal, shared by [`heal_if_tainted`] (every recorded path
/// restaged) and the recovery verb (every path restaged, or otherwise resolved by its deeper
/// analysis): fsync every distinct parent directory `restaged` touched, plus the macOS device
/// flush, then durably clear the taint and the in-memory gate. Only once this whole sequence
/// succeeds may a caller report the taint fully healed — a failure partway (the sync/flush step)
/// must leave the taint standing exactly as it was, which is why this does not itself remove
/// anything on an `Err`.
///
/// `taint_files` is the exact snapshot the caller's own [`taint_utils::read_taints`] call returned
/// (its `TaintState::files`) — threaded straight through to [`taint_utils::remove_taint_files`],
/// never re-derived by scanning the taint directory here. That snapshot is what makes the removal
/// safe under concurrency: it names precisely the files this heal is entitled to delete, so a
/// taint a sibling process records after the read (taint files are born concurrently, from any
/// process, at any time — even a read-only command can self-heal a commit-graph shard and trip
/// one) is never swept just because it happened to land in the directory before this cleanup ran.
///
/// # Returns
/// * `Ok(())`      - The restaged paths are durable and the taint is now fully cleared.
/// * `Err(String)` - The post-restage sync/flush, or clearing the taint files, failed. The taint
///                   is left standing (whatever `restaged` was, remains unproven durable).
pub(crate) fn finish_clean_heal(
    root: &Path,
    restaged: &BTreeSet<PathBuf>,
    taint_files: &[PathBuf],
) -> Result<(), String> {
    let parents: BTreeSet<PathBuf> = restaged.iter()
        .filter_map(|relative| root.join(relative).parent().map(Path::to_path_buf))
        .collect();

    sync_restaged_parents(&parents)?;

    taint_utils::remove_taint_files(root, taint_files)?;
    taint_utils::clear_gate(root);

    Ok(())
}

/// Fsync every distinct parent directory a successful restage pass touched, then (macOS only)
/// flush the device cache once — the raw, taint-unaware counterpart of
/// `file_utils::sync_touched_directories`, used here instead of `sync_dir_or_taint` for the same
/// no-self-trip reason [`rewrite_dentry`] documents. Best-effort across every directory (mirroring
/// `sync_touched_directories`): every one is attempted even after an earlier failure, and only the
/// first error is returned.
///
/// `pub(crate)` — the recovery verb ([`recovery_utils`](crate::util::recovery_utils)) also calls
/// this directly, standalone from [`finish_clean_heal`]'s bundle, for the partial-clear case: a
/// subset of a taint's recorded paths restaged cleanly while others still need the closure walk,
/// and the restaged subset's durability must never wait on how that deeper analysis turns out —
/// see that module's doc comment.
#[cfg(unix)]
pub(crate) fn sync_restaged_parents(parents: &BTreeSet<PathBuf>) -> Result<(), String> {
    let mut first_error: Option<String> = None;

    for parent in parents {
        if let Err(e) = file_utils::fsync_dir_data(parent) {
            first_error.get_or_insert(e);
        }
    }

    if let Some(e) = first_error {
        return Err(e);
    }

    #[cfg(target_os = "macos")]
    if !parents.is_empty() {
        flush_device_caches(parents)?;
    }

    Ok(())
}

#[cfg(not(unix))]
pub(crate) fn sync_restaged_parents(_parents: &BTreeSet<PathBuf>) -> Result<(), String> {
    // Windows has no directory handle to fsync at all (see `file_utils::sync_dir`'s doc comment)
    // and the taint mechanism never fires there in the first place (`sync_touched_directories` is
    // a no-op on non-Unix) — so there is never a taint for this function to be reached with.
    Ok(())
}

/// The macOS device-cache flush after a clean restage pass. Never depends on any taint file
/// existing — a concurrent heal (in another process, or this very call's own caller, whose
/// snapshot-scoped `taint_utils::remove_taint_files` may run moments after this) can leave the
/// taint directory completely empty by the time this runs, and that must never turn a genuinely
/// successful restage into a spurious failure. Instead: group `parents` by their own `st_dev`
/// (memo-documented anchor rule — `F_FULLFSYNC` only reaches the drive the flushed file
/// descriptor actually sits on) and flush each *distinct* device exactly once, via
/// [`flush_via_anchor`] on one representative parent per device. `parents` are object-store shard
/// directories (`objects/<2-hex>/`) or the `objects/pack/` directory — [`flush_via_anchor`] never
/// creates its anchor file inside one of these; see its own doc comment for why.
#[cfg(target_os = "macos")]
fn flush_device_caches(parents: &BTreeSet<PathBuf>) -> Result<(), String> {
    let mut seen_devices: std::collections::HashSet<u64> = std::collections::HashSet::new();

    for parent in parents {
        let parent_dev = std::fs::metadata(parent).map_err(|e| format!(
            "Error while checking the device of \"{}\": {}", parent.to_string_lossy(), e
        ))?.dev();

        if seen_devices.insert(parent_dev) {
            flush_via_anchor(parent)?;
        }
    }

    Ok(())
}

/// Cross-volume fallback: `parent` sits on a different drive than the taint file the shared flush
/// in [`flush_device_caches`] just used, so that flush never reached it. Creates a tiny anchor
/// file in [`anchor_parent_dir`] — same device as `parent`, but never inside `parent` itself when
/// `parent` is an object-store shard directory (or the `objects/pack/` directory) — flushes *its*
/// drive via the same [`file_utils::macos_flush_device_cache`] primitive, then removes the anchor
/// — the anchor's own dentry durability is irrelevant (it is transient plumbing, never data
/// anything else reads); only the device-wide flush it triggers matters. Any failure here —
/// including the anchor's own create — is a heal failure for `parent`'s drive, exactly like an
/// `EIO` from its directory fsync would be: this function never reports success having flushed
/// only the taint file's own drive.
///
/// The anchor must not land inside `parent` when `parent` is a shard directory: `gc`'s sweep
/// (`gc_utils.rs:54-71`) enumerates every non-`.sig` entry inside `objects/<2-hex>/` as a
/// candidate object (`hash = <shard-prefix> + <file-name>`) and, past the grace period,
/// `remove_file`s whichever of those it does not find live — a `.forklift-heal-anchor-*` file
/// sitting in a shard dir during the tiny window it exists would be misparsed as an object and,
/// worst case, swept out from under a concurrent gc/audit walker's own reads. See
/// [`anchor_parent_dir`] for where it goes instead.
#[cfg(target_os = "macos")]
fn flush_via_anchor(parent: &Path) -> Result<(), String> {
    static ANCHOR_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = ANCHOR_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let anchor = anchor_parent_dir(parent)
        .join(format!(".forklift-heal-anchor-{}-{}", std::process::id(), id));

    let result = std::fs::write(&anchor, []).map_err(|e| format!(
        "Error while creating a cross-volume flush anchor near \"{}\": {}", parent.to_string_lossy(), e
    )).and_then(|()| file_utils::macos_flush_device_cache(&anchor));

    let _ = std::fs::remove_file(&anchor);
    result
}

/// Where [`flush_via_anchor`] creates its anchor file for a given restaged parent: one directory
/// up from `parent` when it has one, else `parent` itself (only possible if `parent` were a root,
/// which a restaged path's parent — always at least `objects/<shard>` — never is in practice).
///
/// This satisfies both constraints an anchor location needs:
/// - **Same device as `parent`**, so `F_FULLFSYNC` on the anchor's fd actually reaches the drive
///   `parent` sits on (the whole point of the flush). Moving up one level stays on the same
///   filesystem here because forklift itself lays out the object-store hierarchy as plain
///   subdirectories of `objects/` (`objects/<2-hex>/`, `objects/pack/`) — never as separate mount
///   points — so there is no mount boundary between a shard dir and its parent to cross.
/// - **Not swept by a concurrent object walker.** `parent` is always either an `objects/<2-hex>/`
///   shard directory or `objects/pack/`; going up one level lands in `objects/` itself. gc's sweep
///   (`gc_utils.rs:57-72`) only *descends into* entries of `objects/` whose name is exactly
///   [`file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS`] hex digits (skipping the pack folder and
///   anything else) — and even before that name check, it skips any entry that is not a directory
///   at all (`gc_utils.rs:60-62`), which a plain anchor file always is. So an anchor sitting
///   directly in `objects/` is invisible to that sweep both as a folder to walk into and as a file
///   to inspect.
///
/// Factored out from [`flush_via_anchor`] so a test can pin the anchor's location without needing
/// to catch a file that exists for only a few microseconds mid-flush.
#[cfg(target_os = "macos")]
fn anchor_parent_dir(parent: &Path) -> &Path {
    parent.parent().unwrap_or(parent)
}

fn torn_refusal(root: &Path) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": its record is itself incomplete (a crash interrupted the write \
            that would have named every affected path), so the full scope of what needs \
            restaging is unknown and cannot be healed automatically. {}",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), DURABILITY_TAINT_NEXT_STEP
        ),
        DURABILITY_TAINT_NEXT_STEP,
    )
}

fn read_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\", but its record could not be read ({}); treating this warehouse as \
            unhealed rather than risk trusting unproven state. {}",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error, DURABILITY_TAINT_NEXT_STEP
        ),
        DURABILITY_TAINT_NEXT_STEP,
    )
}

fn sync_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": every recorded path restaged cleanly, but making that durable \
            failed ({}). {}",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error, DURABILITY_TAINT_NEXT_STEP
        ),
        DURABILITY_TAINT_NEXT_STEP,
    )
}

fn unhealable_refusal(
    root: &Path,
    vanished: &BTreeSet<PathBuf>,
    unreadable: &[(PathBuf, String)],
    hash_mismatch: &BTreeSet<PathBuf>,
    restage_failed: &[(PathBuf, String)],
) -> CoreError {
    let mut parts: Vec<String> = Vec::new();
    if !vanished.is_empty() {
        parts.push(format!("vanished: {}", format_paths(vanished.iter())));
    }
    if !unreadable.is_empty() {
        parts.push(format!("unreadable: {}", format_path_errors(unreadable)));
    }
    if !hash_mismatch.is_empty() {
        parts.push(format!("corrupt (content does not match its own hash): {}", format_paths(hash_mismatch.iter())));
    }
    if !restage_failed.is_empty() {
        parts.push(format!("could not be restaged: {}", format_path_errors(restage_failed)));
    }

    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": {} could not be healed automatically ({}). {}",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(),
            vanished.len() + unreadable.len() + hash_mismatch.len() + restage_failed.len(),
            parts.join("; "), DURABILITY_TAINT_NEXT_STEP
        ),
        DURABILITY_TAINT_NEXT_STEP,
    )
}

fn format_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> String {
    paths.map(|p| format!("\"{}\"", p.to_string_lossy())).collect::<Vec<_>>().join(", ")
}

fn format_path_errors(entries: &[(PathBuf, String)]) -> String {
    entries.iter()
        .map(|(p, e)| format!("\"{}\" ({})", p.to_string_lossy(), e))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;
    use crate::util::taint_utils::{ACTIVATION_TEST_LOCK, reset_activation_for_test};

    #[cfg(unix)]
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    fn lock_activation() -> std::sync::MutexGuard<'static, ()> {
        ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-heal-utils-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Hand-write a complete taint file (crash-before-heal simulation) recording `paths`, exactly
    /// mirroring `taint_utils`'s own on-disk format.
    fn plant_taint(forklift_root: &Path, paths: &[&str]) {
        let taint_dir = forklift_root.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        let mut content = String::new();
        for path in paths {
            content.push_str(path);
            content.push('\n');
        }
        content.push_str("END\n");
        std::fs::write(taint_dir.join("taint-99999-0"), content).unwrap();
    }

    fn write_loose_object(forklift_root: &Path, content: &[u8]) -> (String, PathBuf) {
        let hash = object_utils::hash_object_bytes(content);
        let compressed = zstd::encode_all(content, 0).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let final_path = PathBuf::from(&folder).join(&file_name);
        std::fs::write(&final_path, &compressed).unwrap();
        let relative = final_path.strip_prefix(forklift_root).unwrap().to_path_buf();
        (hash, relative)
    }

    fn assert_durability_taint(error: &CoreError, contains: &[&str]) {
        match error {
            CoreError::Refusal { code, message, next_step } => {
                assert_eq!(*code, RefusalCode::DurabilityTaint, "wrong code, message: {}", message);
                assert_eq!(next_step.as_str(), DURABILITY_TAINT_NEXT_STEP);
                for &needle in contains {
                    assert!(message.contains(needle), "expected {:?} in message: {}", needle, message);
                }
            }
            other => panic!("expected a DurabilityTaint refusal, got {:?}", other),
        }
    }

    #[test]
    fn heal_if_tainted_is_a_cheap_ok_when_nothing_is_recorded() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("empty");
        let _scope = StorageRootScope::enter(&root);

        assert!(heal_if_tainted().is_ok());
    }

    #[test]
    fn heal_if_tainted_is_a_no_op_baseline_when_not_activated() {
        // Pins the activation-gate contract for this function specifically: even a hand-planted,
        // clearly-unhealable taint (a vanished path) must not be seen at all before `activate()`.
        let _serial = lock_activation();
        reset_activation_for_test();

        let root = scratch("unactivated");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        plant_taint(&forklift, &["objects/zz/doesnotexist"]);

        assert!(heal_if_tainted().is_ok(), "an unactivated process must see no taint at all");

        taint_utils::activate();
    }

    #[test]
    fn a_hand_written_complete_taint_naming_a_present_non_object_path_is_healed() {
        // Crash-before-heal simulation over a non-content-addressed recorded path (a pack/shard
        // stand-in): no hash check is possible, so this exercises the "restage bytes as read,
        // verbatim" branch. The mutation this catches: a heal that only fsyncs without rewriting
        // would leave the same inode behind.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("non-object-restage");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let pack_dir = forklift.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let pack_file = pack_dir.join("fake.pack");
        std::fs::write(&pack_file, b"not a real pack, just bytes to restage").unwrap();

        #[cfg(unix)]
        let inode_before = std::fs::metadata(&pack_file).unwrap().ino();

        plant_taint(&forklift, &["objects/pack/fake.pack"]);

        let dir_syncs_before = file_utils::dir_sync_count();
        heal_if_tainted().expect("a present, readable, non-content-addressed path must heal");

        assert!(file_utils::dir_sync_count() > dir_syncs_before,
            "the heal must fsync the restaged file's parent directory");

        #[cfg(unix)]
        {
            let inode_after = std::fs::metadata(&pack_file).unwrap().ino();
            assert_ne!(inode_before, inode_after,
                "mutation check: the dentry must actually be rewritten, not just fsynced");
        }

        assert_eq!(
            std::fs::read(&pack_file).unwrap(), b"not a real pack, just bytes to restage",
            "the bytes themselves must be restaged verbatim"
        );

        assert!(taint_utils::read_taints(&forklift).unwrap().recorded.is_empty());
        assert!(taint_utils::gate_check(&forklift).is_ok());
    }

    #[test]
    fn a_hand_written_complete_taint_naming_a_verified_loose_object_is_healed() {
        // The content-verified branch: a real loose object, correctly addressed by its own path.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("loose-object-restage");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let content = b"a genuine loose object's raw bytes";
        let (_hash, relative) = write_loose_object(&forklift, content);
        let relative_str = relative.to_string_lossy().into_owned();
        plant_taint(&forklift, &[relative_str.as_str()]);

        heal_if_tainted().expect("a verified loose object must heal");

        assert!(taint_utils::read_taints(&forklift).unwrap().recorded.is_empty());
        assert!(taint_utils::gate_check(&forklift).is_ok());

        // The bytes still decompress to the original content — restaging never touches content.
        let restaged = std::fs::read(forklift.join(&relative)).unwrap();
        let decompressed = zstd::stream::decode_all(restaged.as_slice()).unwrap();
        assert_eq!(decompressed, content);
    }

    #[test]
    fn a_torn_taint_refuses_immediately_and_survives() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("torn");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        // No terminator: torn by construction (see `taint_utils`'s own format tests).
        std::fs::write(taint_dir.join("taint-1-0"), b"objects/ab/cdef\n").unwrap();

        let error = heal_if_tainted().unwrap_err();
        assert_durability_taint(&error, &["record is itself incomplete"]);

        assert!(taint_dir.join("taint-1-0").exists(), "a torn taint file must never be touched");
    }

    #[test]
    fn a_vanished_recorded_path_refuses_and_lists_it_under_its_own_verdict() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("vanished");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        plant_taint(&forklift, &["objects/zz/doesnotexist"]);

        let error = heal_if_tainted().unwrap_err();
        assert_durability_taint(&error, &["vanished", "objects/zz/doesnotexist"]);

        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint must survive an unhealable verdict");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_recorded_path_refuses_with_a_distinct_verdict() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("unreadable");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let blocked_dir = forklift.join("objects").join("bl");
        std::fs::create_dir_all(&blocked_dir).unwrap();
        let blocked_file = blocked_dir.join("ocked-content-shape");
        std::fs::write(&blocked_file, b"secret").unwrap();
        std::fs::set_permissions(&blocked_file, std::fs::Permissions::from_mode(0o000)).unwrap();

        plant_taint(&forklift, &["objects/bl/ocked-content-shape"]);

        let error = heal_if_tainted().unwrap_err();
        assert_durability_taint(&error, &["unreadable"]);
        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty());

        std::fs::set_permissions(&blocked_file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    #[test]
    fn a_hash_mismatch_on_a_loose_object_refuses_with_a_distinct_verdict() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("hash-mismatch");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        // A path whose *name* claims one hash, but whose bytes decompress to different content —
        // exactly the shape corruption would leave.
        let real_content = b"the real content this path is named for";
        let (_hash, relative) = write_loose_object(&forklift, real_content);
        let relative_str = relative.to_string_lossy().into_owned();
        let final_path = forklift.join(&relative);
        let corrupted = zstd::encode_all(&b"totally different bytes"[..], 0).unwrap();
        std::fs::write(&final_path, corrupted).unwrap();

        plant_taint(&forklift, &[relative_str.as_str()]);

        let error = heal_if_tainted().unwrap_err();
        assert_durability_taint(&error, &["corrupt", relative_str.as_str()]);
        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty());
    }

    #[test]
    fn two_roots_one_tainted_leaves_the_other_unaffected() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root_a = scratch("two-roots-a");
        let root_b = scratch("two-roots-b");

        {
            let _scope = StorageRootScope::enter(&root_a);
            let forklift_a = forklift_root();
            plant_taint(&forklift_a, &["objects/zz/doesnotexist"]);
            assert!(heal_if_tainted().is_err(), "root A's unhealable taint must refuse");
        }

        {
            let _scope = StorageRootScope::enter(&root_b);
            assert!(heal_if_tainted().is_ok(), "root B, never tainted, must be unaffected");
        }

        // Re-entering root A still finds its taint standing — proves the two scopes never leaked
        // into each other in either direction.
        {
            let _scope = StorageRootScope::enter(&root_a);
            let forklift_a = forklift_root();
            assert!(!taint_utils::read_taints(&forklift_a).unwrap().recorded.is_empty());
        }
    }

    #[test]
    fn restage_object_reports_vanished_for_a_missing_path_directly() {
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("restage-vanished-direct");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let outcome = restage_object(&forklift, Path::new("objects/zz/never-written")).unwrap();
        assert_eq!(outcome, RestageOutcome::Vanished);
    }

    #[test]
    fn restage_object_reports_hash_mismatch_for_undecodable_bytes() {
        // Bytes that are not even valid zstd at all — not just wrongly-hashed content — must still
        // land in `HashMismatch`, never a silent panic or an `Unreadable`/IO-class verdict.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("restage-undecodable");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let dir = forklift.join("objects").join("ff");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f".repeat(30));
        std::fs::write(&path, b"not zstd at all").unwrap();

        let relative = path.strip_prefix(&forklift).unwrap();
        let outcome = restage_object(&forklift, relative).unwrap();
        assert_eq!(outcome, RestageOutcome::HashMismatch);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_flush_survives_every_taint_file_being_emptied_before_the_flush_runs() {
        // Pins Part 5: the post-restage macOS flush must never depend on any taint file still
        // existing. In the real sequence `finish_clean_heal` empties the taint directory *after*
        // this flush runs, but a *sibling* process's own heal (or recovery-verb closure walk) can
        // just as easily have already cleared every taint file under this root by the time this
        // call executes — entry-heal runs lock-free, before the warehouse lock. Reverting Part 5
        // (resolving a taint file via `any_taint_file_path` to flush through) turns this into a
        // spurious "Internal error" refusal even though the actual restage succeeded cleanly.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("macos-flush-emptied-taint-dir");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let content = b"a genuine loose object to restage for the macOS flush test";
        let (_hash, relative) = write_loose_object(&forklift, content);
        plant_taint(&forklift, &[relative.to_string_lossy().as_ref()]);

        let outcome = restage_object(&forklift, &relative).unwrap();
        assert_eq!(outcome, RestageOutcome::Restaged, "sanity: the restage itself must succeed");

        // Empty the taint directory *before* the flush — simulating a concurrent heal that already
        // cleared it by the time this flush step runs.
        let taint_dir = forklift.join("taint");
        for entry in std::fs::read_dir(&taint_dir).unwrap() {
            std::fs::remove_file(entry.unwrap().path()).unwrap();
        }
        assert!(std::fs::read_dir(&taint_dir).unwrap().next().is_none(), "sanity: no taint file remains");

        let parent = forklift.join(&relative).parent().unwrap().to_path_buf();
        let parents: BTreeSet<PathBuf> = [parent].into_iter().collect();

        sync_restaged_parents(&parents)
            .expect("the post-restage flush must succeed even with no taint file left to flush through");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn flush_anchor_lands_in_the_objects_root_never_inside_a_shard_dir() {
        // Pins the anchor-location fix directly: `anchor_parent_dir` must place the anchor one
        // level up from a restaged parent, not inside it. `parent` here mirrors the real shape
        // `flush_device_caches` groups by device — an `objects/<2-hex>/` shard dir — so this is
        // exactly the case gc's sweep (`gc_utils.rs:54-71`) walks and would misparse a foreign
        // file inside as an object. Reverting to `parent` itself (the pre-fix behavior) reddens
        // this: `anchor_parent_dir` would then equal `shard_dir`, tripping the `assert_ne!` and
        // the `assert_eq!` against the objects root both.
        let shard_dir = Path::new("/warehouse/.forklift/objects/ab");
        let anchor_dir = anchor_parent_dir(shard_dir);

        assert_eq!(anchor_dir, Path::new("/warehouse/.forklift/objects"),
            "the anchor must land in the objects/ root, one level up from the shard dir");
        assert_ne!(anchor_dir, shard_dir,
            "the anchor must never be placed inside the shard dir gc's sweep walks");

        // Same check for the other real caller shape: the `objects/pack/` directory.
        let pack_dir = Path::new("/warehouse/.forklift/objects/pack");
        assert_eq!(anchor_parent_dir(pack_dir), Path::new("/warehouse/.forklift/objects"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_flush_after_a_real_restage_leaves_no_foreign_file_in_the_shard_dir() {
        // End-to-end companion to `flush_anchor_lands_in_the_objects_root_never_inside_a_shard_dir`:
        // drives a real restage + flush over a genuine loose object and asserts the shard
        // directory holds only the object itself afterward — never a
        // `.forklift-heal-anchor-*` name a concurrent gc/audit walker could trip over. (The
        // anchor's own lifetime is a handful of microseconds, so this does not by itself catch a
        // mid-flight foreign-file window the way the pure-function test above does; it does
        // confirm the flush leaves the shard dir in the expected clean state.)
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("macos-flush-no-foreign-file");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let content = b"a genuine loose object for the no-foreign-file-in-shard-dir check";
        let (_hash, relative) = write_loose_object(&forklift, content);
        plant_taint(&forklift, &[relative.to_string_lossy().as_ref()]);

        heal_if_tainted().expect("a verified loose object must heal");

        let shard_dir = forklift.join(&relative).parent().unwrap().to_path_buf();
        let entries: Vec<String> = std::fs::read_dir(&shard_dir).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();

        for name in &entries {
            assert!(!name.starts_with(".forklift-heal-anchor-"),
                "a heal anchor must never be left behind in the object shard dir: {:?}", entries);
        }
    }
}
