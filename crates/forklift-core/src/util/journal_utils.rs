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

/// The journal file's format version. The file is `{"version":3,"entries":[...]}`; any version
/// but this and [`OLDER_JOURNAL_VERSION`], or anything that does not parse as this shape, is
/// refused by name — never read as an empty journal, which would make `undo` report "nothing to
/// undo" over a history it lost.
const JOURNAL_VERSION: u64 = 3;

/// The one older version still read. Version 3 only widened a snapshot's head to optional (a
/// removed ref, recorded by a delete), so a version-2 entry means exactly what it meant before.
const OLDER_JOURNAL_VERSION: u64 = 2;

/// The op name a pallet delete is journaled under. Its entry records the removed ref with its head
/// before and none after; the removal writes it itself (see
/// [`pallet_utils::remove_pallet_head_in`]).
pub const DELETE_OP: &str = "delete";

/// Cap the journal so it cannot grow without bound; the oldest entries fall off (undo is
/// a recent-history convenience, not an audit trail — that is what `history`/`audit` are).
pub const MAX_ENTRIES: usize = 100;

/// The mutable warehouse state on one side of a journaled operation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The bay's current pallet.
    pub current_pallet: String,

    /// Pallet refs (user and meta), keyed by wire form (`main`, `@office`) → head hash. A
    /// journaled entry keeps only the refs its operation moved, plus both sides' current pallets;
    /// a ref the operation left alone needs no restoring. `None` is a ref the operation removed:
    /// only a delete's `after` records one (see [`validate_entry`]).
    pub heads: BTreeMap<String, Option<String>>,

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
    /// The bay as it is now. Its head is the bay's base — the tree it holds — never a fresh read of
    /// the pallet's ref, which another bay can move without touching this bay.
    fn read() -> Result<BayState, String> {
        let position = pallet_utils::read_position()?;

        Ok(BayState {
            current_pallet: position.pallet,
            current_head: position.base,
            consolidation: read_consolidation()?,
            cherry_pick: read_cherry_pick()?,
        })
    }
}

impl Snapshot {
    /// The head of the current pallet, or `None` while it is unborn.
    pub fn current_head(&self) -> Option<&String> {
        self.heads.get(&self.current_pallet).and_then(Option::as_ref)
    }

    /// The head recorded for ref `wire`, or `None` when none is recorded or it was removed.
    pub fn head(&self, wire: &str) -> Option<&String> {
        self.heads.get(wire).and_then(Option::as_ref)
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
/// the bay's base, recorded as the current pallet's head.
///
/// # Returns
/// * `Ok(Snapshot)` - The snapshot.
/// * `Err(String)`  - If the current pallet, consolidation or cherry-pick state could not be read.
pub fn capture() -> Result<Snapshot, String> {
    not_journaled_marks().remove(&crate::globals::forklift_root());

    let position = pallet_utils::read_position()?;
    let heads = position.base
        .map(|base| BTreeMap::from([(position.pallet.clone(), Some(base))]))
        .unwrap_or_default();

    let snapshot = Snapshot {
        current_pallet: position.pallet,
        heads,
        consolidation: read_consolidation()?,
        cherry_pick: read_cherry_pick()?,
    };

    // Only once the snapshot is whole: a failed capture journals nothing, so it records nothing.
    pallet_utils::start_write_log();

    Ok(snapshot)
}

/// The warehouses whose running command [`not_journaled`] marked, keyed like the write log so
/// parallel tests in separate warehouses never see each other's mark. Cleared by [`capture`] and
/// [`discard`], consumed by [`push_if_changed`].
static NOT_JOURNALED: std::sync::Mutex<BTreeSet<PathBuf>> = std::sync::Mutex::new(BTreeSet::new());

fn not_journaled_marks() -> std::sync::MutexGuard<'static, BTreeSet<PathBuf>> {
    NOT_JOURNALED.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Mark the running command as one [`push_if_changed`] must not journal, though its kind is
/// journaled: the command itself knows which form it took.
pub fn not_journaled() {
    not_journaled_marks().insert(crate::globals::forklift_root());
}

/// Stop recording head writes without journaling them — for an operation that failed, which
/// [`push_if_changed`] must not journal.
pub fn discard() {
    not_journaled_marks().remove(&crate::globals::forklift_root());
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
/// reverts — someone else's move. A current pallet the operation did not write is recorded at the
/// bay's base, not its ref: the gate reads it, but there is nothing to restore.
///
/// # Arguments
/// * `op`     - The command's journal name.
/// * `before` - The snapshot captured by [`capture`] before the command ran.
pub fn push_if_changed(op: &str, mut before: Snapshot) -> Result<(), String> {
    let writes = pallet_utils::take_write_log();

    if not_journaled_marks().remove(&crate::globals::forklift_root()) {
        return Ok(());
    }

    // The undo journal reverses an operation by restoring refs, which cannot un-create a
    // pallet's very first parcel (that would need deleting the ref). So an operation starting
    // from an unborn pallet is not journaled.
    if before.current_head().is_none() {
        return Ok(());
    }

    let position = pallet_utils::read_position()?;
    let before_base = before.current_head().cloned();
    let mut after = Snapshot {
        current_pallet: position.pallet,
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
                before.heads.insert(wire.clone(), Some(old));
            }
        }

        after.heads.insert(wire, new);
    }

    let unwritten: BTreeSet<String> = [&before.current_pallet, &after.current_pallet].into_iter()
        .filter(|pallet| !after.heads.contains_key(*pallet))
        .cloned()
        .collect();

    // An unwritten current pallet is recorded at the base the bay stood on, on each side it was
    // current (the other side mirrors it): the gate compares the base. Its two sides differ only
    // when the bay's base moved to a parcel this operation built, which undo then puts back.
    for pallet in unwritten {
        let was = (pallet == before.current_pallet).then(|| before_base.clone()).flatten();
        let is = (pallet == after.current_pallet).then(|| position.base.clone()).flatten();

        if let Some(head) = was.clone().or(is.clone()) {
            before.heads.insert(pallet.clone(), Some(head.clone()));
            after.heads.insert(pallet, Some(is.or(was).unwrap_or(head)));
        }
    }

    if before.bay_state() == after.bay_state() {
        return Ok(());
    }

    let entry = JournalEntry { op: op.to_string(), before, after };
    validate_entry(&entry)?;

    let mut entries = read()?;
    push_capped(&mut entries, entry);

    write(&entries)
}

/// Append `entry`, dropping the oldest entries past [`MAX_ENTRIES`].
fn push_capped(entries: &mut Vec<JournalEntry>, entry: JournalEntry) {
    entries.push(entry);

    let len = entries.len();
    if len > MAX_ENTRIES {
        entries.drain(0..len - MAX_ENTRIES);
    }
}

/// Append `entry` to the bay's journal for a ref removal, inside its
/// [`RefLock`](crate::util::lock_utils::RefLock) hold and before the unlink, so the entry that
/// keeps the removed head rooted exists before the ref stops reaching it. The write is durable when
/// this returns (the caller refuses inside a bulk store session).
///
/// # Returns
/// * `Ok(Vec<JournalEntry>)` - The entries as read before the append, for
///                             [`write_back_in_hold`] if the removal does not land.
/// * `Err(String)`           - The journal could not be read or written; nothing was appended.
pub(crate) fn append_in_hold(_lock: &crate::util::lock_utils::RefLock,
                             entry: &JournalEntry) -> Result<Vec<JournalEntry>, String> {
    validate_entry(entry)?;

    let read = read()?;
    let mut entries = read.clone();
    push_capped(&mut entries, entry.clone());
    write(&entries)?;

    Ok(read)
}

/// Write the bay's journal back to `entries`, as read by [`append_in_hold`] before a removal that
/// did not land.
pub(crate) fn write_back_in_hold(_lock: &crate::util::lock_utils::RefLock,
                                 entries: &[JournalEntry]) -> Result<(), String> {
    write(entries)
}

/// The removed heads of every delete entry in every bay's journal, the main tree's included: the
/// heads an undo can still put back, which gc, compaction and scope-prune keep. Read under the
/// same rule as the rest of a bay's state: a journal that cannot be read is an error, so a sweep
/// fails closed rather than freeing what it might hold.
///
/// # Returns
/// * `Ok(Vec<String>)` - The removed heads, in bay order.
/// * `Err(String)`     - If the bays could not be listed or a journal could not be read.
pub fn removed_heads() -> Result<Vec<String>, String> {
    let mut heads = Vec::new();

    for dir in crate::util::bay_utils::all_bay_state_dirs()? {
        heads.append(&mut removed_heads_in(&dir)?);
    }

    Ok(heads)
}

/// [`removed_heads`] for the one bay whose state lives in `dir`.
pub fn removed_heads_in(dir: &Path) -> Result<Vec<String>, String> {
    Ok(read_in(dir)?.iter().flat_map(removed_by).map(|(_, head)| head).collect())
}

/// The refs `entry` removed, with the head each held: `(wire, head)`.
pub fn removed_by(entry: &JournalEntry) -> Vec<(String, String)> {
    entry.after.heads.iter()
        .filter(|(_, after)| after.is_none())
        .filter_map(|(wire, _)| entry.before.head(wire).map(|head| (wire.clone(), head.clone())))
        .collect()
}

/// A delete entry removing `victim` at `head`, from a bay on pallet "current".
#[cfg(test)]
pub(crate) fn delete_entry_for_test(victim: &str, head: &str) -> JournalEntry {
    let current = "0".repeat(64);
    let mut before = BTreeMap::new();
    before.insert("current".to_string(), Some(current.clone()));
    before.insert(victim.to_string(), Some(head.to_string()));
    let mut after = BTreeMap::new();
    after.insert("current".to_string(), Some(current));
    after.insert(victim.to_string(), None);

    JournalEntry {
        op: DELETE_OP.to_string(),
        before: Snapshot { current_pallet: "current".to_string(), heads: before, consolidation: None, cherry_pick: None },
        after: Snapshot { current_pallet: "current".to_string(), heads: after, consolidation: None, cherry_pick: None },
    }
}

/// The undo entry for deleting user pallet `wire` at `head` from this bay: the bay's state on both
/// sides (a delete changes none of it), with the pallet at `head` before and removed after. The
/// delete passes it to [`pallet_utils::remove_pallet_head_in`], which writes it in the removal's
/// own hold; [`push_if_changed`] never sees a delete.
///
/// # Returns
/// * `Ok(JournalEntry)` - The entry.
/// * `Err(String)`      - If the bay's position, consolidation or cherry-pick could not be read.
pub fn delete_entry(wire: &str, head: &str) -> Result<JournalEntry, String> {
    let position = pallet_utils::read_position()?;
    let heads = position.base
        .map(|base| BTreeMap::from([(position.pallet.clone(), Some(base))]))
        .unwrap_or_default();

    let mut after = Snapshot {
        current_pallet: position.pallet,
        heads,
        consolidation: read_consolidation()?,
        cherry_pick: read_cherry_pick()?,
    };
    let mut before = after.clone();
    before.heads.insert(wire.to_string(), Some(head.to_string()));
    after.heads.insert(wire.to_string(), None);

    Ok(JournalEntry { op: DELETE_OP.to_string(), before, after })
}

/// How [`restore_deleted`] brought a deleted pallet back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Restored {
    /// The undo created the ref.
    Created,

