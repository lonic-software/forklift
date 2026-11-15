//! The warehouse's parked work: one record, shared by every bay, at `<forklift root>/parked`.
//!
//! Each entry names a parked parcel, whether `heal` found it in the store rather than in the
//! record, and the name it is listed under, oldest first. A parked parcel is an ordinary parcel
//! (its parent is the head it was parked on) carrying a [`ParcelActionType::Park`] action whose
//! description is the park's name; this record is the only thing that keeps it, so a record that
//! cannot be trusted fails every read, and `heal`'s [`rebuild`] restores it from what still reads
//! and from the parked parcels the store holds.
//!
//! The record is digested ([`record_utils::to_bytes_digested`]): damage reads as damage, never as
//! an empty list. An absent record is damage too: preparing a warehouse writes an empty one, and
//! no writer ever removes it. A bay's own `parked` file, the shape before parked work was shared,
//! is a leftover `heal` merges in; while one stands, the record is incomplete and fails every read.
//!
//! Writers take the [`ParkedLock`] across one read and the write it decides; readers take none.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use chrono::{DateTime, Utc};
use crate::enums::parcel_action_type::ParcelActionType;
use crate::globals::forklift_root;
use crate::model::parcel::Parcel;
use crate::util::lock_utils::ParkedLock;
use crate::util::record_utils;
use crate::util::{file_utils, object_utils, pack_utils, redaction_utils};

/// The record's file name in the shared forklift root, and in a bay's state dir the name of a
/// leftover from before parked work was shared.
pub const FILE_NAME_PARKED: &str = "parked";

/// The [`record_utils`] kind and version of the record. Version 1 was a bay's own list of bare
/// hashes, undigested.
const PARKED_RECORD: &str = "parked";
const PARKED_VERSION: u32 = 2;

/// The longest name a park may have, in characters.
pub const MAX_NAME_CHARS: usize = 100;

/// The flag of an entry `heal` found in the store rather than in the record.
const FLAG_RECOVERED: &str = "recovered";

/// The flag of an entry the record listed.
const FLAG_LISTED: &str = "-";

/// One parked parcel, as the record lists it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParkedEntry {
    /// The parked parcel.
    pub parcel: String,

    /// Whether `heal` found the parcel in the store rather than in the record: it may be work that
    /// was popped or dropped before the record was lost.
    pub recovered: bool,

    /// The name it is listed under, unique in the record.
    pub name: String,
}

impl ParkedEntry {
    fn line(&self) -> String {
        format!("{} {} {}", self.parcel, if self.recovered { FLAG_RECOVERED } else { FLAG_LISTED }, self.name)
    }

    fn parse(line: &str) -> Option<ParkedEntry> {
        let (parcel, rest) = line.split_once(' ')?;
        let (flag, name) = rest.split_once(' ')?;
        let recovered = match flag {
            FLAG_LISTED => false,
            FLAG_RECOVERED => true,
            _ => return None,
        };

        (object_utils::is_object_hash(parcel) && validate_name(name).is_ok())
            .then(|| ParkedEntry { parcel: parcel.to_string(), recovered, name: name.to_string() })
    }
}

/// The record's path in the shared forklift root.
pub fn parked_path() -> PathBuf {
    forklift_root().join(FILE_NAME_PARKED)
}

/// What a read refusal names: `heal`'s [`rebuild`], and the server's prepare, which writes a
/// missing record again.
const REBUILT_BY_HEAL: &str = "\"forklift heal\" rebuilds it from the entries that still read and the parked \
    parcels the store holds; a server's warehouse gets an empty one from \"forklift-server prepare\".";

/// Why `name` cannot name a park, or nothing when it can: it must be non-empty, at most
/// [`MAX_NAME_CHARS`] characters, hold no control character, and neither begin nor end in
/// whitespace.
pub fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() {
        return Err("A park's name cannot be empty.".to_string());
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(format!("A park's name can be at most {} characters long.", MAX_NAME_CHARS));
    }
    if name.chars().any(char::is_control) {
        return Err("A park's name cannot hold a control character.".to_string());
    }
    if name.trim() != name {
        return Err("A park's name cannot begin or end with whitespace.".to_string());
    }

    Ok(())
}

/// The name a park on `pallet` at `at` gets when none is given: the pallet and the UTC time.
pub fn default_name(pallet: &str, at: DateTime<Utc>) -> String {
    format!("{} {}", pallet, at.format("%Y-%m-%dT%H:%M:%SZ"))
}

