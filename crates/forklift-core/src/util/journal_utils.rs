use std::collections::BTreeMap;
use std::path::PathBuf;
use serde::{Deserialize, Serialize};
use crate::globals::bay_root;
use crate::util::merge_utils::{self, ConsolidationState};
use crate::util::pallet_utils::{self, PalletRef};
use crate::util::file_utils;

/// The undo journal (§7.8): an append-only list of pre-operation state snapshots kept in
/// the bay, so `undo` can reverse the last state-changing operation — not just the last
/// `stack`. Content addressing makes this cheap: a snapshot is just the pallet refs, the
/// current pallet and any in-progress consolidation (all tiny); the objects they point at
/// are immutable and already stored. The working directory and inventory are the undo
/// command's concern, not the journal's.
const FILE_NAME_JOURNAL: &str = "journal.json";

/// Cap the journal so it cannot grow without bound; the oldest entries fall off (undo is
/// a recent-history convenience, not an audit trail — that is what `history`/`audit` are).
const MAX_ENTRIES: usize = 100;

/// One journaled operation: the mutable warehouse state *before* it ran.
#[derive(Clone, Serialize, Deserialize)]
pub struct JournalEntry {
    /// The command that produced this snapshot (`stack`, `consolidate`, `shift`, …).
    pub op: String,

    /// The current pallet before the operation.
    pub current_pallet: String,

    /// Every pallet ref (user and meta) before the operation, keyed by wire form
    /// (`main`, `@office`) → head hash.
    pub heads: BTreeMap<String, String>,

    /// The same refs immediately *after* the operation ran. Used by `restore_refs` to tell
    /// "the op moved this ref" (safe to roll back) from "something un-journaled moved this
    /// ref later" (must be left alone — see `restore_refs`). `#[serde(default)]` so journal
    /// entries written before this field existed deserialize with it empty; those entries
    /// fall back to the conservative legacy rule.
    #[serde(default)]
    pub heads_after: BTreeMap<String, String>,

    /// Any consolidation in progress before the operation (`their_head`, `their_pallet`).
    pub consolidation: Option<(String, String)>,
}

impl JournalEntry {
    /// Whether two entries describe the same mutable state (ignoring which op produced
    /// them) — used to avoid journaling a no-op command.
    fn same_state(&self, other: &JournalEntry) -> bool {
        self.current_pallet == other.current_pallet
            && self.heads == other.heads
            && self.consolidation == other.consolidation
    }
}

/// Capture the current mutable state as a would-be journal entry for `op`.
///
/// # Returns
/// * `Ok(JournalEntry)` - The snapshot.
/// * `Err(String)`      - If the refs or consolidation state could not be read.
pub fn capture(op: &str) -> Result<JournalEntry, String> {
    let current_pallet = pallet_utils::get_current_pallet_name()?;

    let heads = pallet_utils::all_pallet_refs()?
        .into_iter()
        .map(|(reference, head)| (reference.to_wire(), head))
        .collect();

    let consolidation = merge_utils::read_consolidation_state()?
        .map(|state| (state.their_head, state.their_pallet));

    // Populated later by `push_if_changed` once the operation has actually run; a bare
    // `capture` (e.g. this function's own tests, or the "pre" snapshot before the op
    // executes) has no "after" to report yet.
    Ok(JournalEntry { op: op.to_string(), current_pallet, heads, heads_after: BTreeMap::new(), consolidation })
}

/// Append `pre` to the journal, but only if the state actually changed since it was
/// captured (so a command that turned out to be a no-op leaves no undo entry). Best-effort
/// by contract: the caller ignores the result, because a journaling hiccup must never fail
/// a command that already succeeded.
///
/// # Arguments
/// * `pre` - The pre-operation snapshot captured by [`capture`].
pub fn push_if_changed(pre: JournalEntry) -> Result<(), String> {
    // The undo journal reverses an operation by restoring refs, which cannot un-create a
    // pallet's very first parcel (that would need deleting the ref). So a stack onto an
    // unborn pallet is not journaled — `undo` falls back to its classic "nothing to undo
    // at the first parcel" handling there.
    if !pre.heads.contains_key(&pre.current_pallet) {
        return Ok(());
    }

    let now = capture(&pre.op)?;

    if pre.same_state(&now) {
        return Ok(());
    }

    // Record where every ref landed once the operation actually ran, so `restore_refs` can
    // later tell "the op moved this" from "something un-journaled moved this afterwards".
    let mut pre = pre;
    pre.heads_after = now.heads;

    let mut entries = read();
    entries.push(pre);

    let len = entries.len();
    if len > MAX_ENTRIES {
        entries.drain(0..len - MAX_ENTRIES);
    }

    write(&entries)
}