    /// The name already held the deleted head: an undo interrupted before its pop, a write that
    /// landed while reporting failure, or another writer creating the same head.
    AlreadyThere,
}

/// A deleted pallet brought back by [`restore_deleted`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RestoredPallet {
    /// The pallet the delete removed.
    pub deleted: String,

    /// The name it now stands under: the deleted name, or the one `undo --as` gave.
    pub name: String,

    /// Its head, the one it was deleted at.
    pub head: String,

    pub outcome: Restored,
}

/// Bring back the pallet a delete entry removed, under its own name or, for `undo --as`, under
/// `as_name`. The entry is not popped: the caller pops it once this succeeds.
///
/// What the name holds decides, in order:
/// * absent: the ref is created at the deleted head, by compare-and-set from absent, once no bay
///   stands on the name out of step with that head, the head's history is complete within the
///   fetch scope, and the history passes the warehouse's trust check as it stands now (the anchor
///   and office read for that check must still hold when the ref is written);
/// * the deleted head: the pallet is back already, and that counts as restored;
/// * any other head: the name was created again since, and the undo refuses, keeping the entry. A
///   plain undo names `undo --as`; `undo --as` asks for another name.
///
/// A compare-and-set lost to a writer that created the name meanwhile is judged the same way from
/// the head it found, never swallowed.
///
/// # Arguments
/// * `entry`   - The delete entry `undo` is reversing.
/// * `as_name` - The user pallet to create instead of the deleted one (`undo --as`).
///
/// # Returns
/// * `Ok(RestoredPallet)` - The pallet is back.
/// * `Err(String)`        - The refusal or failure; nothing was written, unless the write failed
///                          after landing, which the next undo finds as restored.
pub fn restore_deleted(entry: &JournalEntry, as_name: Option<&str>) -> Result<RestoredPallet, String> {
    let [(wire, head)] = <[(String, String); 1]>::try_from(removed_by(entry)).map_err(|_| format!(
        "The journal entry for \"{}\" removed no pallet, so there is no deleted pallet to bring back.",
        entry.op
    ))?;

    let deleted = PalletRef::parse(&wire)?;
    let name = match as_name {
        Some(name) => {
            pallet_utils::validate_pallet_name(name)?;
            name.to_string()
        }
        None => deleted.name.clone(),
    };

    let restored = |outcome| RestoredPallet { deleted: wire.clone(), name: name.clone(), head: head.clone(), outcome };

    // A read error is reported once no colliding pallet below explains it.
    let exact = pallet_utils::get_pallet_head_in(PalletNamespace::User, &name);
    if let Ok(Some(found)) = &exact {
        return settle_on(&wire, &name, as_name.is_some(), &head, Some(found.clone())).map(restored);
    }

    // The delete pruned emptied ref folders, so a pallet created since can stand where the ref
    // would go: a folder of the name, or under it.
    let nested = |outer: &str, inner: &str| inner.starts_with(&format!("{}/", outer));
    let users = pallet_utils::list_pallets_in(PalletNamespace::User)?;
    if let Some(other) = users.iter().find(|other| nested(other, &name) || nested(&name, other)) {
        return Err(match as_name {
            Some(_) => format!(
                "Pallet name \"{}\" collides with pallet \"{}\" (one name is a folder of the \
                other); bring deleted pallet \"{}\" back under another name. Nothing was undone.",
                name, other, wire
            ),
            None => format!(
                "Pallet \"{}\" was deleted at {}, and pallet \"{}\" now stands where its ref \
                would go (one name is a folder of the other). Undo will not overwrite it. To bring \
                the deleted pallet back under another name, run \"forklift undo --as <new-name>\". \
                Nothing was undone.",
                wire, head, other
            ),
        });
    }

    exact?;

    let rerun = match as_name {
        Some(as_name) => format!("forklift undo --as {}", as_name),
        None => "forklift undo".to_string(),
    };
    refuse_a_bay_out_of_step(&name, &head, "undo", &rerun, "Nothing was undone.")?;

    let preconditions = trust_preconditions(&wire, &head, "Nothing was undone.")?;

    // The ref write verifies the history is still complete; a refusal names what to run instead.
    create_deleted(&wire, &name, as_name.is_some(), &head, &preconditions)
        .map_err(|e| match e {
            CreateRefused::Incomplete(missing) => format!(
                "The history of deleted pallet \"{}\" ({}) is no longer complete in this warehouse: {} \
                Nothing was undone. If a remote holds that history, \"forklift lower {}\" brings the \
                pallet back from it; otherwise the history is gone.",
                wire, head, missing, deleted.name
            ),
            CreateRefused::Other(error) => error,
        })
        .map(restored)
}

/// Why [`create_deleted`] did not create the pallet.
#[derive(Debug, PartialEq)]
enum CreateRefused {
    /// The deleted head's history is no longer completely present (what is missing).
    Incomplete(String),
    /// Anything else, as the message to report.
    Other(String),
}