/// `base`, or `base (2)`, `base (3)`… — the first that no entry of `entries` holds, cut so the
/// suffix fits within [`MAX_NAME_CHARS`].
pub fn unique_name(base: &str, entries: &[ParkedEntry]) -> String {
    let taken: HashSet<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();

    for attempt in 1usize.. {
        let suffix = if attempt == 1 { String::new() } else { format!(" ({})", attempt) };
        let room = MAX_NAME_CHARS - suffix.chars().count();
        let name = format!("{}{}", base.chars().take(room).collect::<String>().trim_end(), suffix);

        if !taken.contains(name.as_str()) {
            return name;
        }
    }

    unreachable!("an unbounded search ends")
}

/// The leftover `parked` files of the named bays of the warehouse at forklift root `forklift`.
fn leftovers_in(forklift: &Path) -> Result<Vec<PathBuf>, String> {
    let folder = forklift.join(crate::globals::FOLDER_NAME_BAYS_ROOT);
    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", folder.to_string_lossy(), e)),
    };

    let mut found = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("Error while listing \"{}\": {}", folder.to_string_lossy(), e))?
            .path().join(FILE_NAME_PARKED);

        match std::fs::symlink_metadata(&path) {
            Ok(_) => found.push(path),
            Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => (),
            Err(e) => return Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
        }
    }
    found.sort();

    Ok(found)
}

/// The record `bytes` at `path` lists, or why they list nothing this build can trust.
fn decode(bytes: &[u8], path: &Path) -> Result<Vec<ParkedEntry>, String> {
    let lines = record_utils::from_bytes_digested(bytes, PARKED_RECORD, PARKED_VERSION).map_err(|e| format!(
        "The parked-work record \"{}\" {} {}",
        path.to_string_lossy(), e.describe(), match written_by_newer(bytes) {
            true => "This version of Forklift never writes over it.",
            false => REBUILT_BY_HEAL,
        }
    ))?;

    let mut entries: Vec<ParkedEntry> = Vec::with_capacity(lines.len());
    let mut names: HashSet<String> = HashSet::new();

    for (index, line) in lines.iter().enumerate() {
        match ParkedEntry::parse(line) {
            Some(entry) if names.insert(entry.name.clone()) => entries.push(entry),
            _ => return Err(format!(
                "The parked-work record \"{}\" holds an entry Forklift cannot read on line {}. {}",
                path.to_string_lossy(), index + 2, REBUILT_BY_HEAL
            )),
        }
    }

    Ok(entries)
}

fn written_by_newer(bytes: &[u8]) -> bool {
    record_utils::written_by_newer_digested(bytes, PARKED_RECORD, PARKED_VERSION)
}

/// Read the warehouse's parked work, oldest first.
///
/// # Returns
/// * `Ok(Vec<ParkedEntry>)` - The parked work.
/// * `Err(String)`          - The record is missing, damaged, of a version this build does not
///                            know, or a bay still holds a leftover of its own.
pub fn read_parked() -> Result<Vec<ParkedEntry>, String> {
    let forklift = forklift_root();
    let path = forklift.join(FILE_NAME_PARKED);

    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(format!(
            "The parked-work record \"{}\" is missing. {}", path.to_string_lossy(), REBUILT_BY_HEAL
        )),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };
    let entries = decode(&bytes, &path)?;

    if let Some(leftover) = leftovers_in(&forklift)?.first() {
        return Err(format!(
            "A bay still holds parked work of its own at \"{}\", which the parked-work record \"{}\" \
            does not list. \"forklift heal\" merges it into the record.",
            leftover.to_string_lossy(), path.to_string_lossy()
        ));
    }

    Ok(entries)
}

/// Write the warehouse's parked work, oldest first, whole and atomically, under the lock a writer
/// holds across the read that decided it. An empty list is written as an empty record: the record
/// is never removed.
pub fn write_parked(_lock: &ParkedLock, entries: &[ParkedEntry]) -> Result<(), String> {
    file_utils::replace_file_atomically(&parked_path(), &parked_bytes(entries))
}

/// The bytes [`write_parked`] writes for `entries`.
pub fn parked_bytes(entries: &[ParkedEntry]) -> Vec<u8> {
    record_utils::to_bytes_digested(PARKED_RECORD, PARKED_VERSION, entries.iter().map(ParkedEntry::line))
}

/// Write an empty record into the forklift root `forklift` of a warehouse being prepared, when it
/// has none and no bay holds a leftover, which `heal` merges into a record of its own rebuilding.
///
/// # Returns
/// * `Ok(true)`    - The record was written.
/// * `Ok(false)`   - One is there, or a leftover stands.
/// * `Err(String)` - It could not be checked or written.
pub fn create_if_not_exists(forklift: &Path) -> Result<bool, String> {
    let path = forklift.join(FILE_NAME_PARKED);

    match std::fs::symlink_metadata(&path) {
        Ok(_) => return Ok(false),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
    }
    if !leftovers_in(forklift)?.is_empty() {
        return Ok(false);
    }

    file_utils::replace_file_atomically(&path, &parked_bytes(&[]))?;
    Ok(true)
}

