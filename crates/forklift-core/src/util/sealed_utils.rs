//! The sealed-by-hash record: the objects a warehouse holds by hash alone, because a parent that
//! is present names them and the remote holds them. A walk that meets one of these absent
//! tolerates the absence and does not descend, as it does for content outside the fetch scope;
//! everything under an absent recorded tree or recipe is tolerated by that same non-descent. A
//! recorded object that is present seals nothing: it is walked like any other.
//!
//! The record is an append-only log at `<forklift root>/sealed`, shared by every bay. It is
//! replayed in order: a record entry adds a hash, a removal entry drops it.
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
//! [`append_locked`]), so a torn append is the last frame and only that: nothing after it
//! validates, and at most the largest frame's worth of bytes, zeros included, follow the last
//! valid frame. A valid frame anywhere past the last one read, or more bytes than that, is damage
//! inside the log, and
//! fails the read rather than dropping the frames behind it. Damage that leaves no valid frame
//! after it within that many bytes of the end cannot be told from a torn append, so it reads as
//! one: the entries there are not replayed. That is the one loss the log can hide, and it hides
//! only entries: a lost record reads as a missing object, which every walk reports, and a lost
//! removal leaves a present object recorded, which seals nothing.
//! A frame that validates but whose entries do not parse fails the read, as does a header naming
//! another version: neither is guessed at.
//!
//! ## Writing
//!
//! Every write takes the ref lock ([`RefLock`]) and re-reads the record under it, so writes are
//! ordered and each one sees the last. A record that does not parse is never written over. One
//! whose valid prefix is followed by a torn append is cut back to that prefix, then appended to.
//! The first write creates the record atomically, so its header is never torn. Compaction
//! ([`compact`]) rewrites the log as the set it replays to, which drops no recorded hash. The
//! atomic writes are never staged by a bulk store session (they go through
//! [`file_utils::replace_file_atomically`]): a rename that waited for the session to finish would
//! land over the appends made in between.

use std::collections::HashSet;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use crate::globals;
use crate::util::{file_utils, record_utils, sign_utils};
use crate::util::lock_utils::RefLock;

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
/// A writer that finds bytes past `valid_len` rewrites the file to that prefix before appending.
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
                .ok_or_else(|| format!("\"{}\" is not an object hash.", hash))?;
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

