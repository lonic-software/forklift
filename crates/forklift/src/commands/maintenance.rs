use std::path::Path;
use forklift_core::globals::forklift_root;
use forklift_core::util::pack_utils::{self, AutoCompaction};
use forklift_core::util::taint_utils;

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
/// failure never fails the command that just succeeded — **except** that a failure which left a
/// durability taint standing is not silently swallowed: see below.
///
/// Never redeltas: `redelta` re-reads and re-compresses the whole live set (CPU-bound, minutes
/// at scale), which is never appropriate for a background trigger a routine command incurs
/// without asking. Only the explicit CLI `compact --all --redelta` can request it.
///
/// **A `compact` failure is not automatically quiet.** Most `compact` failures here are exactly
/// the best-effort no-op this function documents (nothing due after all, a transient failure
/// that recorded no taint) and must stay silent — spamming a warning on every benign
/// auto-maintenance hiccup would be worse than the bug this fixes. But `compact`'s own directory-
/// sync failures durably [`taint_utils::record_taint`] the affected paths (`file_utils`'s
/// `sync_dir_or_taint`), and that in-memory gate is set *before* the durable write is even
/// attempted (see `taint_utils::record_taint`'s doc comment) — so, in this exact process, right
/// after a failing `compact` call returns, [`taint_utils::gate_check`] is the cheap, already-
/// established predicate for "did that failure leave a taint standing" (the same belt the
/// storage-scope entry-heal chokepoint gates existence checks on). A standing taint is a real
/// durability event — silently returning here would mean this command reports success (exit 0,
/// a clean `--json` envelope) while a *later, unrelated* command inexplicably exits 21
/// (`durability_taint`), with nothing connecting the two for a human or an agent. So a standing
/// taint is surfaced loudly via [`crate::output::warn_standing_taint`] instead of dropped.
///
/// **This command's own exit code is deliberately left unchanged.** This command's own work
/// already succeeded and returned before auto-maintenance ever runs (`main.rs` only calls this
/// after `dispatch` returns `Ok`), and auto-maintenance is itself a best-effort optimization, not
/// part of what the command promises — flipping this command's exit to signal "attention needed"
/// would conflate "my own work is durable" with "background housekeeping hit a snag," and the
/// taint is enforced regardless: the very next command's entry-heal chokepoint
/// (`heal_utils::heal_if_tainted`) refuses with `durability_taint` (exit 21) until `forklift heal`
/// resolves it. Keeping this command's exit at 0 and surfacing the taint as a loud warning keeps
/// that enforcement intact while never punishing the command that merely triggered maintenance.
pub fn run_if_due() {
    let result = match pack_utils::auto_compaction_action().unwrap_or(AutoCompaction::None) {
        AutoCompaction::Incremental => pack_utils::compact(false, false).map(|_| ()),
        AutoCompaction::Repack => pack_utils::compact(true, false).map(|_| ()),
        AutoCompaction::None => return,
    };

    if let Some(gate_message) = report_maintenance_outcome(result, &forklift_root()) {
        crate::output::warn_standing_taint(&gate_message);
    }
}

