//! The sealed-by-hash record: the objects a warehouse holds by hash alone, because a parent that
//! is present names them and the remote holds them. A walk that meets one of these absent
//! tolerates the absence and does not descend, as it does for content outside the fetch scope;
//! everything under an absent recorded tree or recipe is tolerated by that same non-descent. A
//! recorded object that is present seals nothing: it is walked like any other.
//!
//! The record is a log at `<forklift root>/sealed`, shared by every bay. It is appended to, and
//! rewritten whole only atomically (see Writing). It is replayed in order: a record entry adds a
//! hash, a removal entry drops it.
//!
//! ## Format
//!
//! ```text
//! /forklift-sealed 2\n
//! frame*
//! ```
//!
//! Each frame is `magic | length | entries | crc`: the magic is [`FRAME_MAGIC`], the length counts
//! the entries' bytes, and the CRC-32 covers the magic, the length and the entries, all
//! little-endian. An entry is one operation byte ([`SealOp`]) followed by the object's raw 32-byte
//! hash. A frame holds at least one entry and at most [`MAX_FRAME_ENTRIES`], and stands or falls
//! whole: an append of up to that many entries cannot land half-applied. A longer one is written a
//! frame at a time, and lands frame by frame.
//!
//! A reader takes no lock. It stops at the first frame that does not validate, which is how a torn
//! append reads: everything after the last valid frame is ignored. A region of zeros never
//! validates, because the magic is nonzero. Every frame is synced before the next is written (see
//! [`append_locked`]; with fsync turned off this does not hold, and a crash may leave the record
//! damaged rather than torn), so a torn append is the last frame and only that: nothing after it
//! validates, and at most the largest frame's worth of bytes, zeros included, follow the last
//! valid frame. A valid frame anywhere past the last one read, or more bytes than that, is damage
//! inside the log, and fails the read rather than dropping the frames behind it (until `heal`
//! rebuilds the record, see Writing). Damage that
//! leaves no valid frame after it within that many bytes of the end cannot be told from a torn
//! append, so it reads as one: the entries there are not replayed. That is the one loss the log can
//! hide, and it hides only entries: a lost record reads as a missing object, which every walk
//! reports, and a lost removal leaves a present object recorded. That seals nothing while the
//! object is present; were it lost later, it would be held by hash as it was before its refill,
//! on the same premise that the remote holds it. `heal` removes such records (see Removing).
//! A frame that validates but whose entries do not parse fails the read, as does a header naming
//! another version: neither is guessed at.
//!
//! ## Writing
//!
//! Every write takes the ref lock ([`RefLock`]) and re-reads the record under it, so writes are
//! ordered and each one sees the last. A record that does not parse is never written over, except
//! that `heal` rebuilds a damaged one ([`rebuild`]) as its header and the frames before the
//! damage. A header naming another version, or a valid frame whose entries do not parse, is not
//! damage (an intact frame is not what damage leaves), and a record whose first problem is either
//! is never rewritten; one that follows damage is dropped with everything else past it. One whose valid prefix is followed by a torn append is rewritten atomically to that prefix and the
//! new frames: a reader reading the file in pieces must never see a new frame after old torn bytes.
//! The first write creates the record atomically, so its header is never torn. Compaction
//! ([`compact`]) rewrites the log as the set it replays to, which drops no recorded hash. The
//! atomic writes are never staged by a bulk store session (they go through
//! [`file_utils::replace_file_atomically`]): a rename that waited for the session to finish would
//! land over the appends made in between.
//!
//! ## Removing
//!
//! [`unseal`] removes a record once its object is present again. Before it goes,
//! the coverage it gave is pushed down: every absent object reached from the present object, by a
//! walk that does not descend into what is absent, is recorded, since an object being recorded
//! means the remote holds its whole closure. The push-down is keyed by hash alone, never by path
//! or scope. Its records are written before the removals, so a crash between the two leaves the
//! object present and recorded, which seals nothing and the next `heal` repairs. The walk runs
//! without the ref lock, under the caller's store-access hold, so nothing it found present is
//! deleted under it; the commit re-reads the record under the ref lock and removes only what is
//! still recorded. An object the walk could not finish keeps its record.

use std::collections::{BTreeSet, HashSet, VecDeque};
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use crate::enums::object_type::ObjectType;
use crate::globals;
use crate::util::{file_utils, object_utils, pack_utils, record_utils, sign_utils};
use crate::util::lock_utils::{RefLock, StoreAccessExclusive, StoreAccessLock};

/// The record's file name under the shared forklift root.
const FILE_NAME_SEALED: &str = "sealed";

/// The [`record_utils`] kind and version of the record's header line.
const SEALED_RECORD: &str = "sealed";
const SEALED_VERSION: u32 = 2;

/// Opens every frame. Nonzero, so a zero-filled region left by a crash never reads as a frame.
pub const FRAME_MAGIC: u32 = 0x5345_414C;

/// The bytes of a frame around its entries: magic and length before them, the CRC after.
const FRAME_OVERHEAD: usize = 12;

/// The length of an object hash, in bytes.
const HASH_LEN: usize = 32;

/// The length of one entry: an operation byte and a hash.
const ENTRY_LEN: usize = 1 + HASH_LEN;

/// The most entries one frame holds. A longer append writes several frames.
pub const MAX_FRAME_ENTRIES: usize = 1024;

/// The length of the largest frame.
const MAX_FRAME_LEN: usize = FRAME_OVERHEAD + MAX_FRAME_ENTRIES * ENTRY_LEN;

/// What one frame does to the hash it names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SealOp {
    /// The object is held by hash alone from here on.
    Record,
    /// The object is no longer held by hash alone.
    Remove,
}

impl SealOp {
    fn to_byte(self) -> u8 {
        match self {
            SealOp::Record => 1,
            SealOp::Remove => 2,
        }
    }

    fn from_byte(byte: u8) -> Option<SealOp> {
        match byte {
            1 => Some(SealOp::Record),
            2 => Some(SealOp::Remove),
            _ => None,
        }
    }
}

/// The hashes the record holds after replaying every valid frame.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SealedSet {
    hashes: HashSet<String>,
}

impl SealedSet {
    /// Whether `hash` is recorded.
    pub fn contains(&self, hash: &str) -> bool {
        self.hashes.contains(hash)
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Apply one entry, as replaying its frame does.
    fn apply(&mut self, op: SealOp, hash: String) {
        match op {
            SealOp::Record => self.hashes.insert(hash),
            SealOp::Remove => self.hashes.remove(&hash),
        };
    }
}

/// A parsed record: its replayed set, and how many leading bytes are the header and valid frames.
/// A writer that finds bytes past `valid_len` rewrites the file to that prefix and its new frames.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedSealed {
    pub set: SealedSet,
    pub valid_len: usize,
}

/// The path of the record.
pub fn sealed_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_SEALED)
}

/// The header line a record opens with, line ending included.
pub fn header_bytes() -> Vec<u8> {
    let mut header = record_utils::header(SEALED_RECORD, SEALED_VERSION).into_bytes();
    header.push(b'\n');
    header
}

/// Encode one frame that applies `op` to `hash`.
///
/// # Arguments
/// * `op`   - What the frame does.
/// * `hash` - The object's hash, as lowercase hex.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The frame's bytes.
/// * `Err(String)` - If `hash` is not a 32-byte lowercase hex hash.
pub fn encode_frame(op: SealOp, hash: &str) -> Result<Vec<u8>, String> {
    encode_frames(&[(op, hash)]).map(|frames| frames.concat())
}

