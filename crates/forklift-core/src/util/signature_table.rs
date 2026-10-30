//! The signature table format (FORK-333, `docs/format/PARCEL_SIGNATURE_FORMAT.md`): many parcel
//! signatures packed into one immutable file, `objects/pack/<id>.sigs`, so a signed store does not
//! hold one small file per parcel. This module is the only code that knows where anything sits in
//! a table; the pack registry (`pack_utils`) loads tables beside the packs and answers lookups
//! through [`SignatureTable::lookup`].
//!
//! The layout, all integers little-endian:
//!
//! * A 24-byte header: the magic `FORKSIGT`, `u32` version, `u32` entry count, `u32` length of the
//!   entries region, and a `u32` crc32 over header bytes 0..20 followed by the whole fanout table.
//! * The fanout table: 256 entries of 8 bytes, one per first hash byte. Bucket `b` holds the
//!   records whose parcel hash starts with byte `b`; its entry is the `u32` cumulative end of the
//!   bucket and a `u32` crc32 over the bucket's record bytes.
//! * The records, sorted by parcel hash: 32-byte hash, `u32` offset into the entries region,
//!   `u32` length, `u32` crc32 of the entry bytes.
//! * The entries region: each entry is the verbatim V1 signature sidecar, so a read returns the
//!   same bytes from a table as from a loose `.sig` file.
//!
//! Opening a table checks the header, the fanout table and the exact file length. A bucket's
//! records are checked the first time a lookup reads them (crc, strictly ascending hashes that all
//! carry the bucket's byte, every entry inside the entries region), and an entry's crc on every
//! read. A table that fails any check is never read as "no signature": the lookup reports it.

use std::ops::Range;
use std::path::Path;
use std::sync::atomic::{AtomicU8, Ordering};

/// The extension of a signature table in the pack folder.
pub(crate) const SIGNATURE_TABLE_EXTENSION: &str = "sigs";

/// The most entries a table holds: the pack bound, so no table grows without bound.
pub(crate) const MAX_TABLE_ENTRIES: usize = 100_000;

const MAGIC: &[u8; 8] = b"FORKSIGT";
const VERSION: u32 = 1;

/// Versions from 1 up to this are ones a build could plausibly have written; anything else in the
/// version field is rot, not another build.
const PLAUSIBLE_VERSION_LIMIT: u32 = 256;

const HASH_LEN: usize = 32;
const HEADER_LEN: usize = 24;
const HEADER_CRC_OFFSET: usize = 20;
const BUCKETS: usize = 256;
const FANOUT_ENTRY_LEN: usize = 8;
const RECORD_LEN: usize = HASH_LEN + 12;
const RECORDS_START: usize = HEADER_LEN + BUCKETS * FANOUT_ENTRY_LEN;

const UNCHECKED: u8 = 0;
const VALID: u8 = 1;
const CORRUPT: u8 = 2;

fn read_u32_le(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("a 4-byte slice"))
}

/// Why a table could not be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TableError {
    /// The table carries this format's magic but another plausible version: written by another
    /// build, so it is not moved aside; a build that reads it is the remedy.
    ForeignVersion(u32),
    /// The table is truncated or fails a check: its signatures are lost unless restored.
    Corrupt(String),
}

/// What a lookup in one table found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Lookup<'a> {
    Found(&'a [u8]),
    Missing,
    /// The bucket or entry the hash falls in failed its check: the table cannot rule it out.
    Corrupt(String),
}

/// The id of the table holding `entries` (sorted by hash, no duplicates): the Blake3 of the
/// sorted (hash, entry) pairs, each entry length-prefixed. Two writers publishing the same set
/// collide onto one name with identical bytes.
pub(crate) fn table_id(entries: &[([u8; HASH_LEN], Vec<u8>)]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (hash, entry) in entries {
        hasher.update(hash);
        hasher.update(&(entry.len() as u32).to_le_bytes());
        hasher.update(entry);
    }
    hasher.finalize().to_hex().to_string()
}