/// The decision at the crux of this fix, split out of [`run_if_due`] so it is directly testable
/// without a fault hook in the load-bearing `sync_dir` primitive (which stays exactly as it was —
/// no test backdoor belongs in a durability primitive every guarantee in this product flows
/// through) and without capturing the process's real stderr.
///
/// Most `compact` failures here are exactly the best-effort no-op [`run_if_due`] documents
/// (nothing due after all, a transient failure that recorded no taint) and must stay quiet —
/// spamming a warning on every benign auto-maintenance hiccup would be worse than the bug this
/// fixes. But `compact`'s own directory-sync failures durably [`taint_utils::record_taint`] the
/// affected paths (`file_utils`'s `sync_dir_or_taint`), and that in-memory gate is set *before*
/// the durable write is even attempted (see `taint_utils::record_taint`'s doc comment) — so, in
/// this exact process, right after a failing `compact` call returns, [`taint_utils::gate_check`]
/// is the cheap, already-established predicate for "did that failure leave a taint standing" (the
/// same belt the storage-scope entry-heal chokepoint gates existence checks on). A standing taint
/// is a real durability event — silently returning here would mean the triggering command reports
/// success (exit 0, a clean `--json` envelope) while a *later, unrelated* command inexplicably
/// exits 21 (`durability_taint`), with nothing connecting the two for a human or an agent.
///
/// # Returns
/// * `Some(gate_message)` - `result` failed and a taint is genuinely standing for this root
///                          (`taint_utils::gate_check`'s own refusal string) — the caller must
///                          surface it.
/// * `None`               - `result` succeeded, or it failed but left no taint standing (the
///                          quiet, best-effort case).
fn report_maintenance_outcome(result: Result<(), String>, root: &Path) -> Option<String> {
    if result.is_err() {
        if let Err(gate_message) = taint_utils::gate_check(root) {
            return Some(gate_message);
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use forklift_core::globals::StorageRootScope;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forklift-maintenance-test-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// The crux of this fix, pinned directly: given a `compact` failure that left a *real*
    /// standing taint (recorded exactly the way a genuine `sync_dir_or_taint` failure would —
    /// see `taint_utils::record_taint`'s own doc comment), `report_maintenance_outcome` must
    /// surface it — not swallow it the way the pre-fix `let _ = pack_utils::compact(...)` did.
    /// And when `compact` fails but leaves no taint (the ordinary best-effort no-op case), it
    /// must stay quiet. The end-to-end chain this test does not itself re-drive — a real
    /// directory-sync failure inside `compact`'s own pack-folder sync recording a taint and
    /// setting this same gate — is already covered by `forklift-core`'s
    /// `pack_utils::tests::compact_new_pack_dir_sync_failure_taints_the_new_packs_and_skips_the_destructive_sweep`;
    /// this test picks up exactly where that one leaves off: given a taint that IS standing, does
    /// the caller here notice and say so.
    ///
    /// Mutation: make `report_maintenance_outcome` unconditionally return `None` (the
    /// swallow-equivalent decision) — the standing-taint assertions below go red.
    #[test]
    fn report_maintenance_outcome_surfaces_exactly_when_a_real_taint_is_left_standing() {
        taint_utils::activate();

        let root_dir = scratch("report-maintenance-outcome");
        let _scope = StorageRootScope::enter(&root_dir);
        let root = forklift_root();

        // A `compact` failure that left no taint (e.g. the store lock was momentarily busy, or
        // there was genuinely nothing to do) must stay quiet — the ordinary best-effort case.
        assert_eq!(
            report_maintenance_outcome(Err("transient, non-tainting failure".to_string()), &root),
            None,
            "a compact failure with no standing taint must not be surfaced"
        );

        // A `compact` success must never be surfaced regardless of taint state.
        assert_eq!(report_maintenance_outcome(Ok(()), &root), None);

        // Now record a real taint — the exact durable-write-plus-gate-set
        // `taint_utils::record_taint` performs, the same primitive a genuine
        // `sync_dir_or_taint` failure inside `compact` drives.
        let tainted_path = root.join("objects").join("pack").join("fake.pack");
        taint_utils::record_taint(&[tainted_path.as_path()]).expect("record_taint must succeed");

        let gate_message = taint_utils::gate_check(&root)
            .expect_err("the gate must be standing after record_taint");

        assert_eq!(
            report_maintenance_outcome(Err("directory sync failed".to_string()), &root),
            Some(gate_message),
            "a compact failure with a standing taint must surface the gate's own message"
        );

        // And — mirroring `run_if_due`'s own exit-code contract — a *successful* `compact` must
        // still never surface a taint recorded by an unrelated, earlier failure; only a failing
        // `result` triggers the check at all.
        assert_eq!(report_maintenance_outcome(Ok(()), &root), None,
            "a successful compact result must never be surfaced, even with a taint standing");

        // No cleanup of the gate/taint file: `root` is this test's own unique scratch directory
        // (never reused by another test), so leaving it gated for the rest of this process's life
        // cannot affect anything else — the gate-clear primitive is module-private inside
        // `forklift-core`'s `taint_utils` and not reachable from here regardless (the entry-heal
        // chokepoint, not a test, is the intended caller).
    }
}
