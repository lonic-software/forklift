use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use serde::{Deserialize, Serialize};
use crate::globals::bay_root;
use crate::util::cherry_pick_utils::{self, CherryPickState};
use crate::util::merge_utils::{self, ConsolidationState, OutOfScopeSkeleton};
use crate::util::object_utils;
use crate::util::pallet_utils::{self, PalletNamespace, PalletRef, RefUpdateError};
use crate::util::file_utils;

/// The undo journal (§7.8): an append-only list of operation records kept in the bay, so `undo`
/// can reverse the last state-changing operation — not just the last `stack`. Content addressing
/// makes this cheap: a record is just the current pallet, the pallet refs the operation moved and
/// any in-progress consolidation or cherry-pick on each side of it (all tiny); the objects they point at are
/// immutable and already stored. The working directory and inventory are the undo command's
/// concern, not the journal's.
const FILE_NAME_JOURNAL: &str = "journal.json";

/// The journal file's format version. The file is `{"version":2,"entries":[...]}`; any other
/// version, or anything that does not parse as this shape, is refused by name — never read as an
/// empty journal, which would make `undo` report "nothing to undo" over a history it lost.
const JOURNAL_VERSION: u64 = 2;

/// Cap the journal so it cannot grow without bound; the oldest entries fall off (undo is
/// a recent-history convenience, not an audit trail — that is what `history`/`audit` are).
const MAX_ENTRIES: usize = 100;

/// The mutable warehouse state on one side of a journaled operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The bay's current pallet.
    pub current_pallet: String,

    /// Pallet refs (user and meta), keyed by wire form (`main`, `@office`) → head hash. A
    /// journaled entry keeps only the refs its operation moved, plus both sides' current pallets;
    /// a ref the operation left alone needs no restoring.
    pub heads: BTreeMap<String, String>,

    /// The bay's consolidation in progress, if any.
    pub consolidation: Option<ConsolidationInProgress>,

    /// The bay's cherry-pick in progress (one whose merge left conflicts), if any.
    pub cherry_pick: Option<PickInProgress>,
}

/// A consolidation in progress, as journaled: the bay's consolidation-state file and the
/// out-of-scope skeleton beside it. The skeleton travels with the state because the stack that
/// completes the merge requires both; restoring the state alone would leave a merge that can
/// never be completed.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConsolidationInProgress {
    /// The head being merged in.
    pub their_head: String,

    /// The pallet being merged in.
    pub their_pallet: String,

    /// The head the merge was built on: the base its completing parcel stacks on.
    pub our_head: String,

    /// The pallet the merge was started on.
    pub our_pallet: String,

    /// The merge's out-of-scope skeleton, in its on-disk form (empty outside a scoped bay).
    pub skeleton: String,
}

/// A cherry-pick in progress, as journaled: the bay's cherry-pick state file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PickInProgress {
    /// The parcel being applied.
    pub source: String,

    /// The head the pick was applied on: the base its completing parcel stacks on.
    pub our_head: String,

    /// The pallet the pick was applied to.
    pub our_pallet: String,

    /// The description the completing parcel defaults to.
    pub description: Option<String>,
}

/// What `undo` gates on: everything about the bay that a journaled operation can change, and
/// nothing about the refs other bays share.
#[derive(Debug, PartialEq)]
struct BayState {
    current_pallet: String,
    current_head: Option<String>,
    consolidation: Option<ConsolidationInProgress>,
    cherry_pick: Option<PickInProgress>,
}

impl BayState {
    /// The bay as it is now.
    fn read() -> Result<BayState, String> {
        let current_pallet = pallet_utils::get_current_pallet_name()?;
        let current_head = pallet_utils::get_pallet_head(&current_pallet)?;

        Ok(BayState {
            current_pallet,
            current_head,
            consolidation: read_consolidation()?,
            cherry_pick: read_cherry_pick()?,
        })
    }
}

impl Snapshot {
    /// The head of the current pallet, or `None` while it is unborn.
    pub fn current_head(&self) -> Option<&String> {
        self.heads.get(&self.current_pallet)
    }

    /// The bay state this snapshot records. Pallet refs other than the current one are left out:
    /// they are shared by every bay of the warehouse, so a stack in another bay moves them without
    /// touching this bay's history.
    fn bay_state(&self) -> BayState {
        BayState {
            current_pallet: self.current_pallet.clone(),
            current_head: self.current_head().cloned(),
            consolidation: self.consolidation.clone(),
            cherry_pick: self.cherry_pick.clone(),
        }
    }
}

/// The bay's consolidation in progress, as journaled. Its skeleton is required, exactly as the
/// completing stack requires it: a state without one is a broken merge, never an empty skeleton.
fn read_consolidation() -> Result<Option<ConsolidationInProgress>, String> {
    let Some(state) = merge_utils::read_consolidation_state()? else {
        return Ok(None);
    };

    let skeleton = String::from_utf8(OutOfScopeSkeleton::read_required()?.to_bytes())
        .map_err(|_| "The out-of-scope skeleton does not serialize as text.".to_string())?;

    Ok(Some(ConsolidationInProgress {
        their_head: state.their_head,
        their_pallet: state.their_pallet,
        our_head: state.our_head,
        our_pallet: state.our_pallet,
        skeleton,
    }))
}

/// The bay's cherry-pick in progress, as journaled.
fn read_cherry_pick() -> Result<Option<PickInProgress>, String> {
    Ok(cherry_pick_utils::read_state()?
        .map(|state| PickInProgress {
            source: state.source,
            our_head: state.our_head,
            our_pallet: state.our_pallet,
            description: state.description,
        }))
}

/// One journaled operation: the state before it ran, and the state it left behind.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JournalEntry {
    /// The command that was journaled (`stack`, `consolidate`, `cherry-pick`, `shift`).
    pub op: String,

    /// The state before the operation; `undo` restores it.
    pub before: Snapshot,

    /// The state right after the operation. `undo` refuses unless the bay still matches it (see
    /// [`check_undoable`]), and `restore_refs` rolls back only the refs still sitting here.
    pub after: Snapshot,
}

/// The journal file, as written.
#[derive(Serialize)]
struct JournalFile<'a> {
    version: u64,
    entries: &'a [JournalEntry],
}

/// The journal file, as read (once its version has been checked on its own).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalContents {
    #[allow(dead_code)]
    version: u64,
    entries: Vec<JournalEntry>,
}

