//! The operator's repair of a served root's tombstone list and served mark
//! (`forklift-server tombstone repair`).
//!
//! A lost list cannot be rebuilt by name: a parcel carries no pallet name. What it held can still
//! be kept, because every head a tombstone held is history that no durable holder reaches any more.
//! So the repair keeps every entry of the damaged list that still reads, then finds each *lost
//! tip*: a stored parcel that no pallet ref, office pin or kept tombstone reaches, and that no
//! other such parcel names as a parent. Records that hold a head only for a while (journal
//! entries, parked parcels, hauls, the mirror's roots, staged shards) are not counted as holders,
//! since they expire and would hide a head the lost list held.
//!
//! A list that still reads whole lost nothing, so when only the mark is lost or damaged, the
//! repair writes the mark and nothing else.
//!
//! A name comes back only when the bays' undo journals settle it, at any unheld head (a tip or
//! not, since a parcel parked on a deleted head leaves that head a parent): a delete entry removed
//! that pallet at that head, every head any journal names for that pallet is that head or one of
//! its ancestors, and the name has no pallet and no tombstone now. Every lost tip not so named is
//! kept with no name.
//!
//! The journals cannot tell a deletion the lost list held from one whose tombstone the operator
//! dropped, so a dropped name a journal still settles comes back; the report lists each name it
//! gives back, to drop again. A deletion no journal recorded (one over the protocol) at a head
//! another pallet still reaches is not found: that pallet keeps its history only until it moves
//! on, and then gc may free it. Nothing is listed on a guess, and nothing the lost list may have held is
//! freed: the cost is history kept that no clone relies on, which `tombstone drop --head`
//! releases.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use crate::enums::object_type::ObjectType;
use crate::util::lock_utils::{RefLock, ServeLock};
use crate::util::tombstone_utils::{self, Entry, Tombstones};
use crate::util::{audit_utils, bay_utils, file_utils, journal_utils, merge_utils, object_utils, pack_utils, pallet_utils, record_utils};

/// What a repair did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Repaired {
    /// Where each damaged file was moved to.
    pub set_aside: Vec<PathBuf>,
    /// How many entries of the damaged list still read and were kept, named or not.
    pub salvaged: usize,
    /// How many lines of the damaged list did not read and were left in the moved-aside copy.
    pub unreadable_lines: usize,
    /// Each unheld head given its name back from the journals: the pallet's wire name and the head.
    pub named: Vec<(String, String)>,
    /// Each lost tip kept with no name.
    pub kept: Vec<String>,
    /// Why no name was taken from the journals at all, when one could not be read.
    pub journals_unread: Option<String>,
    /// Each pallet ref that did not read, whose history was left unheld and so kept, as a note.
    pub refs_unread: Vec<String>,
    /// Whether the served mark was written.
    pub mark_written: bool,
}

impl Repaired {
    /// The report, one line each.
    pub fn report(&self) -> Vec<String> {
        let mut lines = Vec::new();

        for path in &self.set_aside {
            lines.push(format!("Moved the damaged \"{}\" aside.", path.to_string_lossy()));
        }

        lines.push(format!(
            "Kept {} entr{} of the old list{}.",
            self.salvaged,
            if self.salvaged == 1 { "y" } else { "ies" },
            match self.unreadable_lines {
                0 => String::new(),
                n => format!("; {} line{} did not read and {} only in the copy moved aside", n, if n == 1 { "" } else { "s" }, if n == 1 { "is" } else { "are" }),
            }
        ));

        for (pallet, head) in &self.named {
            lines.push(format!("Named {} as deleted pallet \"{}\", from its deletion in an undo journal.", head, pallet));
        }

        for note in &self.refs_unread {
            lines.push(format!("Kept the history of a ref that does not read: {}", note));
        }

        if let Some(reason) = &self.journals_unread {
            lines.push(format!("No name was taken from the undo journals, since one could not be read: {}", reason));
        }

        match self.kept.len() {
            0 => lines.push("No other history was left unheld, so no head was kept without a name.".to_string()),
            n => lines.push(format!(
                "Kept {} head{} with no name: history no pallet, office pin or tombstone holds, which the lost \
                list may have held. A name it lost can now be reused. \"forklift-server tombstone list\" \
                shows {}, and \"forklift-server tombstone drop --head\" releases one that no clone relies on.",
                n, if n == 1 { "" } else { "s" }, if n == 1 { "it" } else { "them" }
            )),
        }

        if self.mark_written {
            lines.push("Wrote the served mark.".to_string());
        }

        lines
    }
}

