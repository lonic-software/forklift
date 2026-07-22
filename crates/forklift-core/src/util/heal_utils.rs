//! The durable-taint entry-heal (DESIGN.html §3.1.1): the automatic, common-case repair that
//! runs once at every storage-scope entry and either clears a standing taint (see `taint_utils`)
//! or refuses with a machine-coded error naming exactly what it could not resolve.
//!
//! ## Scope
//!
//! This module owns three things: [`restage_object`], the per-path redo-the-write primitive,
//! [`heal_if_tainted`], the automatic chokepoint that drives it over every path a taint
//! recorded, and — `pub(crate)`, for [`recovery_utils`](crate::util::recovery_utils) to build on
//! — [`RestageAttempt`]/[`attempt_restage_all`]/[`attempt_restage_all_parallel`]/
//! [`finish_clean_heal`], the same restage-and-categorize machinery factored out so the
//! `forklift heal` recovery verb can drive it directly instead of only reading
//! `heal_if_tainted`'s refusal string (the parallel form exists for the §8.3 torn rescan, whose
//! candidate set is directory-driven and can be every loose object in the store — see its own
//! doc comment). This module does **not** own the
//! closure walk over durable ref sources or any remedy (refetch/restage-from-worktree/abandon)
//! for a case it cannot auto-heal — that is `recovery_utils`, built on top of what this module
//! exposes. When entry-heal meets a case it cannot resolve itself (a vanished object, one that
//! fails to read back, one whose content no longer matches its own hash, or a torn taint
//! record), it refuses loudly with [`RefusalCode::DurabilityTaint`] and leaves the taint
//! standing exactly as it found it — `forklift heal`, built on top of this refusal, is what
//! gives it remedies; this module's own automatic pass ends at the refusal.
//!
//! ## The entry-heal safety boundary (I1/I2)
//!
//! [`heal_if_tainted`] is the lock-free chokepoint that runs on (almost) every command, including
//! read-only ones that never take [`crate::util::lock_utils::WarehouseLock`] — so it must never
//! rewrite a recorded path a lock-holding writer could legitimately be deleting or replacing at
//! the same moment (a `stack` consuming a staged inventory shard; a `compact --all` dropping a
//! superseded pack). [`restage_object`] hash-verifies a **loose object** before rewriting it
//! (content-addressed: the bytes it reads back are proven to be the bytes the path's own name
//! claims, so a lock-free rewrite is race-benign — worst case a duplicate loose copy, ordinary gc
//! food); for anything else this taint schema can record — a pack data/index file, an inventory
//! shard `data` file, or any other non-object final path a fire site can taint (a pallet ref
//! pointer, a bay/park/journal/commit-graph/sign/office file, …) — it cannot verify anything and
//! just rewrites the bytes it found, verbatim. That verbatim rewrite is exactly the unsound shape:
//! it can resurrect bytes a concurrent lock-holder legitimately deleted moments earlier.
//!
//! So [`heal_if_tainted`] gates on **content-addressability alone** ([`is_content_addressed`], the
//! same [`file_utils::hash_from_object_path`] check [`restage_object`] already uses to choose its
//! own hash-verify-vs-verbatim branch — one predicate, not a per-path special case): if the
//! standing taint's recorded set contains *any* path this returns `false` for, the whole taint
//! escalates — refused with [`RefusalCode::DurabilityTaint`], directing the operator to
//! `forklift heal` — before [`attempt_restage_all`] is even called, rather than partially
//! restaging the content-addressed subset lock-free and leaving the rest standing. `forklift heal`
//! (`recovery_utils::run`, built on the very same [`attempt_restage_all`]/[`restage_object`]) is
//! unaffected by this gate — it runs under the warehouse lock, so a verbatim rewrite there is
//! serialized against whatever legitimately touched the path, and it restages every shape exactly
//! as before this boundary existed. (Content-addressed, present, restageable paths still heal
//! inline through [`heal_if_tainted`] unchanged — the common case this whole mechanism exists for.)
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
//! ## Pack-aware restage (I4)
//!
//! A vanished **loose-shaped** recorded path (`file_utils::hash_from_object_path` recognizes it) is
//! not necessarily lost: `compact` legitimately repacks a loose object (deleting the loose copy) or
//! `compact --all` drops a now-redundant loose copy once the object survives packed, and either one
//! can race a taint's own restage attempt. Before [`restage_object`] reports
//! [`Vanished`](RestageOutcome::Vanished) for such a path, it checks whether the hash the path's own
//! name encodes actually reads back via [`file_utils::read_object_classified`] — the same
//! classifying-read core the ordinary read path is itself expressed through (DESIGN.html §3.1.1),
//! and the same core the `forklift heal` verb's own vanished-classification is built on
//! (`recovery_utils.rs`'s `truly_missing` filter) — to decide a recorded object is not actually
//! lost. Reusing that one core, rather than a second hand-rolled pack check, is what keeps the two
//! tiers from drifting on what counts as "recovered" for a recorded loose path. **Deliberately not
//! a bare index-membership check**: this site's own history is why — an earlier version trusted
//! pack membership alone, which let a single stale index entry for a record that can never actually
//! be reconstructed (its content unreadable) report [`RecoveredPacked`](RestageOutcome::RecoveredPacked)
//! and fully clear a standing taint (see [`RestageAttempt::all_clean`], which never looks at
//! `recovered_packed` again). A `Verified` classification is sound because it is the exact read a
//! later ordinary access would perform, succeeding; `Unverifiable` (present, but unreadable) is
//! folded into `Vanished` here, same as a genuine absence — this dentry has nothing durable to
//! restage over either way, so the closure walk downstream is the right escalation for it, not a
//! silent clear. A vanished **pack-/shard-shaped** path has no hash to check
//! (`hash_from_object_path` returns `None` for it) and falls straight through to `Vanished`,
//! unchanged — I4 only ever widens what a loose-shaped path can resolve to.
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
use crate::util::{fanout_utils, file_utils, object_utils, taint_utils};

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

    /// The recorded path was a vanished **loose-shaped** path (`ENOENT`), but the hash it encoded
    /// is present in a pack (I4, see the module doc comment) — already durable, content-addressed,
    /// and nothing to rewrite. Counts as clean toward [`RestageAttempt::all_clean`], but — unlike
    /// [`Restaged`](Self::Restaged) — must never be folded into the parent-directory sync set: no
    /// dentry was written at this path for a sync to make durable.
    RecoveredPacked,

    /// The recorded path no longer exists (`ENOENT` on the read back), and — for a loose-shaped
    /// path — the hash it encoded is not present in a pack either (see [`RecoveredPacked`](Self::RecoveredPacked)):
    /// a later recovery pass resolves whether that is expected (the object is reachable from
    /// nowhere durable) or a real loss.
    Vanished,

    /// The recorded path exists but its bytes could not be read back — an OS-level (`EIO`-class)
    /// failure on the read itself, distinct from the content simply being wrong (see
    /// [`HashMismatch`](Self::HashMismatch)).
    Unreadable(String),

    /// The recorded path is a loose object (its path has the fan-out shape
    /// [`file_utils::hash_from_object_path`] recognizes) whose read-back content does not address
    /// to the hash its own path encodes — either because the bytes are corrupt, because they will
    /// not even decompress, or because decoding them would exceed
    /// [`object_utils::MAX_OBJECT_BYTES`] (folded into this verdict rather than
    /// [`Unreadable`](Self::Unreadable): all three mean the content cannot be trusted, and only a
    /// decompression failure or an over-ceiling refusal is not, strictly, a failure to *read*).
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
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // I4 (pack-aware restage, see the module doc comment): a vanished *loose-shaped* path
            // may simply have been repacked (`compact`) or dropped as a now-redundant loose copy
            // (`compact --all`) — the object itself can still be durable, just not at this exact
            // dentry. Check via `file_utils::read_object_classified` — the same classifying-read
            // core the ordinary read path is itself expressed through (DESIGN.html §3.1.1) — before
            // concluding the object is actually gone. This is deliberately the *content-verifying*
            // core, not a bare membership/presence check: `RecoveredPacked` tells this taint's own
            // caller "already durable, nothing to rewrite," and `heal_utils::RestageAttempt::all_clean`
            // (see its own doc comment) treats it as clean without ever looking at it again — a
            // membership-only hit here would let a single stale pack index entry for an
            // unreconstructable record fully clear a standing taint. `Unverifiable` — present, but
            // an ordinary read of it would fail — is folded into `Vanished` here rather than kept
            // distinct: either way this exact dentry has nothing durable to restage over, so the
            // taint schema's existing `Vanished` verdict (and the closure walk it feeds) is exactly
            // the right escalation, not a fourth outcome. A vanished pack-/shard-shaped path has no
            // hash to check (`hash_from_object_path` returns `None`) and falls straight through to
            // `Vanished`, unchanged.
            return match file_utils::hash_from_object_path(&final_path) {
                Some(hash) => match file_utils::read_object_classified(&hash) {
                    Ok(file_utils::StoreReadOutcome::Verified(_)) => Ok(RestageOutcome::RecoveredPacked),
                    Ok(file_utils::StoreReadOutcome::Absent)
                    | Ok(file_utils::StoreReadOutcome::Unverifiable(_)) => Ok(RestageOutcome::Vanished),
                    Err(e) => Ok(RestageOutcome::Unreadable(format!(
                        "Error while checking whether \"{}\" survives in a pack: {}",
                        final_path.to_string_lossy(), e
                    ))),
                },
                None => Ok(RestageOutcome::Vanished),
            };
        }
        Err(e) => return Ok(RestageOutcome::Unreadable(format!(
            "Error while reading \"{}\" back to restage it: {}", final_path.to_string_lossy(), e
        ))),
    };

    if let Some(expected_hash) = file_utils::hash_from_object_path(&final_path) {
        match object_utils::decode_object_bounded(bytes.as_slice()) {
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
/// unactivated process has no taint to see in the first place. Once activated, even the "nothing
/// recorded" case makes one further call, to [`taint_utils::resolve_taints`] — reconciling a
/// stray in-memory gate against the (confirmed-empty) taint directory, per its own companion-fix
/// doc comment, rather than trusting whatever gate state this process happened to inherit.
///
/// When a taint is standing:
/// - **Torn** (`state.torn`, see `taint_utils`'s module doc comment): its recorded scope is a
///   lower bound, not the full set the failing batch touched — restaging only what is recorded
///   could clear the taint while an un-recorded path is still unproven. Refuses immediately,
///   before touching anything; the taint is left exactly as found. Unknown scope is never
///   auto-healed — that is the heavier recovery path's job.
/// - **A non-content-addressed path in the remainder** (the entry-heal safety boundary, I1/I2 —
///   see the module doc comment): checked over the *whole* recorded set via
///   [`is_content_addressed`], before [`attempt_restage_all`] is even called. If any recorded path
///   is not a loose object this call could hash-verify, the entire taint escalates — refuses with
///   [`RefusalCode::DurabilityTaint`], directing the operator to `forklift heal` — rather than
///   restaging the content-addressed subset lock-free and leaving the rest standing. Nothing is
///   read or rewritten in this case; the taint is left exactly as found, for `forklift heal` (which
///   holds the warehouse lock) to resolve in full.
/// - **Otherwise** (every recorded path is content-addressed), every one is restaged (see
///   [`restage_object`]), best-effort across the whole set (every path is attempted regardless of
///   an earlier one's verdict, mirroring `sync_touched_directories`'s own best-effort discipline —
///   a caller learns everything this attempt found, not just the first problem). If every single
///   one restaged cleanly, their distinct parent directories are fsynced and (macOS only) the
///   device cache is flushed once (see [`sync_restaged_parents`]); only then is
///   [`taint_utils::resolve_taints`] called to remove the taint files and sync the in-memory gate.
///   If *any* path came back
///   [`Vanished`](RestageOutcome::Vanished), [`Unreadable`](RestageOutcome::Unreadable), or
///   [`HashMismatch`](RestageOutcome::HashMismatch), or failed to restage due to an operational
///   error, the whole heal refuses — the taint stands, nothing is cleared, and the refusal lists
///   every affected path under its own verdict. A partial restage (some paths rewritten, others
///   not) is safe to leave as-is: restaging is idempotent, so the next heal attempt simply repeats
///   the already-restaged ones too.
///
/// # Returns
/// * `Ok(())`         - Not activated, no taint was standing, or every recorded path restaged and
///                      the result was made durable — the taint is now cleared.
/// * `Err(CoreError)` - A [`RefusalCode::DurabilityTaint`] refusal: the taint could not be
///                      resolved automatically (torn, its remainder contains a path this call
///                      cannot hash-verify, or one or more content-addressed paths are in an
///                      unhealable state), or the taint directory itself could not be read. The
///                      taint is left standing in every case.
pub fn heal_if_tainted() -> Result<(), CoreError> {
    let root = forklift_root();
    let state = taint_utils::read_taints(&root).map_err(|e| read_failure_refusal(&root, &e))?;

    if state.torn {
        return Err(torn_refusal(&root));
    }
    if state.recorded.is_empty() {
        // Companion fix (DESIGN.html §3.1.1): "nothing recorded" says nothing about the gate — a
        // durable write that failed *after* `record_taint`'s own `set_gate` succeeded can leave
        // the gate standing over an empty directory. Route through the same gate-owning primitive
        // rather than returning directly, so that stray gate is reconciled against disk (cleared,
        // since nothing is actually standing) instead of persisting for the rest of this process's
        // life. Empty remainder, empty snapshot: there is no file-level work to do here, only the
        // gate sync.
        taint_utils::resolve_taints(&root, &BTreeSet::new(), &[])
            .map_err(|e| sync_failure_refusal(&root, &e))?;
        return Ok(());
    }

    // I1/I2 (the entry-heal safety boundary — see the module doc comment): a path this call
    // cannot hash-verify must never be rewritten lock-free. Gated on the whole recorded set,
    // before any restage is attempted, so a mixed taint escalates entirely rather than partially
    // restaging the content-addressed subset here and leaving the rest for `forklift heal` to
    // finish — one predicate, checked once, not a per-path branch threaded through the restage
    // loop below.
    let non_content_addressed: BTreeSet<PathBuf> = state.recorded.iter()
        .filter(|path| !is_content_addressed(path))
        .cloned()
        .collect();
    if !non_content_addressed.is_empty() {
        return Err(escalation_refusal(&root, &non_content_addressed));
    }

    let attempt = attempt_restage_all(&root, &state.recorded);

    if !attempt.all_clean() {
        return Err(unhealable_refusal(
            &root, &attempt.vanished, &attempt.unreadable, &attempt.hash_mismatch, &attempt.restage_failed,
        ));
    }

    finish_clean_heal(&root, &attempt.restaged, &state.files).map_err(|e| sync_failure_refusal(&root, &e))
}

/// Whether a taint's recorded final path is content-addressed — a loose object [`restage_object`]
/// can hash-verify before rewriting it (see [`RestageOutcome`]). The one predicate that gates
/// [`heal_if_tainted`]'s restage (I1/I2, see the module doc comment): the same
/// [`file_utils::hash_from_object_path`] check `restage_object` already uses to pick its own
/// hash-verify-vs-verbatim branch, reused here rather than reimplemented, so the two can never
/// drift apart on what counts as "content-addressed."
///
/// `false` covers everything this taint schema can otherwise record: a pack data/index file, an
/// inventory shard `data` file, or any other non-object final path a `write_file_atomically`-backed
/// fire site can taint (a pallet ref pointer, a bay/park/journal/commit-graph/sign/office file, …).
/// [`attempt_restage_all`]/[`restage_object`] themselves are unaffected by this predicate — the
/// `forklift heal` verb calls them directly, under the warehouse lock, for every shape — it is
/// consulted only in [`heal_if_tainted`], which is the one caller that ever runs lock-free.
fn is_content_addressed(relative_path: &Path) -> bool {
    file_utils::hash_from_object_path(relative_path).is_some()
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
    /// A vanished loose-shaped path whose hash survives in a pack (I4) — see
    /// [`RestageOutcome::RecoveredPacked`]. Deliberately kept separate from `restaged`: nothing was
    /// written for these, so they must never be folded into a parent-directory sync set.
    pub(crate) recovered_packed: BTreeSet<PathBuf>,
    /// Absent on read-back (`ENOENT`), and — for a loose-shaped path — its hash is not present in a
    /// pack either — see [`RestageOutcome::Vanished`].
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
    /// Whether every recorded path restaged cleanly (or, for a pack-recovered path, needed no
    /// rewrite at all — I4) — the condition under which [`heal_if_tainted`] may proceed to
    /// [`finish_clean_heal`]. `recovered_packed` is deliberately absent from this check: it is
    /// already a clean outcome by construction (see [`RestageOutcome::RecoveredPacked`]), not a
    /// fourth failure category to gate on.
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
        let outcome = restage_object(root, relative);
        categorize_restage_outcome(&mut attempt, relative.clone(), outcome);
    }

    attempt
}

/// The §8.3 torn-rescan's parallel counterpart of [`attempt_restage_all`]: same categorization,
/// same [`restage_object`] discipline, fanned out over `fanout_utils::fanout_map` instead of a
/// plain serial loop. Meant for the one caller (`recovery_utils`'s directory-driven torn rescan)
/// whose candidate set can be *every* loose object in the store rather than a handful of recorded
/// paths — restaging one path there never touches another's temp file or final name
/// (`rewrite_dentry` names its temp per-path), so the batch is exactly the flat, independent-item
/// shape `fanout_utils` documents itself as being for (and `crate::model::task::TaskExecutor`, built
/// for tree recursion, is not). Every other caller keeps using the serial form — this is an
/// addition, not a replacement.
pub(crate) fn attempt_restage_all_parallel(root: &Path, recorded: &BTreeSet<PathBuf>) -> RestageAttempt {
    let paths: Vec<PathBuf> = recorded.iter().cloned().collect();
    let outcomes = fanout_utils::fanout_map(&paths, |relative| restage_object(root, relative));

    let mut attempt = RestageAttempt::default();
    for (relative, outcome) in paths.into_iter().zip(outcomes) {
        categorize_restage_outcome(&mut attempt, relative, outcome);
    }

    attempt
}

/// The one categorization [`attempt_restage_all`] and [`attempt_restage_all_parallel`] both build
/// their [`RestageAttempt`] from, so the serial and parallel forms can never drift on what a given
/// [`RestageOutcome`] means.
fn categorize_restage_outcome(attempt: &mut RestageAttempt, relative: PathBuf, outcome: Result<RestageOutcome, String>) {
    match outcome {
        Ok(RestageOutcome::Restaged) => { attempt.restaged.insert(relative); }
        Ok(RestageOutcome::RecoveredPacked) => { attempt.recovered_packed.insert(relative); }
        Ok(RestageOutcome::Vanished) => { attempt.vanished.insert(relative); }
        Ok(RestageOutcome::Unreadable(e)) => { attempt.unreadable.push((relative, e)); }
        Ok(RestageOutcome::HashMismatch) => { attempt.hash_mismatch.insert(relative); }
        Err(e) => { attempt.restage_failed.push((relative, e)); }
    }
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
/// (its `TaintState::files`) — threaded straight through to [`taint_utils::resolve_taints`] (called
/// here with an empty remainder — every recorded path restaged cleanly, so nothing survives to
/// re-record), never re-derived by scanning the taint directory here. That snapshot is what makes
/// the removal safe under concurrency: it names precisely the files this heal is entitled to
/// delete, so a taint a sibling process records after the read (taint files are born concurrently,
/// from any process, at any time — even a read-only command can self-heal a commit-graph shard and
/// trip one) is never swept just because it happened to land in the directory before this cleanup
/// ran. The same call also brings the in-memory gate into agreement with whatever `resolve_taints`
/// finds actually standing once that removal lands — see its own doc comment.
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

    taint_utils::resolve_taints(root, &BTreeSet::new(), taint_files)
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
/// snapshot-scoped `taint_utils::resolve_taints` may run moments after this) can leave the
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

/// The I1/I2 escalation refusal: the standing taint's recorded set contains at least one path
/// [`is_content_addressed`] rejects, so [`heal_if_tainted`] refuses without attempting any restage
/// at all — see that function's doc comment and the module doc comment's boundary section.
fn escalation_refusal(root: &Path, non_content_addressed: &BTreeSet<PathBuf>) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": {} recorded path(s) cannot be hash-verified before a lock-free \
            rewrite, so entry-heal cannot restage them safely — a concurrent lock-holding writer \
            could legitimately be deleting or replacing the same path: {}. {}",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), non_content_addressed.len(),
            format_paths(non_content_addressed.iter()), DURABILITY_TAINT_NEXT_STEP
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
    use crate::util::{pack_utils, recovery_utils};

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
    fn entry_heal_escalates_a_present_non_object_path_rather_than_restaging_it_lock_free() {
        // I1/I2 (the entry-heal safety boundary): a non-content-addressed recorded path (a
        // pack/shard stand-in) can never be hash-verified, so `heal_if_tainted` must not rewrite
        // it lock-free — a concurrent lock-holding writer could legitimately be deleting or
        // replacing this exact path (the §1.2 race). It refuses instead, before attempting any
        // restage at all, and leaves the taint (and the file) exactly as found. The `forklift
        // heal` verb still restages this same shape, under the lock — see
        // `attempt_restage_all_still_restages_a_present_non_object_path_verbatim`, the direct
        // successor of what this test used to assert before this boundary existed. Mutation: let
        // `heal_if_tainted` fall through to `attempt_restage_all` regardless of shape → this
        // becomes `Ok`, the inode changes, and the taint clears → every assertion below reddens.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("non-object-escalates");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let pack_dir = forklift.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let pack_file = pack_dir.join("fake.pack");
        std::fs::write(&pack_file, b"not a real pack, just bytes to restage").unwrap();

        #[cfg(unix)]
        let inode_before = std::fs::metadata(&pack_file).unwrap().ino();

        plant_taint(&forklift, &["objects/pack/fake.pack"]);

        let error = heal_if_tainted()
            .expect_err("a non-content-addressed recorded path must never auto-heal lock-free");
        assert_durability_taint(&error, &["objects/pack/fake.pack", "forklift heal"]);

        #[cfg(unix)]
        {
            let inode_after = std::fs::metadata(&pack_file).unwrap().ino();
            assert_eq!(inode_before, inode_after,
                "the dentry must never be rewritten lock-free for a non-content-addressed path");
        }

        assert_eq!(
            std::fs::read(&pack_file).unwrap(), b"not a real pack, just bytes to restage",
            "the bytes must be untouched — escalation never attempts a rewrite"
        );

        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint must survive — escalation never clears it");
    }

    #[test]
    fn attempt_restage_all_still_restages_a_present_non_object_path_verbatim() {
        // The `forklift heal` verb (`recovery_utils::run`) calls `attempt_restage_all` directly,
        // under the warehouse lock, and must still restage every shape exactly as before this
        // slice's entry-heal boundary — this pins that the shared primitive itself is untouched;
        // only `heal_if_tainted`'s own gate changed (see the escalation test above). Mutation: a
        // heal that only fsyncs without rewriting the dentry would leave the same inode behind.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("non-object-restage-verbatim");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let pack_dir = forklift.join("objects").join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let pack_file = pack_dir.join("fake.pack");
        std::fs::write(&pack_file, b"not a real pack, just bytes to restage").unwrap();

        #[cfg(unix)]
        let inode_before = std::fs::metadata(&pack_file).unwrap().ino();

        let recorded: BTreeSet<PathBuf> =
            [PathBuf::from("objects/pack/fake.pack")].into_iter().collect();
        #[cfg(unix)]
        let dir_syncs_before = file_utils::dir_sync_count();
        let attempt = attempt_restage_all(&forklift, &recorded);

        assert!(attempt.all_clean(), "a present, readable, non-content-addressed path restages cleanly");
        assert!(attempt.restaged.contains(Path::new("objects/pack/fake.pack")));

        sync_restaged_parents(&attempt.restaged.iter()
            .filter_map(|relative| forklift.join(relative).parent().map(Path::to_path_buf))
            .collect()
        ).expect("syncing the restaged parent must succeed");

        // `sync_dir` is a documented no-op on non-Unix (`file_utils::sync_dir`), so the fault-
        // recording rig behind `dir_sync_count` never records there — the directory-sync
        // assertion below only holds on Unix. The inode-rewrite check is Unix-only for the same
        // reason `inode_before` above is: `MetadataExt::ino` does not exist on non-Unix targets.
        #[cfg(unix)]
        {
            assert!(file_utils::dir_sync_count() > dir_syncs_before,
                "the restage's parent-directory sync must actually run");

            let inode_after = std::fs::metadata(&pack_file).unwrap().ino();
            assert_ne!(inode_before, inode_after,
                "mutation check: the dentry must actually be rewritten, not just fsynced");
        }

        assert_eq!(
            std::fs::read(&pack_file).unwrap(), b"not a real pack, just bytes to restage",
            "the bytes themselves must be restaged verbatim"
        );
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
        // Trusted bytes: this test's own fixture content, read back to check the restage.
        #[allow(clippy::disallowed_methods)]
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
        // A valid-hex, loose-object-shaped placeholder (not the old "objects/zz/doesnotexist" —
        // "zz" is not hex, so `is_content_addressed` would now escalate it before ever attempting
        // a restage; this fixture must be genuinely content-addressed to exercise the `Vanished`
        // verdict below rather than I1/I2's escalation path).
        plant_taint(&forklift, &["objects/ab/cdef"]);

        let error = heal_if_tainted().unwrap_err();
        assert_durability_taint(&error, &["vanished", "objects/ab/cdef"]);

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

        // A valid-hex, loose-object-shaped path (not the old "objects/bl/ocked-content-shape" —
        // "bl" + "ocked-content-shape" contains non-hex letters, so `is_content_addressed` would
        // now escalate it before ever attempting a read; this fixture must be genuinely
        // content-addressed to exercise the `Unreadable` verdict below).
        let blocked_dir = forklift.join("objects").join("de");
        std::fs::create_dir_all(&blocked_dir).unwrap();
        let blocked_file = blocked_dir.join("adbeef");
        std::fs::write(&blocked_file, b"secret").unwrap();
        std::fs::set_permissions(&blocked_file, std::fs::Permissions::from_mode(0o000)).unwrap();

        plant_taint(&forklift, &["objects/de/adbeef"]);

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
    fn a_vanished_loose_path_recovered_by_a_pack_clears_via_entry_heal() {
        // I4 (memo §8.5 test 5): `compact` legitimately repacks a loose object out from under a
        // standing taint's recorded loose path — deleting the loose copy and landing the exact
        // same bytes, content-addressed, in a pack. Entry-heal must treat that as recovered, not
        // refuse it as `Vanished` (exit 21). Mutation: remove the pack check in `restage_object`'s
        // ENOENT branch (revert to a bare `return Ok(RestageOutcome::Vanished)`) → this taint
        // never clears → `heal_if_tainted` refuses → `.expect` panics → red.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("pack-aware-restage");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let content = b"an object that will be repacked out from under its own taint";
        let (hash, relative) = write_loose_object(&forklift, content);

        // Repack while nothing is tainted yet (`compact` itself is gated on a clean existence
        // check, so it must run *before* the taint below is planted): deletes the loose copy,
        // lands the object in a pack instead.
        let stats = pack_utils::compact(false, false).expect("compact must succeed");
        assert_eq!(stats.objects_packed, 1, "sanity: exactly the one object got packed");
        assert!(!forklift.join(&relative).exists(), "sanity: the loose copy is gone after compact");
        assert!(pack_utils::is_in_packs(&hash).unwrap(), "sanity: the object now lives in a pack");

        // Hand-plant the taint (bypassing `taint_utils::record_taint`, exactly like every other
        // fixture in this module) naming the now-repacked loose path — simulating a taint
        // recorded for this path before the repack made the loose dentry moot.
        let relative_str = relative.to_string_lossy().into_owned();
        plant_taint(&forklift, &[relative_str.as_str()]);

        heal_if_tainted().expect("a vanished loose path recovered by a pack must clear, not refuse");

        assert!(taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint must be fully cleared");
        assert!(taint_utils::gate_check(&forklift).is_ok());
    }

    #[test]
    fn the_heal_verb_also_clears_a_vanished_loose_path_recovered_by_a_pack() {
        // Verb-parity companion (memo §8.5 test 5's second half): the exact same fixture as
        // `a_vanished_loose_path_recovered_by_a_pack_clears_via_entry_heal`, driven through
        // `recovery_utils::run` (the locked verb) instead of `heal_if_tainted` (lock-free entry-
        // heal). Both tiers call `restage_object`, and both classify "vanished, present in a
        // pack" through the same `file_utils::read_object_classified` core — the verb's own
        // `truly_missing` filter (recovery_utils.rs) and entry-heal's I4 check can never drift on
        // this fixture, so this must already pass without any verb-side code change.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("pack-aware-restage-verb-parity");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let content = b"a second object, recovered the same way, through the locked verb this time";
        let (hash, relative) = write_loose_object(&forklift, content);

        // Same ordering constraint as the entry-heal test above: repack before the taint exists.
        pack_utils::compact(false, false).expect("compact must succeed");
        assert!(pack_utils::is_in_packs(&hash).unwrap(), "sanity: the object now lives in a pack");

        let relative_str = relative.to_string_lossy().into_owned();
        plant_taint(&forklift, &[relative_str.as_str()]);

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(recovery_utils::run(None))
            .expect("the heal verb must also clear a pack-recovered path, not refuse it");
        assert!(outcome.was_tainted);
        assert!(outcome.resolved.iter().any(|entry| entry == &relative_str),
            "the pack-recovered path must be reported resolved, never silently dropped: {:?}",
            outcome.resolved);

        assert!(taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint must be fully cleared");
    }

    /// (DESIGN.html §3.1.1 — the classifying-read core, the most dangerous of the three recheck
    /// sites) H's recorded loose taint path is genuinely vanished (nothing was ever written
    /// there); H's *only* copy anywhere is a pack delta record whose base was never stored, so it
    /// can never actually be reconstructed. H is referenced from a real pallet head's tree — same
    /// fixture as `recovery_utils::tests::a_vanished_loose_path_whose_only_pack_record_cannot_reconstruct_is_never_reported_recovered`,
    /// driven through entry-heal (`heal_if_tainted`, lock-free) instead of the locked verb.
    ///
    /// Before this fix, `restage_object`'s own ENOENT branch (I4) asked only whether H was present
    /// in *some* pack's index (`file_utils::raw_object_present`, a bare membership check) — true
    /// here — and reported `RecoveredPacked` without ever reading the record back.
    /// `RestageAttempt::all_clean` deliberately ignores `recovered_packed` (see its own doc
    /// comment), so this single membership hit fully cleared the standing taint from
    /// `heal_if_tainted` — on every command's hot path, lock-free — even though the object can
    /// never actually be served. This test is RED against the pre-fix code (confirmed and reported
    /// separately, not asserted here — the fixture is unchanged either way). After the fix,
    /// `file_utils::read_object_classified` decides `Unverifiable` for H, which `restage_object`
    /// folds into `Vanished`, so `attempt_restage_all` reports it unhealable and `heal_if_tainted`
    /// refuses instead of clearing. Mutation: revert `restage_object`'s ENOENT branch back to
    /// `file_utils::raw_object_present` → this test goes red again (the taint incorrectly clears,
    /// `heal_if_tainted()` returns `Ok`).
    #[test]
    fn a_vanished_loose_path_whose_only_pack_record_cannot_reconstruct_never_clears_via_entry_heal() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;
        use crate::util::pack_utils::TransportPackBuilder;
        use crate::util::pallet_utils;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("196-vanished-unreconstructable-entry-heal");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let h_hash = "c".repeat(64);
        let (folder, file_name) = file_utils::get_path_for_object(&h_hash).unwrap();
        let h_absolute = PathBuf::from(&folder).join(&file_name);
        let h_relative = h_absolute.strip_prefix(&forklift).unwrap().to_path_buf();
        // H is never written loose at all — genuinely vanished from the moment the taint fires.

        // H's only copy: a pack delta record against a base that was never stored anywhere —
        // reconstruction (and so an ordinary read) can never succeed, even though index membership
        // alone says "present." Built *before* anything below stores a loose object: a `store()`
        // dedup check is itself the pack registry's first load in this fresh scratch root, and
        // that load result stays cached (nothing here calls `compact`, the only thing that would
        // invalidate it) — so the pack must exist before the first such load or `is_in_packs`
        // would keep answering from a stale, pre-pack snapshot for the rest of this test.
        let never_stored_base = "d".repeat(64);
        let mut builder = TransportPackBuilder::new(&pack_utils::pack_folder()).unwrap();
        builder.append_delta(&h_hash, &never_stored_base, 10, b"not a real delta payload").unwrap();
        builder.finish().unwrap();
        assert!(pack_utils::is_in_packs(&h_hash).unwrap(), "sanity: H is a pack membership hit");
        assert!(matches!(pack_utils::retrieve_from_packs(&h_hash).unwrap(), pack_utils::PackRetrieval::Failed(_)),
            "sanity: H's record cannot actually be reconstructed");

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new("h.txt".to_string(), h_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("references H".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        let relative_str = h_relative.to_string_lossy().into_owned();
        plant_taint(&forklift, &[relative_str.as_str()]);

        let error = heal_if_tainted().expect_err(
            "a vanished loose path whose only pack record cannot reconstruct must never clear via entry-heal"
        );
        assert_durability_taint(&error, &[relative_str.as_str()]);

        assert!(!taint_utils::read_taints(&forklift).unwrap().recorded.is_empty(),
            "the taint must survive — H was never actually recovered");
    }

    #[test]
    fn restage_object_still_reports_vanished_for_a_missing_pack_shaped_path() {
        // I4 non-regression (memo §8.5 test 6): a vanished `.pack`-shaped recorded path has no
        // hash to check against packs at all (`file_utils::hash_from_object_path` returns `None`
        // for it), so it must fall straight through to `Vanished`, exactly as before I4. (A
        // *present* non-object path already escalates before `restage_object` is ever called for
        // it — see `entry_heal_escalates_a_present_non_object_path_rather_than_restaging_it_lock_free`;
        // this pins `restage_object` itself for the vanished case, so a future change to its
        // ENOENT branch cannot accidentally start "recovering" a pack-shaped path.) Mutation:
        // widen the I4 pack check to run regardless of `hash_from_object_path`'s shape (e.g. hash
        // some derived string for any vanished path) → this could spuriously return
        // `RecoveredPacked` → red.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("pack-shaped-still-vanishes");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let outcome = restage_object(&forklift, Path::new("objects/pack/does-not-exist.pack")).unwrap();
        assert_eq!(outcome, RestageOutcome::Vanished);
    }

    #[test]
    fn attempt_restage_all_parallel_categorizes_identically_to_the_serial_form() {
        // Guards the `categorize_restage_outcome` extraction (shared by `attempt_restage_all` and
        // `attempt_restage_all_parallel`, see both doc comments): a mixed batch — one restaged
        // loose object, one vanished path, one hash-mismatched loose object — must land in the
        // exact same `RestageAttempt` buckets whichever form drives it. Mutation: diverge one
        // form's categorization from the other (e.g. only the parallel form forgets
        // `hash_mismatch`) → the two `RestageAttempt`s compare unequal → red.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("parallel-restage-parity");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let (_hash, good_relative) = write_loose_object(&forklift, b"a genuine loose object, parallel-restaged");

        let mismatch_hash = "3".repeat(64);
        let mismatched_bytes = zstd::encode_all(b"wrong content for this hash".as_slice(), 0).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(&mismatch_hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(PathBuf::from(&folder).join(&file_name), &mismatched_bytes).unwrap();
        let mismatch_relative = PathBuf::from("objects").join(&mismatch_hash[0..2]).join(&mismatch_hash[2..]);

        let vanished_relative = PathBuf::from("objects/ab/deadbeef00");

        let recorded: BTreeSet<PathBuf> = [good_relative.clone(), mismatch_relative.clone(), vanished_relative.clone()]
            .into_iter().collect();

        let serial = attempt_restage_all(&forklift, &recorded);
        let parallel = attempt_restage_all_parallel(&forklift, &recorded);

        assert_eq!(serial.restaged, parallel.restaged);
        assert_eq!(serial.recovered_packed, parallel.recovered_packed);
        assert_eq!(serial.vanished, parallel.vanished);
        assert_eq!(serial.hash_mismatch, parallel.hash_mismatch);
        assert_eq!(serial.unreadable.len(), parallel.unreadable.len());
        assert_eq!(serial.restage_failed.len(), parallel.restage_failed.len());

        assert!(serial.restaged.contains(&good_relative));
        assert!(serial.hash_mismatch.contains(&mismatch_relative));
        assert!(serial.vanished.contains(&vanished_relative));
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

    #[test]
    fn restage_object_reports_hash_mismatch_for_a_correctly_addressed_over_ceiling_giant() {
        // INV-4 (site A wiring): a blob of MAX_OBJECT_BYTES + 1 zero bytes, written at the exact
        // fan-out path its own (uncompressed) content's hash encodes — a genuine, correctly-
        // addressed object, not a wrong-hash bomb. A wrong-hash bomb would report `HashMismatch`
        // under both the bounded and the unbounded (`decode_all`) decode, so it could not
        // discriminate the wiring; a correctly-addressed over-ceiling giant can only pass here
        // because the bounded decode refuses to finish, not because the hash happens to be wrong.
        // Mutation: revert this site's decode call from `object_utils::decode_object_bounded`
        // back to `zstd::stream::decode_all` — the giant fully decodes, its hash matches its own
        // path, and the outcome flips to `Restaged` — red.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("restage-over-ceiling-giant");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let giant = vec![0u8; object_utils::MAX_OBJECT_BYTES + 1];
        let (_hash, relative) = write_loose_object(&forklift, &giant);

        let outcome = restage_object(&forklift, &relative).unwrap();
        assert_eq!(outcome, RestageOutcome::HashMismatch);
    }

    #[test]
    fn finish_clean_heal_leaves_the_gate_standing_when_a_fresh_taint_lands_after_the_snapshot() {
        // TEST B (DESIGN.html §3.1.1): the same false-clear defect as `taint_utils`'s own
        // `resolve_taints_leaves_the_gate_standing_when_a_fresh_taint_lands_after_the_snapshot`
        // (Test A), but pinning the site-level wiring — does `finish_clean_heal` actually route
        // through `resolve_taints` — separately from the primitive's own correctness. Calling
        // `finish_clean_heal` directly with `restaged = BTreeSet::new()` makes
        // `sync_restaged_parents` a no-op, so this needs no real restage fixture. Reverting
        // `finish_clean_heal` back to its own `remove_taint_files` + unconditional `clear_gate`
        // pair turns the gate assertion red.
        let _serial = lock_activation();
        taint_utils::activate();

        let root = scratch("finish-clean-heal-false-clear");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("11").join("aabbccdd");
        taint_utils::record_taint(&[object_t1.as_path()]).unwrap();
        let snapshot = taint_utils::read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1, "sanity: exactly T1's file was read");

        // Standing in for a mid-run store failure that lands after the snapshot but before this
        // call — the same shape `resolve_the_rest`'s own heal-driven refetch can hit.
        let object_t2 = forklift.join("objects").join("22").join("eeff0011");
        taint_utils::record_taint(&[object_t2.as_path()]).unwrap();

        finish_clean_heal(&forklift, &BTreeSet::new(), &snapshot.files)
            .expect("finish_clean_heal must succeed: the durable file work has nothing to fail on");

        assert!(taint_utils::gate_check(&forklift).is_err(),
            "T2's taint file is still standing on disk, so the gate must still be standing too");
        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(state.recorded.contains(&PathBuf::from("objects/22/eeff0011")),
            "T2 must survive finish_clean_heal's snapshot-scoped removal completely untouched");
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