/// Build a table's bytes from `entries`, sorted by hash with no duplicate hash and at most
/// [`MAX_TABLE_ENTRIES`] of them.
pub(crate) fn build(entries: &[([u8; HASH_LEN], Vec<u8>)]) -> Vec<u8> {
    assert!(entries.len() <= MAX_TABLE_ENTRIES, "a signature table holds at most {} entries", MAX_TABLE_ENTRIES);
    assert!(entries.windows(2).all(|pair| pair[0].0 < pair[1].0), "a signature table's entries are sorted and unique");

    let mut records = Vec::with_capacity(entries.len() * RECORD_LEN);
    let mut region = Vec::new();
    let mut ends = [0u32; BUCKETS];
    let mut crcs: [crc32fast::Hasher; BUCKETS] = std::array::from_fn(|_| crc32fast::Hasher::new());

    for (hash, entry) in entries {
        let mut record = Vec::with_capacity(RECORD_LEN);
        record.extend_from_slice(hash);
        record.extend_from_slice(&(region.len() as u32).to_le_bytes());
        record.extend_from_slice(&(entry.len() as u32).to_le_bytes());
        record.extend_from_slice(&crc32fast::hash(entry).to_le_bytes());

        crcs[hash[0] as usize].update(&record);
        ends[hash[0] as usize] += 1;
        records.extend_from_slice(&record);
        region.extend_from_slice(entry);
    }

    let mut fanout = Vec::with_capacity(BUCKETS * FANOUT_ENTRY_LEN);
    let mut cumulative = 0u32;
    for (count, crc) in ends.iter().zip(crcs) {
        cumulative += count;
        fanout.extend_from_slice(&cumulative.to_le_bytes());
        fanout.extend_from_slice(&crc.finalize().to_le_bytes());
    }

    let mut bytes = Vec::with_capacity(RECORDS_START + records.len() + region.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&VERSION.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(region.len() as u32).to_le_bytes());

    let mut header_crc = crc32fast::Hasher::new();
    header_crc.update(&bytes[..HEADER_CRC_OFFSET]);
    header_crc.update(&fanout);
    bytes.extend_from_slice(&header_crc.finalize().to_le_bytes());

    bytes.extend_from_slice(&fanout);
    bytes.extend_from_slice(&records);
    bytes.extend_from_slice(&region);
    bytes
}

/// The entry count in a table's first 16 bytes, if they carry this version's magic: enough for a
/// cheap census that does not read the whole table.
pub(crate) fn header_count(header: &[u8; 16]) -> Option<usize> {
    (&header[0..8] == MAGIC && read_u32_le(header, 8) == VERSION).then(|| read_u32_le(header, 12) as usize)
}

/// An opened table over `bytes` (a mapping, or a buffer in tests).
pub(crate) struct SignatureTable<B> {
    bytes: B,
    count: usize,
    region_start: usize,
    region_len: usize,
    verdicts: Box<[AtomicU8]>,
}

impl<B: AsRef<[u8]>> SignatureTable<B> {
    /// Open `bytes`, checking the header, the fanout table and the exact length.
    pub(crate) fn open(bytes: B) -> Result<SignatureTable<B>, TableError> {
        let data = bytes.as_ref();
        let corrupt = |detail: &str| Err(TableError::Corrupt(detail.to_string()));

        if data.len() < 12 || &data[0..8] != MAGIC {
            return corrupt("not a signature table");
        }

        let version = read_u32_le(data, 8);
        if version != VERSION {
            // A version field that rot changed still leaves this version's header checksum
            // matching once the field is read as this version: that table is corrupt, not
            // another build's.
            if data.len() >= RECORDS_START {
                let mut header_crc = crc32fast::Hasher::new();
                header_crc.update(&data[..8]);
                header_crc.update(&VERSION.to_le_bytes());
                header_crc.update(&data[12..HEADER_CRC_OFFSET]);
                header_crc.update(&data[HEADER_LEN..RECORDS_START]);
                if header_crc.finalize() == read_u32_le(data, HEADER_CRC_OFFSET) {
                    return corrupt("version field damaged");
                }
            }
            return if (1..PLAUSIBLE_VERSION_LIMIT).contains(&version) {
                Err(TableError::ForeignVersion(version))
            } else {
                corrupt("unreadable version field")
            };
        }

        if data.len() < RECORDS_START {
            return corrupt("truncated");
        }

        let count = read_u32_le(data, 12) as usize;
        let region_len = read_u32_le(data, 16) as usize;

        let mut header_crc = crc32fast::Hasher::new();
        header_crc.update(&data[..HEADER_CRC_OFFSET]);
        header_crc.update(&data[HEADER_LEN..RECORDS_START]);
        if header_crc.finalize() != read_u32_le(data, HEADER_CRC_OFFSET) {
            return corrupt("header checksum mismatch");
        }

        if count > MAX_TABLE_ENTRIES {
            return corrupt("more entries than a table may hold");
        }

        let mut previous = 0usize;
        for bucket in 0..BUCKETS {
            let end = read_u32_le(data, HEADER_LEN + bucket * FANOUT_ENTRY_LEN) as usize;
            if end < previous {
                return corrupt("fanout table out of order");
            }
            previous = end;
        }
        if previous != count {
            return corrupt("fanout table does not match the entry count");
        }

        let region_start = RECORDS_START + count * RECORD_LEN;
        if data.len() != region_start + region_len {
            return corrupt("truncated or overlong");
        }

        let verdicts = (0..BUCKETS).map(|_| AtomicU8::new(UNCHECKED)).collect();
        Ok(SignatureTable { bytes, count, region_start, region_len, verdicts })
    }

