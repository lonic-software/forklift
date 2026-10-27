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
//! /forklift-sealed 1\n
//! frame*
//! ```
//!
//! Each frame is `magic | length | entry | crc`: the magic is [`FRAME_MAGIC`], the length counts
//! the entry's bytes and is at least 1, and the CRC-32 covers the magic, the length and the entry,
//! all little-endian. An entry is one operation byte ([`SealOp`]) followed by the object's raw
//! 32-byte hash.
//!
//! A reader takes no lock. It stops at the first frame that does not validate, which is how a torn
//! append reads: everything after the last valid frame is ignored. A region of zeros never
//! validates, because the magic is nonzero. A frame that validates but whose entry does not parse
//! fails the read, as does a header naming another version: neither is guessed at.

use std::collections::HashSet;
use std::path::PathBuf;
use crate::globals;
use crate::util::{record_utils, sign_utils};

/// The record's file name under the shared forklift root.
const FILE_NAME_SEALED: &str = "sealed";

/// The [`record_utils`] kind and version of the record's header line.
const SEALED_RECORD: &str = "sealed";
const SEALED_VERSION: u32 = 1;

/// Opens every frame. Nonzero, so a zero-filled region left by a crash never reads as a frame.
pub const FRAME_MAGIC: u32 = 0x5345_414C;

/// The bytes of a frame around its entry: magic and length before it, the CRC after.
const FRAME_OVERHEAD: usize = 12;

/// The length of an object hash, in bytes.
const HASH_LEN: usize = 32;

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
/// * `Err(String)` - If `hash` is not a 32-byte hex hash.
pub fn encode_frame(op: SealOp, hash: &str) -> Result<Vec<u8>, String> {
    let raw = sign_utils::from_hex(hash)
        .ok()
        .filter(|raw| raw.len() == HASH_LEN)
        .ok_or_else(|| format!("\"{}\" is not an object hash.", hash))?;

    let mut entry = Vec::with_capacity(1 + HASH_LEN);
    entry.push(op.to_byte());
    entry.extend_from_slice(&raw);

    let mut frame = Vec::with_capacity(FRAME_OVERHEAD + entry.len());
    frame.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
    frame.extend_from_slice(&(entry.len() as u32).to_le_bytes());
    frame.extend_from_slice(&entry);
    let crc = crc32fast::hash(&frame);
    frame.extend_from_slice(&crc.to_le_bytes());

    Ok(frame)
}

/// The entry of the frame at the start of `bytes` and the frame's whole length, or `None` when no
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

/// Parse a record's bytes (see the module doc comment for the format).
///
/// # Returns
/// * `Ok(ParsedSealed)` - The replayed set and the length of the valid prefix.
/// * `Err(String)`      - If the header is missing or names another version, or a valid frame
///                        holds an entry that does not parse. Phrased to follow "The record ".
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

    let mut hashes = HashSet::new();
    let mut offset = header.len();

    while let Some((entry, frame_len)) = read_frame(&bytes[offset..]) {
        let op = entry.first().copied().and_then(SealOp::from_byte);
        let raw = &entry[1..];

        match op {
            Some(op) if raw.len() == HASH_LEN => {
                let hash = sign_utils::to_hex(raw);
                match op {
                    SealOp::Record => hashes.insert(hash),
                    SealOp::Remove => hashes.remove(&hash),
                };
            }
            _ => {
                return Err(format!(
                    "holds an entry at byte {} that this version of Forklift cannot read.",
                    offset
                ))
            }
        }

        offset += frame_len;
    }

    Ok(ParsedSealed { set: SealedSet { hashes }, valid_len: offset })
}

/// Read the record. A warehouse without one has sealed nothing.
///
/// # Returns
/// * `Ok(SealedSet)` - The recorded hashes (empty when there is no record).
/// * `Err(String)`   - If the record cannot be read or parsed.
pub fn read_sealed() -> Result<SealedSet, String> {
    let path = sealed_path();

    match std::fs::read(&path) {
        Ok(bytes) => parse(&bytes)
            .map(|parsed| parsed.set)
            .map_err(|reason| format!("The sealed-object record \"{}\" {}", path.to_string_lossy(), reason)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(SealedSet::default()),
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn a_frame_with_a_bad_crc_ends_the_record() {
        let mut bytes = record(&[(SealOp::Record, A), (SealOp::Record, B)]);
        let last = bytes.len() - 1;
        bytes[last] ^= 0xff;

        let parsed = parse(&bytes).unwrap();
        assert!(parsed.set.contains(A));
        assert!(!parsed.set.contains(B));
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
        let mut other = b"/forklift-sealed 2\n".to_vec();
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
    fn encode_refuses_what_is_not_a_hash() {
        assert!(encode_frame(SealOp::Record, "abc").is_err());
        assert!(encode_frame(SealOp::Record, &"zz".repeat(32)).is_err());
    }
}
