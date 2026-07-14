use forklift_core::util::pack_utils::{self, AutoCompaction};

/// Run background object-store maintenance if it is due — the recurring counterpart of
/// `import-git`'s one-shot compaction (git's `gc --auto`): pack accumulated loose objects, or
/// consolidate accumulated packs, so the store stays healthy without the user remembering to
/// `compact`. Opt out with `maintenance.auto = false`.
///
/// It runs **synchronously, under the caller's warehouse lock** (call it right after a
/// mutating command's work, before the lock is released). Synchronous on purpose: the
/// warehouse lock is exclusive and fail-fast, so a detached background compaction holding it
/// would break the user's next command — running here, under the lock we already hold, keeps
/// it correct and race-free. It is threshold-gated so it fires rarely, and best-effort, so a
/// failure never fails the command that just succeeded.
///
/// Never redeltas: `redelta` re-reads and re-compresses the whole live set (CPU-bound, minutes
/// at scale), which is never appropriate for a background trigger a routine command incurs
/// without asking. Only the explicit CLI `compact --all --redelta` can request it.
pub fn run_if_due() {
    match pack_utils::auto_compaction_action().unwrap_or(AutoCompaction::None) {
        AutoCompaction::Incremental => { let _ = pack_utils::compact(false, false); }
        AutoCompaction::Repack => { let _ = pack_utils::compact(true, false); }
        AutoCompaction::None => {}
    }
}