    /// The number of entries.
    pub(crate) fn count(&self) -> usize {
        self.count
    }

    fn bucket_span(&self, bucket: usize) -> Range<usize> {
        let data = self.bytes.as_ref();
        let end_of = |b: usize| read_u32_le(data, HEADER_LEN + b * FANOUT_ENTRY_LEN) as usize;
        let start = if bucket == 0 { 0 } else { end_of(bucket - 1) };
        start..end_of(bucket)
    }

    fn record_at(&self, position: usize) -> (&[u8], usize, usize, u32) {
        let data = self.bytes.as_ref();
        let start = RECORDS_START + position * RECORD_LEN;
        (
            &data[start..start + HASH_LEN],
            read_u32_le(data, start + HASH_LEN) as usize,
            read_u32_le(data, start + HASH_LEN + 4) as usize,
            read_u32_le(data, start + HASH_LEN + 8),
        )
    }

    fn bucket_is_valid(&self, bucket: usize) -> bool {
        match self.verdicts[bucket].load(Ordering::Relaxed) {
            VALID => return true,
            CORRUPT => return false,
            _ => {}
        }

        let valid = self.check_bucket(bucket);
        self.verdicts[bucket].store(if valid { VALID } else { CORRUPT }, Ordering::Relaxed);
        valid
    }

    fn check_bucket(&self, bucket: usize) -> bool {
        let span = self.bucket_span(bucket);
        let data = self.bytes.as_ref();
        let start = RECORDS_START + span.start * RECORD_LEN;
        let end = RECORDS_START + span.end * RECORD_LEN;

        if crc32fast::hash(&data[start..end]) != read_u32_le(data, HEADER_LEN + bucket * FANOUT_ENTRY_LEN + 4) {
            return false;
        }

        let mut previous: Option<&[u8]> = None;
        for position in span {
            let (hash, offset, length, _) = self.record_at(position);
            if previous.is_some_and(|previous| previous >= hash) || hash[0] as usize != bucket {
                return false;
            }
            previous = Some(hash);

            if length == 0 || offset.checked_add(length).is_none_or(|end| end > self.region_len) {
                return false;
            }
        }

        true
    }

    /// The entry at record `position` of a valid bucket, checked against its crc.
    fn entry_at(&self, position: usize) -> Option<&[u8]> {
        let (_, offset, length, crc) = self.record_at(position);
        let start = self.region_start + offset;
        let entry = &self.bytes.as_ref()[start..start + length];
        (crc32fast::hash(entry) == crc).then_some(entry)
    }

    /// Find `hash`'s signature, validating only its bucket and its entry.
    pub(crate) fn lookup(&self, hash: &[u8; HASH_LEN]) -> Lookup<'_> {
        let bucket = hash[0] as usize;
        if !self.bucket_is_valid(bucket) {
            return Lookup::Corrupt(format!("bucket {} fails its check", bucket));
        }

        let span = self.bucket_span(bucket);
        let (mut low, mut high) = (span.start, span.end);
        while low < high {
            let mid = low + (high - low) / 2;
            match self.record_at(mid).0.cmp(hash.as_slice()) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => return match self.entry_at(mid) {
                    Some(entry) => Lookup::Found(entry),
                    None => Lookup::Corrupt(format!("the entry of parcel {} fails its check", crate::util::sign_utils::to_hex(hash))),
                },
            }
        }

        Lookup::Missing
    }

    /// Every entry, validating the whole table first: `Err` names the first check that fails.
    pub(crate) fn all_entries(&self) -> Result<Vec<([u8; HASH_LEN], &[u8])>, String> {
        let mut entries = Vec::with_capacity(self.count);
        for bucket in 0..BUCKETS {
            if !self.bucket_is_valid(bucket) {
                return Err(format!("bucket {} fails its check", bucket));
            }
            for position in self.bucket_span(bucket) {
                let hash: [u8; HASH_LEN] = self.record_at(position).0.try_into().expect("a 32-byte hash");
                let entry = self.entry_at(position).ok_or_else(|| format!(
                    "the entry of parcel {} fails its check", crate::util::sign_utils::to_hex(&hash)
                ))?;
                entries.push((hash, entry));
            }
        }
        Ok(entries)
    }
}