/// Repair the tombstone list and served mark of the warehouse at the current storage root (see
/// the module doc). Takes the serve lock, so it refuses while a server serves the root, then the
/// ref lock.
///
/// # Returns
/// * `Ok(Repaired)` - What was repaired.
/// * `Err(String)`  - Both files read, so there is nothing to repair; the root is being served; the
///                    store holds packs whose objects cannot all be listed; or a ref, a pin, the
///                    list or an object could not be read, or a file could not be written. Nothing
///                    was changed, unless the list's write failed after the damaged files were
///                    moved aside: the list is then absent, and a repair run again keeps less by
///                    name, so the moved-aside copies are named in the error to put back first. A
///                    failed write of the mark after the list's leaves the repaired list, which a
///                    rerun finds whole.
///
/// A root with neither file reads as never served, and is not refused here: losing both is the
/// one loss the files cannot show (see [`tombstone_utils::served_state`]), so an operator who knows
/// the root was served repairs it like any other, keeping every unheld tip.
pub fn repair_tombstones() -> Result<Repaired, String> {
    let _serve_lock = ServeLock::acquire().map_err(|e| format!("Refusing to repair the tombstones: {}", e))?;
    let lock = RefLock::acquire()?;

    let mark = std::fs::read(tombstone_utils::served_path());
    let mark_reads = match &mark {
        Ok(bytes) => record_utils::from_bytes(bytes, tombstone_utils::SERVED_RECORD, tombstone_utils::SERVED_VERSION).is_ok(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(format!("Error while reading \"{}\": {}", tombstone_utils::served_path().to_string_lossy(), e)),
    };

    let list_path = tombstone_utils::tombstones_path();
    let list = match std::fs::read(&list_path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Error while reading \"{}\": {}", list_path.to_string_lossy(), e)),
    };

    let strict = list.as_deref().map(strict_list);

    if mark_reads && matches!(strict, Some(Some(_))) {
        return Err(
            "The tombstone list and the served mark of this warehouse both read, so there is nothing to repair. \
            Nothing was changed."
                .to_string()
        );
    }

    let mut repaired = Repaired::default();

    // A list that reads whole lost nothing: only the mark was lost or damaged (or a first serve
    // stopped before writing it), so only the mark is written.
    if let Some(Some(_)) = strict {
        if mark.is_ok() {
            repaired.set_aside.push(set_aside(&tombstone_utils::served_path())?);
        }

        tombstone_utils::write_mark(&lock).map_err(|e| format!("{} Run the repair again.", e))?;
        repaired.mark_written = true;

        return Ok(repaired);
    }

    refuse_unlisted_packs()?;

    // Keep what still reads. The damaged list is moved aside only once everything else has been
    // worked out, so a repair that fails before then leaves it in place for the next.
    let mut tombstones = match &list {
        Some(bytes) => {
            let (salvaged, unreadable) = salvage(bytes);
            repaired.unreadable_lines = unreadable;
            salvaged
        }
        None => Tombstones::default(),
    };
    repaired.salvaged = tombstones.named.len() + tombstones.kept.len();

    let lost = lost_parcels(&tombstones)?;
    repaired.refs_unread = lost.refs_unread;

    let evidence = match JournalEvidence::read() {
        Ok(evidence) => Some(evidence),
        Err(reason) => {
            repaired.journals_unread = Some(reason);
            None
        }
    };
    let live: BTreeSet<String> = pallet_utils::list_pallets()?.into_iter().collect();

    // A name comes back at any head a journal deleted it at that the store still holds: a tip or
    // not, since a head the lost list held can have unheld descendants, such as a parcel parked on
    // it; and held or not, since what holds it now (another pallet standing there) may move on.
    let mut named_heads: BTreeSet<String> = BTreeSet::new();

    if let Some(evidence) = &evidence {
        for head in evidence.deleted_heads() {
            if !lost.unheld.contains(head) && !file_utils::is_present_for_sweep(head)? {
                continue;
            }

            if let Some(name) = evidence.name_of(head, &live, &tombstones)? {
                // A head the damaged list kept with no name is now held by the name alone.
                tombstones.kept.remove(head);
                tombstones.named.insert(name.clone(), head.clone());
                repaired.named.push((name, head.clone()));
                named_heads.insert(head.clone());
            }
        }
    }

    for tip in lost.tips.into_iter().filter(|tip| !named_heads.contains(tip)) {
        tombstones.kept.insert(tip.clone());
        repaired.kept.push(tip);
    }

    // The mark first: a repair stopped between the two still finds the list damaged, as before.
    if mark.is_ok() && !mark_reads {
        repaired.set_aside.push(set_aside(&tombstone_utils::served_path())?);
    }

    if list.is_some() {
        let moved = set_aside(&list_path).map_err(|e| match repaired.set_aside.first() {
            Some(mark) => format!("{} The damaged served mark was moved aside to \"{}\" first.", e, mark.to_string_lossy()),
            None => e,
        })?;
        repaired.set_aside.push(moved);
    }

    let moved = |error: String| match repaired.set_aside.is_empty() {
        true => error,
        false => format!(
            "{} The damaged files were moved aside first; move them back before repairing again: {}",
            error,
            repaired.set_aside.iter().map(|path| format!("\"{}\"", path.to_string_lossy())).collect::<Vec<_>>().join(", ")
        ),
    };

    tombstone_utils::write_tombstones(&lock, &tombstones).map_err(moved)?;

    // The repaired list is in place, so a rerun finds it whole and writes only the mark.
    if !mark_reads {
        tombstone_utils::write_mark(&lock).map_err(|e| format!("{} The tombstone list was repaired; run the repair again to write the mark.", e))?;
        repaired.mark_written = true;
    }

    Ok(repaired)
}