/// Settle a park whose record write is not known to have landed: under the [`ParkedLock`], an
/// entry listing `parcel` with the listed flag means the work is safe in the record. A recovered
/// entry for it is one a rebuild found in the store meanwhile, of work the record never listed,
/// so it is removed.
///
/// # Returns
/// * `Ok(true)`    - The record lists the parcel: the park reached it.
/// * `Ok(false)`   - It does not, or listed it only as recovered, which is now removed.
/// * `Err(String)` - The record could not be read or written.
pub fn settle_unlisted(parcel: &str) -> Result<bool, String> {
    let lock = ParkedLock::acquire()?;
    let mut entries = read_parked()?;

    match entries.iter().position(|entry| entry.parcel == parcel) {
        Some(index) if !entries[index].recovered => Ok(true),
        Some(index) => {
            entries.remove(index);
            write_parked(&lock, &entries)?;
            Ok(false)
        }
        None => Ok(false),
    }
}

/// What [`rebuild`] did with a damaged, missing or incomplete record.
#[derive(Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// Where the damaged record was copied to, when there was one.
    pub set_aside: Option<PathBuf>,
    /// How many entries still read whole, from the record and the bays' leftovers, and were kept.
    pub kept: usize,
    /// How many parked parcels the store held that no kept entry names, added as recovered.
    pub recovered: usize,
    /// How many lines were dropped: they did not read whole, or named no parcel the store holds.
    pub dropped: usize,
    /// How many objects or pack indexes the store scan could not read, and skipped.
    pub unreadable: usize,
    /// How many bays' leftovers were merged in and removed.
    pub merged_bays: usize,
}