/// Encode `ops` as frames of at most [`MAX_FRAME_ENTRIES`] entries each, in order.
///
/// # Returns
/// * `Ok(Vec<Vec<u8>>)` - Each frame's bytes (none for no ops).
/// * `Err(String)`      - If a hash is not a 32-byte lowercase hex hash.
fn encode_frames(ops: &[(SealOp, &str)]) -> Result<Vec<Vec<u8>>, String> {
    let mut frames = Vec::new();

    for batch in ops.chunks(MAX_FRAME_ENTRIES) {
        let mut entries = Vec::with_capacity(batch.len() * ENTRY_LEN);
        for (op, hash) in batch {
            // Lowercase only, the spelling a replay gives back, so a caller's set and the record's agree.
            let raw = sign_utils::from_hex(hash)
                .ok()
                .filter(|raw| raw.len() == HASH_LEN && sign_utils::to_hex(raw) == *hash)
                .ok_or_else(|| format!("\"{}\" is not an object hash (64 lowercase hex characters).", hash))?;
            entries.push(op.to_byte());
            entries.extend_from_slice(&raw);
        }

        let mut frame = Vec::with_capacity(FRAME_OVERHEAD + entries.len());
        frame.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        frame.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        frame.extend_from_slice(&entries);
        let crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frames.push(frame);
    }

    Ok(frames)
}

/// The entries of the frame at the start of `bytes` and the frame's whole length, or `None` when no
/// valid frame starts there: too few bytes, the wrong magic, a zero length, or a CRC mismatch.
fn read_frame(bytes: &[u8]) -> Option<(&[u8], usize)> {
    let word = |at: usize| -> Option<u32> {
        bytes.get(at..at + 4).map(|b| u32::from_le_bytes(b.try_into().expect("four bytes")))
    };

    if word(0)? != FRAME_MAGIC {
        return None;
    }

    let length = word(4)? as usize;
    if length == 0 {
        return None;
    }

    let entry_end = 8usize.checked_add(length)?;
    let stored_crc = word(entry_end)?;
    if crc32fast::hash(&bytes[..entry_end]) != stored_crc {
        return None;
    }

    Some((&bytes[8..entry_end], entry_end + 4))
}

/// Whether `unread`, everything after the last valid frame, is what a torn append leaves: one
/// frame's worth of bytes at most, zeros included (a crash can extend the file over the write it
/// cut short, and no further), and no valid frame among them, since a frame is synced before the
/// next is written. The torn frame's own header is not trusted for its length: a header torn
/// inside its length field can name a shorter one.
fn is_torn_tail(unread: &[u8]) -> bool {
    unread.len() <= MAX_FRAME_LEN && !(0..unread.len()).any(|at| read_frame(&unread[at..]).is_some())
}

/// Whether a frame may declare `length` bytes of entries: a whole number of them, at least one,
/// at most [`MAX_FRAME_ENTRIES`].
fn is_entries_len(length: usize) -> bool {
    length > 0 && length.is_multiple_of(ENTRY_LEN) && length / ENTRY_LEN <= MAX_FRAME_ENTRIES
}

/// Why a record's bytes do not parse.
#[derive(Debug, PartialEq, Eq)]
enum Unparsed {
    /// Damage: the header is missing, or what follows the valid frames is not what an interrupted
    /// write leaves. A write with fsync off, a failed sync followed by a later append, or stale
    /// blocks after a crash can leave it. The first `valid_len` bytes are the header and the frames
    /// before the damage (none when the header is missing), which [`rebuild`] rewrites the record to.
    Damaged { reason: String, valid_len: usize },

    /// A header naming another version: that version wrote the record. Never rewritten.
    OtherVersion(String),

    /// A frame whose CRC holds over entries this version's header does not define, before any
    /// damage. An intact frame is not what damage leaves, and no version is named to read it, so
    /// the record is never rewritten.
    Unreadable(String),
}

impl Unparsed {
    fn reason(&self) -> &str {
        match self {
            Unparsed::Damaged { reason, .. } | Unparsed::OtherVersion(reason) | Unparsed::Unreadable(reason) => reason,
        }
    }
}

/// Parse a record's bytes (see the module doc comment for the format).
///
/// # Returns
/// * `Ok(ParsedSealed)` - The replayed set and the length of the valid prefix.
/// * `Err(String)`      - If the header is missing or names another version, a valid frame
///                        is of a size no frame has or holds an entry that does not parse, or
///                        what follows the last valid frame is not a torn append: more than one
///                        frame's worth of bytes, or a valid frame among them. Phrased to follow "The record ".
pub fn parse(bytes: &[u8]) -> Result<ParsedSealed, String> {
    parse_classified(bytes).map_err(|unparsed| unparsed.reason().to_string())
}

/// [`parse`], telling damage, which [`rebuild`] repairs, from what this version does not read.
fn parse_classified(bytes: &[u8]) -> Result<ParsedSealed, Unparsed> {
    let header = header_bytes();

    if !bytes.starts_with(&header) {
        let tag = format!("/forklift-{} ", SEALED_RECORD);
        let first = bytes.split(|&b| b == b'\n').next().unwrap_or_default();
        return Err(if first.starts_with(tag.as_bytes()) {
            Unparsed::OtherVersion(format!(
                "is in a format this version of Forklift does not know (\"{}\").",
                String::from_utf8_lossy(first)
            ))
        } else {
            Unparsed::Damaged {
                reason: "does not open with its header, so it is not a sealed-object record.".to_string(),
                valid_len: 0,
            }
        });
    }

    let mut set = SealedSet::default();
    let mut offset = header.len();

    while let Some((entries, frame_len)) = read_frame(&bytes[offset..]) {
        let unreadable = |at: usize| Unparsed::Unreadable(format!(
            "holds an entry at byte {} that this version of Forklift cannot read.",
            at
        ));

        if !is_entries_len(entries.len()) {
            return Err(Unparsed::Unreadable(format!("holds a frame at byte {} of a size no frame has.", offset)));
        }

        for (index, entry) in entries.chunks(ENTRY_LEN).enumerate() {
            let op = SealOp::from_byte(entry[0]).ok_or_else(|| unreadable(offset + 8 + index * ENTRY_LEN))?;
            set.apply(op, sign_utils::to_hex(&entry[1..]));
        }

        offset += frame_len;
    }

    // A torn append leaves at most one frame's bytes, zeros the filesystem extended the file with
    // included. Anything longer is not a torn tail: the frames behind it would be lost.
    if !is_torn_tail(&bytes[offset..]) {
        return Err(Unparsed::Damaged {
            reason: format!(
                "is damaged at byte {}: what follows it is not what an interrupted write leaves.",
                offset
            ),
            valid_len: offset,
        });
    }

    Ok(ParsedSealed { set, valid_len: offset })
}

/// Read the record. A warehouse without one has sealed nothing.
///
/// # Returns
/// * `Ok(SealedSet)` - The recorded hashes (empty when there is no record).
/// * `Err(String)`   - If the record cannot be read or parsed.
pub fn read_sealed() -> Result<SealedSet, String> {
    let path = sealed_path();

    Ok(match read_record(&path)? {
        Some((_, parsed)) => parsed.set,
        None => SealedSet::default(),
    })
}