/// The list in `bytes`, when it reads whole as this version's list.
fn strict_list(bytes: &[u8]) -> Option<Tombstones> {
    let lines = record_utils::from_bytes(bytes, "tombstones", tombstone_utils::TOMBSTONES_VERSION).ok()?;
    let mut tombstones = Tombstones::default();

    for line in &lines {
        let fresh = match tombstone_utils::parse_entry(line)? {
            Entry::Named(head, pallet) => tombstones.named.insert(pallet.to_string(), head.to_string()).is_none(),
            Entry::Kept(head) => tombstones.kept.insert(head.to_string()),
        };

        if !fresh {
            return None;
        }
    }

    Some(tombstones)
}

/// Every entry of a damaged list that still reads, of any version, and how many lines did not.
/// The first tombstone of a name is kept; a later one of the same name is kept as a head with no
/// name, so its history is not freed either. A first line that is a record header is skipped.
fn salvage(bytes: &[u8]) -> (Tombstones, usize) {
    let mut tombstones = Tombstones::default();
    let mut unreadable = 0;

    for (index, raw) in bytes.split(|byte| *byte == b'\n').enumerate() {
        if raw.is_empty() || (index == 0 && raw.starts_with(b"/forklift-")) {
            continue;
        }

        match std::str::from_utf8(raw).ok().and_then(tombstone_utils::parse_entry) {
            Some(Entry::Named(head, pallet)) => {
                if tombstones.named.get(pallet).is_some_and(|first| first != head) {
                    tombstones.kept.insert(head.to_string());
                } else {
                    tombstones.named.insert(pallet.to_string(), head.to_string());
                }
            }
            Some(Entry::Kept(head)) => {
                tombstones.kept.insert(head.to_string());
            }
            None => unreadable += 1,
        }
    }

    (tombstones, unreadable)
}

/// Move a damaged file aside, beside itself, under a name no other repair uses.
pub(crate) fn set_aside(path: &std::path::Path) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let name = format!("{}.damaged-{}", path.file_name().unwrap_or_default().to_string_lossy(), stamp);
    let target = path.with_file_name(name);

    file_utils::rename(path, &target).map_err(|e| format!(
        "Error while moving \"{}\" aside to \"{}\": {}", path.to_string_lossy(), target.to_string_lossy(), e
    ))?;

    Ok(target)
}

/// Refuse while the store holds a pack whose objects cannot all be listed: a lost tip in it would
/// be missed, and then freed.
fn refuse_unlisted_packs() -> Result<(), String> {
    let status = pack_utils::store_status()?;
    let problems: Vec<String> = [
        &status.quarantined_packs, &status.damaged_packs, &status.unenumerable_indexes, &status.foreign_version_indexes,
    ].into_iter().flatten().map(|problem| format!("{} ({})", problem.index_path, problem.error)).collect();

    match problems.is_empty() {
        true => Ok(()),
        false => Err(format!(
            "Refusing to repair the tombstones: the store holds packs whose objects cannot all be listed, \
            so history the lost list held could be missed. \"forklift store\" names them. Nothing was \
            changed. The packs: {}",
            problems.join("; ")
        )),
    }
}