/// Parse a record's bytes (see the module doc comment for the format).
///
/// # Returns
/// * `Ok(ParsedSealed)` - The replayed set and the length of the valid prefix.
/// * `Err(String)`      - If the header is missing or names another version, a valid frame
///                        holds entries that do not parse, or more than one frame's worth of
///                        bytes follows the last valid frame. Phrased to follow "The record ".
pub fn parse(bytes: &[u8]) -> Result<ParsedSealed, String> {
    let header = header_bytes();

    if !bytes.starts_with(&header) {
        let tag = format!("/forklift-{} ", SEALED_RECORD);
        let first = bytes.split(|&b| b == b'\n').next().unwrap_or_default();
        return Err(if first.starts_with(tag.as_bytes()) {
            format!(
                "is in a format this version of Forklift does not know (\"{}\").",
                String::from_utf8_lossy(first)
            )
        } else {
            "does not open with its header, so it is not a sealed-object record.".to_string()
        });
    }

    let mut set = SealedSet::default();
    let mut offset = header.len();

    while let Some((entries, frame_len)) = read_frame(&bytes[offset..]) {
        let unreadable = |at: usize| format!(
            "holds an entry at byte {} that this version of Forklift cannot read.",
            at
        );

        if !is_entries_len(entries.len()) {
            return Err(unreadable(offset));
        }

        for (index, entry) in entries.chunks(ENTRY_LEN).enumerate() {
            let op = SealOp::from_byte(entry[0]).ok_or_else(|| unreadable(offset + 8 + index * ENTRY_LEN))?;
            set.apply(op, sign_utils::to_hex(&entry[1..]));
        }

        offset += frame_len;
    }

    // A torn append leaves at most one frame's bytes, then perhaps zeros the filesystem extended
    // the file with. Anything longer is not a torn tail: the frames behind it would be lost.
    if !is_torn_tail(&bytes[offset..]) {
        return Err(format!(
            "is damaged at byte {}: what follows it is not what an interrupted write leaves.",
            offset
        ));
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
        Ok(bytes) => parse(&bytes)
            .map(|parsed| Some((bytes, parsed)))
            .map_err(|reason| format!("The sealed-object record \"{}\" {}", path.to_string_lossy(), reason)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!(
            "Error while reading the sealed-object record \"{}\": {}",
            path.to_string_lossy(),
            e
        )),
    }
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
/// frames, the record is cut back to them and synced. Then the frames are appended, each synced
/// before the next is written, so a torn append can only ever be the last frame. Every way, `ops` are on disk when this returns —
/// unless fsync is turned off, when a crash may leave the record damaged.
///
/// # Arguments
/// * `_lock` - The held ref lock, which orders this write against every other.
/// * `ops`   - What to record and remove, in order. None writes nothing.
///
/// # Returns
/// * `Ok(SealedSet)` - The set the record now replays to.
/// * `Err(String)`   - If a hash is not an object hash, the record cannot be read or parsed, or
///                     the write fails. Nothing is written in the first two cases. A failed write may still have landed some of its frames
///                     whole, and may leave a torn one, which the next write drops: read the
///                     record back to learn which.
pub fn append_locked(_lock: &RefLock, ops: &[(SealOp, &str)]) -> Result<SealedSet, String> {
    let frames = encode_frames(ops)?;
    let path = sealed_path();
    let existing = read_record(&path)?;

    // Only the lengths are kept: the bytes are not needed past the parse.
    let (lengths, mut set) = match existing {
        Some((bytes, parsed)) => (Some((bytes.len(), parsed.valid_len)), parsed.set),
        None => (None, SealedSet::default()),
    };

    for (op, hash) in ops {
        set.apply(*op, hash.to_string());
    }

    if frames.is_empty() {
        return Ok(set);
    }

    match lengths {
        Some((len, valid_len)) => append_frames(&path, len, valid_len, &frames)?,
        None => {
            let mut created = header_bytes();
            created.extend(frames.concat());
            file_utils::replace_file_atomically(&path, &created)?;
        }
    }

    Ok(set)
}

/// Append `frames` to the record at `path`, `len` bytes long, after cutting it back to its first
/// `valid_len` bytes when a torn append follows them, syncing the cut and each frame before the
/// next write.
fn append_frames(path: &Path, len: usize, valid_len: usize, frames: &[Vec<u8>]) -> Result<(), String> {
    let failed = |e: std::io::Error| format!(
        "Error while appending to the sealed-object record \"{}\": {}",
        path.to_string_lossy(),
        e
    );

    // A write handle that is not an append-only one: Windows flushes only a handle opened for
    // writing. The ref lock keeps every other writer out, so the length read stays the length.
    let mut file = std::fs::OpenOptions::new().write(true).open(path).map_err(failed)?;

    if valid_len < len {
        file.set_len(valid_len as u64).map_err(failed)?;
        if file_utils::fsync_enabled() {
            file.sync_all().map_err(failed)?;
        }
    }
    file.seek(std::io::SeekFrom::Start(valid_len as u64)).map_err(failed)?;

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
/// * `Err(String)`   - If the record cannot be read or parsed, or the rewrite fails. The record is
///                     left as it was.
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
        assert!(parse(&short_hash).unwrap_err().contains("cannot read"));
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
        assert!(parse(&ragged).unwrap_err().contains("cannot read"));

        let mut oversized = record(&[(SealOp::Record, A)]);
        let entries: Vec<u8> = (0..MAX_FRAME_ENTRIES + 1).flat_map(|_| {
            let mut entry = vec![1u8];
            entry.extend_from_slice(&[0xaa; HASH_LEN]);
            entry
        }).collect();
        oversized.extend(frame_with_entry(&entries));
        assert!(parse(&oversized).unwrap_err().contains("cannot read"));
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

        let set = append(&[(SealOp::Record, B), (SealOp::Remove, A)]).unwrap();
        assert!(!set.contains(A) && set.contains(B));
        assert_eq!(read_sealed().unwrap(), set);

        let mut expected = record(&[(SealOp::Record, A)]);
        expected.extend(encode_frames(&[(SealOp::Record, B), (SealOp::Remove, A)]).unwrap().concat());
        assert_eq!(std::fs::read(sealed_path()).unwrap(), expected, "appended in place, one frame");

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
}