/// Capture the bay's state, to journal an operation about to run, and start recording the head
/// writes this process makes to the warehouse (`pallet_utils`' write log) — [`push_if_changed`]
/// builds the entry's refs from that record, so the snapshot holds only the bay-local state and
/// the current pallet's head.
///
/// # Returns
/// * `Ok(Snapshot)` - The snapshot.
/// * `Err(String)`  - If the current pallet, consolidation or cherry-pick state could not be read.
pub fn capture() -> Result<Snapshot, String> {
    let current_pallet = pallet_utils::get_current_pallet_name()?;
    let heads = pallet_utils::get_pallet_head(&current_pallet)?
        .map(|head| BTreeMap::from([(current_pallet.clone(), head)]))
        .unwrap_or_default();

    let snapshot = Snapshot {
        current_pallet,
        heads,
        consolidation: read_consolidation()?,
        cherry_pick: read_cherry_pick()?,
    };

    // Only once the snapshot is whole: a failed capture journals nothing, so it records nothing.
    pallet_utils::start_write_log();

    Ok(snapshot)
}

/// Stop recording head writes without journaling them — for an operation that failed, which
/// [`push_if_changed`] must not journal.
pub fn discard() {
    pallet_utils::take_write_log();
}

/// Journal `op`, which ran from `before` to the state the bay is in now — but only if it changed
/// what `undo` gates on (so a command that turned out to be a no-op leaves no undo entry).
/// Best-effort by contract: the caller ignores the result, because a journaling hiccup must never
/// fail a command that already succeeded. A journal that cannot be read is left untouched.
///
/// The entry's refs are the ones this process wrote since [`capture`], from the write log, which
/// this stops: each ref's first replaced head before, its last written head after. A ref the
/// operation created has no before (undo cannot delete a ref), however often it then moved it. A
/// ref another bay or process moved meanwhile is not in the log, so the entry never records — and `undo` never
/// reverts — someone else's move. A current pallet the operation did not write is read once, here,
/// and recorded identically on both sides: the gate reads it, but there is nothing to restore.
///
/// # Arguments
/// * `op`     - The command's journal name.
/// * `before` - The snapshot captured by [`capture`] before the command ran.
pub fn push_if_changed(op: &str, mut before: Snapshot) -> Result<(), String> {
    let writes = pallet_utils::take_write_log();

    // The undo journal reverses an operation by restoring refs, which cannot un-create a
    // pallet's very first parcel (that would need deleting the ref). So an operation starting
    // from an unborn pallet is not journaled.
    if before.current_head().is_none() {
        return Ok(());
    }

    let current_pallet = pallet_utils::get_current_pallet_name()?;
    let mut after = Snapshot {
        current_pallet,
        heads: BTreeMap::new(),
        consolidation: read_consolidation()?,
        cherry_pick: read_cherry_pick()?,
    };

    before.heads.clear();
    let mut seen = BTreeSet::new();

    for (wire, old, new) in writes {
        // Only a ref's first write tells what it held before the operation.
        if seen.insert(wire.clone()) {
            if let Some(old) = old {
                before.heads.insert(wire.clone(), old);
            }
        }

        after.heads.insert(wire, new);
    }

    let unwritten: BTreeSet<String> = [&before.current_pallet, &after.current_pallet].into_iter()
        .filter(|pallet| !after.heads.contains_key(*pallet))
        .cloned()
        .collect();

    for pallet in unwritten {
        if let Some(head) = pallet_utils::get_pallet_head(&pallet)? {
            before.heads.insert(pallet.clone(), head.clone());
            after.heads.insert(pallet, head);
        }
    }

    if before.bay_state() == after.bay_state() {
        return Ok(());
    }

    let mut entries = read()?;
    entries.push(JournalEntry { op: op.to_string(), before, after });

    let len = entries.len();
    if len > MAX_ENTRIES {
        entries.drain(0..len - MAX_ENTRIES);
    }

    write(&entries)
}

/// Look at the most recent journal entry without consuming it. The caller (`undo`) must
/// validate — and, for a soft reset, actually restore — an entry *before* it commits to
/// removing it from the journal: `pop` throws the entry away the moment it is called, so a
/// refused or failed undo must call this first and only `pop` once it has fully succeeded.
///
/// # Returns
/// * `Ok(Some(JournalEntry))` - The most recent entry, left in place.
/// * `Ok(None)`               - If the journal is empty or absent.
/// * `Err(String)`            - If the journal cannot be read.
pub fn peek() -> Result<Option<JournalEntry>, String> {
    Ok(read()?.pop())
}

/// Remove and return the most recent journal entry. Only call this once the undo it
/// describes has actually gone through — never as a way to decide whether it can.
///
/// # Returns
/// * `Ok(Some(JournalEntry))` - The popped entry.
/// * `Ok(None)`               - If the journal is empty or absent.
/// * `Err(String)`            - If the journal could not be read or rewritten.
pub fn pop() -> Result<Option<JournalEntry>, String> {
    let mut entries = read()?;
    let popped = entries.pop();

    if popped.is_some() {
        write(&entries)?;
    }

    Ok(popped)
}

/// Whether the bay is still exactly where `entry`'s operation left it: the same current pallet,
/// that pallet at the same head, and the same consolidation and cherry-pick in progress (a
/// conflicted cherry-pick moves no ref). Anything else means an
/// operation outside the journal ran since (`palletize` and `deliver` switch the current pallet,
/// a fast-forward `lower` or a haul merge moves its head), and reversing `entry` now would undo an
/// older operation than the user's last. Refs other than the current pallet are not compared: they
/// are shared by every bay, and `restore_refs` already leaves any that moved alone.
///
/// # Arguments
/// * `entry` - The entry `undo` is about to reverse.
///
/// # Returns
/// * `Ok(())`      - The bay matches; the entry may be reversed.
/// * `Err(String)` - The refusal, naming what differs; or the state could not be read.
pub fn check_undoable(entry: &JournalEntry) -> Result<(), String> {
    let now = BayState::read()?;
    let after = entry.after.bay_state();

    if now.current_pallet != after.current_pallet {
        return Err(format!(
            "The last journaled operation (\"{}\") left pallet \"{}\" current, but \"{}\" is \
            current now: something outside the undo journal (such as palletize or deliver) \
            switched pallets since, and undo cannot reach past it.",
            entry.op, after.current_pallet, now.current_pallet
        ));
    }

    if now.current_head != after.current_head {
        return Err(current_pallet_moved(&after.current_pallet, &entry.op));
    }

    if now.consolidation != after.consolidation {
        let describe = |consolidation: &Option<ConsolidationInProgress>| match consolidation {
            Some(consolidation) => format!("a consolidation of \"{}\"", crate::error::sanitize(&consolidation.their_pallet)),
            None => "no consolidation".to_string(),
        };

        return Err(format!(
            "The consolidation state has changed since that operation (\"{}\"): it left {} in \
            progress, and {} is in progress now. Undo will not reverse an older operation than \
            the last one.",
            entry.op, describe(&after.consolidation), describe(&now.consolidation)
        ));
    }

    if now.cherry_pick != after.cherry_pick {
        let describe = |pick: &Option<PickInProgress>| match pick {
            Some(pick) => format!("a cherry-pick of {}", pick.source),
            None => "no cherry-pick".to_string(),
        };

        return Err(format!(
            "The cherry-pick state has changed since that operation (\"{}\"): it left {} in \
            progress, and {} is in progress now. Undo will not reverse an older operation than \
            the last one.",
            entry.op, describe(&after.cherry_pick), describe(&now.cherry_pick)
        ));
    }

    Ok(())
}