/// The error a lookup reports for an unreadable table at `path`, with the remedy its cause calls
/// for: a table of another version is kept for a build that reads it; a corrupt one is moved
/// aside, at the stated cost.
pub(crate) fn unreadable_message(path: &Path, error: &TableError) -> String {
    match error {
        TableError::ForeignVersion(version) => format!(
            "Signature table \"{}\" uses format version {}, which this build cannot read. Use a \
            build that reads it; do not move it aside, since the signatures it holds have no other \
            local copy.",
            path.to_string_lossy(), version
        ),
        TableError::Corrupt(detail) => format!(
            "Signature table \"{}\" is corrupt ({}). Move it out of the pack folder to continue: the \
            signatures it holds are then lost locally, and those parcels read as unsigned until the \
            warehouse is cloned again.",
            path.to_string_lossy(), detail
        ),
    }
}

/// The message for a table that could not be read at all: the remedy is access, not a move.
pub(crate) fn io_message(path: &Path, error: &dyn std::fmt::Display) -> String {
    format!(
        "Signature table \"{}\" could not be read: {}. Restore access to it; the signatures it \
        holds have no other local copy.",
        path.to_string_lossy(), error
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(first: u8, last: u8) -> [u8; HASH_LEN] {
        let mut hash = [0u8; HASH_LEN];
        hash[0] = first;
        hash[HASH_LEN - 1] = last;
        hash
    }

    fn sample() -> Vec<([u8; HASH_LEN], Vec<u8>)> {
        vec![
            (hash(0x00, 1), b"first".to_vec()),
            (hash(0x17, 1), b"second".to_vec()),
            (hash(0x17, 2), b"third".to_vec()),
            (hash(0xff, 9), b"fourth".to_vec()),
        ]
    }

    #[test]
    fn a_built_table_answers_every_entry_and_misses_the_rest() {
        let bytes = build(&sample());
        let table = SignatureTable::open(bytes.as_slice()).unwrap();

        assert_eq!(table.count(), 4);
        for (hash, entry) in sample() {
            assert_eq!(table.lookup(&hash), Lookup::Found(entry.as_slice()));
        }
        assert_eq!(table.lookup(&hash(0x17, 3)), Lookup::Missing);
        assert_eq!(table.lookup(&hash(0x42, 0)), Lookup::Missing);
        assert_eq!(table.all_entries().unwrap().len(), 4);
    }

    #[test]
    fn a_flipped_entry_byte_reads_as_corrupt_never_missing() {
        let mut bytes = build(&sample());
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let table = SignatureTable::open(bytes.as_slice()).unwrap();

        assert!(matches!(table.lookup(&hash(0xff, 9)), Lookup::Corrupt(_)));
        assert_eq!(table.lookup(&hash(0x00, 1)), Lookup::Found(b"first".as_slice()));
        assert!(table.all_entries().is_err());
    }

    #[test]
    fn a_flipped_record_byte_fails_its_bucket() {
        let mut bytes = build(&sample());
        // The second record's offset field, in bucket 0x17.
        bytes[RECORDS_START + RECORD_LEN + HASH_LEN] ^= 1;
        let table = SignatureTable::open(bytes.as_slice()).unwrap();

        assert!(matches!(table.lookup(&hash(0x17, 3)), Lookup::Corrupt(_)));
        assert_eq!(table.lookup(&hash(0x00, 1)), Lookup::Found(b"first".as_slice()));
    }

    #[test]
    fn open_tells_a_foreign_version_from_corruption() {
        // Another build's table carries its own header checksum; rot changes the field alone.
        let mut bytes = build(&sample());
        bytes[8] = 2;
        assert!(matches!(SignatureTable::open(bytes.as_slice()).err(), Some(TableError::Corrupt(_))));
        let mut crc = crc32fast::Hasher::new();
        crc.update(&bytes[..HEADER_CRC_OFFSET]);
        crc.update(&bytes[HEADER_LEN..RECORDS_START]);
        let crc = crc.finalize();
        bytes[HEADER_CRC_OFFSET..HEADER_LEN].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(SignatureTable::open(bytes.as_slice()).err(), Some(TableError::ForeignVersion(2)));

        let bytes = build(&sample());
        assert!(matches!(SignatureTable::open(&bytes[..bytes.len() - 1]).err(), Some(TableError::Corrupt(_))));

        let mut bytes = build(&sample());
        bytes[HEADER_LEN] ^= 1;
        assert!(matches!(SignatureTable::open(bytes.as_slice()).err(), Some(TableError::Corrupt(_))));
    }

    #[test]
    fn the_id_follows_the_content() {
        let mut other = sample();
        other[0].1 = b"changed".to_vec();
        assert_eq!(table_id(&sample()), table_id(&sample()));
        assert_ne!(table_id(&sample()), table_id(&other));
    }
}