/// The compare-and-set [`restore_deleted`] ends in: create `name` at `head`, judging a lost race
/// by the head the name was found at, and a failed write by reading the name back. In a served
/// warehouse the delete left a tombstone at `head`, which the creation acknowledges and clears; one
/// left under `name` at another head refuses it.
fn create_deleted(wire: &str,
                  name: &str,
                  renamed: bool,
                  head: &str,
                  preconditions: &pallet_utils::RefPreconditions) -> Result<Restored, CreateRefused> {
    match pallet_utils::recreate_pallet_head(name, head, preconditions, head) {
        Ok(()) => Ok(Restored::Created),
        Err(RefUpdateError::Moved(moved)) => settle_on(wire, name, renamed, head, moved.found).map_err(CreateRefused::Other),
        Err(RefUpdateError::Failed(error)) => match pallet_utils::get_pallet_head(name) {
            Ok(Some(found)) if found == head => Ok(Restored::Created),
            _ => Err(CreateRefused::Other(format!("{} The undo entry is kept; run \"forklift undo\" again.", error))),
        },
        Err(RefUpdateError::Incomplete { missing, .. }) => Err(CreateRefused::Incomplete(missing)),
        Err(error) => Err(CreateRefused::Other(error.to_string())),
    }
}

/// Judge a name found holding `found` while bringing back `wire`'s `head` (see [`restore_deleted`]).
fn settle_on(wire: &str, name: &str, renamed: bool, head: &str, found: Option<String>) -> Result<Restored, String> {
    match found {
        Some(found) if found == head => Ok(Restored::AlreadyThere),
        Some(found) if renamed => Err(format!(
            "Pallet \"{}\" already exists (at {}); bring deleted pallet \"{}\" back under another \
            name. Nothing was undone.",
            name, found, wire
        )),
        Some(found) => Err(format!(
            "Pallet \"{}\" was deleted at {}, and a pallet of that name exists again, at {}. Undo \
            will not overwrite it. To bring the deleted pallet back under another name, run \
            \"forklift undo --as <new-name>\". Nothing was undone.",
            wire, head, found
        )),
        None => Err(format!(
            "Pallet \"{}\" changed while undo was creating it; nothing was undone. Run \"forklift \
            undo\" again.",
            name
        )),
    }
}

/// Refuse to create `name` at `head` while a checkout stands on the name at no base or another one:
/// the ref would appear under it out of step with its files. A checkout at exactly `head` is the
/// usual loser of a delete's race with a shift, and the restore puts it back in step. `act` names
/// the command in the refusal, `rerun` what to run again, and `closing` ends it.
pub(crate) fn refuse_a_bay_out_of_step(name: &str,
                                       head: &str,
                                       act: &str,
                                       rerun: &str,
                                       closing: &str) -> Result<(), String> {
    for (which, position) in crate::util::bay_utils::bays_on(name)? {
        if position.base.as_deref() == Some(head) {
            continue;
        }

        let at = match &position.base {
            Some(base) => format!("at {}", base),
            None => "on nothing stacked".to_string(),
        };
        let mut which = which;
        which[..1].make_ascii_uppercase();

        return Err(format!(
            "{} stands on pallet \"{}\" {}, and the {} would create that pallet at {}, out of \
            step with its files. Shift it to another pallet, run \"{}\" again, then shift it back. \
            {}",
            which, name, at, act, head, rerun, closing
        ));
    }

    Ok(())
}