/// The refusal for an undo whose current pallet moved after the operation it would reverse.
fn current_pallet_moved(pallet: &str, op: &str) -> String {
    format!(
        "Pallet \"{}\" has moved since that operation (\"{}\") — something outside the undo \
        journal advanced it, and undo will not rewind that too. Continue forward instead \
        (e.g. re-stack).",
        pallet, op
    )
}

/// Restore the pallet refs, current pallet, consolidation and cherry-pick state an entry recorded before its
/// operation. The working directory and inventory are left untouched — the `undo` command decides
/// whether to re-materialize (a soft reset keeps them; reversing a `shift` re-materializes).
///
/// Only the journaled operation itself is undone — never anything that happened after it.
/// Commands outside the journal (office/meta-pallet mutations such as a key retirement, a stack
/// in another bay) can move other refs between the journaled op and this `undo`; a ref they
/// touched must not be rolled back just because it was also in this snapshot. So each ref is
/// restored by a compare-and-set from where the journaled op left it: a ref that has moved since,
/// that the op did not leave behind, or that no longer exists is left alone.
///
/// The current pallet's ref goes first. If it moved after [`check_undoable`] passed, the undo is
/// refused with that check's refusal, before any other ref or any bay-local state is touched.
///
/// # Arguments
/// * `entry` - The entry to reverse.
pub fn restore_refs(entry: &JournalEntry) -> Result<(), String> {
    let current = &entry.after.current_pallet;
    let order = entry.before.heads.keys()
        .filter(|wire| *wire == current)
        .chain(entry.before.heads.keys().filter(|wire| *wire != current));

    for wire in order {
        let Some(post_head) = entry.after.heads.get(wire) else {
            continue;
        };

        let reference = PalletRef::parse(wire)?;
        let pre_head = &entry.before.heads[wire];

        match pallet_utils::update_pallet_head_in(reference.namespace, &reference.name, Some(post_head), pre_head) {
            Ok(()) => {}
            Err(RefUpdateError::Moved(_)) if wire == current => {
                return Err(current_pallet_moved(current, &entry.op));
            }
            // Another writer moved it after the operation: theirs stands.
            Err(RefUpdateError::Moved(_)) => {}
            // The write failed: put the current pallet back where the operation left it, so the
            // bay still matches the entry and a retried undo passes the gate again.
            Err(RefUpdateError::Failed(e)) => return Err(un_restore_current(entry, e)),
        }
    }

    pallet_utils::set_current_pallet_name(&entry.before.current_pallet)?;

    // The skeleton goes down before the state and comes off after it, as `consolidate` and
    // `stack` order them, so no window shows a consolidation without its skeleton.
    match &entry.before.consolidation {
        Some(consolidation) => {
            // Validated when the journal was read; written back as recorded.
            file_utils::write_file_atomically(&OutOfScopeSkeleton::path(), consolidation.skeleton.as_bytes())?;
            merge_utils::write_consolidation_state(&ConsolidationState {
                their_head: consolidation.their_head.clone(),
                their_pallet: consolidation.their_pallet.clone(),
                our_head: consolidation.our_head.clone(),
                our_pallet: consolidation.our_pallet.clone(),
            })?;
        },
        None => {
            merge_utils::clear_consolidation_state()?;
            OutOfScopeSkeleton::clear()?;
        },
    }

    // Undoing the stack that completed a conflicted pick puts the pick back in progress, so the
    // next stack completes it again with the source's authors.
    match &entry.before.cherry_pick {
        Some(pick) => cherry_pick_utils::write_state(
            &CherryPickState {
                source: pick.source.clone(),
                our_head: pick.our_head.clone(),
                our_pallet: pick.our_pallet.clone(),
                description: pick.description.clone(),
            }
        )?,
        None => cherry_pick_utils::clear_state()?,
    }

    Ok(())
}

/// After a ref write failed partway through [`restore_refs`], move the current pallet's ref back
/// from its restored head to the one the operation left (if it was restored), and return the
/// error to report. Other refs restored before the failure stay restored: a retried undo finds
/// them moved off the operation's heads and leaves them alone.
fn un_restore_current(entry: &JournalEntry, error: String) -> String {
    let current = &entry.after.current_pallet;
    let (Some(pre_head), Some(post_head)) = (entry.before.heads.get(current), entry.after.heads.get(current)) else {
        return error;
    };

    let e = match pallet_utils::update_pallet_head_in(PalletNamespace::User, current, Some(pre_head), post_head) {
        Ok(()) | Err(RefUpdateError::Moved(_)) => return error,
        Err(RefUpdateError::Failed(e)) => e,
    };

    // A write can fail after landing (not known durable); what the ref holds now decides.
    let at = match pallet_utils::get_pallet_head(current) {
        Ok(head) if head.as_deref() == Some(post_head.as_str()) => return error,
        Ok(Some(head)) => format!("it is at {}", head),
        Ok(None) => "it no longer exists".to_string(),
        Err(read) => format!("it could not be read back either ({})", read),
    };

    format!(
        "{} Pallet \"{}\" could not be put back where the operation left it ({}): {}, so a \
        retried undo will not pass its check.",
        error, current, e, at
    )
}

/// The journal file path (bay-local).
fn path() -> PathBuf {
    bay_root().join(FILE_NAME_JOURNAL)
}

/// The refusal for a journal that cannot be read, naming the file and the way out.
fn unreadable(path: &Path, reason: &str) -> String {
    format!(
        "The undo journal \"{}\" {}. Remove it to start a fresh journal; undo history is lost.",
        path.to_string_lossy(), reason
    )
}