/// Remove and return the most recent journal entry.
///
/// # Returns
/// * `Ok(Some(JournalEntry))` - The popped entry.
/// * `Ok(None)`               - If the journal is empty.
/// * `Err(String)`            - If the journal could not be rewritten.
pub fn pop() -> Result<Option<JournalEntry>, String> {
    let mut entries = read();
    let popped = entries.pop();

    if popped.is_some() {
        write(&entries)?;
    }

    Ok(popped)
}

/// Restore the pallet refs, current pallet and consolidation state from an entry. The
/// working directory and inventory are left untouched — the `undo` command decides whether
/// to re-materialize (a soft reset keeps them; reversing a `shift` re-materializes).
///
/// Only the journaled operation itself is undone — never anything that happened after it.
/// Commands outside the journal (office/meta-pallet mutations such as a key retirement, and
/// anything else `journal_op` excludes) can run between the journaled op and this `undo`;
/// a ref they touched must not be rolled back just because it was also in this snapshot.
/// So each ref is restored only if it still sits exactly where the journaled op left it:
///
/// * `heads_after` known (the normal case) and unchanged since → safe, restore `pre_head`.
/// * `heads_after` known but the ref has since moved → something un-journaled advanced it;
///   leave it alone.
/// * the ref no longer exists → never resurrect a deleted ref.
/// * `heads_after` missing entirely (a journal entry written before this field existed) →
///   conservative fallback: restore user pallets as before, but never touch a meta-pallet
///   ref (`@office` and friends) we have no "after" evidence for.
///
/// # Arguments
/// * `entry` - The snapshot to restore.
pub fn restore_refs(entry: &JournalEntry) -> Result<(), String> {
    for (wire, pre_head) in &entry.heads {
        let reference = PalletRef::parse(wire)?;

        let current_head = pallet_utils::get_pallet_head_in(reference.namespace, &reference.name)?;
        let Some(current_head) = current_head else {
            // Deleted since the op ran (or never born) — nothing to roll back to.
            continue;
        };

        let should_restore = match entry.heads_after.get(wire) {
            Some(post_head) => &current_head == post_head,
            None => reference.namespace == pallet_utils::PalletNamespace::User,
        };

        if should_restore {
            pallet_utils::set_pallet_head_in(reference.namespace, &reference.name, pre_head)?;
        }
    }

    pallet_utils::set_current_pallet_name(&entry.current_pallet)?;

    match &entry.consolidation {
        Some((their_head, their_pallet)) => merge_utils::write_consolidation_state(
            &ConsolidationState { their_head: their_head.clone(), their_pallet: their_pallet.clone() }
        )?,
        None => merge_utils::clear_consolidation_state()?,
    }

    Ok(())
}

/// The journal file path (bay-local).
fn path() -> PathBuf {
    bay_root().join(FILE_NAME_JOURNAL)
}

/// Read the journal, tolerating a missing or corrupt file by treating it as empty — a
/// broken journal must never block a command.
fn read() -> Vec<JournalEntry> {
    let path = path();

    if !path.exists() {
        return Vec::new();
    }

    std::fs::read_to_string(&path)
        .ok()
        .and_then(|content| serde_json::from_str(&content).ok())
        .unwrap_or_default()
}