/// Read and parse the record at `path`.
///
/// # Returns
/// * `Ok(Some((bytes, parsed)))` - The record's bytes and what they parse to.
/// * `Ok(None)`                  - There is no record.
/// * `Err(String)`               - If the record cannot be read or parsed.
fn read_record(path: &Path) -> Result<Option<(Vec<u8>, ParsedSealed)>, String> {
    match std::fs::read(path) {
        Ok(bytes) => match parse_classified(&bytes) {
            Ok(parsed) => Ok(Some((bytes, parsed))),
            Err(unparsed) => Err(format!(
                "The sealed-object record \"{}\" {} {}",
                path.to_string_lossy(),
                unparsed.reason(),
                match unparsed {
                    Unparsed::Damaged { valid_len: 0, .. } => "Run \"forklift heal\", which rebuilds it empty: \
                        every record in it is dropped, and an absent object it held by hash then reads as missing.",
                    Unparsed::Damaged { .. } => "Run \"forklift heal\", which rebuilds it from the part before the \
                        damage and reports what it drops.",
                    Unparsed::OtherVersion(_) => "Use the version of Forklift that wrote it: this one never \
                        writes over it.",
                    Unparsed::Unreadable(_) => "Nothing rebuilds an intact frame this version cannot read: \
                        restore the record from a good copy.",
                }
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Error while reading the sealed-object record \"{}\": {}",
            path.to_string_lossy(),
            e
        )),
    }
}

/// What [`rebuild`] dropped from a damaged record.
#[derive(Debug, PartialEq, Eq)]
pub struct Rebuilt {
    /// The byte the damage starts at: everything before it was kept.
    pub at: usize,

    /// How many bytes from there on were dropped.
    pub dropped_bytes: usize,
}

/// [`rebuild_locked`] under a ref lock it takes and releases, taken only when a read without it
/// finds the record damaged: a sound record, the usual case, waits on no writer.
pub fn rebuild() -> Result<Option<Rebuilt>, String> {
    match std::fs::read(sealed_path()) {
        Ok(bytes) if !matches!(parse_classified(&bytes), Err(Unparsed::Damaged { .. })) => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        _ => {}
    }

    let lock = RefLock::acquire()?;
    rebuild_locked(&lock)
}

/// Rewrite a damaged record, atomically, as its header and the frames before the damage.
///
/// Only damage is rewritten (see [`parse`]): a record that reads, a torn append included, is left
/// for the next write, and one this version does not read is left as it is. The frames are kept
/// as they were and nothing past the damage is salvaged, though intact frames may follow it: their
/// order against the entries lost in the damage cannot be known, and a frame from stale blocks
/// could record a hash again. Dropping entries can only lose what the log can already lose (see
/// the module doc comment): a dropped record reads as a missing object, which every walk reports,
/// and a dropped removal leaves a present object recorded, which `heal` removes.
///
/// # Returns
/// * `Ok(Some(Rebuilt))` - The record was damaged and now holds what it held before the damage.
/// * `Ok(None)`          - There is no record, or it is not damaged.
/// * `Err(String)`       - If the record cannot be read or the rewrite fails. A rewrite that fails
///                         after its rename has still replaced the record, so the message says
///                         what the rewrite drops.
fn rebuild_locked(_lock: &RefLock) -> Result<Option<Rebuilt>, String> {
    let path = sealed_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!(
            "Error while reading the sealed-object record \"{}\": {}",
            path.to_string_lossy(),
            e
        )),
    };

    let Err(Unparsed::Damaged { valid_len, .. }) = parse_classified(&bytes) else {
        return Ok(None);
    };

    let rebuilt = match valid_len {
        0 => header_bytes(),
        _ => bytes[..valid_len].to_vec(),
    };
    file_utils::replace_file_atomically(&path, &rebuilt).map_err(|error| format!(
        "Rebuilding the damaged sealed-object record, which drops the {} byte(s) from byte {} on, \
        failed, though the rewrite may have landed: {}",
        bytes.len() - valid_len, valid_len, error
    ))?;

    Ok(Some(Rebuilt { at: valid_len, dropped_bytes: bytes.len() - valid_len }))
}

/// Whether `hash` is in `sealed`, where `None` means no record applies (the push gate).
pub fn is_recorded(sealed: Option<&SealedSet>, hash: &str) -> bool {
    sealed.is_some_and(|set| set.contains(hash))
}

/// [`append_locked`] under a ref lock it takes and releases.
pub fn append(ops: &[(SealOp, &str)]) -> Result<SealedSet, String> {
    let lock = RefLock::acquire()?;
    append_locked(&lock, ops)
}

/// Apply `ops` to the record, in order, for a caller that holds the ref lock.
///
/// The record is re-read under the lock first. A record that does not parse is left as it is. With
/// no record yet, it is created holding `ops`, atomically. When a torn append follows the valid
/// frames, the record is rewritten atomically to them and `ops`, which drops the torn bytes, and a
/// reader that read the old file in pieces never sees new frames after them. Otherwise the frames
/// are appended, each synced before the next is written, so a torn append can only ever be the
/// last frame. Every way, `ops` are on disk when this returns, unless fsync is turned off, when a
/// crash may leave the record damaged.
///
/// # Arguments
/// * `_lock` - The held ref lock, which orders this write against every other.
/// * `ops`   - What to record and remove, in order. None writes nothing.
///
/// # Returns
/// * `Ok(SealedSet)` - The set the record now replays to.
/// * `Err(String)`   - If a hash is not an object hash, the record cannot be read or parsed, or
///                     the write fails. Nothing is written in the first two cases. A failed
///                     write may still have landed: some of its frames whole (and perhaps a
///                     torn one, which the next write drops), or the whole new record when the
///                     create failed after its rename. Read the record back to learn which.
pub fn append_locked(lock: &RefLock, ops: &[(SealOp, &str)]) -> Result<SealedSet, String> {
    let ops: Vec<(SealOp, String)> = ops.iter().map(|(op, hash)| (*op, hash.to_string())).collect();
    append_decided_locked(lock, |_| Ok(ops))
}

/// [`append_locked`] with the ops decided from the set the record replays to when it is re-read
/// under the lock, so a decision about what is recorded is made on what is recorded.
fn append_decided_locked(
    _lock: &RefLock,
    decide: impl FnOnce(&SealedSet) -> Result<Vec<(SealOp, String)>, String>,
) -> Result<SealedSet, String> {
    let path = sealed_path();
    let existing = read_record(&path)?;

    // A prefix to rewrite the record from: the header when there is no record, the valid frames
    // when a torn append follows them, and none when the frames can be appended in place.
    let (rewrite_from, mut set) = match existing {
        Some((mut bytes, parsed)) => {
            let torn = parsed.valid_len < bytes.len();
            bytes.truncate(parsed.valid_len);
            (torn.then_some(bytes), parsed.set)
        }
        None => (Some(header_bytes()), SealedSet::default()),
    };

    let ops = decide(&set)?;
    let borrowed: Vec<(SealOp, &str)> = ops.iter().map(|(op, hash)| (*op, hash.as_str())).collect();
    let frames = encode_frames(&borrowed)?;

    for (op, hash) in ops {
        set.apply(op, hash);
    }

    if frames.is_empty() {
        return Ok(set);
    }

    match rewrite_from {
        Some(mut prefix) => {
            prefix.extend(frames.concat());
            file_utils::replace_file_atomically(&path, &prefix)?;
        }
        None => append_frames(&path, &frames)?,
    }

    Ok(set)
}

/// Append `frames` to the end of the record at `path`, syncing each before writing the next.
fn append_frames(path: &Path, frames: &[Vec<u8>]) -> Result<(), String> {
    let failed = |e: std::io::Error| format!(
        "Error while appending to the sealed-object record \"{}\": {}",
        path.to_string_lossy(),
        e
    );

    // A write handle that is not an append-only one: Windows flushes only a handle opened for
    // writing. The ref lock keeps every other writer out, so the end found here stays the end.
    let mut file = std::fs::OpenOptions::new().write(true).open(path).map_err(failed)?;
    file.seek(std::io::SeekFrom::End(0)).map_err(failed)?;

    for frame in frames {
        file.write_all(frame).map_err(failed)?;

        if file_utils::fsync_enabled() {
            file.sync_all().map_err(failed)?;
        }
    }

    Ok(())
}

/// [`compact_locked`] under a ref lock it takes and releases.
pub fn compact() -> Result<SealedSet, String> {
    let lock = RefLock::acquire()?;
    compact_locked(&lock)
}