/// Check that a snapshot read from the journal holds only values the warehouse could have
/// written, so a damaged entry is refused here rather than half-restored by `undo`.
fn validate_snapshot(snapshot: &Snapshot) -> Result<(), String> {
    let invalid_name = |e: String| format!("names an invalid pallet ({})", e);

    pallet_utils::validate_pallet_name(&snapshot.current_pallet).map_err(invalid_name)?;

    for (wire, head) in &snapshot.heads {
        PalletRef::parse(wire).map_err(invalid_name)?;

        if !object_utils::is_object_hash(head) {
            return Err(format!("records a head for \"{}\" that is not an object hash", wire));
        }
    }

    if let Some(consolidation) = &snapshot.consolidation {
        if !object_utils::is_object_hash(&consolidation.their_head)
            || !object_utils::is_object_hash(&consolidation.our_head)
            || pallet_utils::validate_pallet_name(&consolidation.our_pallet).is_err() {
            return Err("records a malformed consolidation".to_string());
        }

        // `their_pallet` is the merge's label, not a ref: a haul merge labels it with the haul's
        // recorded source and lift with "remote", and the consolidation state round-trips any
        // label. Nothing resolves it, so any non-empty string is well-formed here (the
        // consolidation state refuses an empty one).
        if consolidation.their_pallet.is_empty() {
            return Err("records a consolidation with no label".to_string());
        }

        OutOfScopeSkeleton::from_bytes(consolidation.skeleton.as_bytes(), &path())
            .map_err(|_| "records a malformed out-of-scope skeleton".to_string())?;
    }

    if let Some(pick) = &snapshot.cherry_pick {
        if !object_utils::is_object_hash(&pick.source) || !object_utils::is_object_hash(&pick.our_head)
            || pallet_utils::validate_pallet_name(&pick.our_pallet).is_err() {
            return Err("records a malformed cherry-pick".to_string());
        }
    }

    Ok(())
}

/// Read the journal. A missing file is an empty journal; a file that cannot be read, does not
/// parse, has another version or holds an impossible value is an error naming the file.
fn read() -> Result<Vec<JournalEntry>, String> {
    let path = path();

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(unreadable(&path, &format!("could not be read ({})", e))),
    };

    // Parse once; the version is checked on its own first, so an unknown version is named as
    // such rather than as whatever shape mismatch its entries happen to produce.
    let value: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| unreadable(&path, &format!("is not a readable journal ({})", e)))?;

    let version = match value.get("version") {
        Some(version) => version.as_u64().ok_or_else(|| unreadable(&path, &format!(
            "has an unreadable format version ({}), which this forklift does not understand \
            (it reads version {})", version, JOURNAL_VERSION
        )))?,
        None => return Err(unreadable(&path, "is not a readable journal (it records no format version)")),
    };

    if version != JOURNAL_VERSION {
        return Err(unreadable(&path, &format!(
            "has format version {}, which this forklift does not understand (it reads version {})",
            version, JOURNAL_VERSION
        )));
    }

    let contents: JournalContents = serde_json::from_value(value)
        .map_err(|e| unreadable(&path, &format!("is malformed ({})", e)))?;

    for entry in &contents.entries {
        validate_snapshot(&entry.before)
            .and_then(|_| validate_snapshot(&entry.after))
            .map_err(|reason| unreadable(&path, &format!("is malformed (an entry {})", reason)))?;
    }

    Ok(contents.entries)
}