/// Write the journal atomically.
fn write(entries: &[JournalEntry]) -> Result<(), String> {
    let json = serde_json::to_string(entries)
        .map_err(|e| format!("Error while serializing the undo journal: {}", e))?;

    file_utils::write_file_atomically(&path(), json.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    /// A fresh warehouse root for one test (its own `.forklift` folder exists, but
    /// nothing is prepared beyond that — the journal only ever needs `bay_root()` to
    /// resolve, which it does the moment `.forklift` exists).
    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-journal-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Builds a legacy-shaped entry (no `heads_after`) — most existing tests below predate
    /// that field and exercise the pre-upgrade fallback rule in `restore_refs`.
    fn entry(op: &str, pallet: &str, head: &str) -> JournalEntry {
        let mut heads = BTreeMap::new();
        heads.insert(pallet.to_string(), head.to_string());

        JournalEntry {
            op: op.to_string(),
            current_pallet: pallet.to_string(),
            heads,
            heads_after: BTreeMap::new(),
            consolidation: None,
        }
    }

    #[test]
    fn capture_reflects_a_bare_warehouses_default_state() {
        let _scratch = Scratch::new("capture-bare");

        let snapshot = capture("stack").unwrap();
        assert_eq!(snapshot.current_pallet, crate::util::pallet_utils::DEFAULT_PALLET_NAME);
        assert!(snapshot.heads.is_empty(), "an unborn warehouse has no pallet refs yet");
        assert!(snapshot.consolidation.is_none());
    }

    #[test]
    fn push_if_changed_is_a_noop_when_the_current_pallet_is_unborn() {
        let _scratch = Scratch::new("push-unborn");

        // The default pallet has never been stacked to, so `heads` cannot contain it —
        // exactly the "stack onto an unborn pallet" case `push_if_changed` opts out of.
        let pre = capture("stack").unwrap();
        push_if_changed(pre).unwrap();

        assert!(pop().unwrap().is_none(), "an unborn-pallet snapshot must never be journaled");
    }

    #[test]
    fn push_if_changed_records_a_real_state_change() {
        let _scratch = Scratch::new("push-real-change");

        pallet_utils::set_pallet_head("main", &"a".repeat(64)).unwrap();
        let pre = capture("stack").unwrap();

        // The operation itself: the pallet head actually moves.
        pallet_utils::set_pallet_head("main", &"b".repeat(64)).unwrap();
        push_if_changed(pre).unwrap();

        let popped = pop().unwrap().expect("a real change must be journaled");
        assert_eq!(popped.heads.get("main"), Some(&"a".repeat(64)));
        assert!(pop().unwrap().is_none(), "popping removes the entry");
    }

    #[test]
    fn push_if_changed_skips_a_true_noop() {
        let _scratch = Scratch::new("push-noop");

        pallet_utils::set_pallet_head("main", &"a".repeat(64)).unwrap();
        let pre = capture("stack").unwrap();

        // Nothing actually changed between capturing `pre` and calling `push_if_changed`.
        push_if_changed(pre).unwrap();

        assert!(pop().unwrap().is_none(), "a no-op command must leave no undo entry");
    }

    #[test]
    fn pop_returns_entries_most_recent_first() {
        let _scratch = Scratch::new("pop-lifo");

        push_if_changed(entry("stack", "main", "1".repeat(64).as_str())).unwrap();
        push_if_changed(entry("shift", "main", "2".repeat(64).as_str())).unwrap();

        assert_eq!(pop().unwrap().unwrap().op, "shift");
        assert_eq!(pop().unwrap().unwrap().op, "stack");
        assert!(pop().unwrap().is_none());
    }

    #[test]
    fn restore_refs_puts_pallet_heads_current_pallet_and_consolidation_back() {
        let _scratch = Scratch::new("restore-refs");

        // Both refs currently sit exactly where the journaled op left them (`heads_after`),
        // so `restore_refs` is free to roll them back to their pre-op values.
        pallet_utils::set_pallet_head("main", &"2".repeat(64)).unwrap();
        pallet_utils::set_meta_pallet_head("office", &"4".repeat(64)).unwrap();

        let mut heads = BTreeMap::new();
        heads.insert("main".to_string(), "a".repeat(64));
        heads.insert("@office".to_string(), "b".repeat(64));

        let mut heads_after = BTreeMap::new();
        heads_after.insert("main".to_string(), "2".repeat(64));
        heads_after.insert("@office".to_string(), "4".repeat(64));

        let restored = JournalEntry {
            op: "consolidate".to_string(),
            current_pallet: "feature".to_string(),
            heads,
            heads_after,
            consolidation: Some(("c".repeat(64), "their-pallet".to_string())),
        };

        restore_refs(&restored).unwrap();

        assert_eq!(
            pallet_utils::get_pallet_head("main").unwrap(),
            Some("a".repeat(64))
        );
        assert_eq!(
            pallet_utils::get_meta_pallet_head("office").unwrap(),
            Some("b".repeat(64))
        );
        assert_eq!(pallet_utils::get_current_pallet_name().unwrap(), "feature");

        let consolidation = merge_utils::read_consolidation_state().unwrap().unwrap();
        assert_eq!(consolidation.their_head, "c".repeat(64));
        assert_eq!(consolidation.their_pallet, "their-pallet");
    }

    #[test]
    fn restore_refs_leaves_a_ref_untouched_if_it_moved_after_the_journaled_op() {
        let _scratch = Scratch::new("restore-refs-moved");

        // "main" sits exactly where the op left it: safe to restore.
        pallet_utils::set_pallet_head("main", &"2".repeat(64)).unwrap();
        // "feature" has since moved on to a third value — some un-journaled operation ran
        // between the journaled op and this undo. It must be left exactly where it is.
        pallet_utils::set_pallet_head("feature", &"5".repeat(64)).unwrap();

        let mut heads = BTreeMap::new();
        heads.insert("main".to_string(), "1".repeat(64));
        heads.insert("feature".to_string(), "3".repeat(64));

        let mut heads_after = BTreeMap::new();
        heads_after.insert("main".to_string(), "2".repeat(64));
        heads_after.insert("feature".to_string(), "4".repeat(64));

        let restored = JournalEntry {
            op: "stack".to_string(),
            current_pallet: "main".to_string(),
            heads,
            heads_after,
            consolidation: None,
        };

        restore_refs(&restored).unwrap();

        assert_eq!(
            pallet_utils::get_pallet_head("main").unwrap(),
            Some("1".repeat(64)),
            "unmoved ref must be restored"
        );
        assert_eq!(
            pallet_utils::get_pallet_head("feature").unwrap(),
            Some("5".repeat(64)),
            "a ref moved by a later un-journaled op must be left untouched"
        );
    }

    #[test]
    fn restore_refs_on_a_legacy_entry_skips_meta_refs_but_restores_user_refs() {
        let _scratch = Scratch::new("restore-refs-legacy");

        pallet_utils::set_pallet_head("main", &"9".repeat(64)).unwrap();
        pallet_utils::set_meta_pallet_head("office", &"8".repeat(64)).unwrap();

        let mut heads = BTreeMap::new();
        heads.insert("main".to_string(), "1".repeat(64));
        heads.insert("@office".to_string(), "2".repeat(64));

        // A pre-upgrade journal entry: no `heads_after` at all.
        let restored = JournalEntry {
            op: "stack".to_string(),
            current_pallet: "main".to_string(),
            heads,
            heads_after: BTreeMap::new(),
            consolidation: None,
        };

        restore_refs(&restored).unwrap();

        assert_eq!(
            pallet_utils::get_pallet_head("main").unwrap(),
            Some("1".repeat(64)),
            "a legacy entry still restores user-namespace refs"
        );
        assert_eq!(
            pallet_utils::get_meta_pallet_head("office").unwrap(),
            Some("8".repeat(64)),
            "a legacy entry must never move a meta-pallet ref like @office"
        );
    }

    #[test]
    fn push_if_changed_populates_heads_after() {
        let _scratch = Scratch::new("push-heads-after");

        pallet_utils::set_pallet_head("main", &"a".repeat(64)).unwrap();
        let pre = capture("stack").unwrap();

        // The operation itself: the pallet head moves.
        pallet_utils::set_pallet_head("main", &"b".repeat(64)).unwrap();
        push_if_changed(pre).unwrap();

        let popped = pop().unwrap().expect("a real change must be journaled");
        assert_eq!(popped.heads.get("main"), Some(&"a".repeat(64)));
        assert_eq!(
            popped.heads_after.get("main"),
            Some(&"b".repeat(64)),
            "heads_after must record where the ref landed once the op ran"
        );
    }

    #[test]
    fn restore_refs_clears_consolidation_when_the_entry_has_none() {
        let _scratch = Scratch::new("restore-clears-consolidation");

        merge_utils::write_consolidation_state(&ConsolidationState {
            their_head: "d".repeat(64),
            their_pallet: "stray".to_string(),
        }).unwrap();

        restore_refs(&entry("stack", "main", &"a".repeat(64))).unwrap();

        assert!(merge_utils::read_consolidation_state().unwrap().is_none());
    }

    #[test]
    fn a_corrupt_journal_file_is_tolerated_as_empty() {
        let _scratch = Scratch::new("corrupt-journal");

        std::fs::create_dir_all(path().parent().unwrap()).unwrap();
        std::fs::write(path(), b"not valid json at all").unwrap();

        assert!(read().is_empty(), "a corrupt journal must be treated as empty, never panic");
        assert!(pop().unwrap().is_none());
    }

    #[test]
    fn a_truncated_journal_file_is_tolerated_as_empty() {
        let _scratch = Scratch::new("truncated-journal");

        push_if_changed(entry("stack", "main", &"a".repeat(64))).unwrap();

        // Truncate the well-formed JSON array the previous push just wrote.
        let full = std::fs::read_to_string(path()).unwrap();
        let truncated = &full[..full.len() / 2];
        std::fs::write(path(), truncated).unwrap();

        assert!(read().is_empty(), "a truncated journal must be tolerated, not panic");
    }

    #[test]
    fn the_journal_is_capped_at_max_entries() {
        let _scratch = Scratch::new("journal-cap");

        // A fixed real head that never matches any of the fake `pre` snapshots below, so
        // every `push_if_changed` call sees a real change and actually journals.
        pallet_utils::set_pallet_head("main", &"f".repeat(64)).unwrap();

        for i in 0..(MAX_ENTRIES + 10) {
            push_if_changed(entry("stack", "main", &format!("{:064x}", i))).unwrap();
        }

        assert_eq!(read().len(), MAX_ENTRIES, "the journal must never grow past its cap");

        // The oldest entries are the ones dropped: the newest surviving entry is the very
        // last one pushed.
        let newest = pop().unwrap().unwrap();
        assert_eq!(newest.heads.get("main"), Some(&format!("{:064x}", MAX_ENTRIES + 9)));
    }
}