/// What no durable holder reaches (see the module doc).
struct LostParcels {
    /// Every stored parcel no durable holder reaches.
    unheld: BTreeSet<String>,
    /// Those of them no other one names as a parent: the lost tips.
    tips: Vec<String>,
    /// A note for each pallet ref that did not read: what only it reaches is unheld too, and kept,
    /// so that heal, which a damaged list refuses, can rebuild it once the list is repaired.
    refs_unread: Vec<String>,
}

/// Every parcel no durable holder reaches, given the tombstones the repair keeps.
fn lost_parcels(tombstones: &Tombstones) -> Result<LostParcels, String> {
    let durable = crate::util::recovery_utils::durable_root_sources(tombstones.heads())?;
    let (reached, _gaps) = audit_utils::collect_reachable_noting_gaps(&durable.heads(), file_utils::is_present_for_sweep)?;

    let mut unheld: BTreeSet<String> = BTreeSet::new();
    let mut parents: BTreeSet<String> = BTreeSet::new();

    for prefix in 0..=u8::MAX {
        for hash in pallet_utils::object_hashes_with_prefix(&format!("{:02x}", prefix))? {
            if reached.contains(&hash) {
                continue;
            }

            let bytes = file_utils::retrieve_object_by_hash_uncached(&hash)?;
            let (kind, _) = object_utils::peek_object_header(&bytes)
                .map_err(|e| format!("Object {} could not be read: {}", hash, e))?;

            if kind == ObjectType::Parcel {
                parents.extend(object_utils::load_parcel(&hash)?.parents);
                unheld.insert(hash);
            }
        }
    }

    let tips = unheld.iter().filter(|hash| !parents.contains(*hash)).cloned().collect();

    Ok(LostParcels { unheld, tips, refs_unread: durable.degraded })
}

/// What the bays' undo journals record of each pallet's heads.
struct JournalEvidence {
    /// Each pallet a delete entry removed, with every head one removed it at.
    deleted: BTreeMap<String, BTreeSet<String>>,
    /// Each pallet any entry names, with every head it names it at.
    heads: BTreeMap<String, BTreeSet<String>>,
}

impl JournalEvidence {
    /// Read every bay's journal.
    fn read() -> Result<JournalEvidence, String> {
        let mut evidence = JournalEvidence { deleted: BTreeMap::new(), heads: BTreeMap::new() };

        for dir in bay_utils::all_bay_state_dirs()? {
            for entry in journal_utils::read_in(&dir)? {
                if entry.op == journal_utils::DELETE_OP {
                    for (wire, head) in journal_utils::removed_by(&entry) {
                        evidence.deleted.entry(wire).or_default().insert(head);
                    }
                }

                for snapshot in [&entry.before, &entry.after] {
                    for (wire, head) in &snapshot.heads {
                        if let Some(head) = head {
                            evidence.heads.entry(wire.clone()).or_default().insert(head.clone());
                        }
                    }
                }
            }
        }

        Ok(evidence)
    }

    /// Every head a delete entry removed some pallet at, each once.
    fn deleted_heads(&self) -> impl Iterator<Item = &String> {
        self.deleted.values().flatten().collect::<BTreeSet<_>>().into_iter()
    }

    /// The name unheld head `tip` gets back, if the journals settle one (see the module doc).
    /// `live` is every user pallet there is; `tombstones` what the repair keeps so far.
    fn name_of(&self, tip: &str, live: &BTreeSet<String>, tombstones: &Tombstones) -> Result<Option<String>, String> {
        let candidates: Vec<&String> = self.deleted.iter()
            .filter(|(_, heads)| heads.contains(tip))
            .map(|(wire, _)| wire)
            .collect();

        // Deleted under two names at one head: which one the lost list held cannot be told.
        let [wire] = candidates.as_slice() else {
            return Ok(None);
        };

        if pallet_utils::validate_pallet_name(wire).is_err() || live.contains(*wire) || tombstones.named.contains_key(*wire) {
            return Ok(None);
        }

        for head in self.heads.get(*wire).into_iter().flatten() {
            // A head of the name that is not the tip's own history: the name moved on after the
            // deletion, so the lost list may have held it at another head.
            if head != tip && !merge_utils::is_ancestor(head, tip).unwrap_or(false) {
                return Ok(None);
            }
        }

        Ok(Some(wire.to_string()))
    }
}