/// Write the journal atomically.
fn write(entries: &[JournalEntry]) -> Result<(), String> {
    let json = serde_json::to_string(&JournalFile { version: JOURNAL_VERSION, entries })
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

    /// A consolidation of `pallet` at `head`, with the empty skeleton a full bay writes.
    fn consolidating(head: &str, pallet: &str) -> ConsolidationInProgress {
        ConsolidationInProgress {
            their_head: head.to_string(),
            their_pallet: pallet.to_string(),
            our_head: hash('e'), our_pallet: "main".to_string(),
            skeleton: String::from_utf8(OutOfScopeSkeleton::default().to_bytes()).unwrap(),
        }
    }

    /// Start a consolidation in the bay as `consolidate` does: the skeleton, then the state.
    fn start_consolidation(head: &str, pallet: &str) {
        OutOfScopeSkeleton::default().write().unwrap();
        merge_utils::write_consolidation_state(&ConsolidationState {
            their_head: head.to_string(),
            their_pallet: pallet.to_string(),
            our_head: hash('e'), our_pallet: "main".to_string(),
        }).unwrap();
    }

    fn hash(digit: char) -> String {
        digit.to_string().repeat(64)
    }

    /// A snapshot on `pallet` holding only that pallet's ref at `head`, with no consolidation.
    fn snapshot(pallet: &str, head: &str) -> Snapshot {
        let mut heads = BTreeMap::new();
        heads.insert(pallet.to_string(), head.to_string());

        Snapshot { current_pallet: pallet.to_string(), heads, consolidation: None, cherry_pick: None }
    }

    /// An entry recording `op` moving `pallet` from `before` to `after`.
    fn entry(op: &str, pallet: &str, before: &str, after: &str) -> JournalEntry {
        JournalEntry { op: op.to_string(), before: snapshot(pallet, before), after: snapshot(pallet, after) }
    }

    fn plant_journal(content: &str) {
        std::fs::write(path(), content).unwrap();
    }

    /// Every read path refuses the journal, naming the file and the way out, and a push leaves
    /// the file exactly as it was.
    fn assert_refused_everywhere(content: &str, reason: &str) {
        plant_journal(content);

        for result in [peek().map(|_| ()), pop().map(|_| ())] {
            let error = result.expect_err("an unreadable journal must be refused, never read as empty");
            assert!(error.contains(&path().to_string_lossy().to_string()), "{}", error);
            assert!(error.contains(reason), "expected {:?} in: {}", reason, error);
            assert!(error.contains("Remove it to start a fresh journal"), "{}", error);
        }

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        assert!(push_if_changed("stack", before).is_err());
        assert_eq!(std::fs::read_to_string(path()).unwrap(), content, "a refused journal is never rewritten");
    }

    #[test]
    fn capture_reflects_a_bare_warehouses_default_state() {
        let _scratch = Scratch::new("capture-bare");

        let snapshot = capture().unwrap();
        assert_eq!(snapshot.current_pallet, crate::util::pallet_utils::DEFAULT_PALLET_NAME);
        assert!(snapshot.heads.is_empty(), "an unborn warehouse has no pallet refs yet");
        assert!(snapshot.consolidation.is_none());
    }

    #[test]
    fn push_if_changed_is_a_noop_when_the_current_pallet_is_unborn() {
        let _scratch = Scratch::new("push-unborn");

        // The default pallet has never been stacked to, so `heads` cannot contain it —
        // exactly the "stack onto an unborn pallet" case `push_if_changed` opts out of.
        let before = capture().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        push_if_changed("stack", before).unwrap();

        assert!(pop().unwrap().is_none(), "an unborn-pallet snapshot must never be journaled");
    }

    #[test]
    fn push_if_changed_records_both_sides_of_the_operation() {
        let _scratch = Scratch::new("push-real-change");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();

        // The operation itself: the head moves, a consolidation starts and another ref moves.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('c'),
        ).unwrap();
        start_consolidation(&hash('d'), "side");
        push_if_changed("consolidate", before).unwrap();

        let popped = pop().unwrap().expect("a real change must be journaled");
        assert_eq!(popped.op, "consolidate");
        assert_eq!(popped.before, snapshot("main", &hash('a')));
        assert_eq!(popped.after.current_pallet, "main");
        assert_eq!(popped.after.heads.get("main"), Some(&hash('b')));
        assert_eq!(popped.after.heads.get("side"), Some(&hash('c')));
        assert_eq!(popped.after.consolidation, Some(consolidating(&hash('d'), "side")));
        assert!(pop().unwrap().is_none(), "popping removes the entry");
    }

    #[test]
    fn push_if_changed_keeps_only_the_refs_the_operation_moved() {
        let _scratch = Scratch::new("push-prunes");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "idle",
            pallet_utils::get_pallet_head("idle").unwrap().as_deref(),
            &hash('e'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('f'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('1'),
        ).unwrap();
        let before = capture().unwrap();

        // The operation moves "main" and "side"; "idle" and "@office" stay where they were.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        push_if_changed("stack", before).unwrap();

        let popped = pop().unwrap().unwrap();
        let keys = |heads: &BTreeMap<String, String>| heads.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&popped.before.heads), ["main", "side"]);
        assert_eq!(keys(&popped.after.heads), ["main", "side"]);
    }

    /// Move `pallet` to `head` the way another bay or process would: a ref write outside this
    /// process's write log.
    fn move_elsewhere(pallet: &str, head: &str) {
        let path = pallet_utils::get_pallet_ref_path(PalletNamespace::User, pallet);
        file_utils::write_file_atomically(&path, format!("{}\n", head).as_bytes()).unwrap();
    }

    #[test]
    fn an_undo_never_reverts_a_ref_another_bay_moved_while_the_operation_ran() {
        let _scratch = Scratch::new("journal-source");

        pallet_utils::update_pallet_head("main", None, &hash('a')).unwrap();
        pallet_utils::update_pallet_head("side", None, &hash('1')).unwrap();
        let before = capture().unwrap();

        // The operation moves "main" twice; meanwhile another bay moves "side".
        pallet_utils::update_pallet_head("main", Some(&hash('a')), &hash('b')).unwrap();
        move_elsewhere("side", &hash('2'));
        pallet_utils::update_pallet_head("main", Some(&hash('b')), &hash('c')).unwrap();
        push_if_changed("stack", before).unwrap();

        let entry = peek().unwrap().expect("the operation moved the current pallet");
        assert_eq!(entry.before.heads, BTreeMap::from([("main".to_string(), hash('a'))]),
            "before holds each written ref's first replaced head, and nothing else");
        assert_eq!(entry.after.heads, BTreeMap::from([("main".to_string(), hash('c'))]),
            "after holds each written ref's last written head, and nothing else");

        restore_refs(&entry).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('a')));
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(hash('2')),
            "the other bay's move must stand");
    }

    #[test]
    fn a_ref_the_operation_created_has_no_before_however_often_it_moved() {
        let _scratch = Scratch::new("journal-created");

        pallet_utils::update_pallet_head("main", None, &hash('a')).unwrap();
        let before = capture().unwrap();

        pallet_utils::update_pallet_head("main", Some(&hash('a')), &hash('b')).unwrap();
        pallet_utils::update_pallet_head("fresh", None, &hash('1')).unwrap();
        pallet_utils::update_pallet_head("fresh", Some(&hash('1')), &hash('2')).unwrap();
        push_if_changed("haul", before).unwrap();

        let entry = peek().unwrap().expect("the operation moved the current pallet");
        assert!(!entry.before.heads.contains_key("fresh"),
            "a created ref must have no before, not its intermediate head: {:?}", entry.before.heads);
        assert_eq!(entry.after.heads.get("fresh"), Some(&hash('2')));
    }

    #[test]
    fn a_failed_capture_records_no_writes() {
        let _scratch = Scratch::new("journal-capture-fails");

        pallet_utils::update_pallet_head("main", None, &hash('a')).unwrap();
        std::fs::write(crate::globals::bay_root().join("consolidation"), "not a state").unwrap();
        assert!(capture().is_err(), "a malformed consolidation state must fail the capture");

        pallet_utils::update_pallet_head("main", Some(&hash('a')), &hash('b')).unwrap();
        assert!(pallet_utils::take_write_log().is_empty(), "nothing may record for a failed capture");
    }

    #[test]
    fn push_if_changed_skips_a_noop_whose_current_pallet_another_bay_moved() {
        let _scratch = Scratch::new("push-moved-elsewhere");

        pallet_utils::update_pallet_head("main", None, &hash('a')).unwrap();
        let before = capture().unwrap();

        // The command writes nothing; another bay stacks on the current pallet meanwhile.
        move_elsewhere("main", &hash('b'));
        push_if_changed("stack", before).unwrap();

        assert!(pop().unwrap().is_none(), "another bay's move is not this operation's to journal");
    }

    #[test]
    fn push_if_changed_keeps_both_current_pallets_across_a_switch() {
        let _scratch = Scratch::new("push-prunes-switch");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        pallet_utils::set_current_pallet_name("side").unwrap();
        push_if_changed("shift", before).unwrap();

        let popped = pop().unwrap().unwrap();
        assert_eq!(popped.before.current_head(), Some(&hash('a')));
        assert_eq!(popped.after.current_head(), Some(&hash('b')));
    }

    #[test]
    fn push_if_changed_records_a_conflicted_cherry_pick() {
        let _scratch = Scratch::new("push-cherry-pick");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();

        // A conflicted pick moves no ref; it leaves the pick in progress.
        cherry_pick_utils::write_state(&CherryPickState { source: hash('c'), our_head: hash('e'), our_pallet: "main".to_string(), description: None }).unwrap();
        push_if_changed("cherry-pick", before).unwrap();

        let popped = pop().unwrap().expect("a pick left in progress must be journaled");
        assert_eq!(popped.before.cherry_pick, None);
        assert_eq!(popped.after.cherry_pick, Some(PickInProgress { source: hash('c'), our_head: hash('e'), our_pallet: "main".to_string(), description: None }));
    }

    #[test]
    fn push_if_changed_records_a_switch_of_the_current_pallet() {
        let _scratch = Scratch::new("push-switch");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        pallet_utils::set_current_pallet_name("side").unwrap();
        push_if_changed("shift", before).unwrap();

        let popped = pop().unwrap().expect("a shift must be journaled");
        assert_eq!(popped.before.current_pallet, "main");
        assert_eq!(popped.after.current_pallet, "side");
    }

    #[test]
    fn push_if_changed_skips_a_true_noop() {
        let _scratch = Scratch::new("push-noop");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();

        // Nothing actually changed between capturing `before` and calling `push_if_changed`.
        push_if_changed("stack", before).unwrap();

        assert!(pop().unwrap().is_none(), "a no-op command must leave no undo entry");
    }

    #[test]
    fn push_if_changed_skips_an_operation_that_moved_only_another_pallet() {
        let _scratch = Scratch::new("push-other-ref");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        // Only a ref other than the current pallet moved (as a stack in another bay would).
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('c'),
        ).unwrap();
        push_if_changed("stack", before).unwrap();

        assert!(pop().unwrap().is_none(), "nothing undo gates on changed, so nothing is journaled");
    }

    #[test]
    fn pop_returns_entries_most_recent_first() {
        let _scratch = Scratch::new("pop-lifo");

        write(&[entry("stack", "main", &hash('1'), &hash('2')), entry("cherry-pick", "main", &hash('2'), &hash('3'))]).unwrap();

        assert_eq!(pop().unwrap().unwrap().op, "cherry-pick");
        assert_eq!(pop().unwrap().unwrap().op, "stack");
        assert!(pop().unwrap().is_none());
    }

    #[test]
    fn peek_returns_the_most_recent_entry_without_consuming_it() {
        let _scratch = Scratch::new("peek-leaves-it");

        write(&[entry("stack", "main", &hash('1'), &hash('2'))]).unwrap();

        assert_eq!(peek().unwrap().unwrap().op, "stack");
        assert_eq!(peek().unwrap().unwrap().op, "stack", "peek must not remove the entry");
        assert_eq!(pop().unwrap().unwrap().op, "stack", "the entry is still there to pop");
        assert!(peek().unwrap().is_none());
    }

    #[test]
    fn a_missing_journal_is_empty() {
        let _scratch = Scratch::new("missing-journal");

        assert!(!path().exists());
        assert!(peek().unwrap().is_none());
        assert!(pop().unwrap().is_none());
    }

    #[test]
    fn the_journal_file_is_a_versioned_envelope() {
        let _scratch = Scratch::new("envelope");

        write(&[entry("stack", "main", &hash('1'), &hash('2'))]).unwrap();

        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
        assert_eq!(value["version"], 2);
        assert_eq!(value["entries"][0]["op"], "stack");
        assert_eq!(value["entries"][0]["before"]["heads"]["main"], hash('1'));
        assert_eq!(value["entries"][0]["after"]["heads"]["main"], hash('2'));
    }

    #[test]
    fn a_corrupt_journal_is_refused_by_name() {
        let _scratch = Scratch::new("corrupt-journal");

        assert_refused_everywhere("not valid json at all", "is not a readable journal");
    }

    #[test]
    fn a_truncated_journal_is_refused_by_name() {
        let _scratch = Scratch::new("truncated-journal");

        write(&[entry("stack", "main", &hash('1'), &hash('2'))]).unwrap();
        let full = std::fs::read_to_string(path()).unwrap();

        assert_refused_everywhere(&full[..full.len() / 2], "is not a readable journal");
    }

    #[test]
    fn a_journal_of_another_version_is_refused_by_name() {
        let _scratch = Scratch::new("version-journal");

        // Version 1 is the format before consolidations and picks recorded their base.
        assert_refused_everywhere(r#"{"version":1,"entries":[]}"#, "has format version 1");
        assert_refused_everywhere(r#"{"version":3,"entries":[]}"#, "has format version 3");
        assert_refused_everywhere(r#"{"version":"1","entries":[]}"#, "has an unreadable format version (\"1\")");
    }

    #[test]
    fn a_bare_entry_array_is_refused_rather_than_read_as_empty() {
        let _scratch = Scratch::new("bare-array-journal");

        let entries = serde_json::to_string(&[entry("stack", "main", &hash('1'), &hash('2'))]).unwrap();

        assert_refused_everywhere(&entries, "is not a readable journal");
    }

    #[test]
    fn an_entry_missing_its_after_state_is_refused() {
        let _scratch = Scratch::new("no-after-journal");

        let content = format!(
            r#"{{"version":2,"entries":[{{"op":"stack","before":{{"current_pallet":"main","heads":{{"main":"{}"}},"consolidation":null}}}}]}}"#,
            hash('1')
        );

        assert_refused_everywhere(&content, "is malformed");
    }

    #[test]
    fn an_entry_with_an_impossible_value_is_refused() {
        let _scratch = Scratch::new("bad-hash-journal");

        let mut bad_head = entry("stack", "main", &hash('1'), &hash('2'));
        bad_head.after.heads.insert("main".to_string(), "not-a-hash".to_string());
        let mut bad_name = entry("stack", "main", &hash('1'), &hash('2'));
        bad_name.before.current_pallet = "../escape".to_string();
        let mut bad_skeleton = entry("consolidate", "main", &hash('1'), &hash('2'));
        bad_skeleton.after.consolidation = Some(ConsolidationInProgress {
            skeleton: "not a skeleton".to_string(),
            ..consolidating(&hash('3'), "side")
        });
        let mut bad_pick = entry("stack", "main", &hash('1'), &hash('2'));
        bad_pick.before.cherry_pick = Some(PickInProgress { source: "nope".to_string(), our_head: hash('e'), our_pallet: "main".to_string(), description: None });
        let mut bad_label = entry("consolidate", "main", &hash('1'), &hash('2'));
        bad_label.after.consolidation = Some(consolidating(&hash('3'), ""));

        for (bad, reason) in [
            (bad_head, "not an object hash"),
            (bad_name, "names an invalid pallet"),
            (bad_skeleton, "malformed out-of-scope skeleton"),
            (bad_pick, "malformed cherry-pick"),
            (bad_label, "consolidation with no label"),
        ] {
            let content = serde_json::to_string(&JournalFile { version: JOURNAL_VERSION, entries: &[bad] }).unwrap();
            assert_refused_everywhere(&content, reason);
        }
    }

    #[test]
    fn check_undoable_passes_while_the_bay_is_where_the_operation_left_it() {
        let _scratch = Scratch::new("gate-pass");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap();
    }

    #[test]
    fn check_undoable_tolerates_another_pallet_moving() {
        let _scratch = Scratch::new("gate-other-ref");

        // The op left "side" at 3; a stack in another bay has since moved it to 4.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('4'),
        ).unwrap();

        let mut stacked = entry("stack", "main", &hash('1'), &hash('2'));
        stacked.after.heads.insert("side".to_string(), hash('3'));

        check_undoable(&stacked).unwrap();
    }

    #[test]
    fn check_undoable_refuses_once_the_current_pallet_switched() {
        let _scratch = Scratch::new("gate-switched");

        // The op left "main" current at 2; an un-journaled palletize has since switched to
        // "feature" at the same head.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_pallet_head(
            "feature",
            pallet_utils::get_pallet_head("feature").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::set_current_pallet_name("feature").unwrap();

        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("left pallet \"main\" current, but \"feature\" is current now"), "{}", error);
    }

    #[test]
    fn check_undoable_refuses_once_the_current_pallet_moved() {
        let _scratch = Scratch::new("gate-moved");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('9'),
        ).unwrap();

        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("Pallet \"main\" has moved since that operation"), "{}", error);
    }

    #[test]
    fn check_undoable_refuses_once_the_current_pallet_is_unborn_again() {
        let _scratch = Scratch::new("gate-unborn");

        // No ref for "main" exists at all, where the op left one.
        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("has moved since that operation"), "{}", error);
    }

    #[test]
    fn check_undoable_refuses_once_the_consolidation_state_changed() {
        let _scratch = Scratch::new("gate-consolidation");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        start_consolidation(&hash('5'), "side");

        // The op left no consolidation in progress; one is in progress now.
        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("it left no consolidation in progress, and a consolidation of \"side\""), "{}", error);

        // And the converse: the op left this consolidation in progress, and it still is.
        let mut started = entry("consolidate", "main", &hash('2'), &hash('2'));
        started.after.consolidation = Some(consolidating(&hash('5'), "side"));
        check_undoable(&started).unwrap();
    }

    #[test]
    fn check_undoable_refuses_once_a_cherry_pick_was_left_in_progress() {
        let _scratch = Scratch::new("gate-cherry-pick");

        // The op left no pick in progress; a conflicted cherry-pick has since started one
        // without moving any ref.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        cherry_pick_utils::write_state(&CherryPickState { source: hash('6'), our_head: hash('e'), our_pallet: "main".to_string(), description: None }).unwrap();

        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("it left no cherry-pick in progress"), "{}", error);

        // And the converse: the op left this pick in progress, and it still is.
        let mut picked = entry("cherry-pick", "main", &hash('2'), &hash('2'));
        picked.after.cherry_pick = Some(PickInProgress { source: hash('6'), our_head: hash('e'), our_pallet: "main".to_string(), description: None });
        check_undoable(&picked).unwrap();
    }

    #[test]
    fn restore_refs_puts_pallet_heads_current_pallet_and_consolidation_back() {
        let _scratch = Scratch::new("restore-refs");

        // Both refs currently sit exactly where the journaled op left them (`after`), so
        // `restore_refs` is free to roll them back to their pre-op values.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('4'),
        ).unwrap();

        let mut restored = entry("consolidate", "main", &hash('a'), &hash('2'));
        restored.before.current_pallet = "feature".to_string();
        restored.before.heads.insert("@office".to_string(), hash('b'));
        restored.before.consolidation = Some(consolidating(&hash('c'), "their-pallet"));
        restored.after.heads.insert("@office".to_string(), hash('4'));

        restore_refs(&restored).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('a')));
        assert_eq!(pallet_utils::get_meta_pallet_head("office").unwrap(), Some(hash('b')));
        assert_eq!(pallet_utils::get_current_pallet_name().unwrap(), "feature");

        let consolidation = merge_utils::read_consolidation_state().unwrap().unwrap();
        assert_eq!(consolidation.their_head, hash('c'));
        assert_eq!(consolidation.their_pallet, "their-pallet");
        assert!(OutOfScopeSkeleton::read_required().is_ok(), "the skeleton must come back with the state");
    }

    #[test]
    fn restore_refs_puts_back_the_recorded_out_of_scope_skeleton() {
        let _scratch = Scratch::new("restore-skeleton");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        // The stack that completed a scoped merge consumed a non-empty skeleton with the state.
        let skeleton = OutOfScopeSkeleton::from_actions(&[merge_utils::MergeAction::ResolveOutOfScope {
            path: "docs/old".to_string(),
            resolution: None,
        }]);
        let mut completed = entry("stack", "main", &hash('1'), &hash('2'));
        completed.before.consolidation = Some(ConsolidationInProgress {
            skeleton: String::from_utf8(skeleton.to_bytes()).unwrap(),
            ..consolidating(&hash('c'), "side")
        });

        restore_refs(&completed).unwrap();

        assert!(OutOfScopeSkeleton::read_required().unwrap() == skeleton, "the recorded skeleton must be restored");

        // And an entry with no consolidation before it clears both.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        restore_refs(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap();
        assert!(merge_utils::read_consolidation_state().unwrap().is_none());
        assert!(OutOfScopeSkeleton::read().unwrap().is_empty());
    }

    #[test]
    fn a_journaled_consolidation_keeps_a_label_that_is_not_a_pallet_name() {
        // A haul merge labels its consolidation with the haul's recorded source, which is
        // whatever string the signed haul cell holds; the stack completing it is journaled.
        let _scratch = Scratch::new("label-not-a-pallet");
        let label = "not a\npallet/../name";

        pallet_utils::set_current_pallet_name("main").unwrap();
        pallet_utils::update_pallet_head("main", None, &hash('1')).unwrap();
        start_consolidation(&hash('d'), label);

        let before = capture().unwrap();
        pallet_utils::update_pallet_head("main", Some(&hash('1')), &hash('2')).unwrap();
        merge_utils::clear_consolidation_state().unwrap();
        push_if_changed("stack", before).unwrap();

        let entry = peek().expect("the journal must stay readable").unwrap();
        assert_eq!(entry.before.consolidation.unwrap().their_pallet, label);
    }

    #[cfg(unix)]
    #[test]
    fn an_undo_whose_later_ref_write_fails_leaves_the_current_pallet_where_the_operation_left_it() {
        let _scratch = Scratch::new("restore-partial-failure");

        pallet_utils::set_current_pallet_name("main").unwrap();
        pallet_utils::update_pallet_head("main", None, &hash('1')).unwrap();
        pallet_utils::update_meta_pallet_head("kv", None, &hash('a')).unwrap();

        let before = capture().unwrap();
        pallet_utils::update_pallet_head("main", Some(&hash('1')), &hash('2')).unwrap();
        pallet_utils::update_meta_pallet_head("kv", Some(&hash('a')), &hash('b')).unwrap();
        push_if_changed("stack", before).unwrap();
        let entry = peek().unwrap().unwrap();

        // "@kv" reads, but can no longer be written: its folder is read-only now.
        use std::os::unix::fs::PermissionsExt;
        let meta = crate::globals::forklift_root().join("meta");
        std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o555)).unwrap();

        // Permissions do not bind a privileged user (root in a container): no fault, no test.
        if std::fs::write(meta.join("probe"), b"").is_ok() {
            std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }

        let restored = restore_refs(&entry);
        std::fs::set_permissions(&meta, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(restored.is_err(), "the \"@kv\" write must fail");
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('2')),
            "the current pallet must be back where the operation left it");
        check_undoable(&entry).expect("a retried undo must still pass the gate");
    }

    #[test]
    fn capture_refuses_a_consolidation_without_its_skeleton() {
        let _scratch = Scratch::new("capture-no-skeleton");

        merge_utils::write_consolidation_state(&ConsolidationState {
            their_head: hash('d'),
            their_pallet: "side".to_string(),
            our_head: hash('e'), our_pallet: "main".to_string(),
        }).unwrap();

        assert!(capture().is_err(), "a broken merge must not be journaled with an empty skeleton");
    }

    #[test]
    fn restore_refs_leaves_a_ref_untouched_if_it_moved_after_the_journaled_op() {
        let _scratch = Scratch::new("restore-refs-moved");

        // "main" sits exactly where the op left it: safe to restore.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        // "feature" has since moved on to a third value — some un-journaled operation ran
        // between the journaled op and this undo. It must be left exactly where it is.
        pallet_utils::update_pallet_head(
            "feature",
            pallet_utils::get_pallet_head("feature").unwrap().as_deref(),
            &hash('5'),
        ).unwrap();

        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("feature".to_string(), hash('3'));
        restored.after.heads.insert("feature".to_string(), hash('4'));

        restore_refs(&restored).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('1')), "unmoved ref must be restored");
        assert_eq!(
            pallet_utils::get_pallet_head("feature").unwrap(),
            Some(hash('5')),
            "a ref moved by a later un-journaled op must be left untouched"
        );
    }

    #[test]
    fn restore_refs_never_touches_a_ref_the_operation_did_not_leave_behind() {
        let _scratch = Scratch::new("restore-refs-absent-after");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('8'),
        ).unwrap();

        // "@office" existed before the op but not after it (deleted by it); something has
        // recreated it since. With no "after" evidence it is never rolled back.
        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("@office".to_string(), hash('7'));

        restore_refs(&restored).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('1')));
        assert_eq!(pallet_utils::get_meta_pallet_head("office").unwrap(), Some(hash('8')));
    }

    #[test]
    fn restore_refs_never_resurrects_a_deleted_ref() {
        let _scratch = Scratch::new("restore-refs-deleted");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("gone".to_string(), hash('3'));
        restored.after.heads.insert("gone".to_string(), hash('4'));

        restore_refs(&restored).unwrap();

        assert!(!pallet_utils::does_pallet_exist("gone"));
    }

    #[test]
    fn restore_refs_clears_consolidation_when_the_entry_has_none() {
        let _scratch = Scratch::new("restore-clears-consolidation");

        merge_utils::write_consolidation_state(&ConsolidationState {
            their_head: hash('d'),
            their_pallet: "stray".to_string(),
            our_head: hash('e'), our_pallet: "main".to_string(),
        }).unwrap();
        pallet_utils::update_pallet_head("main", None, &hash('b')).unwrap();

        restore_refs(&entry("stack", "main", &hash('a'), &hash('b'))).unwrap();

        assert!(merge_utils::read_consolidation_state().unwrap().is_none());
    }

    #[test]
    fn restore_refs_puts_a_completed_cherry_pick_back_in_progress() {
        let _scratch = Scratch::new("restore-cherry-pick");

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        // The stack that completed the pick consumed its state file.
        let mut completed = entry("stack", "main", &hash('1'), &hash('2'));
        completed.before.cherry_pick = Some(PickInProgress { source: hash('c'), our_head: hash('e'), our_pallet: "main".to_string(), description: Some("pick".to_string()) });

        restore_refs(&completed).unwrap();

        let state = cherry_pick_utils::read_state().unwrap().expect("the pick must be in progress again");
        assert_eq!(state.source, hash('c'));
        assert_eq!(state.description.as_deref(), Some("pick"));

        // And an entry with no pick before it clears one.
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        restore_refs(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap();
        assert!(cherry_pick_utils::read_state().unwrap().is_none());
    }

    #[test]
    fn the_journal_is_capped_at_max_entries() {
        let _scratch = Scratch::new("journal-cap");

        for i in 0..(MAX_ENTRIES + 10) {
            pallet_utils::update_pallet_head(
                "main",
                pallet_utils::get_pallet_head("main").unwrap().as_deref(),
                &format!("{:064x}", i),
            ).unwrap();
            let before = capture().unwrap();
            pallet_utils::update_pallet_head(
                "main",
                pallet_utils::get_pallet_head("main").unwrap().as_deref(),
                &format!("{:064x}", i + 1),
            ).unwrap();
            push_if_changed("stack", before).unwrap();
        }

        assert_eq!(read().unwrap().len(), MAX_ENTRIES, "the journal must never grow past its cap");

        // The oldest entries are the ones dropped: the newest surviving entry is the very
        // last one pushed, and the oldest is the eleventh.
        let entries = read().unwrap();
        assert_eq!(entries[0].before.heads.get("main"), Some(&format!("{:064x}", 10)));
        let newest = pop().unwrap().unwrap();
        assert_eq!(newest.before.heads.get("main"), Some(&format!("{:064x}", MAX_ENTRIES + 9)));
    }
}