/// The trust state a restored head must pass, and the preconditions that pin it for the write: in
/// a warehouse with an anchor, `head`'s history must verify against the anchor and office as they
/// are now, and both must still hold when the ref is written; without one, there must still be
/// none, so an enrollment cannot slip in between and miss the head. `closing` ends a refusal.
pub(crate) fn trust_preconditions(wire: &str, head: &str, closing: &str) -> Result<pallet_utils::RefPreconditions, String> {
    use crate::util::{audit_utils, office_utils};
    use pallet_utils::{AnchorPrecondition, OfficePrecondition, PalletsPrecondition, RefPreconditions};

    let Some((anchor, bytes)) = office_utils::read_trust_anchor_with_bytes()? else {
        return Ok(RefPreconditions {
            office: OfficePrecondition::NotConsumed,
            anchor: AnchorPrecondition::Absent,
            pallets: PalletsPrecondition::NotConsumed,
        });
    };

    let Some(office_head) = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)? else {
        return Err(format!("Trust is established but the office pallet is missing. {}", closing));
    };

    let untrusted = |e: String| format!(
        "The history of deleted pallet \"{}\" no longer passes this warehouse's trust check: {} {}",
        wire, e, closing
    );

    let office_state = audit_utils::verify_office_chain(&anchor, &office_head).map_err(|e| untrusted(e.into()))?;
    audit_utils::verify_pallet_history(head, &anchor, &office_state, &[]).map_err(|e| untrusted(e.into()))?;

    Ok(RefPreconditions {
        office: OfficePrecondition::At(Some(office_head)),
        anchor: AnchorPrecondition::Bytes(bytes),
        pallets: PalletsPrecondition::NotConsumed,
    })
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
/// standing on the same base, and the same consolidation and cherry-pick in progress (a
/// conflicted cherry-pick moves no ref). Or whether an undo of `entry` stopped halfway, after
/// writing the bay's earlier position and before putting the refs back (see
/// [`is_half_undone`]), which a retry resumes. Anything else means an
/// operation outside the journal ran since (`palletize` and `deliver` switch the current pallet,
/// a fast-forward `lower` or a haul merge moves its head), and reversing `entry` now would undo an
/// older operation than the user's last. Refs other than the current pallet are not compared: they
/// are shared by every bay, and `restore_refs` already leaves any that moved alone.
///
/// A bay that matches is still refused when the entry would put back a cherry-pick whose source
/// this store no longer holds (see [`refuse_a_pick_without_its_source`]), and, in a served
/// warehouse, when it would put a user pallet back (see [`refuse_a_served_rewind`]). A half-undone
/// entry passes there, so its retry settles the bay.
///
/// A delete is never refused here, nor is the bay read for it: it changed no bay state and puts
/// none back, so nothing the bay did since stands between it and its undo.
///
/// # Arguments
/// * `entry` - The entry `undo` is about to reverse.
///
/// # Returns
/// * `Ok(())`      - The bay matches; the entry may be reversed.
/// * `Err(String)` - The refusal, naming what differs or the lost pick source; or the state could
///                   not be read.
pub fn check_undoable(entry: &JournalEntry) -> Result<(), String> {
    if entry.op == DELETE_OP {
        return Ok(());
    }

    let now = BayState::read()?;
    let after = entry.after.bay_state();

    // A half-undone entry is resumed even in a served warehouse: putting its refs back is refused
    // there, and that refusal puts the bay back at the operation's head (see `restore_refs`).
    if now != after && is_half_undone(entry, &now)? {
        return Ok(());
    }

    // Judged only once the bay still matches the entry, so a bay that moved on is refused for
    // that, as anywhere else, and never told to revert an operation that is no longer its last.
    if now == after {
        refuse_a_served_rewind(entry)?;
        return refuse_a_pick_without_its_source(entry);
    }

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

/// Whether the bay is where an undo of `entry` that stopped halfway left it: [`restore_refs`]
/// writes the earlier position before it puts any ref back, so a crash or failure between the two
/// leaves the position `entry.before` records with the consolidation and cherry-pick still
/// `entry.after`'s. A ref still at `entry.before`'s head is the other window — the undo finished
/// and only its journal pop was lost — and is not this state. A `shift` entry is undone by
/// shifting back, which writes no ref, so it never stops halfway.
fn is_half_undone(entry: &JournalEntry, now: &BayState) -> Result<bool, String> {
    let before = entry.before.bay_state();

    if entry.op == "shift"
        || now.current_pallet != before.current_pallet
        || now.current_head != before.current_head
        || now.consolidation != entry.after.consolidation
        || now.cherry_pick != entry.after.cherry_pick {
        return Ok(false);
    }

    Ok(pallet_utils::get_pallet_head(&before.current_pallet)? != before.current_head)
}

/// Refuses, in a served warehouse, an undo that would put a user pallet back. Every head a
/// journaled operation wrote moved its pallet forward, so putting one back leaves a head the
/// warehouse's clones may rely on, which nothing would root once the ref leaves it; the ref write
/// itself is refused too (see [`pallet_utils::update_pallet_head_in`]), and this refuses before
/// anything is written. Only a pallet still at the head the operation left counts: one another
/// writer moved on since is left alone by the put-back, so it moves nothing back. A `shift` is
/// undone by shifting back, which puts no ref back.
fn refuse_a_served_rewind(entry: &JournalEntry) -> Result<(), String> {
    if entry.op == "shift" {
        return Ok(());
    }

    // In `put_back_refs`' order, so the refusal names the current pallet when it is one.
    for wire in put_back_order(entry) {
        let (Some(pre_head), Some(Some(post_head))) = (entry.before.head(wire), entry.after.heads.get(wire)) else {
            continue;
        };

        let reference = PalletRef::parse(wire)?;

        if pre_head == post_head
            || reference.namespace != PalletNamespace::User
            || pallet_utils::get_pallet_head(&reference.name)?.as_deref() != Some(post_head.as_str())
            // Read last, so an undo that moves no pallet back never depends on the mark.
            || !crate::util::tombstone_utils::is_served()? {
            continue;
        }

        return Err(format!(
            "Undoing \"{}\" would move pallet \"{}\" back off the head that operation left, and \
            this warehouse has been served, so its clones may rely on that head. Nothing was undone, \
            and undo cannot reach an older operation past this one. To take the change out, stack a \
            parcel that reverts it; a pallet deleted since this warehouse was served comes back \
            with \"forklift palletize --restore <name>\".",
            entry.op, crate::error::sanitize(&reference.name)
        ));
    }

    Ok(())
}

/// Refuses an undo that would put back a cherry-pick whose source this store no longer holds. The
/// journal is no gc root, and the stack that completed a pick is single-parent, so once the pick
/// is complete nothing need reach its source. A pick put back without it could never complete.
/// A consolidation's `their_head` needs no such check: the completing stack made it a parent of
/// the head this undo starts from.
fn refuse_a_pick_without_its_source(entry: &JournalEntry) -> Result<(), String> {
    let Some(pick) = &entry.before.cherry_pick else { return Ok(()) };

    if file_utils::does_object_exist(&pick.source)? {
        return Ok(());
    }

    Err(format!(
        "Undoing \"{}\" would put the cherry-pick of {} back in progress, but this warehouse no \
        longer holds that parcel, so the pick could never be completed again. Nothing was undone. \
        The parcel that completed the pick stays; to change it, stack another on top.",
        entry.op, pick.source
    ))
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
/// The bay's position goes back first, to the base `entry.before` records — the tree the bay held
/// before the operation, never a fresh read of the ref. Then the current pallet's ref: if it moved
/// after [`check_undoable`] passed, the undo is refused with that check's refusal, before any other
/// ref or any merge state is touched. A ref whose heads are equal on both sides is not put back at
/// all, so a ref another bay moved since cannot refuse it. That covers every ref the operation
/// never wrote but one: a stack whose compare-and-set found its own parcel already on the pallet
/// moves the bay's base without a logged write, and that parcel, the operation's own, is put back
/// like a written one.
///
/// When putting the refs back fails, the position is decided by what the current pallet's ref
/// holds now, never by the kind of error (a write can fail after it landed): at the head the
/// operation left, or one descending from it, the bay goes back to `entry.after`'s position, whose
/// tree the soft reset kept; anywhere else the earlier position stays, and its difference to the
/// files carries every change the undo kept. A crash between the position write and the refs
/// leaves the half-undone state [`check_undoable`] accepts, and a retry resumes here.
///
/// # Arguments
/// * `entry` - The entry to reverse.
pub fn restore_refs(entry: &JournalEntry) -> Result<(), String> {
    let current = &entry.after.current_pallet;

    pallet_utils::write_position(&pallet_utils::BayPosition::new(
        &entry.before.current_pallet, entry.before.current_head().cloned(),
    ))?;

    if let Err(error) = put_back_refs(entry) {
        // Which base the bay stands on now: the operation's result while the ref still holds it
        // or a parcel built on it, else the earlier one. An ancestry that cannot be read keeps the
        // earlier base, whose catch-up parks the operation's change rather than losing it.
        let operation_left = entry.after.head(current);

        let keeps_operation = match (pallet_utils::get_pallet_head(current), operation_left) {
            (Ok(Some(head)), Some(left)) => head == *left || merge_utils::is_ancestor(left, &head).unwrap_or(false),
            _ => false,
        };

        if keeps_operation {
            pallet_utils::write_position(&pallet_utils::BayPosition::new(current, operation_left.cloned()))
                .map_err(|e| format!("{} {}", error, e))?;
        }

        return Err(error);
    }

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

/// Put back the refs `entry`'s operation wrote, the current pallet's first: see [`restore_refs`].
fn put_back_refs(entry: &JournalEntry) -> Result<(), String> {
    let current = &entry.after.current_pallet;

    for wire in put_back_order(entry) {
        let (Some(pre_head), Some(post_after)) = (entry.before.head(wire), entry.after.heads.get(wire)) else {
            continue;
        };

        // A removed ref is put back by the delete's own restore, never here.
        let Some(post_head) = post_after else {
            return Err(format!(
                "The journal entry for \"{}\" removed pallet \"{}\", which this restore does not put back.",
                entry.op, wire
            ));
        };

        // The heads are equal: there is nothing to put back.
        if pre_head == post_head {
            continue;
        }

        let reference = PalletRef::parse(wire)?;

        match pallet_utils::update_pallet_head_in(reference.namespace, &reference.name, Some(post_head), pre_head, &pallet_utils::RefPreconditions::NONE) {
            Ok(()) => {}
            Err(RefUpdateError::Moved(_)) if wire == current => {
                return Err(current_pallet_moved(current, &entry.op));
            }
            // Another writer moved it after the operation: theirs stands.
            Err(RefUpdateError::Moved(_)) => {}
            // The write failed, or was refused because the pre-head's history is no longer
            // complete (no precondition is consumed, so nothing else refuses it): put the current
            // pallet back where the operation left it, so the bay still matches the entry and a
            // retried undo passes the gate again.
            Err(e) => return Err(un_restore_current(entry, e.to_string())),
        }
    }

    Ok(())
}

/// The wires of `entry`'s refs in the order an undo puts them back: the current pallet first.
fn put_back_order(entry: &JournalEntry) -> impl Iterator<Item = &String> {
    let current = &entry.after.current_pallet;

    entry.before.heads.keys()
        .filter(move |wire| *wire == current)
        .chain(entry.before.heads.keys().filter(move |wire| *wire != current))
}

/// After a ref write failed partway through [`restore_refs`], move the current pallet's ref back
/// from its restored head to the one the operation left (if it was restored), and return the
/// error to report. Other refs restored before the failure stay restored: a retried undo finds
/// them moved off the operation's heads and leaves them alone.
fn un_restore_current(entry: &JournalEntry, error: String) -> String {
    let current = &entry.after.current_pallet;
    let (Some(pre_head), Some(post_head)) = (entry.before.head(current), entry.after.head(current)) else {
        return error;
    };

    let e = match pallet_utils::update_pallet_head_in(PalletNamespace::User, current, Some(pre_head), post_head, &pallet_utils::RefPreconditions::NONE) {
        Ok(()) | Err(RefUpdateError::Moved(_)) => return error,
        Err(e) => e.to_string(),
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

/// The journal file path of the bay whose state lives in `dir`.
fn path_in(dir: &Path) -> PathBuf {
    dir.join(FILE_NAME_JOURNAL)
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

        if head.as_ref().is_some_and(|head| !object_utils::is_object_hash(head)) {
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

/// Check that an entry holds only what the warehouse could have written: two valid snapshots,
/// and a removed ref only where a delete recorded it — in `after`, exactly one, a user pallet with
/// a head before, never a current pallet. So every restore but the delete's meets no removal.
pub(crate) fn validate_entry(entry: &JournalEntry) -> Result<(), String> {
    validate_snapshot(&entry.before)?;
    validate_snapshot(&entry.after)?;

    if entry.before.heads.values().any(Option::is_none) {
        return Err("records a removed ref before its operation".to_string());
    }

    let removed: Vec<&String> = entry.after.heads.iter()
        .filter(|(_, head)| head.is_none())
        .map(|(wire, _)| wire)
        .collect();

    let well_formed = match removed.as_slice() {
        [] => entry.op != DELETE_OP,
        [wire] => entry.op == DELETE_OP
            && entry.before.head(wire).is_some()
            && **wire != entry.after.current_pallet
            && **wire != entry.before.current_pallet
            && PalletRef::parse(wire).is_ok_and(|reference| reference.namespace == PalletNamespace::User),
        _ => false,
    };

    if !well_formed {
        return Err(format!("records the \"{}\" operation with a removal it cannot have made", entry.op));
    }

    Ok(())
}

/// Read the active bay's journal (see [`read_from`]).
fn read() -> Result<Vec<JournalEntry>, String> {
    read_from(&path())
}

/// Read the journal of the bay whose state lives in `dir` (see [`read_from`]).
fn read_in(dir: &Path) -> Result<Vec<JournalEntry>, String> {
    read_from(&path_in(dir))
}

/// Read the journal at `path`. A missing file is an empty journal; a file that cannot be read,
/// does not parse, has a version this forklift does not read or holds an impossible value is an
/// error naming the file.
fn read_from(path: &Path) -> Result<Vec<JournalEntry>, String> {
    let path = path.to_path_buf();

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
            (it reads versions {} and {})", version, OLDER_JOURNAL_VERSION, JOURNAL_VERSION
        )))?,
        None => return Err(unreadable(&path, "is not a readable journal (it records no format version)")),
    };

    if version != JOURNAL_VERSION && version != OLDER_JOURNAL_VERSION {
        return Err(unreadable(&path, &format!(
            "has format version {}, which this forklift does not understand (it reads versions {} and {})",
            version, OLDER_JOURNAL_VERSION, JOURNAL_VERSION
        )));
    }

    let contents: JournalContents = serde_json::from_value(value)
        .map_err(|e| unreadable(&path, &format!("is malformed ({})", e)))?;

    for entry in &contents.entries {
        validate_entry(entry)
            .map_err(|reason| unreadable(&path, &format!("is malformed (an entry {})", reason)))?;

        // A version-2 entry means exactly what it meant before: it cannot record a removal.
        if version == OLDER_JOURNAL_VERSION && !removed_by(entry).is_empty() {
            return Err(unreadable(&path, &format!(
                "is malformed (an entry records a removed ref, which a version {} journal cannot hold)",
                OLDER_JOURNAL_VERSION
            )));
        }
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
            crate::util::pallet_utils::create_position_if_not_exists().unwrap();

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
        crate::util::pallet_utils::tests::stub_parcel(&digit.to_string())
    }

    /// A snapshot on `pallet` holding only that pallet's ref at `head`, with no consolidation.
    fn snapshot(pallet: &str, head: &str) -> Snapshot {
        let mut heads = BTreeMap::new();
        heads.insert(pallet.to_string(), Some(head.to_string()));

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

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();
        advance(
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

    /// Only a delete records a removal, and it writes its entry itself: an ordinary journaled
    /// command whose writes include one refuses to journal it, leaving the journal readable,
    /// rather than writing an entry every later read would refuse. Mutation: drop the entry
    /// check from `push_if_changed` → red (the journal becomes unreadable).
    #[test]
    fn push_if_changed_refuses_an_entry_with_a_removal() {
        let _scratch = Scratch::new("push-removal");
        advance("main", None, &hash('1')).unwrap();
        pallet_utils::update_pallet_head("side", None, &hash('3'), &pallet_utils::RefPreconditions::NONE).unwrap();

        let before = capture().unwrap();
        advance("main", Some(&hash('1')), &hash('2')).unwrap();
        let delete = delete_entry("side", &hash('3'));
        pallet_utils::remove_pallet_head_in(PalletNamespace::User, "side", &hash('3'),
                                            &pallet_utils::RefPreconditions::NONE, Some(&delete)).unwrap();

        let refused = push_if_changed("stack", before);
        assert!(refused.is_err_and(|error| error.contains("with a removal it cannot have made")));
        assert_eq!(read().unwrap(), vec![delete], "the journal stays readable, with nothing more journaled");
    }

    #[test]
    fn push_if_changed_is_a_noop_when_the_current_pallet_is_unborn() {
        let _scratch = Scratch::new("push-unborn");

        // The default pallet has never been stacked to, so `heads` cannot contain it —
        // exactly the "stack onto an unborn pallet" case `push_if_changed` opts out of.
        let before = capture().unwrap();
        advance(
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

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        let before = capture().unwrap();

        // The operation itself: the head moves, a consolidation starts and another ref moves.
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        advance(
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
        assert_eq!(popped.after.head("main"), Some(&hash('b')));
        assert_eq!(popped.after.head("side"), Some(&hash('c')));
        assert_eq!(popped.after.consolidation, Some(consolidating(&hash('d'), "side")));
        assert!(pop().unwrap().is_none(), "popping removes the entry");
    }

    #[test]
    fn push_if_changed_keeps_only_the_refs_the_operation_moved() {
        let _scratch = Scratch::new("push-prunes");

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        advance(
            "idle",
            pallet_utils::get_pallet_head("idle").unwrap().as_deref(),
            &hash('e'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('f'),
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('1'),
        ).unwrap();
        let before = capture().unwrap();

        // The operation moves "main" and "side"; "idle" and "@office" stay where they were.
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        push_if_changed("stack", before).unwrap();

        let popped = pop().unwrap().unwrap();
        let keys = |heads: &BTreeMap<String, Option<String>>| heads.keys().cloned().collect::<Vec<_>>();
        assert_eq!(keys(&popped.before.heads), ["main", "side"]);
        assert_eq!(keys(&popped.after.heads), ["main", "side"]);
    }

    /// Move `pallet` to `new` as an operation in this bay does: the ref, and this bay's position
    /// with it when `pallet` is the one it stands on.
    fn advance(pallet: &str, expected: Option<&str>, new: &str) -> Result<(), pallet_utils::RefUpdateError> {
        pallet_utils::update_pallet_head(pallet, expected, new, &pallet_utils::RefPreconditions::NONE)?;

        if pallet_utils::read_position().unwrap().pallet == pallet {
            pallet_utils::write_position(&pallet_utils::BayPosition::new(pallet, Some(new.to_string()))).unwrap();
        }

        Ok(())
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

        advance("main", None, &hash('a')).unwrap();
        advance("side", None, &hash('1')).unwrap();
        let before = capture().unwrap();

        // The operation moves "main" twice; meanwhile another bay moves "side".
        advance("main", Some(&hash('a')), &hash('b')).unwrap();
        move_elsewhere("side", &hash('2'));
        advance("main", Some(&hash('b')), &hash('c')).unwrap();
        push_if_changed("stack", before).unwrap();

        let entry = peek().unwrap().expect("the operation moved the current pallet");
        assert_eq!(entry.before.heads, BTreeMap::from([("main".to_string(), Some(hash('a')))]),
            "before holds each written ref's first replaced head, and nothing else");
        assert_eq!(entry.after.heads, BTreeMap::from([("main".to_string(), Some(hash('c')))]),
            "after holds each written ref's last written head, and nothing else");

        restore_refs(&entry).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('a')));
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(hash('2')),
            "the other bay's move must stand");
    }

    #[test]
    fn a_ref_the_operation_created_has_no_before_however_often_it_moved() {
        let _scratch = Scratch::new("journal-created");

        advance("main", None, &hash('a')).unwrap();
        let before = capture().unwrap();

        advance("main", Some(&hash('a')), &hash('b')).unwrap();
        advance("fresh", None, &hash('1')).unwrap();
        advance("fresh", Some(&hash('1')), &hash('2')).unwrap();
        push_if_changed("haul", before).unwrap();

        let entry = peek().unwrap().expect("the operation moved the current pallet");
        assert!(!entry.before.heads.contains_key("fresh"),
            "a created ref must have no before, not its intermediate head: {:?}", entry.before.heads);
        assert_eq!(entry.after.head("fresh"), Some(&hash('2')));
    }

    #[test]
    fn a_failed_capture_records_no_writes() {
        let _scratch = Scratch::new("journal-capture-fails");

        advance("main", None, &hash('a')).unwrap();
        std::fs::write(crate::globals::bay_root().join("consolidation"), "not a state").unwrap();
        assert!(capture().is_err(), "a malformed consolidation state must fail the capture");

        advance("main", Some(&hash('a')), &hash('b')).unwrap();
        assert!(pallet_utils::take_write_log().is_empty(), "nothing may record for a failed capture");
    }

    #[test]
    fn push_if_changed_skips_a_noop_whose_current_pallet_another_bay_moved() {
        let _scratch = Scratch::new("push-moved-elsewhere");

        advance("main", None, &hash('a')).unwrap();
        let before = capture().unwrap();

        // The command writes nothing; another bay stacks on the current pallet meanwhile.
        move_elsewhere("main", &hash('b'));
        push_if_changed("stack", before).unwrap();

        assert!(pop().unwrap().is_none(), "another bay's move is not this operation's to journal");
    }

    #[test]
    fn push_if_changed_keeps_both_current_pallets_across_a_switch() {
        let _scratch = Scratch::new("push-prunes-switch");

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        pallet_utils::write_position(&pallet_utils::BayPosition::new("side", pallet_utils::get_pallet_head("side").unwrap())).unwrap();
        push_if_changed("shift", before).unwrap();

        let popped = pop().unwrap().unwrap();
        assert_eq!(popped.before.current_head(), Some(&hash('a')));
        assert_eq!(popped.after.current_head(), Some(&hash('b')));
    }

    #[test]
    fn push_if_changed_records_a_conflicted_cherry_pick() {
        let _scratch = Scratch::new("push-cherry-pick");

        advance(
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

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        pallet_utils::write_position(&pallet_utils::BayPosition::new("side", pallet_utils::get_pallet_head("side").unwrap())).unwrap();
        push_if_changed("shift", before).unwrap();

        let popped = pop().unwrap().expect("a shift must be journaled");
        assert_eq!(popped.before.current_pallet, "main");
        assert_eq!(popped.after.current_pallet, "side");
    }

    #[test]
    fn push_if_changed_skips_a_true_noop() {
        let _scratch = Scratch::new("push-noop");

        advance(
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

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('a'),
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('b'),
        ).unwrap();
        let before = capture().unwrap();

        // Only a ref other than the current pallet moved (as a stack in another bay would).
        advance(
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
        assert_eq!(value["version"], 3);
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
        assert_refused_everywhere(r#"{"version":4,"entries":[]}"#, "has format version 4");
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
        bad_head.after.heads.insert("main".to_string(), Some("not-a-hash".to_string()));
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

    /// A delete entry removing `victim` at `head`, from a bay on `main`.
    fn delete_entry(victim: &str, head: &str) -> JournalEntry {
        let mut entry = entry(DELETE_OP, "main", &hash('1'), &hash('1'));
        entry.before.heads.insert(victim.to_string(), Some(head.to_string()));
        entry.after.heads.insert(victim.to_string(), None);
        entry
    }

    #[test]
    fn an_entry_records_a_removal_only_as_a_delete_of_one_other_user_pallet() {
        let _scratch = Scratch::new("bad-removal-journal");

        let mut not_a_delete = delete_entry("side", &hash('3'));
        not_a_delete.op = "stack".to_string();
        let current = delete_entry("main", &hash('3'));
        let mut moved_onto = delete_entry("side", &hash('3'));
        moved_onto.after.current_pallet = "side".to_string();
        let meta = delete_entry("@tags", &hash('3'));
        let mut two = delete_entry("side", &hash('3'));
        two.before.heads.insert("other".to_string(), Some(hash('4')));
        two.after.heads.insert("other".to_string(), None);
        let mut unrecorded = delete_entry("side", &hash('3'));
        unrecorded.before.heads.remove("side");
        let mut none = delete_entry("side", &hash('3'));
        none.after.heads.insert("side".to_string(), Some(hash('3')));
        let mut before = delete_entry("side", &hash('3'));
        before.before.heads.insert("side".to_string(), None);

        for (bad, reason) in [
            (not_a_delete, "with a removal it cannot have made"),
            (current, "with a removal it cannot have made"),
            (moved_onto, "with a removal it cannot have made"),
            (meta, "with a removal it cannot have made"),
            (two, "with a removal it cannot have made"),
            (unrecorded, "with a removal it cannot have made"),
            (none, "with a removal it cannot have made"),
            (before, "records a removed ref before its operation"),
        ] {
            let content = serde_json::to_string(&JournalFile { version: JOURNAL_VERSION, entries: &[bad] }).unwrap();
            assert_refused_everywhere(&content, reason);
        }
    }

    #[test]
    fn a_version_2_journal_is_read_and_written_back_at_version_3() {
        let _scratch = Scratch::new("v2-journal");

        plant_journal(&format!(
            r#"{{"version":2,"entries":[{{"op":"stack","before":{{"current_pallet":"main","heads":{{"main":"{}"}},"consolidation":null}},"after":{{"current_pallet":"main","heads":{{"main":"{}"}},"consolidation":null}}}}]}}"#,
            hash('1'), hash('2')
        ));

        let entries = read().unwrap();
        assert_eq!(entries, vec![entry("stack", "main", &hash('1'), &hash('2'))]);
        assert!(removed_heads_in(&bay_root()).unwrap().is_empty(), "a version-2 journal holds no removals");

        write(&entries).unwrap();
        let value: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(path()).unwrap()).unwrap();
        assert_eq!(value["version"], 3);

        // The same delete entry a version-3 journal holds is refused by name at version 2.
        let delete = serde_json::to_string(&JournalFile { version: OLDER_JOURNAL_VERSION, entries: &[delete_entry("side", &hash('3'))] }).unwrap();
        assert_refused_everywhere(&delete, "which a version 2 journal cannot hold");
    }

    #[test]
    fn a_delete_entry_is_written_in_the_removal_hold_and_its_head_stays_rooted() {
        let _scratch = Scratch::new("delete-entry");
        pallet_utils::update_pallet_head("victim", None, &hash('5'), &pallet_utils::RefPreconditions::NONE).unwrap();
        let delete = delete_entry("victim", &hash('5'));

        pallet_utils::remove_pallet_head_in(PalletNamespace::User, "victim", &hash('5'),
                                            &pallet_utils::RefPreconditions::NONE, Some(&delete)).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("victim").unwrap(), None);
        assert_eq!(read().unwrap().last(), Some(&delete));
        assert_eq!(removed_by(&delete), vec![("victim".to_string(), hash('5'))]);
        assert_eq!(removed_heads_in(&bay_root()).unwrap(), vec![hash('5')]);
        assert_eq!(removed_heads().unwrap(), vec![hash('5')], "every bay's removals, the main tree's included");

        // The gc roots read it through the bay-scoped roots.
        let dirs = crate::util::bay_utils::all_bay_state_dirs().unwrap();
        let roots = crate::util::bay_utils::collect_bay_scoped_parcel_roots(&dirs, crate::util::bay_utils::BayReadPolicy::FailClosed)
            .unwrap().roots;
        assert!(roots.contains(&hash('5')), "{:?}", roots);
    }

    /// A removal that does not land puts back the journal it read before its append, never the
    /// appended one with its last entry popped: on a full journal the append drained the oldest
    /// entry, here a live delete whose head must stay rooted. Mutation: write back the appended
    /// entries minus the last → red (the oldest delete is gone).
    #[test]
    #[cfg(unix)]
    fn a_removal_that_does_not_land_leaves_a_full_journal_exactly_as_it_was() {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = Scratch::new("delete-unlanded");
        pallet_utils::update_pallet_head("victim", None, &hash('5'), &pallet_utils::RefPreconditions::NONE).unwrap();

        let mut full = vec![delete_entry("old", &hash('7'))];
        full.extend((1..MAX_ENTRIES).map(|i| entry("stack", "main", &format!("{:064x}", i), &format!("{:064x}", i + 1))));
        write(&full).unwrap();

        let pallets = pallet_utils::get_pallet_ref_path(PalletNamespace::User, "victim").parent().unwrap().to_path_buf();
        std::fs::set_permissions(&pallets, std::fs::Permissions::from_mode(0o555)).unwrap();
        let refused = pallet_utils::remove_pallet_head_in(PalletNamespace::User, "victim", &hash('5'),
                                                          &pallet_utils::RefPreconditions::NONE,
                                                          Some(&delete_entry("victim", &hash('5'))));
        std::fs::set_permissions(&pallets, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(matches!(refused, Err(RefUpdateError::Failed(_))), "{:?}", refused);
        assert_eq!(pallet_utils::get_pallet_head("victim").unwrap(), Some(hash('5')));
        assert_eq!(read().unwrap(), full, "the journal is exactly as the removal found it");
        assert_eq!(removed_heads_in(&bay_root()).unwrap(), vec![hash('7')]);
    }

    /// A journaled stack moving "main" from a parcel to a child of it; the entry and both heads.
    fn stacked_on_a_child() -> (JournalEntry, String, String) {
        let pre = hash('1');
        let post = pallet_utils::tests::child_of(&pre, "stacked");
        advance("main", pallet_utils::get_pallet_head("main").unwrap().as_deref(), &pre).unwrap();

        let before = capture().unwrap();
        advance("main", Some(&pre), &post).unwrap();
        push_if_changed("stack", before).unwrap();

        (peek().unwrap().unwrap(), pre, post)
    }

    // Revert: drop the served refusal from the gate → the gate passes.
    #[test]
    fn check_undoable_refuses_putting_a_pallet_back_in_a_served_root() {
        let _scratch = Scratch::new("gate-served");
        let (entry, _pre, post) = stacked_on_a_child();
        check_undoable(&entry).unwrap();

        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());
        let refused = check_undoable(&entry).unwrap_err();

        assert!(refused.contains("has been served") && refused.contains("\"main\""), "{}", refused);
        assert_eq!(pallet_utils::read_position().unwrap().base, Some(post), "the bay is untouched");
    }

    // Revert: judge the served refusal before the bay is found to match the entry → the served
    // refusal hides the switched pallet and points at an operation that is no longer the last.
    #[test]
    fn a_bay_switched_since_in_a_served_root_is_refused_for_that() {
        let _scratch = Scratch::new("gate-served-switched");
        let (entry, _pre, post) = stacked_on_a_child();
        advance("other", None, &post).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("other", Some(post))).unwrap();
        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());

        let refused = check_undoable(&entry).unwrap_err();

        assert!(refused.contains("\"other\" is current now") && !refused.contains("has been served"), "{}", refused);
    }

    // Revert: judge pallets in the journal's order → the refusal names "aside", not the current
    // pallet the undo is about.
    #[test]
    fn the_served_refusal_names_the_current_pallet_first() {
        let _scratch = Scratch::new("gate-served-order");
        let first = hash('2');
        let second = pallet_utils::tests::child_of(&first, "aside");
        let pre = hash('1');
        let post = pallet_utils::tests::child_of(&pre, "stacked");
        advance("aside", None, &first).unwrap();
        advance("main", pallet_utils::get_pallet_head("main").unwrap().as_deref(), &pre).unwrap();

        // One operation moves both pallets.
        let before = capture().unwrap();
        advance("aside", Some(&first), &second).unwrap();
        advance("main", Some(&pre), &post).unwrap();
        push_if_changed("consolidate", before).unwrap();
        let entry = peek().unwrap().unwrap();
        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());

        let refused = check_undoable(&entry).unwrap_err();

        assert!(refused.contains("has been served") && refused.contains("\"main\""), "{}", refused);
    }

    // Revert: count every changed user wire whatever its ref holds now → the gate refuses an
    // undo whose put-back would move nothing back.
    #[test]
    fn a_pallet_moved_on_since_the_operation_does_not_refuse_the_undo_in_a_served_root() {
        let _scratch = Scratch::new("gate-served-other");
        let (mut entry, _pre, post) = stacked_on_a_child();
        let first = hash('2');
        let second = pallet_utils::tests::child_of(&first, "aside");
        advance("aside", None, &second).unwrap();

        // An entry that moved only "aside", with the bay at the head it left.
        entry.before.heads = BTreeMap::from([("main".to_string(), Some(post.clone())), ("aside".to_string(), Some(first))]);
        entry.after.heads = BTreeMap::from([("main".to_string(), Some(post)), ("aside".to_string(), Some(second.clone()))]);
        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());
        assert!(check_undoable(&entry).unwrap_err().contains("\"aside\""));

        // Another writer moves "aside" on: the put-back would leave it alone.
        move_elsewhere("aside", &pallet_utils::tests::child_of(&second, "later"));

        check_undoable(&entry).unwrap();
    }

    // Revert: drop the ref core's served refusal → the put-back lands after the gate passed.
    #[test]
    fn a_root_served_after_the_gate_refuses_the_put_back_and_keeps_the_bay_at_the_operation() {
        let _scratch = Scratch::new("restore-served-race");
        let (entry, _pre, post) = stacked_on_a_child();
        check_undoable(&entry).unwrap();

        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());
        let refused = restore_refs(&entry).unwrap_err();

        assert!(refused.contains("has been served"), "{}", refused);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(post.clone()));
        assert_eq!(pallet_utils::read_position().unwrap().base, Some(post), "the bay is back at the operation");
        assert!(check_undoable(&entry).is_err(), "a retry is refused by the gate");
    }

    // Revert: judge the served refusal before the half-undone pass → the gate refuses the retry,
    // stranding the bay at the earlier head.
    #[test]
    fn a_half_undone_entry_in_a_served_root_passes_the_gate_and_its_retry_settles_the_bay() {
        let _scratch = Scratch::new("restore-served-half");
        let (entry, pre, post) = stacked_on_a_child();

        // Where an undo that stopped after its first write leaves the bay.
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(pre))).unwrap();
        assert!(crate::util::tombstone_utils::mark_served(false).unwrap());

        check_undoable(&entry).expect("a half-undone entry is resumed");
        let refused = restore_refs(&entry).unwrap_err();

        assert!(refused.contains("has been served"), "the core's refusal, not another failure: {}", refused);

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(post.clone()));
        assert_eq!(pallet_utils::read_position().unwrap().base, Some(post), "the bay is back at the operation");
        assert!(check_undoable(&entry).unwrap_err().contains("has been served"));
    }

    #[test]
    fn check_undoable_passes_while_the_bay_is_where_the_operation_left_it() {
        let _scratch = Scratch::new("gate-pass");

        advance(
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
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        advance(
            "side",
            pallet_utils::get_pallet_head("side").unwrap().as_deref(),
            &hash('4'),
        ).unwrap();

        let mut stacked = entry("stack", "main", &hash('1'), &hash('2'));
        stacked.after.heads.insert("side".to_string(), Some(hash('3')));

        check_undoable(&stacked).unwrap();
    }

    #[test]
    fn check_undoable_refuses_once_the_current_pallet_switched() {
        let _scratch = Scratch::new("gate-switched");

        // The op left "main" current at 2; an un-journaled palletize has since switched to
        // "feature" at the same head.
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        advance(
            "feature",
            pallet_utils::get_pallet_head("feature").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("feature", pallet_utils::get_pallet_head("feature").unwrap())).unwrap();

        let error = check_undoable(&entry("stack", "main", &hash('1'), &hash('2'))).unwrap_err();
        assert!(error.contains("left pallet \"main\" current, but \"feature\" is current now"), "{}", error);
    }

    #[test]
    fn check_undoable_refuses_once_the_current_pallet_moved() {
        let _scratch = Scratch::new("gate-moved");

        advance(
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

        advance(
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
        advance(
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
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('4'),
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let mut restored = entry("consolidate", "main", &hash('a'), &hash('2'));
        restored.before.current_pallet = "feature".to_string();
        restored.before.heads.insert("@office".to_string(), Some(hash('b')));
        let mut journaled = consolidating(&hash('c'), "their-pallet");
        journaled.our_pallet = "feature".to_string();
        restored.before.consolidation = Some(journaled);
        restored.after.heads.insert("@office".to_string(), Some(hash('4')));

        restore_refs(&restored).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('a')));
        assert_eq!(pallet_utils::get_meta_pallet_head("office").unwrap(), Some(hash('b')));
        assert_eq!(pallet_utils::read_position().unwrap().pallet, "feature");

        let consolidation = merge_utils::read_consolidation_state().unwrap().unwrap();
        assert_eq!(consolidation.their_head, hash('c'));
        assert_eq!(consolidation.their_pallet, "their-pallet");
        assert_eq!(consolidation.our_pallet, "feature", "the pallet the merge started on must come back");
        assert!(OutOfScopeSkeleton::read_required().is_ok(), "the skeleton must come back with the state");
    }

    #[test]
    fn restore_refs_puts_back_the_recorded_out_of_scope_skeleton() {
        let _scratch = Scratch::new("restore-skeleton");

        advance(
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
        advance(
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

        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", pallet_utils::get_pallet_head("main").unwrap())).unwrap();
        advance("main", None, &hash('1')).unwrap();
        start_consolidation(&hash('d'), label);

        let before = capture().unwrap();
        advance("main", Some(&hash('1')), &hash('2')).unwrap();
        merge_utils::clear_consolidation_state().unwrap();
        push_if_changed("stack", before).unwrap();

        let entry = peek().expect("the journal must stay readable").unwrap();
        assert_eq!(entry.before.consolidation.unwrap().their_pallet, label);
    }

    #[cfg(unix)]
    #[test]
    fn an_undo_whose_later_ref_write_fails_leaves_the_current_pallet_where_the_operation_left_it() {
        let _scratch = Scratch::new("restore-partial-failure");

        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", pallet_utils::get_pallet_head("main").unwrap())).unwrap();
        advance("main", None, &hash('1')).unwrap();
        pallet_utils::update_meta_pallet_head("kv", None, &hash('a'), &pallet_utils::RefPreconditions::NONE).unwrap();

        let before = capture().unwrap();
        advance("main", Some(&hash('1')), &hash('2')).unwrap();
        pallet_utils::update_meta_pallet_head("kv", Some(&hash('a')), &hash('b'), &pallet_utils::RefPreconditions::NONE).unwrap();
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
        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        // "feature" has since moved on to a third value — some un-journaled operation ran
        // between the journaled op and this undo. It must be left exactly where it is.
        advance(
            "feature",
            pallet_utils::get_pallet_head("feature").unwrap().as_deref(),
            &hash('5'),
        ).unwrap();

        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("feature".to_string(), Some(hash('3')));
        restored.after.heads.insert("feature".to_string(), Some(hash('4')));

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

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();
        pallet_utils::update_meta_pallet_head(
            "office",
            pallet_utils::get_meta_pallet_head("office").unwrap().as_deref(),
            &hash('8'),
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // "@office" existed before the op but not after it (deleted by it); something has
        // recreated it since. With no "after" evidence it is never rolled back.
        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("@office".to_string(), Some(hash('7')));

        restore_refs(&restored).unwrap();

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(hash('1')));
        assert_eq!(pallet_utils::get_meta_pallet_head("office").unwrap(), Some(hash('8')));
    }

    #[test]
    fn restore_refs_never_resurrects_a_deleted_ref() {
        let _scratch = Scratch::new("restore-refs-deleted");

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        let mut restored = entry("stack", "main", &hash('1'), &hash('2'));
        restored.before.heads.insert("gone".to_string(), Some(hash('3')));
        restored.after.heads.insert("gone".to_string(), Some(hash('4')));

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
        advance("main", None, &hash('b')).unwrap();

        restore_refs(&entry("stack", "main", &hash('a'), &hash('b'))).unwrap();

        assert!(merge_utils::read_consolidation_state().unwrap().is_none());
    }

    #[test]
    fn restore_refs_puts_a_completed_cherry_pick_back_in_progress() {
        let _scratch = Scratch::new("restore-cherry-pick");

        advance(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &hash('2'),
        ).unwrap();

        // The stack that completed the pick consumed its state file.
        let mut completed = entry("stack", "main", &hash('1'), &hash('2'));
        completed.before.cherry_pick = Some(PickInProgress { source: hash('c'), our_head: hash('e'), our_pallet: "side".to_string(), description: Some("pick".to_string()) });

        restore_refs(&completed).unwrap();

        let state = cherry_pick_utils::read_state().unwrap().expect("the pick must be in progress again");
        assert_eq!(state.source, hash('c'));
        assert_eq!(state.description.as_deref(), Some("pick"));
        assert_eq!(state.our_pallet, "side", "the pallet the pick was applied to must come back");

        // And an entry with no pick before it clears one.
        advance(
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
        let head = |i: usize| crate::util::pallet_utils::tests::stub_parcel(&i.to_string());

        for i in 0..(MAX_ENTRIES + 10) {
            advance(
                "main",
                pallet_utils::get_pallet_head("main").unwrap().as_deref(),
                &head(i),
            ).unwrap();
            let before = capture().unwrap();
            advance(
                "main",
                pallet_utils::get_pallet_head("main").unwrap().as_deref(),
                &head(i + 1),
            ).unwrap();
            push_if_changed("stack", before).unwrap();
        }

        assert_eq!(read().unwrap().len(), MAX_ENTRIES, "the journal must never grow past its cap");

        // The oldest entries are the ones dropped: the newest surviving entry is the very
        // last one pushed, and the oldest is the eleventh.
        let entries = read().unwrap();
        assert_eq!(entries[0].before.head("main"), Some(&head(10)));
        let newest = pop().unwrap().unwrap();
        assert_eq!(newest.before.head("main"), Some(&head(MAX_ENTRIES + 9)));
    }
    #[test]
    fn a_lost_create_of_a_deleted_pallet_is_judged_by_the_head_it_found() {
        let _scratch = Scratch::new("create-deleted-race");
        let none = pallet_utils::RefPreconditions::NONE;

        // Created meanwhile at the deleted head: restored, for undo and for "--as" alike.
        pallet_utils::update_pallet_head("side", None, &hash('a'), &none).unwrap();
        assert_eq!(create_deleted("side", "side", false, &hash('a'), &none), Ok(Restored::AlreadyThere));
        assert_eq!(create_deleted("side", "side", true, &hash('a'), &none), Ok(Restored::AlreadyThere));

        // Created meanwhile at another head: refused, theirs stands.
        pallet_utils::update_pallet_head("other", None, &hash('b'), &none).unwrap();
        let Err(CreateRefused::Other(refused)) = create_deleted("side", "other", false, &hash('a'), &none) else { panic!() };
        assert!(refused.contains("exists again") && refused.contains("forklift undo --as <new-name>"), "{}", refused);
        let Err(CreateRefused::Other(refused)) = create_deleted("side", "other", true, &hash('a'), &none) else { panic!() };
        assert!(refused.contains("Pallet \"other\" already exists"), "{}", refused);
        assert_eq!(pallet_utils::get_pallet_head("other").unwrap(), Some(hash('b')));

        assert_eq!(create_deleted("side", "fresh", true, &hash('c'), &none), Ok(Restored::Created));
        assert_eq!(pallet_utils::get_pallet_head("fresh").unwrap(), Some(hash('c')));
    }

    #[test]
    fn a_create_of_an_incomplete_deleted_head_refuses() {
        let _scratch = Scratch::new("create-deleted-incomplete");

        // No such parcel is here at all.
        let absent = "a".repeat(64);
        let refused = create_deleted("side", "side", false, &absent, &pallet_utils::RefPreconditions::NONE);
        assert!(matches!(&refused, Err(CreateRefused::Incomplete(missing)) if missing.contains(&absent)), "{:?}", refused);
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);
    }

    #[test]
    fn a_restore_without_trust_pins_the_anchor_absent() {
        let _scratch = Scratch::new("restore-no-anchor");

        // An enrollment between the trust read and the write must make the create lose.
        let preconditions = trust_preconditions("side", &hash('a'), "Nothing was undone.").unwrap();
        assert!(matches!(preconditions.anchor, pallet_utils::AnchorPrecondition::Absent));
    }

    #[test]
    fn a_delete_made_during_a_pick_undoes_after_the_picked_parcel_is_gone() {
        let _scratch = Scratch::new("delete-during-pick");

        advance("main", pallet_utils::get_pallet_head("main").unwrap().as_deref(), &hash('2')).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(hash('2')))).unwrap();
        cherry_pick_utils::write_state(&CherryPickState { source: hash('6'), our_head: hash('2'), our_pallet: "main".to_string(), description: None }).unwrap();

        // The delete leaves the pick in progress as it found it, so undoing it puts no pick back.
        let deleted = super::delete_entry("side", &hash('3')).unwrap();
        assert!(deleted.before.cherry_pick.is_some());
        check_undoable(&deleted).unwrap();
    }
}