/// Rewrite the record, atomically, as the set it replays to: one record entry per hash, sorted.
///
/// That collapses each removal with the record before it and keeps any record after it, so
/// evict → refill → evict leaves the hash recorded. It drops no recorded hash, so it needs no
/// push-down: a removal that lost coverage was pushed down when it was appended. A torn append
/// after the valid frames is dropped with the rest of the old log.
///
/// # Returns
/// * `Ok(SealedSet)` - The set the record replays to, unchanged.
/// * `Err(String)`   - If the record cannot be read or parsed, which leaves it as it was, or the
///                     rewrite fails. A rewrite that fails after its rename has still replaced
///                     the record (with the same set): read it back to learn which.
pub fn compact_locked(_lock: &RefLock) -> Result<SealedSet, String> {
    let path = sealed_path();
    let Some((bytes, parsed)) = read_record(&path)? else {
        return Ok(SealedSet::default());
    };

    let mut hashes: Vec<&String> = parsed.set.hashes.iter().collect();
    hashes.sort();
    let ops: Vec<(SealOp, &str)> = hashes.into_iter().map(|hash| (SealOp::Record, hash.as_str())).collect();

    let mut compacted = header_bytes();
    compacted.extend(encode_frames(&ops)?.concat());

    if compacted != bytes {
        file_utils::replace_file_atomically(&path, &compacted)?;
    }

    Ok(parsed.set)
}

/// A hold on the store-access lock: the shared [`StoreAccessLock`], or the [`StoreAccessExclusive`]
/// a deleter holds. While one is held, no other process deletes an object from the store.
pub trait StoreAccessHold {}

impl StoreAccessHold for StoreAccessLock {}

impl StoreAccessHold for StoreAccessExclusive {}

/// What [`unseal`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Unsealed {
    /// The objects whose records were removed, sorted.
    pub removed: Vec<String>,
    /// The absent objects beneath them that the record holds now and did not before, sorted.
    pub pushed_down: Vec<String>,
    /// The objects left recorded, each with why: one not in the store, or one whose closure could
    /// not be walked.
    pub kept: Vec<(String, String)>,
}

/// The recorded objects the store holds, sorted. Each seals nothing while it is present, and
/// [`unseal`] removes its record.
///
/// # Returns
/// * `Ok(Vec<String>)` - The recorded hashes present in the store, loose or packed.
/// * `Err(String)`     - If the store could not be checked.
pub fn present_recorded(record: &SealedSet) -> Result<Vec<String>, String> {
    let mut present = Vec::new();

    for hash in &record.hashes {
        if file_utils::raw_object_present(hash)? {
            present.push(hash.clone());
        }
    }

    present.sort();
    Ok(present)
}

/// Remove the records of the present objects `hashes`, each after a deep push-down: every absent
/// object its present closure reaches is recorded first, so the removal exposes nothing it held.
/// An absent object beneath a recorded one is held on the same premise as that one, that the
/// remote holds its closure. The walk is by hash, not path: it records an absent object whatever
/// the fetch scope says of the paths that reach it.
///
/// The walk runs without the ref lock. The commit takes it, re-reads the record, and appends the
/// push-down's records, then the removals of those `hashes` still recorded, synced before this
/// returns. Records first: a crash between them leaves an object present and recorded, which this
/// repairs on the next run. Every absence the walks found is recorded, even beneath an object
/// whose record another writer removed meanwhile: the premise is what the remote holds, and that
/// object was recorded when it was walked. An absence a fetch has filled since the walk is not,
/// unless the fill lands after the last check, just before the commit.
///
/// # Arguments
/// * `_hold`  - The caller's store-access hold, shared or exclusive. It must span the whole call,
///              so that nothing deletes an object the walk found present before the commit lands.
///              This never takes one of its own, so a deleter calling it does not wait on itself.
/// * `hashes` - The objects to unseal. One not in the store, or whose closure cannot be walked
///              (an object beneath it is unreadable, or may be in a pack index that cannot be
///              read), stays recorded and is listed in [`Unsealed::kept`].
///
/// # Returns
/// * `Ok(Unsealed)` - What was removed, pushed down and kept.
/// * `Err(String)`  - If the record cannot be read or parsed under the lock, the ref lock cannot be
///                    taken, or the append fails ([`append_locked`] says what a failed append may
///                    have left).
pub fn unseal(_hold: &impl StoreAccessHold, hashes: &[String]) -> Result<Unsealed, String> {
    // Trees and recipes whose whole closure a walk that finished has checked: a later walk that
    // reaches one has nothing more to find beneath it.
    let mut closed: HashSet<String> = HashSet::new();
    let mut absent: BTreeSet<String> = BTreeSet::new();
    let mut walked: Vec<String> = Vec::new();
    let mut kept: Vec<(String, String)> = Vec::new();

    for hash in hashes {
        match closure_absences(hash, &closed) {
            Ok(closure) => {
                closed.extend(closure.walked);
                absent.extend(closure.absent);
                walked.push(hash.clone());
            }
            Err(reason) => kept.push((hash.clone(), reason)),
        }
    }

    if walked.is_empty() {
        return Ok(Unsealed { kept, ..Unsealed::default() });
    }

    // An absence a fetch has filled since the walk is not recorded: recorded and present, it would
    // be the very state this repairs. Checked before the lock, so the time it is held does not grow
    // with the push-down; a fill landing after this check seals nothing, and the next run repairs it.
    let mut still_absent = Vec::new();
    for hash in absent {
        if !file_utils::raw_object_present(&hash)? {
            still_absent.push(hash);
        }
    }

    let lock = RefLock::acquire()?;
    let mut removed = Vec::new();
    let mut pushed_down = Vec::new();

    append_decided_locked(&lock, |set| {
        pushed_down = still_absent.into_iter().filter(|hash| !set.contains(hash)).collect();
        removed = walked.into_iter().filter(|hash| set.contains(hash)).collect::<BTreeSet<String>>().into_iter().collect();

        Ok(pushed_down.iter().map(|hash| (SealOp::Record, hash.clone()))
            .chain(removed.iter().map(|hash| (SealOp::Remove, hash.clone())))
            .collect())
    })?;

    Ok(Unsealed { removed, pushed_down, kept })
}

/// What one push-down walk found.
struct ClosureAbsences {
    /// The trees and recipes it walked.
    walked: HashSet<String>,
    /// The absent objects it reached.
    absent: Vec<String>,
}

/// Walk the present closure of `hash`, collecting every absent object it reaches. An absent object
/// is not descended: whatever lies beneath it is held by its own record. A tree or recipe in
/// `closed` is not walked again.
///
/// # Returns
/// * `Ok(ClosureAbsences)` - What the walk went through and what it found absent.
/// * `Err(String)`         - Why the closure could not be walked: `hash` is not in the store, or
///                           an object beneath it could not be read or may be in a pack index
///                           that cannot be read.
fn closure_absences(hash: &str, closed: &HashSet<String>) -> Result<ClosureAbsences, String> {
    let mut walk = ClosureAbsences { walked: HashSet::new(), absent: Vec::new() };

    if !is_present(hash)? {
        return Err("it is not in the store".to_string());
    }

    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
    let mut trees: VecDeque<String> = VecDeque::new();

    match object_utils::peek_object_header(&bytes)?.0 {
        ObjectType::Tree => trees.push_back(hash.to_string()),
        ObjectType::Recipe => recipe_absences(hash, closed, &mut walk)?,
        ObjectType::Blob | ObjectType::Chunk => {}
        // A parcel names its tree and its parents, a history this walk does not follow, so its
        // record is not removed on a push-down that would miss them.
        ObjectType::Parcel => return Err("it is a parcel, and the history it names is not walked here".to_string()),
    }

    while let Some(tree_hash) = trees.pop_front() {
        if closed.contains(&tree_hash) || !walk.walked.insert(tree_hash.clone()) {
            continue;
        }

        let tree = object_utils::load_tree_shared(&tree_hash)?;

        for (_, file) in tree.get_files() {
            if !is_present(&file.hash)? {
                walk.absent.push(file.hash.clone());
            } else if file.item_type.is_chunked() {
                recipe_absences(&file.hash, closed, &mut walk)?;
            }
        }

        for (_, subtree) in tree.get_subtrees() {
            if is_present(&subtree.hash)? {
                trees.push_back(subtree.hash.clone());
            } else {
                walk.absent.push(subtree.hash.clone());
            }
        }
    }

    Ok(walk)
}