/// Whether the record wants a rebuild: it is missing, it does not read (and no newer build wrote
/// it), or a bay holds a leftover. The record's bytes, when there are any.
fn damaged(forklift: &Path) -> Result<Option<Option<Vec<u8>>>, String> {
    let path = forklift.join(FILE_NAME_PARKED);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => Some(bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let wants = match &bytes {
        None => true,
        Some(bytes) if written_by_newer(bytes) => false,
        Some(bytes) => decode(bytes, &path).is_err() || !leftovers_in(forklift)?.is_empty(),
    };

    Ok(wants.then_some(bytes))
}

/// The entries that still read whole of a damaged record or a bay's leftover, of either version:
/// a version-1 line is a bare hash, which gets the listed flag and a default name. Each kept entry
/// names a parcel the store holds, of any marker; a name already taken gets a suffix.
fn salvage(bytes: &[u8], into: &mut Vec<ParkedEntry>) -> usize {
    let (lines, mut dropped) = record_utils::salvage(record_utils::without_digest_line(bytes), |line| {
        ParkedEntry::parse(line).map(|entry| (entry, None))
            .or_else(|| object_utils::is_object_hash(line).then(|| (ParkedEntry {
                parcel: line.to_string(), recovered: false, name: String::new(),
            }, Some(()))))
    });

    for (mut entry, bare) in lines {
        let parcel = match object_utils::load_parcel(&entry.parcel) {
            Ok(parcel) => parcel,
            Err(_) => {
                dropped += 1;
                continue;
            }
        };
        if into.iter().any(|kept| kept.parcel == entry.parcel) {
            continue;
        }

        let base = match bare {
            Some(()) => default_name_of(&parcel),
            None => entry.name.clone(),
        };
        entry.name = unique_name(&base, into);
        into.push(entry);
    }

    dropped
}

/// The name a parked parcel with no recorded name gets: its Park action's description, else the
/// default name of its author's time.
fn default_name_of(parcel: &Parcel) -> String {
    if let Some(name) = park_name_of(parcel) {
        return name;
    }

    let at = parcel.actions.first().map(|action| action.timestamp).unwrap_or_default();
    default_name("parked", at)
}

/// The name a parked parcel's Park action records, when it carries one that is a valid name.
fn park_name_of(parcel: &Parcel) -> Option<String> {
    parcel.actions.iter()
        .find(|action| matches!(action.action, ParcelActionType::Park))
        .and_then(|action| action.description.clone())
        .filter(|name| validate_name(name).is_ok())
}

/// A parked parcel a store scan found: when it was parked, its hash, and its name.
type Found = (DateTime<Utc>, String, String);

/// The parked parcels the store holds that `kept` does not name, no ref reaches, and no applied
/// redaction's guard refuses, oldest Park first, with how many objects or indexes did not read.
fn scan_store(kept: &[ParkedEntry]) -> Result<(Vec<Found>, usize), String> {
    let (hashes, mut unreadable) = pack_utils::stored_object_hashes()?;
    let named: HashSet<&str> = kept.iter().map(|entry| entry.parcel.as_str()).collect();
    let mut found: Vec<Found> = Vec::new();

    for hash in hashes {
        if named.contains(hash.as_str()) {
            continue;
        }
        let bytes = match file_utils::retrieve_object_by_hash_uncached(&hash) {
            Ok(bytes) => bytes,
            Err(_) => {
                unreadable += 1;
                continue;
            }
        };
        if !matches!(object_utils::peek_object_header(&bytes), Ok((crate::enums::object_type::ObjectType::Parcel, _))) {
            continue;
        }
        let Ok(parcel) = object_utils::decode_parcel(&hash, &bytes) else {
            unreadable += 1;
            continue;
        };
        let Some(park) = parcel.actions.iter().find(|action| matches!(action.action, ParcelActionType::Park)) else {
            continue;
        };

        let name = park_name_of(&parcel).unwrap_or_else(|| default_name("parked", park.timestamp));
        found.push((park.timestamp, hash, name));
    }

    if found.is_empty() {
        return Ok((found, unreadable));
    }

    // A parcel a ref reaches is history, not parked work.
    let mut heads: Vec<String> = Vec::new();
    for pallet in crate::util::pallet_utils::list_pallets()? {
        heads.extend(crate::util::pallet_utils::get_pallet_head(&pallet)?);
    }
    let reachable = crate::util::gc_utils::collect_reachable_for_sweep(&heads)?;
    let guard = redaction_utils::staging_guard()?;

    let mut kept_found = Vec::new();
    for (at, hash, name) in found {
        if reachable.contains(&hash) {
            continue;
        }
        if let Some(guard) = &guard {
            let tree = object_utils::load_parcel(&hash)?.tree_hash;
            if guard.names_tree(&tree)?.is_some() {
                continue;
            }
        }
        kept_found.push((at, hash, name));
    }
    kept_found.sort();

    Ok((kept_found, unreadable))
}

/// Rebuild a missing, damaged or incomplete record under the [`ParkedLock`], first copying a
/// damaged one aside ([`file_utils::copy_aside`]). It keeps every entry that still reads whole of
/// the record and of the bays' leftovers, then adds as recovered every parked parcel the store
/// holds that none names, no ref reaches and no applied redaction refuses, and last removes the
/// leftovers it merged. A record a newer build wrote is not damage, and is left as it is.
///
/// # Returns
/// * `Ok(Some(Rebuilt))` - The record wanted a rebuild, and is rebuilt.
/// * `Ok(None)`          - It reads, or a newer Forklift wrote it.
/// * `Err(String)`       - The record, a leftover or the store could not be read, or the record
///                         could not be written.
pub fn rebuild() -> Result<Option<Rebuilt>, String> {
    let forklift = forklift_root();

    // Judged first without the lock, so a record that reads costs no lock and no scan.
    if damaged(&forklift)?.is_none() {
        return Ok(None);
    }

    let lock = ParkedLock::acquire()?;
    let Some(bytes) = damaged(&forklift)? else {
        return Ok(None);
    };

    let mut entries: Vec<ParkedEntry> = Vec::new();
    let mut dropped = bytes.as_deref().map_or(0, |bytes| salvage(bytes, &mut entries));

    let leftovers = leftovers_in(&forklift)?;
    for leftover in &leftovers {
        let bytes = std::fs::read(leftover)
            .map_err(|e| format!("Error while reading \"{}\": {}", leftover.to_string_lossy(), e))?;
        dropped += salvage(&bytes, &mut entries);
    }
    let kept = entries.len();

    let (found, unreadable) = scan_store(&entries)?;
    let recovered = found.len();
    for (_, parcel, name) in found {
        let name = unique_name(&name, &entries);
        entries.push(ParkedEntry { parcel, recovered: true, name });
    }

    let path = forklift.join(FILE_NAME_PARKED);
    let set_aside = bytes.map(|bytes| file_utils::copy_aside(&path, &bytes)).transpose()?;
    write_parked(&lock, &entries)?;

    for leftover in &leftovers {
        file_utils::remove_file(leftover)
            .map_err(|e| format!("Error while removing \"{}\": {}", leftover.to_string_lossy(), e))?;
    }

    Ok(Some(Rebuilt { set_aside, kept, recovered, dropped, unreadable, merged_bays: leftovers.len() }))
}

/// The parcels the record lists, for a census of every object something holds.
pub fn parked_parcels() -> Result<BTreeSet<String>, String> {
    Ok(read_parked()?.into_iter().map(|entry| entry.parcel).collect())
}

/// Write the record listing `parcels`, oldest first, under default names, for a test fixture.
#[cfg(test)]
pub(crate) fn park_for_test(parcels: &[&str]) {
    let entries: Vec<ParkedEntry> = parcels.iter().enumerate()
        .map(|(index, parcel)| ParkedEntry { parcel: parcel.to_string(), recovered: false, name: format!("parked {}", index + 1) })
        .collect();
    std::fs::create_dir_all(forklift_root()).unwrap();
    std::fs::write(parked_path(), parked_bytes(&entries)).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> String {
        object_utils::hash_object_bytes(seed.as_bytes())
    }

    fn entry(seed: &str, name: &str) -> ParkedEntry {
        ParkedEntry { parcel: hash(seed), recovered: false, name: name.to_string() }
    }

    /// The record reads back in order from the bytes the writer writes, an empty one included.
    #[test]
    fn the_parked_record_round_trips() {
        let entries = vec![entry("first", "main 2026-11-15T17:30:00Z"), ParkedEntry { recovered: true, ..entry("second", "a b") }];
        let path = Path::new("parked");

        assert_eq!(decode(&parked_bytes(&entries), path).unwrap(), entries);
        assert_eq!(decode(&parked_bytes(&[]), path).unwrap(), Vec::new());
    }

    /// A version-1 file, a bay's bare hash lines with no digest, is damage, not an empty list.
    ///
    /// Revert: read a record whose digest is absent as its header says → the v1 file reads.
    #[test]
    fn a_version_one_file_reads_as_damage() {
        let v1 = record_utils::to_bytes(PARKED_RECORD, 1, [hash("bare")]);

        let error = decode(&v1, Path::new("parked")).unwrap_err();
        assert!(error.contains("is damaged") && error.contains("forklift heal"), "{}", error);
    }

    /// A digit of the header flipped to a newer version, with the digest left as it was, is damage,
    /// not a newer build's record.
    #[test]
    fn a_header_flipped_to_a_newer_version_is_damage() {
        let mut bytes = parked_bytes(&[entry("x", "x")]);
        bytes[b"/forklift-parked ".len()] = b'3';

        assert!(!written_by_newer(&bytes));
        let error = decode(&bytes, Path::new("parked")).unwrap_err();
        assert!(error.contains("is damaged"), "{}", error);
    }

    /// Two entries under one name, a name the rules refuse, an unknown flag and a non-hash are each
    /// refused.
    #[test]
    fn an_entry_the_rules_refuse_is_refused() {
        let good = entry("a", "a").line();
        for bad in [
            format!("{}\n{} - a", good, hash("b")),
            format!("{} - ", hash("b")),
            format!("{} - \tx", hash("b")),
            format!("{} - x ", hash("b")),
            format!("{} ? x", hash("b")),
            "not-a-hash - x".to_string(),
        ] {
            let lines: Vec<&str> = bad.split('\n').collect();
            let bytes = record_utils::to_bytes_digested(PARKED_RECORD, PARKED_VERSION, lines);
            let error = decode(&bytes, Path::new("parked")).unwrap_err();
            assert!(error.contains("cannot read on line"), "{:?}: {}", bad, error);
        }
    }

    /// A taken name gets the first free suffix, and a long one is cut so its suffix fits.
    #[test]
    fn a_taken_name_gets_the_first_free_suffix() {
        let entries = vec![entry("a", "x"), entry("b", "x (2)")];
        assert_eq!(unique_name("x", &entries), "x (3)");
        assert_eq!(unique_name("y", &entries), "y");

        let long = "n".repeat(MAX_NAME_CHARS);
        let taken = vec![entry("c", &long)];
        let suffixed = unique_name(&long, &taken);
        assert_eq!(suffixed.chars().count(), MAX_NAME_CHARS);
        assert!(suffixed.ends_with(" (2)"), "{}", suffixed);
        assert!(validate_name(&suffixed).is_ok());
    }

    #[test]
    fn the_default_name_is_the_pallet_and_the_utc_time() {
        use chrono::TimeZone;
        let at = Utc.with_ymd_and_hms(2026, 11, 15, 17, 30, 0).unwrap();
        assert_eq!(default_name("main", at), "main 2026-11-15T17:30:00Z");
    }
}