/// Collect the absent chunks of the present recipe `hash` into `walk`, unless `closed` or `walk`
/// already holds it.
fn recipe_absences(hash: &str, closed: &HashSet<String>, walk: &mut ClosureAbsences) -> Result<(), String> {
    if closed.contains(hash) || !walk.walked.insert(hash.to_string()) {
        return Ok(());
    }

    for chunk in object_utils::recipe_chunk_hashes(hash)? {
        if !is_present(&chunk)? {
            walk.absent.push(chunk);
        }
    }

    Ok(())
}

/// Whether the store holds `hash`. An object only an unreadable pack index might hold is neither:
/// its bytes may be on disk, and what it references may be present, so the walk cannot go on.
fn is_present(hash: &str) -> Result<bool, String> {
    if file_utils::raw_object_present(hash)? {
        return Ok(true);
    }

    match pack_utils::sweep_unreadable_reason(hash)? {
        None => Ok(false),
        Some(reason) => Err(format!("{} may be in the store but cannot be read ({})", hash, reason)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The length of a frame of one entry.
    const FRAME_LEN: usize = FRAME_OVERHEAD + ENTRY_LEN;

    const A: &str = "aa00000000000000000000000000000000000000000000000000000000000001";
    const B: &str = "bb00000000000000000000000000000000000000000000000000000000000002";

    fn record(frames: &[(SealOp, &str)]) -> Vec<u8> {
        let mut bytes = header_bytes();
        for (op, hash) in frames {
            bytes.extend(encode_frame(*op, hash).unwrap());
        }
        bytes
    }

    /// A frame whose CRC is right over an entry this build cannot read.
    fn frame_with_entry(entry: &[u8]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
        frame.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        frame.extend_from_slice(entry);
        let crc = crc32fast::hash(&frame);
        frame.extend_from_slice(&crc.to_le_bytes());
        frame
    }

    #[test]
    fn frames_replay_in_order() {
        let bytes = record(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]);
        let parsed = parse(&bytes).unwrap();

        assert!(!parsed.set.contains(A));
        assert!(parsed.set.contains(B));
        assert_eq!(parsed.valid_len, bytes.len());

        // Recorded again after its removal: the last frame wins.
        let bytes = record(&[(SealOp::Record, A), (SealOp::Remove, A), (SealOp::Record, A)]);
        assert!(parse(&bytes).unwrap().set.contains(A));
    }

    #[test]
    fn a_torn_tail_is_ignored_and_the_valid_prefix_measured() {
        let whole = record(&[(SealOp::Record, A), (SealOp::Record, B)]);
        let first_end = header_bytes().len() + FRAME_OVERHEAD + 1 + HASH_LEN;

        for cut in first_end..whole.len() {
            let parsed = parse(&whole[..cut]).unwrap();
            assert!(parsed.set.contains(A), "cut at {}", cut);
            assert!(!parsed.set.contains(B), "cut at {}", cut);
            assert_eq!(parsed.valid_len, first_end, "cut at {}", cut);
        }
    }

    #[test]
    fn zeros_after_the_last_frame_never_read_as_frames() {
        let mut bytes = record(&[(SealOp::Record, A)]);
        let valid = bytes.len();
        bytes.extend(std::iter::repeat_n(0u8, 4096));

        let parsed = parse(&bytes).unwrap();
        assert!(parsed.set.contains(A));
        assert_eq!(parsed.valid_len, valid);
    }

    #[test]
    fn zeros_past_one_frame_are_damage_not_a_torn_tail() {
        // A crash extends the file over the one write it cut short, and no further: a longer run
        // of zeros is synced frames lost to damage, and must not be cut away as a torn tail.
        let mut bytes = record(&[(SealOp::Record, A)]);
        bytes.extend(std::iter::repeat_n(0u8, MAX_FRAME_LEN));
        assert!(parse(&bytes).is_ok());

        bytes.push(0);
        assert!(parse(&bytes).unwrap_err().contains("damaged"));
    }

    #[test]
    fn an_unreadable_entry_is_named_at_its_own_byte() {
        let mut bytes = record(&[(SealOp::Record, A)]);
        let frame_at = bytes.len();
        let mut entries = vec![1u8];
        entries.extend_from_slice(&[0xaa; HASH_LEN]);
        entries.push(9);
        entries.extend_from_slice(&[0xbb; HASH_LEN]);
        bytes.extend(frame_with_entry(&entries));

        let err = parse(&bytes).unwrap_err();
        assert!(err.contains(&format!("at byte {} ", frame_at + 8 + ENTRY_LEN)), "{}", err);
    }

    #[test]
    fn a_frame_with_a_bad_crc_ends_the_record() {
        let mut bytes = record(&[(SealOp::Record, A), (SealOp::Record, B)]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;

        let parsed = parse(&bytes).unwrap();
        assert!(parsed.set.contains(A));
        assert!(!parsed.set.contains(B));
    }

    #[test]
    fn a_whole_torn_frame_is_still_a_torn_tail() {
        let mut bytes = record(&[(SealOp::Record, A), (SealOp::Remove, A)]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;

        let parsed = parse(&bytes).unwrap();
        assert!(parsed.set.contains(A), "the torn removal is not replayed");
    }

    #[test]
    fn damage_with_frames_behind_it_fails_the_read() {
        let mut bytes = record(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]);
        let middle = header_bytes().len() + FRAME_LEN + 9;
        bytes[middle] ^= 0xff;

        let err = parse(&bytes).expect_err("a later removal must not be silently dropped");
        assert!(err.contains("damaged"), "{}", err);
    }

    #[test]
    fn a_valid_frame_holding_an_unknown_entry_fails_the_read() {
        let mut unknown_op = record(&[(SealOp::Record, A)]);
        let mut entry = vec![9u8];
        entry.extend_from_slice(&[0u8; HASH_LEN]);
        unknown_op.extend(frame_with_entry(&entry));
        assert!(parse(&unknown_op).unwrap_err().contains("cannot read"));

        let mut short_hash = record(&[(SealOp::Record, A)]);
        short_hash.extend(frame_with_entry(&[1u8, 0xaa, 0xbb]));
        assert!(parse(&short_hash).unwrap_err().contains("of a size no frame has"));
    }

    #[test]
    fn another_version_or_no_header_fails_the_read() {
        let mut other = b"/forklift-sealed 3\n".to_vec();
        other.extend(encode_frame(SealOp::Record, A).unwrap());
        assert!(parse(&other).unwrap_err().contains("format this version"));

        let bare = encode_frame(SealOp::Record, A).unwrap();
        assert!(parse(&bare).unwrap_err().contains("header"));
        assert!(parse(b"").unwrap_err().contains("header"));
    }

    #[test]
    fn a_header_alone_records_nothing() {
        let parsed = parse(&header_bytes()).unwrap();
        assert!(parsed.set.is_empty());
        assert_eq!(parsed.valid_len, header_bytes().len());
    }

    #[test]
    fn a_frame_holds_many_entries_and_stands_or_falls_whole() {
        let mut bytes = header_bytes();
        bytes.extend(encode_frames(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]).unwrap().concat());
        let parsed = parse(&bytes).unwrap();
        assert!(!parsed.set.contains(A));
        assert!(parsed.set.contains(B));

        // Torn anywhere inside, the frame replays none of its entries.
        for cut in header_bytes().len()..bytes.len() {
            let parsed = parse(&bytes[..cut]).unwrap();
            assert!(parsed.set.is_empty(), "cut at {}", cut);
        }
    }

    #[test]
    fn a_long_batch_splits_into_frames_of_at_most_the_largest_size() {
        let hashes: Vec<String> = (0..MAX_FRAME_ENTRIES + 1).map(|i| format!("{:064x}", i + 1)).collect();
        let ops: Vec<(SealOp, &str)> = hashes.iter().map(|hash| (SealOp::Record, hash.as_str())).collect();

        let frames = encode_frames(&ops).unwrap();
        assert_eq!(frames.iter().map(Vec::len).collect::<Vec<_>>(), vec![MAX_FRAME_LEN, FRAME_OVERHEAD + ENTRY_LEN]);

        let mut bytes = header_bytes();
        bytes.extend(frames.concat());
        let parsed = parse(&bytes).unwrap();
        assert!(hashes.iter().all(|hash| parsed.set.contains(hash)));
    }

    #[test]
    fn a_torn_frame_whose_header_never_landed_is_bounded_by_the_largest_frame() {
        // The append's first bytes never reached the disk; later ones did.
        let mut bytes = record(&[(SealOp::Record, A)]);
        let valid = bytes.len();
        bytes.extend(std::iter::repeat_n(0u8, 4096));
        bytes.extend(std::iter::repeat_n(0x5au8, MAX_FRAME_LEN - 4096));
        assert_eq!(parse(&bytes).unwrap().valid_len, valid);

        bytes.push(0x5a);
        assert!(parse(&bytes).unwrap_err().contains("damaged"));
    }

    #[test]
    fn a_torn_frame_whose_length_field_tore_is_still_a_torn_tail() {
        // A long append whose second length byte never landed, though later bytes of it did:
        // the header now names a one-entry frame, far shorter than what follows.
        let hashes: Vec<String> = (0..257).map(|i| format!("{:064x}", i + 1)).collect();
        let ops: Vec<(SealOp, &str)> = hashes.iter().map(|hash| (SealOp::Record, hash.as_str())).collect();
        let mut torn = encode_frames(&ops).unwrap().concat();
        assert_eq!(&torn[4..6], &[0x21, 0x21]);
        torn[5] = 0;
        torn.truncate(torn.len() - 1);

        let mut bytes = record(&[(SealOp::Record, A)]);
        let valid = bytes.len();
        bytes.extend(&torn);
        assert_eq!(parse(&bytes).unwrap().valid_len, valid);
    }

    #[test]
    fn a_valid_frame_after_the_break_is_damage_however_near_the_end() {
        // Each frame is synced before the next is written, so a torn append has nothing valid
        // after it: a frame that validates there proves the break is damage.
        let mut bytes = record(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]);
        bytes[header_bytes().len() + FRAME_LEN] ^= 0xff;

        let err = parse(&bytes).expect_err("the removal behind a damaged magic must not be dropped");
        assert!(err.contains("damaged"), "{}", err);
    }

    #[test]
    fn a_valid_frame_of_a_size_no_frame_has_fails_the_read() {
        let mut ragged = record(&[(SealOp::Record, A)]);
        let mut entries = vec![1u8];
        entries.extend_from_slice(&[0xaa; HASH_LEN]);
        entries.push(1);
        ragged.extend(frame_with_entry(&entries));
        assert!(parse(&ragged).unwrap_err().contains("of a size no frame has"));

        let mut oversized = record(&[(SealOp::Record, A)]);
        let entries: Vec<u8> = (0..MAX_FRAME_ENTRIES + 1).flat_map(|_| {
            let mut entry = vec![1u8];
            entry.extend_from_slice(&[0xaa; HASH_LEN]);
            entry
        }).collect();
        oversized.extend(frame_with_entry(&entries));
        assert!(parse(&oversized).unwrap_err().contains("of a size no frame has"));
    }

    fn scratch(name: &str) -> (PathBuf, crate::globals::StorageRootScope) {
        let root = std::env::temp_dir().join(format!("forklift-sealed-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = crate::globals::StorageRootScope::enter(&root);
        (root, scope)
    }

    #[test]
    fn the_first_append_creates_the_record_and_later_ones_append_in_order() {
        let (root, _scope) = scratch("append");

        assert!(append(&[]).unwrap().is_empty());
        assert!(!sealed_path().exists(), "no ops, no record");

        let set = append(&[(SealOp::Record, A)]).unwrap();
        assert!(set.contains(A));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), record(&[(SealOp::Record, A)]));

        #[cfg(unix)]
        let created = {
            use std::os::unix::fs::MetadataExt;
            std::fs::metadata(sealed_path()).unwrap().ino()
        };
        let set = append(&[(SealOp::Record, B), (SealOp::Remove, A)]).unwrap();
        assert!(!set.contains(A) && set.contains(B));
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(std::fs::metadata(sealed_path()).unwrap().ino(), created, "not rewritten");
        }
        assert_eq!(read_sealed().unwrap(), set);

        let mut expected = record(&[(SealOp::Record, A)]);
        expected.extend(encode_frames(&[(SealOp::Record, B), (SealOp::Remove, A)]).unwrap().concat());
        assert_eq!(std::fs::read(sealed_path()).unwrap(), expected, "appended in place, one frame");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_append_past_one_frame_lands_as_several_frames_in_place() {
        let (root, _scope) = scratch("split-append");
        append(&[(SealOp::Record, A)]).unwrap();

        let hashes: Vec<String> = (0..MAX_FRAME_ENTRIES + 1).map(|i| format!("{:064x}", i + 1)).collect();
        let ops: Vec<(SealOp, &str)> = hashes.iter().map(|hash| (SealOp::Record, hash.as_str())).collect();
        let set = append(&ops).unwrap();

        let mut expected = record(&[(SealOp::Record, A)]);
        expected.extend(encode_frames(&ops).unwrap().concat());
        assert_eq!(std::fs::read(sealed_path()).unwrap(), expected, "two frames, appended");
        assert_eq!(read_sealed().unwrap(), set);
        assert!(set.contains(A) && hashes.iter().all(|hash| set.contains(hash)));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_append_after_a_torn_one_drops_the_torn_bytes() {
        let (root, _scope) = scratch("torn");
        append(&[(SealOp::Record, A)]).unwrap();

        let mut bytes = std::fs::read(sealed_path()).unwrap();
        bytes.extend(&encode_frame(SealOp::Remove, A).unwrap()[..20]);
        bytes.extend([0u8; 100]);
        std::fs::write(sealed_path(), &bytes).unwrap();

        let set = append(&[(SealOp::Record, B)]).unwrap();
        assert!(set.contains(A) && set.contains(B), "the torn removal was never replayed");
        assert_eq!(std::fs::read(sealed_path()).unwrap(), record(&[(SealOp::Record, A), (SealOp::Record, B)]));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_write_leaves_a_record_it_cannot_read_as_it_is() {
        let (root, _scope) = scratch("damaged");

        let mut damaged = record(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]);
        damaged[header_bytes().len() + FRAME_LEN + 9] ^= 0xff;
        std::fs::write(sealed_path(), &damaged).unwrap();

        assert!(append(&[(SealOp::Record, B)]).unwrap_err().contains("damaged"));
        assert!(compact().unwrap_err().contains("damaged"));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), damaged);

        let other = b"/forklift-sealed 3\n".to_vec();
        std::fs::write(sealed_path(), &other).unwrap();
        assert!(append(&[(SealOp::Record, B)]).unwrap_err().contains("format this version"));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), other);

        let _ = std::fs::remove_dir_all(&root);
    }

    /// FORK-309: heal's rebuild keeps the header and the frames before the damage, verbatim, and
    /// drops the rest, intact frames after it included; writes then work again. Mutations: rebuild
    /// to the frames past the damage as well → the removal of A is replayed → red; skip the
    /// rewrite → the read still fails → red.
    #[test]
    fn a_damaged_record_is_rebuilt_from_the_frames_before_the_damage() {
        let (root, _scope) = scratch("rebuild");

        let mut damaged = record(&[(SealOp::Record, A), (SealOp::Record, B), (SealOp::Remove, A)]);
        let at = header_bytes().len() + FRAME_LEN;
        damaged[at + 9] ^= 0xff;
        std::fs::write(sealed_path(), &damaged).unwrap();
        let err = read_sealed().unwrap_err();
        assert!(err.contains("damaged") && err.contains("Run \"forklift heal\""), "{}", err);

        assert_eq!(rebuild().unwrap(), Some(Rebuilt { at, dropped_bytes: damaged.len() - at }));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), record(&[(SealOp::Record, A)]));
        assert!(read_sealed().unwrap().contains(A), "the removal past the damage is not replayed");
        assert_eq!(rebuild().unwrap(), None, "a sound record is not rebuilt again");

        assert!(append(&[(SealOp::Record, B)]).unwrap().contains(B));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A record whose header is missing is damage too: its read says it is rebuilt empty, and it is
    /// rebuilt as a header alone. Mutation: name the salvage step for it → red.
    #[test]
    fn a_record_without_its_header_is_rebuilt_empty() {
        let (root, _scope) = scratch("rebuild-headless");

        let headless = encode_frame(SealOp::Record, A).unwrap();
        std::fs::write(sealed_path(), &headless).unwrap();
        let err = read_sealed().unwrap_err();
        assert!(err.contains("rebuilds it empty"), "{}", err);

        assert_eq!(rebuild().unwrap(), Some(Rebuilt { at: 0, dropped_bytes: headless.len() }));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), header_bytes());
        assert!(read_sealed().unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// What is not damage is never rebuilt: a record that reads (a torn append left for the next
    /// write), one in another version, and one holding an intact frame this version cannot read.
    /// Each is left byte for byte, and the read names a remedy other than heal for the last two.
    /// Mutation: rebuild every record that does not parse → the other version is rewritten → red.
    #[test]
    fn what_is_not_damage_is_never_rebuilt() {
        let (root, _scope) = scratch("rebuild-refused");

        let mut torn = record(&[(SealOp::Record, A)]);
        torn.extend(&encode_frame(SealOp::Record, B).unwrap()[..20]);
        let mut other = b"/forklift-sealed 3\n".to_vec();
        other.extend(encode_frame(SealOp::Record, A).unwrap());
        let mut unknown_op = record(&[(SealOp::Record, A)]);
        let mut entry = vec![9u8];
        entry.extend_from_slice(&[0u8; HASH_LEN]);
        unknown_op.extend(frame_with_entry(&entry));
        unknown_op.extend(std::iter::repeat_n(0xabu8, MAX_FRAME_LEN + 1));

        for (bytes, remedy) in [
            (&torn, None),
            (&other, Some("Use the version of Forklift that wrote it")),
            (&unknown_op, Some("restore the record from a good copy")),
        ] {
            std::fs::write(sealed_path(), bytes).unwrap();
            assert_eq!(rebuild().unwrap(), None);
            assert_eq!(&std::fs::read(sealed_path()).unwrap(), bytes);
            if let Some(remedy) = remedy {
                let err = read_sealed().unwrap_err();
                assert!(err.contains(remedy) && !err.contains("forklift heal"), "{}", err);
            }
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn an_append_naming_something_that_is_not_a_hash_writes_nothing() {
        let (root, _scope) = scratch("bad-hash");
        append(&[(SealOp::Record, A)]).unwrap();
        let before = std::fs::read(sealed_path()).unwrap();

        assert!(append(&[(SealOp::Record, B), (SealOp::Record, "abc")]).is_err());
        // An uppercase spelling is refused too: the replay would give it back lowercase.
        assert!(append(&[(SealOp::Record, &B.to_uppercase())]).is_err());
        assert_eq!(std::fs::read(sealed_path()).unwrap(), before);

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compaction_keeps_the_set_and_a_record_after_a_removal() {
        let (root, _scope) = scratch("compact");
        const C: &str = "cc00000000000000000000000000000000000000000000000000000000000003";

        // evict → refill → evict for A; evict → refill for B; a removal of C nothing recorded.
        append(&[(SealOp::Record, A), (SealOp::Record, B)]).unwrap();
        append(&[(SealOp::Remove, A), (SealOp::Remove, B), (SealOp::Remove, C)]).unwrap();
        let before = append(&[(SealOp::Record, A)]).unwrap();

        let set = compact().unwrap();
        assert_eq!(set, before);
        assert!(set.contains(A) && !set.contains(B) && !set.contains(C));
        assert_eq!(std::fs::read(sealed_path()).unwrap(), record(&[(SealOp::Record, A)]));
        assert_eq!(read_sealed().unwrap(), before);

        // Compacting a compact record, or no record, writes nothing: no rename, so the same inode.
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let inode = || std::fs::metadata(sealed_path()).unwrap().ino();
            let compacted = inode();
            compact().unwrap();
            assert_eq!(inode(), compacted);
        }
        std::fs::remove_file(sealed_path()).unwrap();
        assert!(compact().unwrap().is_empty());
        assert!(!sealed_path().exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn compaction_drops_a_torn_tail() {
        let (root, _scope) = scratch("compact-torn");
        append(&[(SealOp::Record, B), (SealOp::Record, A)]).unwrap();

        let mut bytes = std::fs::read(sealed_path()).unwrap();
        bytes.extend(&encode_frame(SealOp::Remove, A).unwrap()[..30]);
        std::fs::write(sealed_path(), &bytes).unwrap();

        compact().unwrap();
        let mut expected = header_bytes();
        expected.extend(encode_frames(&[(SealOp::Record, A), (SealOp::Record, B)]).unwrap().concat());
        assert_eq!(std::fs::read(sealed_path()).unwrap(), expected, "one frame, sorted");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn encode_refuses_what_is_not_a_hash() {
        assert!(encode_frame(SealOp::Record, "abc").is_err());
        assert!(encode_frame(SealOp::Record, &"zz".repeat(32)).is_err());
    }

    /// The shared store-access hold, which nothing else contends for in a scratch store.
    fn hold() -> StoreAccessLock {
        StoreAccessLock::shared(|| panic!("nothing holds the store exclusively")).unwrap()
    }

    /// Unsealing a present tree records every absent object its present closure reaches before its
    /// own record is removed: an absent subtree, blob and recipe, an absent chunk of a present
    /// recipe, and an absent blob of a present subtree. Nothing present is recorded, nothing
    /// beneath an absent object is looked for, and an absence already recorded is not recorded
    /// again.
    ///
    /// Mutations, each red on the `pushed_down` assertion: skip any one absence arm of the walk
    /// (subtree, file, chunk), skip descending a present subtree or recipe, or record what is
    /// present; and drop the removal → red on the record assertion.
    #[test]
    fn unsealing_a_present_tree_records_every_absence_beneath_it() {
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::util::gc_utils::tests::{store_chunk, store_recipe, store_tree};

        let (root, _scope) = scratch("unseal-tree");
        let lost = |digit: char| digit.to_string().repeat(64);
        let (lost_tree, lost_blob, lost_recipe, lost_chunk, lost_deep, recorded) =
            (lost('1'), lost('2'), lost('3'), lost('4'), lost('5'), lost('6'));

        let present_chunk = store_chunk(b"a chunk that is present");
        let recipe = store_recipe(&[(present_chunk.clone(), 23), (lost_chunk.clone(), 23)]);
        let inner = store_tree(&[("deep.txt", &lost_deep, DirEntryType::Normal)]);
        let tree = store_tree(&[
            ("lost", &lost_tree, DirEntryType::Tree),
            ("lost.txt", &lost_blob, DirEntryType::Normal),
            ("lost.bin", &lost_recipe, DirEntryType::NormalChunked),
            ("big.bin", &recipe, DirEntryType::NormalChunked),
            ("inner", &inner, DirEntryType::Tree),
            ("recorded.txt", &recorded, DirEntryType::Normal),
            ("here.txt", &present_chunk, DirEntryType::Normal),
        ]);
        append(&[(SealOp::Record, &tree), (SealOp::Record, &recorded)]).unwrap();

        let present = present_recorded(&read_sealed().unwrap()).unwrap();
        assert_eq!(present, vec![tree.clone()]);

        let unsealed = unseal(&hold(), &present).unwrap();
        assert_eq!(unsealed.removed, vec![tree.clone()]);
        assert_eq!(unsealed.pushed_down, vec![lost_tree, lost_blob, lost_recipe, lost_chunk, lost_deep]);
        assert!(unsealed.kept.is_empty(), "{:?}", unsealed.kept);

        let record = read_sealed().unwrap();
        assert!(!record.contains(&tree), "the present tree is no longer recorded");
        assert!(record.contains(&recorded));
        for hash in &unsealed.pushed_down {
            assert!(record.contains(hash), "{} is recorded", hash);
        }
        assert!(present_recorded(&record).unwrap().is_empty(), "nothing present is recorded");

        std::fs::remove_dir_all(&root).ok();
    }

    /// The push-down's records land before the removal: a crash after the first frame of a longer
    /// append leaves the tree recorded, not its absences exposed. The tree names one absence more
    /// than a frame holds, so the append is two frames. Mutation: append the removal before the
    /// records → the first frame holds it → red.
    #[test]
    fn the_removal_lands_after_the_push_down() {
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::util::gc_utils::tests::store_tree;

        let (root, _scope) = scratch("unseal-order");
        let names: Vec<(String, String)> = (0..=MAX_FRAME_ENTRIES)
            .map(|n| (format!("f{}", n), format!("{:064x}", n + 1)))
            .collect();
        let entries: Vec<(&str, &str, DirEntryType)> = names.iter()
            .map(|(name, hash)| (name.as_str(), hash.as_str(), DirEntryType::Normal))
            .collect();
        let tree = store_tree(&entries);
        append(&[(SealOp::Record, &tree)]).unwrap();
        let before = std::fs::read(sealed_path()).unwrap().len();

        let unsealed = unseal(&hold(), &[tree.clone()]).unwrap();
        assert_eq!(unsealed.removed, vec![tree.clone()]);
        assert_eq!(unsealed.pushed_down.len(), MAX_FRAME_ENTRIES + 1);

        // The record as a crash after the first new frame leaves it.
        let bytes = std::fs::read(sealed_path()).unwrap();
        let first_frame = parse(&bytes[..before + MAX_FRAME_LEN]).unwrap();
        assert_eq!(first_frame.valid_len, before + MAX_FRAME_LEN);
        assert!(first_frame.set.contains(&tree), "the first frame removes nothing");
        assert!(!parse(&bytes).unwrap().set.contains(&tree));

        std::fs::remove_dir_all(&root).ok();
    }

    /// An object whose closure cannot be walked stays recorded, and so does one not in the store;
    /// with nothing walked, nothing is written. One that is present but no longer recorded is not
    /// removed. Mutations: drop the not-in-the-store check → the absent object is removed → red;
    /// remove what the re-read no longer records → `removed` names it → red.
    #[test]
    fn what_cannot_be_walked_or_is_not_recorded_is_not_removed() {
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::util::gc_utils::tests::{store_chunk, store_tree};

        let (root, _scope) = scratch("unseal-kept");
        let chunk = store_chunk(b"not a tree");
        let broken = store_tree(&[("sub", &chunk, DirEntryType::Tree)]);
        let absent = "7".repeat(64);
        append(&[(SealOp::Record, &broken), (SealOp::Record, &absent)]).unwrap();
        let before = std::fs::read(sealed_path()).unwrap();

        let unsealed = unseal(&hold(), &[broken.clone(), absent.clone()]).unwrap();
        assert!(unsealed.removed.is_empty() && unsealed.pushed_down.is_empty(), "{:?}", unsealed);
        let kept: Vec<&str> = unsealed.kept.iter().map(|(hash, _)| hash.as_str()).collect();
        assert_eq!(kept, vec![broken.as_str(), absent.as_str()]);
        assert!(unsealed.kept[0].1.contains("not a tree"), "{}", unsealed.kept[0].1);
        assert_eq!(unsealed.kept[1].1, "it is not in the store");
        assert_eq!(std::fs::read(sealed_path()).unwrap(), before, "nothing was written");

        // A parcel names a history the walk does not follow. Mutation: treat it as a leaf → its
        // record is removed → red.
        let mut object = crate::builder::object::loose_object_builder::LooseObjectBuilder::build_parcel(
            &crate::model::parcel::Parcel { tree_hash: absent.clone(), parents: Vec::new(), actions: Vec::new(), description: None },
        );
        let parcel = object.hash.clone();
        object.store().unwrap();
        append(&[(SealOp::Record, &parcel)]).unwrap();
        let unsealed = unseal(&hold(), &[parcel.clone()]).unwrap();
        assert!(unsealed.removed.is_empty() && unsealed.pushed_down.is_empty(), "{:?}", unsealed);
        assert!(unsealed.kept[0].1.contains("parcel"), "{:?}", unsealed);
        assert!(read_sealed().unwrap().contains(&parcel));

        // Present, walkable, and not recorded.
        let unrecorded = store_tree(&[]);
        let unsealed = unseal(&hold(), &[unrecorded]).unwrap();
        assert_eq!(unsealed, Unsealed::default());

        std::fs::remove_dir_all(&root).ok();
    }

    /// A walk that fails counts nothing it went through as checked: a later object reaching the
    /// same subtree walks it again. The first tree reaches `shared` and then fails at `z-broken`
    /// before descending `shared`'s present child; the second reaches `shared` too. Mutation: mark
    /// a failed walk's trees closed → the second walk skips `shared` → the absence beneath it is
    /// not recorded → red.
    #[test]
    fn a_failed_walk_closes_nothing_for_the_next() {
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::util::gc_utils::tests::{store_chunk, store_tree};

        let (root, _scope) = scratch("unseal-closed");
        let lost = "8".repeat(64);
        let deep = store_tree(&[("lost.txt", &lost, DirEntryType::Normal)]);
        let shared = store_tree(&[("deep", &deep, DirEntryType::Tree)]);
        let chunk = store_chunk(b"not a tree either");
        let first = store_tree(&[("shared", &shared, DirEntryType::Tree), ("z-broken", &chunk, DirEntryType::Tree)]);
        let second = store_tree(&[("shared", &shared, DirEntryType::Tree), ("other", &store_tree(&[]), DirEntryType::Tree)]);
        append(&[(SealOp::Record, &first), (SealOp::Record, &second)]).unwrap();

        let unsealed = unseal(&hold(), &[first.clone(), second.clone()]).unwrap();
        assert_eq!(unsealed.removed, vec![second]);
        assert_eq!(unsealed.pushed_down, vec![lost]);
        assert_eq!(unsealed.kept.len(), 1);

        std::fs::remove_dir_all(&root).ok();
    }
}
