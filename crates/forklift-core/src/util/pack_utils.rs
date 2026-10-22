//! Packing the loose object store into bounded packs (DESIGN.html §4.5, object-store
//! scaling phase 1 — see `docs/OBJECT_STORE_SCALING.md`).
//!
//! Every object is normally its own zstd-compressed file (`file_utils::write_object_to_file`).
//! At git.git scale that is ~400k tiny files: each pays filesystem slack, and a whole-history
//! walk does that many random `open`+`read`s. `compact` sweeps the loose set into a handful of
//! **packs** — an append-only data file plus a sorted index — so a read is a binary search in one
//! fanout bucket of a mapped index and one slice at an offset, and the store is a few large files
//! instead of a sea of small ones. An index is validated bucket by bucket as reads touch it, so a
//! command's cost follows the objects it reads, not the size of the store (`index_format`).
//!
//! Two invariants keep this safe and aligned with Forklift's philosophy:
//!
//! * **Packs are plural and bounded.** A pack rolls over at a size *or* object-count threshold,
//!   so no single pack (or its index) grows without bound — the same promise the per-directory
//!   inventory makes for staging. RAM for lookups is O(packed object count), never O(store bytes).
//! * **Durable before destructive.** A loose object is deleted only after the pack that now holds
//!   it is fully written, fsynced and renamed into place *and the pack directory is fsynced* (so
//!   the rename survives power loss, not just a process crash). A crash at any point leaves every
//!   object readable (loose, packed, or — harmlessly — both).
//!
//! A pack record is one of two kinds (phase 2, §9.1 #1): a **full** object (its zstd blob,
//! byte-identical to the loose file) or a **delta** — the object encoded as its difference
//! from a similar *base* already in the store, via the same zstd-dictionary machinery bundles
//! use for transport (`delta_utils`). Deltas collapse the version-to-version redundancy git's
//! packs exploit — a file edited many times costs one full copy plus small deltas, not a full
//! copy per version. A delta is only ever kept when it is smaller than the full blob. Every read
//! out of a pack — a full record or a reconstructed delta alike — is re-hashed and checked
//! against the object's address before it is returned (`resolve_record` →
//! `object_utils::verify_object_bytes`), so a corrupt record or a delta rebuilt against the wrong
//! base can only fail a read, never return wrong bytes silently.

use std::collections::{HashMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime};

use crate::enums::object_type::ObjectType;
use crate::model::object::loose_object::LooseObject;

use crate::util::{
    audit_utils, bundle_utils, byte_utils, delta_utils, fanout_utils, file_utils, graph_utils,
    lock_utils, object_utils, pallet_utils, sign_utils,
};

/// The folder under the object store that holds packs.
const PACK_FOLDER_NAME: &str = "pack";

/// The extension of a pack's data file (concatenated object blobs). `pub(crate)` so the
/// durability-taint recovery walk (`recovery_utils`) can recognize a recorded taint path as a
/// pack file without duplicating the literal.
pub(crate) const PACK_DATA_EXTENSION: &str = "pack";

/// The extension of a pack's index file (sorted hash → offset/length records). `pub(crate)` for
/// the same reason as [`PACK_DATA_EXTENSION`].
pub(crate) const PACK_INDEX_EXTENSION: &str = "idx";

/// Magic + version prefixing a pack data file (so a truncated or foreign file is rejected).
const PACK_DATA_MAGIC: &[u8; 8] = b"FORKPACK";

/// Magic + version prefixing a pack index file.
const PACK_INDEX_MAGIC: &[u8; 8] = b"FORKPIDX";

/// The pack format version, shared by the data file and the index. Version 3 (FORK-118) added the
/// index's fanout table; versions 1 and 2 are refused by name, never read (see
/// [`index_format`]). Bump on any further incompatible layout change.
const PACK_FORMAT_VERSION: u32 = 3;

/// Record kinds: every data record leads with one of these bytes.
/// A full object: the kind byte is followed by the object's zstd blob (as a loose file holds).
const RECORD_FULL: u8 = 0;
/// A delta: the kind byte is followed by `base hash (32) || target length (VLQ) || zstd delta`.
const RECORD_DELTA: u8 = 1;

/// How many recently-written objects a new object may be deltated against. Objects are packed
/// in size order, so the window holds similar-sized neighbours — the pairs most likely to
/// delta well. Bounding it keeps compaction O(objects × window), not O(objects²), and caps
/// the delta attempts per object.
const DELTA_WINDOW: usize = 10;

/// Evict the delta window down to this many resident bytes, so a run of large objects cannot
/// make the window (which holds each candidate base decompressed) grow without bound.
const DELTA_WINDOW_MEMORY: usize = 64 * 1024 * 1024;

/// Objects larger than this are always stored full and never used as (or offered a) delta
/// base — deltating huge blobs costs more RAM/CPU than it saves, and it bounds window memory.
/// Shared with `bundle_utils` and, crucially, enforced on the *read* side by
/// `delta_utils::decompress_delta`, where it is the decompression-bomb bound.
use crate::util::delta_utils::MAX_DELTA_TARGET_BYTES as MAX_DELTA_OBJECT_SIZE;
use crate::util::progress_utils::Progress;

/// The longest delta chain a base may already carry before a new delta refuses to extend it.
/// Reconstructing a delta reads its base (recursively), so this bounds that recursion — the
/// same bound bundles use (`bundle_utils::MAX_DELTA_CHAIN`). Enforced two ways: the window
/// mechanism's own bookkeeping (`WindowEntry::depth`, always exact — a window base is always
/// already written when chosen) and `compact`'s write-time depth ledger (`true_depth`, exact
/// too — see `MAX_RECONSTRUCT_DEPTH`'s doc comment), which is what makes this bound *real* for a
/// path-base delta as well, not just a window one. `compute_path_bases`'s own path-hop counter
/// also checks against this same constant, but only as a coarse, approximate pre-filter on how
/// many candidates the ledger even has to consider — see its doc comment.
const MAX_DELTA_CHAIN: u32 = 50;

/// The length of a pack data file header: magic (8) + version (4).
const PACK_DATA_HEADER_LEN: u64 = 12;

/// An object hash is a Blake3 digest: 32 raw bytes (64 hex characters).
const HASH_LEN: usize = 32;

/// One index record: the 32-byte hash, then the u64 offset and u64 length of the blob in
/// the data file. Records are stored sorted by hash so a lookup is a binary search.
const INDEX_RECORD_LEN: usize = HASH_LEN + 8 + 8;

/// Roll a pack over once its data file reaches this size, so no single pack is unbounded.
const PACK_ROLLOVER_BYTES: u64 = 512 * 1024 * 1024;

/// Roll a pack over once it holds this many objects, so no single index is unbounded (an
/// index's size — and the RAM to hold it — scales with object *count*, not their bytes; a
/// pack full of tiny tree objects would otherwise carry a huge index).
pub(crate) const PACK_ROLLOVER_OBJECTS: usize = 100_000;

/// The largest native-pack data section a transport bundle may declare. A writer rolls after
/// crossing `PACK_ROLLOVER_BYTES`, so the record that crosses it may itself be one maximal object.
/// Keeping the import cap derived from those two writer bounds prevents a hostile bundle from
/// turning one declared section length into an unbounded disk-fill stream.
pub(crate) const MAX_TRANSPORT_PACK_BYTES: u64 =
    PACK_ROLLOVER_BYTES + object_utils::MAX_OBJECT_BYTES as u64 + 2 * 1024 * 1024;

/// The largest native-pack index section a transport bundle may declare: the exact length of an
/// index holding `PACK_ROLLOVER_OBJECTS` records, the most one writer carries before it rolls,
/// computed by the same function the writer and the reader use.
pub(crate) const MAX_TRANSPORT_INDEX_BYTES: u64 = index_format::index_len(PACK_ROLLOVER_OBJECTS);

/// Bound the number of native packs in one bundle independently of its byte lengths. Real bundle
/// builders need one pack per ~512 MiB or 100k objects; 4096 leaves enormous headroom while keeping
/// a hostile count from allocating an unbounded section table.
pub(crate) const MAX_TRANSPORT_PACKS: usize = 4096;

/// The fan-out folder sampled to estimate the loose object count for auto-maintenance (git's
/// `gc --auto` trick: count one folder, multiply by the 256 folders). Any fixed folder works —
/// hashes are uniform.
const AUTO_SAMPLE_FOLDER: &str = "17";

/// Loose-object count above which a background incremental compaction is due (git's default).
const AUTO_LOOSE_THRESHOLD: usize = 6700;

/// Pack count above which a background consolidating repack is due.
const AUTO_PACK_THRESHOLD: usize = 20;

/// The maintenance a warehouse is due for — decided cheaply by [`auto_compaction_action`].
pub enum AutoCompaction {
    /// Nothing to do.
    None,
    /// Enough loose objects have accumulated to pack them (`compact`).
    Incremental,
    /// Enough packs have accumulated to consolidate them (`compact --all`).
    Repack,
}

/// Decide, cheaply, whether background object-store maintenance is due — the recurring
/// counterpart of `import-git`'s one-shot compaction (git's `gc --auto`). It does **not** scan
/// the whole store: it estimates the loose count from one fan-out folder × 256 and counts
/// packs. The caller runs the returned action in the background. Opt out with
/// `maintenance.auto = false`.
///
/// # Returns
/// * `Ok(AutoCompaction)` - What is due (often `None`).
/// * `Err(String)`        - If the store or configuration could not be read.
pub fn auto_compaction_action() -> Result<AutoCompaction, String> {
    use crate::util::config_utils;

    if !maintenance_auto_enabled()? {
        return Ok(AutoCompaction::None);
    }

    // Thresholds are configurable (like git's gc.auto / gc.autoPackLimit) but default sensibly.
    let loose_threshold = config_threshold(config_utils::KEY_MAINTENANCE_LOOSE, AUTO_LOOSE_THRESHOLD)?;
    let pack_threshold = config_threshold(config_utils::KEY_MAINTENANCE_PACKS, AUTO_PACK_THRESHOLD)?;

    // Loose objects have accumulated → pack them.
    if estimate_loose_count()? > loose_threshold {
        return Ok(AutoCompaction::Incremental);
    }

    // Many packs (many past incremental compactions) → consolidate them.
    if count_pack_files()? > pack_threshold {
        return Ok(AutoCompaction::Repack);
    }

    Ok(AutoCompaction::None)
}

/// One pack's contribution to the object store, for the `store` census.
pub struct PackSummary {
    /// The pack's id (its file stem — a Blake3 of the sorted hashes it holds).
    pub id: String,
    /// Objects the pack holds (its index record count).
    pub objects: usize,
    /// Of `objects`, how many are stored as deltas against a base.
    pub deltas: usize,
    /// On-disk bytes of the pack: its data file plus its index file.
    pub bytes: u64,
}

/// One quarantined or unenumerable pack index in a [`StoreStatus`] census (FORK-47,
/// DESIGN.html §3.1.1) — an operator-facing pairing of the index file's path and why the loader
/// could not fully trust it. Shared shape for both `StoreStatus::quarantined_packs` (index parsed;
/// its data file did not load, or the index failed full validation — `error` says which) and
/// `StoreStatus::unenumerable_indexes` (the index itself did not parse — `error` is the parse
/// failure); which list an entry is in is what tells the two apart.
pub struct PackIndexProblem {
    /// The index file's on-disk path.
    pub index_path: String,
    /// Why this index could not be fully trusted.
    pub error: String,
}

/// A read-only snapshot of the object store's health, produced by [`store_status`]. Every
/// count is exact — a full scan, unlike the sampled estimate the background auto-maintenance
/// trigger ([`auto_compaction_action`]) uses to decide cheaply.
pub struct StoreStatus {
    /// Loose (unpacked) object files.
    pub loose_objects: usize,
    /// Total on-disk bytes of the loose objects.
    pub loose_bytes: u64,
    /// One entry per pack file.
    pub packs: Vec<PackSummary>,
    /// Objects held across all packs (the sum of the per-pack counts).
    pub packed_objects: usize,
    /// Objects stored as deltas across all packs.
    pub deltas: usize,
    /// Total on-disk bytes of the packs.
    pub pack_bytes: u64,
    /// Whether background maintenance (`maintenance.auto`) is enabled.
    pub auto_enabled: bool,
    /// The effective loose-object threshold above which an incremental compaction is due.
    pub loose_threshold: usize,
    /// The effective pack-count threshold above which a consolidating repack is due.
    pub pack_threshold: usize,
    /// Whether an incremental compaction is due now (loose objects over the threshold).
    pub incremental_due: bool,
    /// Whether a consolidating repack is due now (pack files over the threshold).
    pub repack_due: bool,
    /// Whether this store was bulk-ingested (`import-git`'s pack-direct path, or a franchise's
    /// native bundle install) and has not since been through a `compact --all --redelta` pass —
    /// see `mark_densify_pending`. A suggestion only; nothing ever acts on it automatically.
    pub densify_pending: bool,
    /// Pack indexes that parsed but whose paired data file could not be loaded (FORK-47,
    /// DESIGN.html §3.1.1). The objects they name are quarantined, not lost: a read of one
    /// classifies an honest `Unverifiable` (never a bare `Absent`), and `forklift heal` can still
    /// refetch from a configured remote any of them under a durability taint. Empty on a healthy
    /// store.
    pub quarantined_packs: Vec<PackIndexProblem>,
    /// Packs that load but whose index fails full validation (a corrupt bucket, or records that
    /// do not tile the data file). Reads keep serving their sound buckets; an object in a corrupt
    /// bucket reads `Unverifiable`. A full repack ([`compact`]) salvages what the data file still
    /// holds and removes the pack when every live object it may hold reads from another copy. Not
    /// counted among `packs`. Empty on a healthy store.
    pub damaged_packs: Vec<PackIndexProblem>,
    /// Pack index files that could not even be parsed (FORK-47, DESIGN.html §3.1.1) — their hash
    /// set is unknown, so they are inert (never gate a read verdict) but worth fixing: move the
    /// file aside and re-run. An index of another format version is listed in
    /// `foreign_version_indexes` instead. Empty on a healthy store.
    pub unenumerable_indexes: Vec<PackIndexProblem>,
    /// Pack indexes written by a build with another pack format version. Unlike an unparseable
    /// file, each gates every read it might answer: such a read fails naming the version rather
    /// than reporting the object absent. Moving one aside would make its objects read as missing;
    /// the remedy is re-creating the warehouse with this build. Empty on a healthy store.
    pub foreign_version_indexes: Vec<PackIndexProblem>,
}

/// Take an exact, read-only census of the object store: how many objects are loose vs packed,
/// how many packs (and how delta-dense) they are, the on-disk sizes, and whether an incremental
/// compaction or a consolidating repack is currently due per the `maintenance.*` thresholds. The
/// read counterpart of [`compact`] / [`auto_compaction_action`] — it scans the whole store, so
/// its numbers are exact rather than sampled.
///
/// # Returns
/// * `Ok(StoreStatus)` - The census.
/// * `Err(String)`     - If the store could not be read.
pub fn store_status() -> Result<StoreStatus, String> {
    // Loose objects: exact count and on-disk bytes (file metadata only, no content is read).
    let loose = enumerate_loose_objects()?;
    let loose_objects = loose.len();
    let loose_bytes: u64 = loose.iter().map(|target| target.size).sum();

    // Packs: reuse the reader (maps each data file and index). Per pack we report its object count
    // (the index), its delta count (the record kinds), and its on-disk size (data file + index
    // file, both already sized once loaded). `PackSet`'s registry-independent load means a single
    // orphaned or unparseable pack pair never fails this census (FORK-47, DESIGN.html §3.1.1) —
    // the problem lists are surfaced below instead. The load validates every index in full, so a
    // pack with a corrupt bucket is listed as damaged rather than counted.
    let PackSet { packs: loaded, quarantined, unenumerable, damaged, .. } = load_packs_from_disk(&pack_folder(), IndexCheck::Full)?;

    let mut packs = Vec::new();
    for pack in loaded {
        // A record leads with its kind byte; a delta is `RECORD_DELTA`. The full load already
        // validated every record against the data file's length.
        let deltas = pack.records()?.iter()
            .filter(|record| pack.data.get(record.offset as usize) == Some(&RECORD_DELTA))
            .count();

        let id = pack.data_path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();

        packs.push(PackSummary {
            id,
            objects: pack.index.count(),
            deltas,
            bytes: pack.data.len() as u64 + pack.index.byte_len() as u64,
        });
    }

    let packed_objects: usize = packs.iter().map(|pack| pack.objects).sum();
    let deltas: usize = packs.iter().map(|pack| pack.deltas).sum();
    let pack_bytes: u64 = packs.iter().map(|pack| pack.bytes).sum();

    // Maintenance thresholds and the current verdict, from the exact counts above.
    let auto_enabled = maintenance_auto_enabled()?;
    let loose_threshold = config_threshold(crate::util::config_utils::KEY_MAINTENANCE_LOOSE, AUTO_LOOSE_THRESHOLD)?;
    let pack_threshold = config_threshold(crate::util::config_utils::KEY_MAINTENANCE_PACKS, AUTO_PACK_THRESHOLD)?;
    let incremental_due = loose_objects > loose_threshold;
    let repack_due = packs.len() > pack_threshold;

    let quarantined_packs = quarantined.into_iter().map(|q| PackIndexProblem {
        index_path: q.index_path.to_string_lossy().into_owned(),
        error: q.error,
    }).collect();
    let (foreign, unenumerable): (Vec<_>, Vec<_>) = unenumerable.into_iter().partition(|u| u.foreign_version);
    let unenumerable_indexes = unenumerable.into_iter().map(|u| PackIndexProblem {
        index_path: u.index_path.to_string_lossy().into_owned(),
        error: u.error,
    }).collect();
    let foreign_version_indexes = foreign.into_iter().map(|u| PackIndexProblem {
        index_path: u.index_path.to_string_lossy().into_owned(),
        error: u.error,
    }).collect();
    let damaged_packs = damaged.into_iter().map(|d| PackIndexProblem {
        index_path: d.index_path.to_string_lossy().into_owned(),
        error: d.error,
    }).collect();

    Ok(StoreStatus {
        loose_objects,
        loose_bytes,
        packs,
        packed_objects,
        deltas,
        pack_bytes,
        auto_enabled,
        loose_threshold,
        pack_threshold,
        incremental_due,
        repack_due,
        densify_pending: densify_pending()?,
        quarantined_packs,
        damaged_packs,
        unenumerable_indexes,
        foreign_version_indexes,
    })
}

/// Whether background object-store maintenance is enabled (`maintenance.auto`, on when unset).
/// The one check both [`auto_compaction_action`] and the status report make.
fn maintenance_auto_enabled() -> Result<bool, String> {
    use crate::util::config_utils;

    match config_utils::get_effective_value(config_utils::KEY_MAINTENANCE_AUTO)? {
        Some((value, _)) => config_utils::parse_maintenance_auto(&value),
        None => Ok(true),
    }
}

/// Read a numeric maintenance threshold from configuration, falling back to `default` only when
/// it is unset — a value that is not a count is an error, never the default.
fn config_threshold(key: &str, default: usize) -> Result<usize, String> {
    use crate::util::config_utils;

    match config_utils::get_effective_value(key)? {
        Some((value, _)) => config_utils::parse_maintenance_threshold(key, &value),
        None => Ok(default),
    }
}

/// Estimate the loose object count without a full scan: count one fan-out folder (excluding
/// sidecars and temp files) and multiply by the 256 folders.
fn estimate_loose_count() -> Result<usize, String> {
    let folder = PathBuf::from(file_utils::get_path_objects_root()).join(AUTO_SAMPLE_FOLDER);

    let entries = match std::fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("Error while sampling loose objects: {}", error)),
    };

    let mut count = 0;
    for entry in entries {
        let name = entry.map_err(|e| format!("Error while sampling loose objects: {}", e))?
            .file_name().to_string_lossy().to_string();
        if !name.ends_with(sign_utils::FILE_SUFFIX_SIGNATURE) && !name.contains(".tmp") {
            count += 1;
        }
    }

    Ok(count * 256)
}

/// Count the pack data files in the object store's pack folder.
fn count_pack_files() -> Result<usize, String> {
    let entries = match std::fs::read_dir(pack_folder()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(format!("Error while counting packs: {}", error)),
    };

    let mut count = 0;
    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while counting packs: {}", e))?;
        if entry.path().extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION) {
            count += 1;
        }
    }

    Ok(count)
}

/// What a `compact` run did.
#[derive(Debug)]
pub struct CompactStats {
    /// Loose objects moved into packs.
    pub objects_packed: usize,

    /// Packs written (a run rolls over into several when the loose set is large).
    pub packs_written: usize,

    /// Loose object files removed after their pack was durably written.
    pub loose_removed: usize,

    /// Of `objects_packed`, how many were stored as deltas against a base.
    pub deltas: usize,

    /// Total bytes written into the packs (delta-compressed where deltas were used).
    pub bytes_packed: u64,

    /// Loose objects whose bytes did not decode, or did not hash to their own filename —
    /// skipped rather than packed, and left on disk exactly as found. Never a write, never a
    /// delete: the file stays in its best-recoverable (loose) state for `forklift audit`/`heal`
    /// or a future re-fetch to deal with.
    pub corrupt_skipped: usize,

    /// Loose objects whose decoded size would exceed the whole-object ceiling
    /// ([`object_utils::MAX_OBJECT_BYTES`]) — grandfathered giants authored before the ceiling
    /// existed, not corruption. Left loose forever rather than packed: a bounded decode cannot
    /// produce the bytes to pack, and the loose read path stays unbounded so the object stays
    /// fully readable.
    pub over_ceiling_skipped: usize,

    /// Damaged packs (an index that fails full validation: a corrupt bucket, or records that do
    /// not tile the data file) a full repack salvaged and deleted: every live object they might
    /// hold reads verified from another copy. Each entry is the index file's path. Always empty
    /// for an incremental compaction, which never reads existing packs.
    pub damaged_packs_dropped: Vec<String>,

    /// Damaged packs a full repack salvaged what it could from but kept, because some live object
    /// they might hold has no other copy that reads verified. Always empty for an incremental
    /// compaction.
    pub damaged_packs_kept: Vec<KeptDamagedPack>,

    /// Damaged packs a full repack run without salvage (auto-maintenance) left exactly as found:
    /// not salvaged, copied from or deleted. Each entry is the validation error, which names the
    /// index file. Always empty for a run with salvage, and for an incremental compaction.
    pub damaged_packs_skipped: Vec<String>,
}

/// A damaged pack a full repack kept (see [`CompactStats::damaged_packs_kept`]).
#[derive(Debug)]
pub struct KeptDamagedPack {
    /// The pack's index file.
    pub index_path: String,

    /// Why the index fails full validation.
    pub error: String,

    /// The live objects the pack might hold that no other copy serves verified, sorted. The pack
    /// is the only place their bytes could still be.
    pub unaccounted: Vec<String>,
}

/// The pack index format (version 3, FORK-118, DESIGN.html §4.5), and the only code that knows
/// where a record sits in an index file. Nothing outside this module computes a byte position in
/// an index: every reader goes through a [`CheckedIndex`](index_format::CheckedIndex), which
/// validates a bucket of records before it hands any of them out.
///
/// The layout, all integers little-endian:
///
/// * A 24-byte header: the magic `FORKPIDX`, `u32` version, `u32` record count, `u8` fanout bits
///   (at most 20), three zero bytes, and a `u32` crc32 over header bytes 0..20 followed by the
///   whole fanout table.
/// * The fanout table: one 8-byte entry per bucket, `2^fanout_bits` of them. Bucket `b` holds the
///   records whose hash's top `fanout_bits` bits equal `b`. Its entry is the `u32` cumulative end
///   of the bucket (the number of records in buckets `0..=b`) and a `u32` crc32 over the bucket's
///   record bytes.
/// * The records, sorted by hash: 32-byte hash, `u64` data offset, `u64` length.
///
/// Opening an index checks the header and the fanout table only, under 1% of the file. A bucket's
/// records are checked the first time something reads them: the crc matches, the hashes are
/// strictly ascending and all carry the bucket's prefix, and, for an index paired with a data
/// file, every record is a non-empty range inside that file's body. A bucket that passes proves,
/// for its prefix, that a hit is in bounds and that a miss is real. Whole-index consumers
/// validate every bucket first ([`CheckedIndex::all_records`]).
mod index_format {
    use std::collections::HashSet;
    use std::ops::Range;
    use std::path::Path;
    use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};

    use super::{read_u32_le, read_u64_le, HASH_LEN, INDEX_RECORD_LEN, PACK_DATA_HEADER_LEN,
                PACK_FORMAT_VERSION, PACK_INDEX_MAGIC};

    /// The fixed header: magic (8), version (4), count (4), fanout bits (1), zero (3), crc (4).
    const HEADER_LEN: usize = 24;

    /// Where the header's own crc sits; the crc covers the bytes before it.
    const HEADER_CRC_OFFSET: usize = 20;

    /// One fanout entry: the bucket's cumulative end (4) and its records' crc32 (4).
    const FANOUT_ENTRY_LEN: usize = 8;

    /// The most fanout bits an index may declare: a 2^20-entry table is 8 MiB, enough for an
    /// average of 32 records per bucket up to 32M records.
    const MAX_FANOUT_BITS: u32 = 20;

    /// The writer sizes the table so a bucket averages at most this many records, which keeps a
    /// lookup's validation to about 1.5 KiB of crc.
    const TARGET_BUCKET_RECORDS: usize = 32;

    /// The bytes that identify an index: its magic and its version.
    pub(super) const IDENTITY_LEN: usize = 12;

    /// Versions from 1 up to this are ones a build could plausibly have written; anything else in
    /// a version field is rot, not another build.
    const PLAUSIBLE_VERSION_LIMIT: u32 = 256;

    /// The version of an index that carries this format's magic but another plausible version —
    /// one written by another build, which this build refuses by name. `None` for this version,
    /// for a version field that can only be corruption (zero, or too large), and for anything
    /// without the magic.
    pub(super) fn foreign_version(bytes: &[u8]) -> Option<u32> {
        if bytes.len() < IDENTITY_LEN || &bytes[0..8] != PACK_INDEX_MAGIC {
            return None;
        }
        Some(read_u32_le(bytes, 8))
            .filter(|version| *version != PACK_FORMAT_VERSION && (1..PLAUSIBLE_VERSION_LIMIT).contains(version))
    }

    /// The error for a bucket of the index at `path` that failed validation.
    pub(super) fn corrupt_bucket_message(path: &Path, bucket: usize) -> String {
        format!("Pack index \"{}\" is corrupt (bucket {}).", path.to_string_lossy(), bucket)
    }

    /// The fanout bits the writer picks for `count` records: the smallest that brings the average
    /// bucket to [`TARGET_BUCKET_RECORDS`] or below, capped at [`MAX_FANOUT_BITS`].
    pub(super) const fn fanout_bits_for(count: usize) -> u32 {
        let mut bits = 0;
        while bits < MAX_FANOUT_BITS && count.div_ceil(1 << bits) > TARGET_BUCKET_RECORDS {
            bits += 1;
        }
        bits
    }

    /// The exact file length of an index of `count` records with `bits` fanout bits.
    const fn len_with(count: u64, bits: u32) -> u64 {
        HEADER_LEN as u64 + FANOUT_ENTRY_LEN as u64 * (1u64 << bits) + INDEX_RECORD_LEN as u64 * count
    }

    /// The exact file length of the index the writer produces for `count` records.
    pub(super) const fn index_len(count: usize) -> u64 {
        len_with(count as u64, fanout_bits_for(count))
    }

    /// How many index buckets this process has validated (each at most once per loaded index),
    /// for `FORKLIFT_DEBUG_PACK_COUNTS`.
    pub(super) static BUCKETS_VALIDATED: AtomicU64 = AtomicU64::new(0);

    /// The bucket a hash belongs to: its top `bits` bits.
    fn bucket_of(hash: &[u8], bits: u32) -> usize {
        if bits == 0 {
            return 0;
        }
        let top = u32::from_be_bytes([hash[0], hash[1], hash[2], hash[3]]);
        (top >> (32 - bits)) as usize
    }

    /// The bucket `hash` falls in, in the index the writer produces for `count` records, so a test
    /// can place a hash inside or outside one before the pack exists.
    #[cfg(test)]
    pub(super) fn bucket_in_index_of(count: usize, hash: &[u8]) -> usize {
        bucket_of(hash, fanout_bits_for(count))
    }

    /// Build an index over `records`, which must already be sorted by hash.
    pub(super) fn build(records: &[([u8; HASH_LEN], u64, u64)]) -> Vec<u8> {
        let bits = fanout_bits_for(records.len());
        let buckets = 1usize << bits;
        let mut bytes = Vec::with_capacity(len_with(records.len() as u64, bits) as usize);

        bytes.extend_from_slice(PACK_INDEX_MAGIC);
        bytes.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(records.len() as u32).to_le_bytes());
        bytes.push(bits as u8);
        bytes.extend_from_slice(&[0u8; 3]);
        bytes.extend_from_slice(&[0u8; 4]);

        let fanout_start = bytes.len();
        bytes.resize(fanout_start + FANOUT_ENTRY_LEN * buckets, 0);

        let mut position = 0;
        for bucket in 0..buckets {
            let mut crc = crc32fast::Hasher::new();
            while position < records.len() && bucket_of(&records[position].0, bits) == bucket {
                let (hash, offset, length) = &records[position];
                let mut record = [0u8; INDEX_RECORD_LEN];
                record[..HASH_LEN].copy_from_slice(hash);
                record[HASH_LEN..HASH_LEN + 8].copy_from_slice(&offset.to_le_bytes());
                record[HASH_LEN + 8..].copy_from_slice(&length.to_le_bytes());
                crc.update(&record);
                bytes.extend_from_slice(&record);
                position += 1;
            }

            let entry = fanout_start + bucket * FANOUT_ENTRY_LEN;
            bytes[entry..entry + 4].copy_from_slice(&(position as u32).to_le_bytes());
            bytes[entry + 4..entry + 8].copy_from_slice(&crc.finalize().to_le_bytes());
        }

        let header_crc = header_crc(&bytes, buckets);
        bytes[HEADER_CRC_OFFSET..HEADER_LEN].copy_from_slice(&header_crc.to_le_bytes());
        bytes
    }

    /// crc32 over header bytes 0..20 and the fanout table.
    fn header_crc(bytes: &[u8], buckets: usize) -> u32 {
        let mut crc = crc32fast::Hasher::new();
        crc.update(&bytes[..HEADER_CRC_OFFSET]);
        crc.update(&bytes[HEADER_LEN..HEADER_LEN + FANOUT_ENTRY_LEN * buckets]);
        crc.finalize()
    }

    /// What an index's header and fanout table declare, once both have been checked.
    #[derive(Clone, Copy, Debug)]
    pub(super) struct Layout {
        count: usize,
        bits: u32,
    }

    impl Layout {
        /// Check an index's header and fanout table: the magic, version 3, the fanout bits and
        /// the zero bytes, the exact file length, the header crc, and that the bucket ends never
        /// decrease and finish at the record count. Reads no record.
        pub(super) fn parse(bytes: &[u8], path: &Path) -> Result<Layout, String> {
            let corrupt = || format!("Pack index \"{}\" is corrupt or has an unknown format.", path.to_string_lossy());

            if let Some(version) = foreign_version(bytes) {
                return Err(format!(
                    "Pack index \"{}\" has format version {}, but this build reads only version {}.",
                    path.to_string_lossy(), version, PACK_FORMAT_VERSION
                ));
            }
            if bytes.len() < HEADER_LEN || &bytes[0..8] != PACK_INDEX_MAGIC || read_u32_le(bytes, 8) != PACK_FORMAT_VERSION {
                return Err(corrupt());
            }

            let count = read_u32_le(bytes, 12) as usize;
            let bits = bytes[16] as u32;
            if bits > MAX_FANOUT_BITS || bytes[17..20] != [0, 0, 0] {
                return Err(corrupt());
            }

            if bytes.len() as u64 != len_with(count as u64, bits) {
                return Err(corrupt());
            }

            let buckets = 1usize << bits;
            if read_u32_le(bytes, HEADER_CRC_OFFSET) != header_crc(bytes, buckets) {
                return Err(corrupt());
            }

            let mut previous = 0;
            for bucket in 0..buckets {
                let end = read_u32_le(bytes, HEADER_LEN + bucket * FANOUT_ENTRY_LEN) as usize;
                if end < previous {
                    return Err(corrupt());
                }
                previous = end;
            }
            if previous != count {
                return Err(corrupt());
            }

            Ok(Layout { count, bits })
        }
    }

    /// The answer to "where is this hash in this index?".
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Located {
        /// The record's `(offset, length)` in the data file; its bucket passed validation.
        Found(u64, u64),
        /// The hash's bucket passed validation and does not hold it.
        Missing,
        /// The hash's bucket failed validation, so this index cannot say either way.
        CorruptBucket(usize),
    }

    impl Located {
        /// The record's `(offset, length)`, if the hash was found.
        pub(super) fn found(self) -> Option<(u64, u64)> {
            match self {
                Located::Found(offset, length) => Some((offset, length)),
                Located::Missing | Located::CorruptBucket(_) => None,
            }
        }
    }

    /// One index record.
    #[derive(Clone, Copy)]
    pub(super) struct Record<'a> {
        pub(super) hash: &'a [u8],
        pub(super) offset: u64,
        pub(super) length: u64,
    }

    /// A bucket's verdict, cached per loaded index.
    const UNCHECKED: u8 = 0;
    const VALID: u8 = 1;
    const CORRUPT: u8 = 2;

    /// An index whose header and fanout table have been checked, over bytes it owns (a mapping or
    /// a buffer), with a verdict cache that validates each bucket at most once.
    pub(super) struct CheckedIndex<B> {
        bytes: B,
        layout: Layout,
        /// The paired data file's length, which every record must fall inside, or `None` for an
        /// index read without its data file; then only crc, order and prefix are checked.
        data_len: Option<u64>,
        verdicts: Box<[AtomicU8]>,
    }

    impl<B: AsRef<[u8]>> CheckedIndex<B> {
        /// Wrap `bytes`, which `layout` was parsed from.
        pub(super) fn new(bytes: B, layout: Layout, data_len: Option<u64>) -> CheckedIndex<B> {
            assert_eq!(bytes.as_ref().len() as u64, len_with(layout.count as u64, layout.bits),
                "an index's layout must come from its own bytes");
            let verdicts = (0..1usize << layout.bits).map(|_| AtomicU8::new(UNCHECKED)).collect();
            CheckedIndex { bytes, layout, data_len, verdicts }
        }

        /// The number of records.
        pub(super) fn count(&self) -> usize {
            self.layout.count
        }

        /// The index file's length in bytes.
        pub(super) fn byte_len(&self) -> usize {
            self.bytes.as_ref().len()
        }

        fn buckets(&self) -> usize {
            1 << self.layout.bits
        }

        fn records_start(&self) -> usize {
            HEADER_LEN + FANOUT_ENTRY_LEN * self.buckets()
        }

        /// The record positions bucket `bucket` spans, from the checked fanout table.
        fn bucket_span(&self, bucket: usize) -> Range<usize> {
            let bytes = self.bytes.as_ref();
            let end_of = |b: usize| read_u32_le(bytes, HEADER_LEN + b * FANOUT_ENTRY_LEN) as usize;
            let start = if bucket == 0 { 0 } else { end_of(bucket - 1) };
            start..end_of(bucket)
        }

        fn record_at(&self, position: usize) -> Record<'_> {
            let bytes = self.bytes.as_ref();
            let start = self.records_start() + position * INDEX_RECORD_LEN;
            Record {
                hash: &bytes[start..start + HASH_LEN],
                offset: read_u64_le(bytes, start + HASH_LEN),
                length: read_u64_le(bytes, start + HASH_LEN + 8),
            }
        }

        /// Whether bucket `bucket` passes validation, computing and caching the verdict on first
        /// use. Two threads may both compute it; they reach the same answer.
        fn bucket_is_valid(&self, bucket: usize) -> bool {
            match self.verdicts[bucket].load(Ordering::Relaxed) {
                VALID => return true,
                CORRUPT => return false,
                _ => {}
            }

            BUCKETS_VALIDATED.fetch_add(1, Ordering::Relaxed);
            let valid = self.check_bucket(bucket);
            self.verdicts[bucket].store(if valid { VALID } else { CORRUPT }, Ordering::Relaxed);
            valid
        }

        fn check_bucket(&self, bucket: usize) -> bool {
            let span = self.bucket_span(bucket);
            let bytes = self.bytes.as_ref();
            let start = self.records_start() + span.start * INDEX_RECORD_LEN;
            let end = self.records_start() + span.end * INDEX_RECORD_LEN;

            let stored_crc = read_u32_le(bytes, HEADER_LEN + bucket * FANOUT_ENTRY_LEN + 4);
            if crc32fast::hash(&bytes[start..end]) != stored_crc {
                return false;
            }

            let mut previous: Option<&[u8]> = None;
            for position in span {
                let record = self.record_at(position);

                if previous.is_some_and(|previous| previous >= record.hash) {
                    return false;
                }
                previous = Some(record.hash);

                if bucket_of(record.hash, self.layout.bits) != bucket {
                    return false;
                }

                if let Some(data_len) = self.data_len {
                    let in_bounds = record.offset.checked_add(record.length)
                        .is_some_and(|end| end <= data_len);
                    if record.length == 0 || record.offset < PACK_DATA_HEADER_LEN || !in_bounds {
                        return false;
                    }
                }
            }

            true
        }

        /// Find `hash`, validating only its bucket.
        pub(super) fn locate(&self, hash: &[u8; HASH_LEN]) -> Located {
            let bucket = bucket_of(hash, self.layout.bits);
            if !self.bucket_is_valid(bucket) {
                return Located::CorruptBucket(bucket);
            }

            let span = self.bucket_span(bucket);
            let (mut low, mut high) = (span.start, span.end);
            while low < high {
                let mid = low + (high - low) / 2;
                let record = self.record_at(mid);
                match record.hash.cmp(hash.as_slice()) {
                    std::cmp::Ordering::Less => low = mid + 1,
                    std::cmp::Ordering::Greater => high = mid,
                    std::cmp::Ordering::Equal => return Located::Found(record.offset, record.length),
                }
            }

            Located::Missing
        }

        /// The buckets that can hold a hash whose hex form starts with `prefix`, or an empty range
        /// when no hash can (a character that is not lowercase hex, or a prefix longer than a
        /// hash).
        fn buckets_for_hex_prefix(&self, prefix: &str) -> Range<usize> {
            let bits = self.layout.bits;
            if prefix.len() > HASH_LEN * 2 || !prefix.bytes().all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')) {
                return 0..0;
            }

            // The prefix's leading `known` bits, as a number.
            let mut value = 0usize;
            let mut known = 0;
            for c in prefix.bytes() {
                if known >= bits {
                    break;
                }
                let nibble = (c as char).to_digit(16).expect("checked to be hex above") as usize;
                value = (value << 4) | nibble;
                known += 4;
            }
            if known > bits {
                value >>= known - bits;
                known = bits;
            }

            let first = value << (bits - known);
            first..first + (1 << (bits - known))
        }

        /// The records of every bucket a hash starting with `prefix` could be in, validating each
        /// of those buckets first. `Err(bucket)` names the first that failed.
        pub(super) fn records_for_hex_prefix(&self, prefix: &str) -> Result<Vec<Record<'_>>, usize> {
            let mut records = Vec::new();
            for bucket in self.buckets_for_hex_prefix(prefix) {
                if !self.bucket_is_valid(bucket) {
                    return Err(bucket);
                }
                records.extend(self.bucket_span(bucket).map(|position| self.record_at(position)));
            }
            Ok(records)
        }

        /// Validate every bucket, then give access to every record. `Err(bucket)` names the first
        /// bucket that failed.
        pub(super) fn all_records(&self) -> Result<AllRecords<'_, B>, usize> {
            for bucket in 0..self.buckets() {
                if !self.bucket_is_valid(bucket) {
                    return Err(bucket);
                }
            }
            Ok(AllRecords { index: self })
        }

        /// The records of every bucket that passes validation, in index order: what a damaged
        /// index still vouches for.
        pub(super) fn valid_bucket_records(&self) -> Vec<Record<'_>> {
            (0..self.buckets())
                .filter(|&bucket| self.bucket_is_valid(bucket))
                .flat_map(|bucket| self.bucket_span(bucket).map(|position| self.record_at(position)))
                .collect()
        }

        /// Every hash the record table names, read as raw entries without consulting the fanout
        /// table or any bucket's verdict: every hash a damaged index could be naming, whatever
        /// its corruption.
        pub(super) fn raw_record_hashes(&self) -> HashSet<[u8; HASH_LEN]> {
            (0..self.count()).map(|position| {
                let mut hash = [0u8; HASH_LEN];
                hash.copy_from_slice(self.record_at(position).hash);
                hash
            }).collect()
        }

        /// Where record `position` starts in the file, so a test can corrupt it.
        #[cfg(test)]
        pub(super) fn record_byte_position(&self, position: usize) -> usize {
            self.records_start() + position * INDEX_RECORD_LEN
        }

        /// The bucket `hash` belongs to, so a test can pick hashes inside or outside one.
        #[cfg(test)]
        pub(super) fn bucket_of_hash(&self, hash: &[u8]) -> usize {
            bucket_of(hash, self.layout.bits)
        }
    }

    /// Every record of an index all of whose buckets passed validation.
    pub(super) struct AllRecords<'a, B> {
        index: &'a CheckedIndex<B>,
    }

    impl<'a, B: AsRef<[u8]>> AllRecords<'a, B> {
        pub(super) fn len(&self) -> usize {
            self.index.count()
        }

        /// Record `position`, which must be below [`Self::len`].
        pub(super) fn get(&self, position: usize) -> Record<'a> {
            assert!(position < self.len(), "record position {} is past the index's {} records", position, self.len());
            self.index.record_at(position)
        }

        pub(super) fn iter(&self) -> impl Iterator<Item = Record<'a>> + '_ {
            (0..self.len()).map(|position| self.get(position))
        }

        /// Whether the records' ranges cover the data body from its header to `data_len` exactly
        /// once, with no gap or overlap: the check that makes every byte of a published pack
        /// accountable to one hash. It needs every record, so only whole-pack consumers run it.
        pub(super) fn tile(&self, data_len: u64) -> bool {
            let mut ranges: Vec<(u64, u64)> = self.iter()
                .map(|record| (record.offset, record.offset.saturating_add(record.length)))
                .collect();
            super::sort_ranges_by_start(&mut ranges);

            let mut expected = PACK_DATA_HEADER_LEN;
            for (start, end) in ranges {
                if start != expected {
                    return false;
                }
                expected = end;
            }
            expected == data_len
        }
    }
}

use index_format::{CheckedIndex, Located};

/// A pack loaded for reading: its data file and its index both memory-mapped. Loading checked the
/// index's header and fanout table only; a lookup validates just the bucket it reads
/// ([`CheckedIndex`]), and a whole-pack consumer runs [`LoadedPack::validate_full`] first.
struct LoadedPack {
    data_path: PathBuf,
    /// The data file mapped into memory for the life of the loaded pack, so a read is a slice
    /// into mapped pages — no `open`/`seek`/`read` syscall and no buffer copy per object, which
    /// on a history or blame walk is tens of thousands of syscalls and copies saved. A pack is
    /// immutable once written (write-once, then deleted whole; never truncated) and a `compact`
    /// invalidates the whole registry, so the mapping never goes stale.
    data: memmap2::Mmap,
    /// The index, mapped for the same reason and under the same immutability, with every record
    /// bounded by `data`'s length.
    index: CheckedIndex<memmap2::Mmap>,
    /// [`Self::validate_full`]'s verdict, computed at most once per loaded pack.
    full: OnceLock<Result<(), String>>,
}

/// How many packs this process has fully validated ([`LoadedPack::validate_full`]). Printed with
/// the index bucket count under `FORKLIFT_DEBUG_PACK_COUNTS`, so a test can prove a command
/// validated only the buckets it read.
static PACK_FULL_VALIDATIONS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The process-wide count of index buckets validated — see [`PACK_FULL_VALIDATIONS`].
pub fn pack_buckets_validated() -> u64 {
    index_format::BUCKETS_VALIDATED.load(std::sync::atomic::Ordering::Relaxed)
}

/// The process-wide count of packs fully validated — see [`PACK_FULL_VALIDATIONS`].
pub fn pack_full_validations() -> u64 {
    PACK_FULL_VALIDATIONS.load(std::sync::atomic::Ordering::Relaxed)
}

impl LoadedPack {
    /// Where `hash_bytes` is in this pack's data file, validating only the index bucket it would
    /// be in.
    fn locate(&self, hash_bytes: &[u8; HASH_LEN]) -> Located {
        self.index.locate(hash_bytes)
    }

    fn index_path(&self) -> PathBuf {
        self.data_path.with_extension(PACK_INDEX_EXTENSION)
    }

    /// The error a reader reports for an index bucket that failed validation. While a full repack
    /// reads objects (target collection and packing), a pack its salvage could not completely
    /// recover also says so, because no in-store remedy remains for what only its damaged part
    /// held (see [`SALVAGE_REGISTRY`]).
    fn corrupt_bucket_error(&self, bucket: usize) -> String {
        let index_path = self.index_path();
        let message = index_format::corrupt_bucket_message(&index_path, bucket);
        if !is_incompletely_salvaged(&index_path) {
            return message;
        }
        format!(
            "{} This full repack could not completely salvage that pack, so an object read through \
            its damaged part may be held nowhere else and cannot be recovered from this store. \
            Nothing was deleted.",
            message
        )
    }

    /// Validate the whole index: every bucket, and that the records tile the data body exactly
    /// once. Run before anything that reads every record of a pack (a transport import, a
    /// compaction's copy, a census), so whole-pack work never trusts a record a lookup would have
    /// refused. Memoized: the second call is free.
    fn validate_full(&self) -> Result<(), String> {
        self.full.get_or_init(|| {
            PACK_FULL_VALIDATIONS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            // The plain message, never the salvage context: this verdict is memoized for the life
            // of the loaded pack, and the context holds only while a repack is collecting.
            let records = self.index.all_records()
                .map_err(|bucket| index_format::corrupt_bucket_message(&self.index_path(), bucket))?;
            if !records.tile(self.data.len() as u64) {
                return Err(format!(
                    "Pack index \"{}\" is corrupt: its records do not cover the data file exactly once.",
                    self.index_path().to_string_lossy()
                ));
            }
            Ok(())
        }).clone()
    }

    /// Every record, after [`Self::validate_full`] passes.
    fn records(&self) -> Result<index_format::AllRecords<'_, memmap2::Mmap>, String> {
        self.validate_full()?;
        self.index.all_records().map_err(|bucket| index_format::corrupt_bucket_message(&self.index_path(), bucket))
    }

    /// A borrowed slice of `length` bytes at `offset` in this pack's mapped data file — no copy,
    /// no syscall. Bounds-checked so a corrupt index offset is a clean error, not a fault.
    fn slice(&self, offset: u64, length: u64) -> Result<&[u8], String> {
        let start = offset as usize;
        let end = start.checked_add(length as usize)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| format!(
                "Pack \"{}\" record at offset {} length {} is out of bounds.",
                self.data_path.to_string_lossy(), offset, length
            ))?;
        Ok(&self.data[start..end])
    }
}

/// A pack index that parsed cleanly but whose data file could not be loaded (missing, unmappable,
/// or shape-mismatched against the index — see `load_pack_pair`) — an orphaned `.idx`. Its index
/// bytes are kept resident behind the same per-bucket checks [`LoadedPack`] uses, so membership
/// ([`QuarantinedPack::contains`]) is a bucket lookup, no per-hash parse. `error` is `load_pack_pair`'s own failure message, reused verbatim in the
/// `Unverifiable` wording a quarantine-named read produces (`file_utils::read_object_classified`).
struct QuarantinedPack {
    index_path: PathBuf,
    /// Read into memory, never mapped: a quarantined index is not trusted to stay unchanged.
    index: CheckedIndex<Vec<u8>>,
    error: String,
    /// Revalidation state (see [`reloaded_if_stale`]): the pair's files as they stood just before
    /// this load, what kind of failure it was, and when it was attempted.
    stamp: PairStamp,
    failure: FailureKind,
    attempted_at: Instant,
}

/// A pack index that could not even be read or parsed, so its hash set is unknown. `error` is the
/// read or parse failure; `stamp`, `failure` and `attempted_at` mean what they do on
/// [`QuarantinedPack`].
struct UnenumerablePack {
    index_path: PathBuf,
    error: String,
    /// The index carries this format's magic but another version: a store written by another
    /// build, whose objects this build cannot read. Unlike an unparseable file, it is known to be
    /// a pack index, so it gates every verdict (see [`quarantine_reason`]): a read it might
    /// answer fails naming its version instead of reporting the object absent.
    foreign_version: bool,
    stamp: PairStamp,
    failure: FailureKind,
    attempted_at: Instant,
}

/// A pack pair that loads but whose index fails full validation (a corrupt bucket, or records that
/// do not tile the data file). Only a census ([`IndexCheck::Full`]) tells it apart: the registry
/// loads it and keeps serving its sound buckets, so its objects are neither quarantined nor lost
/// except those in a corrupt bucket, which read `Unverifiable`. `error` is the validation failure.
struct DamagedPack {
    index_path: PathBuf,
    error: String,
}

/// What kind of failure left a pair broken, which decides when [`reloaded_if_stale`] retries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FailureKind {
    /// An `open` failed with something other than not-found (permissions, descriptor exhaustion).
    /// The file is there and may open next time; retrying is two `open`s, so it is retried on every
    /// miss.
    Open,
    /// A read or map failed after a successful `open` (EIO, ENOMEM, too many mappings). It may
    /// clear without the file changing, but retrying means re-reading the index, so it is retried
    /// at most once per [`IO_FAILURE_RETRY_INTERVAL`].
    Io,
    /// A file was not found, or its content was rejected. It stays as it is until a file changes,
    /// which the pair's [`PairStamp`] notices.
    Fixed,
}

/// How often a pair broken by a post-`open` I/O failure ([`FailureKind::Io`]) is retried while
/// its files stay unchanged: often enough that a passing fault clears within seconds, rarely
/// enough that a persistent one does not re-read an index on every miss.
const IO_FAILURE_RETRY_INTERVAL: Duration = Duration::from_secs(10);

/// One file as `stat` saw it — enough to tell that it was created, removed, rewritten, or renamed
/// over since. The inode is what catches a same-length, same-second rename-over on unix; elsewhere
/// it reads 0 and the other fields carry the comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileStamp {
    Absent,
    /// `stat` itself failed with something other than not-found. Equal to itself, so a file that
    /// stays unstatable is not re-read on every miss.
    Unstatable,
    Present { len: u64, modified: Option<SystemTime>, inode: u64 },
}

fn file_stamp(path: &Path) -> FileStamp {
    match std::fs::metadata(path) {
        Ok(metadata) => FileStamp::Present {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            inode: inode_of(&metadata),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileStamp::Absent,
        Err(_) => FileStamp::Unstatable,
    }
}

#[cfg(unix)]
fn inode_of(metadata: &std::fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.ino()
}

#[cfg(not(unix))]
fn inode_of(_metadata: &std::fs::Metadata) -> u64 {
    0
}

/// A pack pair's two files, stamped together.
#[derive(Debug, Clone, PartialEq, Eq)]
struct PairStamp {
    index: FileStamp,
    data: FileStamp,
    /// When a file was modified within [`MTIME_GRANULARITY`] of this stamp being taken, it may
    /// change again in the same mtime tick without its stamp moving (an in-place, same-length
    /// rewrite on a coarse-mtime filesystem). This is the moment that window closes: once it has
    /// passed, [`broken_pair_changed`] re-classifies the pair once, and the stamp the reload takes
    /// then no longer falls inside the window. `None` when no file was that fresh.
    settles_at: Option<SystemTime>,
}

impl PairStamp {
    /// Whether both files still stat as they did when `self` was taken.
    fn files_match(&self, other: &PairStamp) -> bool {
        self.index == other.index && self.data == other.data
    }
}

fn pair_stamp(index_path: &Path) -> PairStamp {
    let read_at = SystemTime::now();
    let index = file_stamp(index_path);
    let data = file_stamp(&index_path.with_extension(PACK_DATA_EXTENSION));

    let settles_at = [&index, &data].into_iter()
        .filter_map(|stamp| match stamp {
            FileStamp::Present { modified: Some(modified), .. } => Some(*modified + MTIME_GRANULARITY),
            _ => None,
        })
        .filter(|settles| *settles >= read_at)
        .max();

    PairStamp { index, data, settles_at }
}

/// What the pack folder looked like when a [`PackSet`] was loaded from it, taken *before* the
/// listing so a pack published during the load moves the folder past the stamp rather than
/// slipping in under it.
struct FolderStamp {
    /// The sorted names of every pack index and pack data file in the folder. Temp files end in
    /// `.tmp`, so they are not among them.
    names: Vec<OsString>,
    /// The folder's mtime and when it was read. Both were taken before `names` was listed, and a
    /// racy check that lists the folder again and finds the same names replaces both with the
    /// values it read before *its* listing — so a stamp stops being racy once the granularity
    /// margin has passed, and is never newer than the names it vouches for.
    times: Mutex<StampTimes>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct StampTimes {
    /// `None` when the folder did not exist. `Some(None)` when it existed but the platform reported
    /// no modification time — such a stamp is always racy, so every miss compares names.
    modified: Option<Option<SystemTime>>,
    read_at: SystemTime,
}

impl StampTimes {
    /// Whether the folder may have changed again within the same mtime tick it was stamped at, so
    /// an equal mtime does not prove the listing is current.
    fn is_racy(&self) -> bool {
        match self.modified {
            None => false,
            Some(None) => true,
            Some(Some(modified)) => modified + MTIME_GRANULARITY >= self.read_at,
        }
    }
}

/// How coarse a filesystem's modification times may be. A folder modified within this margin of
/// when it was stamped may have changed again in the same tick, so its mtime alone cannot prove
/// that nothing was published since.
const MTIME_GRANULARITY: Duration = Duration::from_secs(2);

impl QuarantinedPack {
    /// Whether this quarantined index names `hash_bytes`. There is no data file to bound records
    /// against, so its buckets are checked for crc, order and prefix only. A bucket that fails
    /// counts as naming every hash it could hold: the index cannot rule them out, so a read of one
    /// is `Unverifiable`, never a bare `Absent`.
    fn contains(&self, hash_bytes: &[u8; HASH_LEN]) -> bool {
        match self.index.locate(hash_bytes) {
            Located::Found(..) | Located::CorruptBucket(_) => true,
            Located::Missing => false,
        }
    }
}

/// The pack registry's load-time outcome: every pack pair that loaded cleanly, plus the two
/// broken shapes a loader must survive without failing the whole load (DESIGN.html §3.1.1,
/// FORK-47) — an orphaned index (quarantined: enumerable, but unservable) and an index that could
/// not even be parsed (unenumerable: its hash set is unknown). Presence and reads treat both as
/// "as if the file were absent" *except* that a `quarantined`-named hash gets an honest
/// `Unverifiable` instead of a bare `Absent` at the single gate that produces it (see
/// `file_utils::read_object_classified`). `unenumerable` is consulted there only for an index of
/// another format version, which gates every hash; any other unparseable file is inert.
struct PackSet {
    /// Loaded pairs, sorted by file name (see `load_packs_from_disk`).
    packs: Vec<LoadedPack>,
    /// Index parses; data missing/unmappable/bad. Sorted by index file name for the same
    /// deterministic-error-message reason `packs` is.
    quarantined: Vec<QuarantinedPack>,
    /// Index itself unreadable or corrupt. Its hash set is unknown, so it gates no verdict
    /// unless it is of another format version.
    unenumerable: Vec<UnenumerablePack>,
    /// Pairs that load but fail full validation. Filled only by a census load
    /// ([`IndexCheck::Full`]); the registry loads these pairs into `packs`.
    damaged: Vec<DamagedPack>,
    /// The folder as it stood when this set was loaded (see [`reloaded_if_stale`]).
    stamp: FolderStamp,
}

impl PackSet {
    fn empty() -> PackSet {
        PackSet {
            packs: Vec::new(),
            quarantined: Vec::new(),
            unenumerable: Vec::new(),
            damaged: Vec::new(),
            stamp: FolderStamp {
                names: Vec::new(),
                times: Mutex::new(StampTimes { modified: None, read_at: SystemTime::UNIX_EPOCH }),
            },
        }
    }
}

/// The read cache: each warehouse's object store maps to the packs loaded for it. Keyed by
/// the objects-root path so one process serving several warehouse roots (the server, via a
/// storage-root scope) never mixes their packs. Loaded on first use, and revalidated against the
/// pack folder on every lookup that misses (see [`reloaded_if_stale`]), so a pack another process
/// publishes — an external `compact` beside a live server — is seen by the next miss. `compact`
/// still invalidates its own root's entry so a same-process read sees its packs at once.
static PACK_REGISTRY: OnceLock<Mutex<HashMap<String, Arc<PackSet>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<PackSet>>> {
    PACK_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pack folder for the active warehouse's object store. `pub(crate)` so the durability-taint
/// recovery walk (`recovery_utils`) can recognize a recorded taint path as living under it,
/// without duplicating the folder layout.
pub(crate) fn pack_folder() -> PathBuf {
    PathBuf::from(file_utils::get_path_objects_root()).join(PACK_FOLDER_NAME)
}

/// Read the packs for the active warehouse, loading and caching them on first use. The
/// returned `Arc` lets a lookup search without holding the registry lock.
fn loaded_packs() -> Result<Arc<PackSet>, String> {
    let key = file_utils::get_path_objects_root();

    if let Some(packs) = registry().lock().expect("the pack registry lock is poisoned").get(&key) {
        return Ok(Arc::clone(packs));
    }

    reload_packs()
}

/// How many times this process read a pack folder into a pack set, and how many of those sets a
/// preload cached. Test-only observability across the subprocess boundary, printed under
/// `FORKLIFT_DEBUG_PACK_COUNTS`: a preload that raced the render into a second read, or one that
/// fired for a walk with nothing to render, is invisible in the output alone.
static PACK_SET_READ_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static PACK_PRELOAD_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The process-wide pack-folder read count — see [`PACK_SET_READ_COUNT`].
pub fn pack_set_read_count() -> u64 {
    PACK_SET_READ_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// The process-wide count of pack sets a preload cached — see [`PACK_SET_READ_COUNT`].
pub fn pack_preload_count() -> u64 {
    PACK_PRELOAD_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Where the active warehouse's pack set lives: its registry key and its pack folder, resolved
/// when this is made. A storage-root scope is thread-local, so a caller that hands the read to
/// other threads resolves this first, on its own thread.
#[derive(Clone, Debug)]
pub struct PackSetLocation {
    key: String,
    folder: PathBuf,
}

impl PackSetLocation {
    /// The active warehouse's pack set, as the calling thread resolves it.
    pub fn current() -> Self {
        Self { key: file_utils::get_path_objects_root(), folder: pack_folder() }
    }

    /// Start reading this pack set on a background thread. Opening a pack set reads and validates
    /// every index, ~4.4 ms for git.git's ~500k records (FORK-183), so a command that walks first
    /// and looks objects up after overlaps the two instead of paying them in turn.
    ///
    /// The set is cached only if nothing was cached for the key meanwhile, and a read error is
    /// dropped: the first real lookup then reads the folder itself and reports the error where it
    /// always has. Dropping the result without waiting on it detaches the thread, and process
    /// exit does not wait for it.
    pub fn preload(&self) -> PackPreload {
        let PackSetLocation { key, folder } = self.clone();

        let handle = std::thread::Builder::new()
            .name("forklift-pack-preload".to_string())
            .spawn(move || {
                if registry().lock().expect("the pack registry lock is poisoned").contains_key(&key) {
                    return;
                }

                if let Ok(packs) = load_packs_from_disk(&folder, IndexCheck::Lazy) {
                    registry().lock().expect("the pack registry lock is poisoned")
                        .entry(key)
                        .or_insert_with(|| {
                            PACK_PRELOAD_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                            Arc::new(packs)
                        });
                }
            })
            .ok();

        PackPreload { handle }
    }
}

/// A pack set being read on a background thread ahead of its first lookup (see
/// [`PackSetLocation::preload`]).
pub struct PackPreload {
    handle: Option<std::thread::JoinHandle<()>>,
}

impl PackPreload {
    /// Wait for the preload to land, so the next lookup finds the set cached rather than racing
    /// the background read into a second one. The join runs on tokio's blocking pool, so the
    /// awaiting task's worker stays free meanwhile.
    pub async fn landed(self) {
        if let Some(handle) = self.handle {
            let _ = tokio::task::spawn_blocking(move || handle.join()).await;
        }
    }
}

/// Load the active warehouse's packs from disk and cache them, replacing whatever was cached.
/// Two threads reloading at once may overwrite each other; an older set that wins carries an
/// older stamp, so the next miss sees the folder past it and reloads again.
fn reload_packs() -> Result<Arc<PackSet>, String> {
    let key = file_utils::get_path_objects_root();
    let packs = Arc::new(load_packs_from_disk(&pack_folder(), IndexCheck::Lazy)?);

    registry().lock().expect("the pack registry lock is poisoned")
        .insert(key, Arc::clone(&packs));

    Ok(packs)
}

/// Revalidate `cached` against the pack folder after a lookup in it missed, reloading when the
/// folder has moved on: `Ok(Some(fresh))` when it reloaded (retry the lookup there), `Ok(None)`
/// when `cached` is still current.
///
/// This is what lets a long-lived process (a live server) see packs another process published
/// after it cached its set — an external `compact`, a bundle install, a bulk ingest — without
/// paying a full reload on every miss. A hit never comes here, so it stays syscall-free.
///
/// Two checks, in order:
///
/// 1. **Broken entries.** A quarantined or unenumerable pair is not a function of the folder's
///    names, so each one is checked on its own: a full reload if either of its files changed
///    since it was stamped, or when its [`FailureKind`] says to retry: an `open` failure once both
///    files open, a post-`open` I/O failure once [`IO_FAILURE_RETRY_INTERVAL`] has passed. A
///    fixed-state failure whose files are unchanged costs two `stat`s and is never re-read.
/// 2. **The folder.** A reload if its mtime differs from the stamp. An equal mtime is proof only
///    when the stamp is not racy ([`StampTimes::is_racy`]); a racy one lists the folder and
///    reloads if the pack names differ, otherwise refreshes the stamp's times from values read
///    before that listing.
///
/// Precondition: a local filesystem whose directory mtime moves when an entry is created,
/// renamed or removed, and whose clock does not step backwards past [`MTIME_GRANULARITY`].
fn reloaded_if_stale(cached: &PackSet) -> Result<Option<Arc<PackSet>>, String> {
    let folder = pack_folder();
    if pack_set_is_current(cached, &folder)? {
        return Ok(None);
    }

    // One reload at a time: after an external compact, every concurrent miss on a live server
    // finds the same set stale, and each would otherwise read every index into its own copy. A
    // thread that waited here takes the set the thread before it loaded, if that one is current.
    let key = file_utils::get_path_objects_root();
    let reload_lock = Arc::clone(RELOAD_LOCKS.get_or_init(|| Mutex::new(HashMap::new()))
        .lock().expect("the pack reload locks are poisoned")
        .entry(key.clone()).or_default());
    let _reloading = reload_lock.lock().expect("the pack reload lock is poisoned");

    let newer = registry().lock().expect("the pack registry lock is poisoned")
        .get(&key)
        .filter(|entry| !std::ptr::eq(Arc::as_ptr(entry), cached))
        .map(Arc::clone);

    if let Some(newer) = newer {
        if pack_set_is_current(&newer, &folder)? {
            return Ok(Some(newer));
        }
    }

    reload_packs().map(Some)
}

/// Serializes [`reloaded_if_stale`]'s reloads per warehouse object store (keyed like
/// [`PACK_REGISTRY`]), so one warehouse's reload never holds up another's.
static RELOAD_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

/// The check behind [`reloaded_if_stale`]. A `stat` or listing failure on the folder reads as
/// "not current", so the reload that follows reports it as the environment failure it is.
fn pack_set_is_current(set: &PackSet, folder: &Path) -> Result<bool, String> {
    let broken = set.quarantined.iter()
        .map(|q| (&q.index_path, &q.stamp, q.failure, q.attempted_at, q.index_path.with_extension(PACK_DATA_EXTENSION)))
        .chain(set.unenumerable.iter()
            .map(|u| (&u.index_path, &u.stamp, u.failure, u.attempted_at, u.index_path.clone())));

    for (index_path, stamp, failure, attempted_at, open_probe) in broken {
        if broken_pair_changed(index_path, stamp, failure, attempted_at, &open_probe) {
            return Ok(false);
        }
    }

    let stored = *set.stamp.times.lock().expect("the pack stamp lock is poisoned");

    // Read before the stat, and the stat before the listing (see `FolderStamp`).
    let read_at = SystemTime::now();
    let modified = match std::fs::metadata(folder) {
        Ok(metadata) => Some(metadata.modified().ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Ok(false),
    };

    if modified != stored.modified {
        return Ok(false);
    }

    if !stored.is_racy() {
        return Ok(true);
    }

    let Ok(entries) = std::fs::read_dir(folder) else {
        return Ok(false);
    };

    let mut names = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else {
            return Ok(false);
        };
        if is_pack_file_name(&entry.path()) {
            names.push(entry.file_name());
        }
    }
    names.sort();

    if names != set.stamp.names {
        return Ok(false);
    }

    *set.stamp.times.lock().expect("the pack stamp lock is poisoned") = StampTimes { modified, read_at };
    Ok(true)
}

/// Whether a broken pair may now classify differently, so the set must be reloaded (which
/// re-classifies it with the loader's own classifier): either file changed since it was stamped,
/// its stamp was taken inside a file's mtime tick and that tick has since passed
/// ([`PairStamp::settles_at`]), or its failure kind ([`FailureKind`]) says to retry now — an
/// `open` failure once `open_probe` opens (one `open` per miss, no read), a post-`open` I/O
/// failure once [`IO_FAILURE_RETRY_INTERVAL`] has passed since it was attempted.
///
/// `open_probe` is the file whose `open` decides the pair's classification: the index for an
/// unenumerable pair (its index is what could not be read), the data file for a quarantined one
/// (its index was read; a data file that opens is what would load it).
fn broken_pair_changed(index_path: &Path,
                       stamp: &PairStamp,
                       failure: FailureKind,
                       attempted_at: Instant,
                       open_probe: &Path) -> bool {
    if !pair_stamp(index_path).files_match(stamp) {
        return true;
    }

    if stamp.settles_at.is_some_and(|settles_at| SystemTime::now() > settles_at) {
        return true;
    }

    match failure {
        FailureKind::Fixed => false,
        FailureKind::Io => attempted_at.elapsed() >= IO_FAILURE_RETRY_INTERVAL,
        FailureKind::Open => !matches!(
            std::fs::File::open(open_probe), Err(error) if error.kind() != std::io::ErrorKind::NotFound
        ),
    }
}

/// Whether `hash` is named only by a quarantined (data-missing) pack index — the honest-wording
/// half of FORK-47's `Absent` gate (DESIGN.html §3.1.1): a hash a quarantined index names is not
/// "no surviving index names this hash," so its ordinary read must say so rather than reading a
/// bare, indistinguishable-from-never-existed `Absent`. Membership is a binary search over each
/// quarantined index's resident sorted bytes ([`QuarantinedPack::contains`]) — no per-hash parse.
///
/// Consults [`loaded_packs`] (current in every caller today: this is only ever reached right
/// after a `retrieve_from_packs` miss, which revalidated the set against the pack folder). An
/// unparseable index's hash set is unknown, so it never gates a verdict (I4) — except an index of
/// another format version ([`UnenumerablePack::foreign_version`]), which is known to be a pack
/// index this build cannot read: it gates every hash, so a store written by another build fails
/// its reads by name rather than reading its objects as absent.
///
/// # Returns
/// * `Ok(Some(message))` - `hash` is named by a quarantined index, or a foreign-version index
///   may hold it; `message` is ready to use as the [`file_utils::StoreReadOutcome::Unverifiable`]
///   payload, naming the index file and its error.
/// * `Ok(None)`          - No quarantined index names `hash`, and no foreign-version index exists.
/// * `Err(String)`       - The pack registry itself could not be loaded.
pub(crate) fn quarantine_reason(hash: &str) -> Result<Option<String>, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(None);
    };

    let pack_set = loaded_packs()?;

    for quarantined in pack_set.quarantined.iter() {
        if quarantined.contains(&hash_bytes) {
            return Ok(Some(format!(
                "named by pack index \"{}\" whose data file could not be loaded: {}",
                quarantined.index_path.to_string_lossy(), quarantined.error
            )));
        }
    }

    // A pack index of another format version may hold any hash, so it can rule none out.
    if let Some(foreign) = pack_set.unenumerable.iter().find(|pack| pack.foreign_version) {
        return Ok(Some(format!(
            "possibly held by pack index \"{}\", which this build cannot read: {}",
            foreign.index_path.to_string_lossy(), foreign.error
        )));
    }

    Ok(None)
}

/// Why the liveness walk of a sweep that deletes cannot read a presence miss on `hash` as absent
/// (FORK-193). Asked only after `file_utils::does_object_exist` missed, which revalidated the
/// cached pack set against the pack folder, so this answers from that set with no I/O of its own.
///
/// Stricter than [`quarantine_reason`], which gates the verdict of an ordinary read: on top of a
/// quarantined or foreign-version index, a corrupt bucket of a loaded pack may hold the hash, and
/// so may an index that could not be parsed at all. A read may treat such an index as inert (I4),
/// since the worst a missed copy costs it is an honest `Absent`. A sweep may not: what the hash
/// references would be deleted.
///
/// # Returns
/// * `Ok(Some(reason))` - Some pack index may hold `hash` but cannot serve it; `reason` names it.
/// * `Ok(None)`         - No pack index could hold `hash` unseen.
/// * `Err(String)`      - The pack registry itself could not be loaded.
pub(crate) fn sweep_unreadable_reason(hash: &str) -> Result<Option<String>, String> {
    if let Some(reason) = quarantine_reason(hash)? {
        return Ok(Some(reason));
    }

    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(None);
    };

    let pack_set = loaded_packs()?;

    for pack in pack_set.packs.iter() {
        if let Located::CorruptBucket(bucket) = pack.locate(&hash_bytes) {
            if is_exonerated_by_salvage(&pack.index_path(), &hash_bytes) {
                continue;
            }
            return Ok(Some(pack.corrupt_bucket_error(bucket)));
        }
    }

    // `quarantine_reason` answered for a foreign-version index, so this one is unparseable.
    if let Some(unparsed) = pack_set.unenumerable.first() {
        return Ok(Some(format!(
            "possibly held by pack index \"{}\", which could not be parsed: {}; restore it, or move \
            it out of the pack folder if it holds nothing this warehouse needs",
            unparsed.index_path.to_string_lossy(), unparsed.error
        )));
    }

    Ok(None)
}

/// What the running full repack's salvage ([`salvage_damaged_packs`]) concluded about each damaged
/// pack, keyed by index path. Registered and cleared by [`SalvageRegistration`], which lives only
/// as long as the repack reads objects (target collection and packing), under its locks and
/// before anything is deleted.
static SALVAGE_REGISTRY: Mutex<Vec<(PathBuf, Salvaged)>> = Mutex::new(Vec::new());

/// One damaged pack's salvage verdict (see [`SALVAGE_REGISTRY`]).
#[derive(Clone)]
enum Salvaged {
    /// Every record the pack's data file holds now reads verified from another copy, so a hash
    /// its raw record table (held here) does not name was never in it, whatever bucket it falls
    /// in: [`sweep_unreadable_reason`] does not answer for it.
    Complete(Arc<HashSet<[u8; HASH_LEN]>>),
    /// Some record could not be recovered: an object read through the pack's damaged part may
    /// have no other copy, and [`LoadedPack::corrupt_bucket_error`] says so.
    Incomplete,
}

/// Whether a completely salvaged pack's index at `index_path` is exonerated from holding
/// `hash_bytes` (see [`SALVAGE_REGISTRY`]).
fn is_exonerated_by_salvage(index_path: &Path, hash_bytes: &[u8; HASH_LEN]) -> bool {
    let registry = SALVAGE_REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.iter().any(|(path, salvaged)| {
        path == index_path && matches!(salvaged, Salvaged::Complete(raw_table) if !raw_table.contains(hash_bytes))
    })
}

/// Whether the running full repack could not completely salvage the pack at `index_path` (see
/// [`SALVAGE_REGISTRY`]).
fn is_incompletely_salvaged(index_path: &Path) -> bool {
    let registry = SALVAGE_REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    registry.iter().any(|(path, salvaged)| path == index_path && matches!(salvaged, Salvaged::Incomplete))
}

/// The registration of a full repack's salvage verdicts in [`SALVAGE_REGISTRY`], removed again on
/// drop, so neither the exoneration nor the salvage context outlives the repack's reading phase
/// (and the locks) it was established under.
struct SalvageRegistration {
    index_paths: Vec<PathBuf>,
}

impl SalvageRegistration {
    fn register(packs: Vec<(PathBuf, Salvaged)>) -> SalvageRegistration {
        let index_paths = packs.iter().map(|(path, _)| path.clone()).collect();
        let mut registry = SALVAGE_REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        registry.extend(packs);
        SalvageRegistration { index_paths }
    }
}

impl Drop for SalvageRegistration {
    fn drop(&mut self) {
        let mut registry = SALVAGE_REGISTRY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        for index_path in &self.index_paths {
            if let Some(at) = registry.iter().position(|(path, _)| path == index_path) {
                registry.remove(at);
            }
        }
    }
}

/// [`pack_health_snapshot`]'s result: every quarantined index's `(path, load error)` and every
/// unenumerable index's `(path, parse error)`, as of one uncached load.
pub(crate) struct PackHealthSnapshot {
    pub quarantined: Vec<(PathBuf, String)>,
    pub unenumerable: Vec<(PathBuf, String)>,
    pub damaged: Vec<(PathBuf, String)>,
    pub foreign: Vec<(PathBuf, String)>,
}

/// A point-in-time snapshot of the pack folder's problem lists (FORK-47, DESIGN.html §3.1.1):
/// quarantined, unenumerable and foreign-version indexes as the registry classifies them, plus
/// damaged packs, which only this census load ([`IndexCheck::Full`]) tells apart. Loaded
/// **uncached** — `load_packs_from_disk` directly, exactly the pattern
/// [`store_status`] uses, reading nothing from and writing nothing to the shared registry
/// [`loaded_packs`] caches. A report path (this one, and `store_status`) must never
/// `invalidate_cache()` or otherwise perturb a live registry a concurrent read depends on; the
/// residual staleness window — an index rotting between this call and whatever later reads it —
/// is irreducible (the same window every other snapshot-then-act sequence in this crate has) and
/// advisory-only: it can change a warning line, never a read verdict, since a verdict is decided
/// fresh, per hash, by the read path itself at the moment it runs.
///
/// `forklift heal`'s recovery pass ([`crate::util::recovery_utils::run`]) calls this once at
/// entry so every problem index is warned about on every run, independent of
/// what (if anything) is actually tainted.
///
/// # Returns
/// * `Ok(PackHealthSnapshot)` - The quarantined and unenumerable lists as of this call.
/// * `Err(String)`            - The pack folder itself could not be enumerated (an environment
///                              failure — see [`load_packs_from_disk`]).
pub(crate) fn pack_health_snapshot() -> Result<PackHealthSnapshot, String> {
    let PackSet { quarantined, unenumerable, damaged, .. } = load_packs_from_disk(&pack_folder(), IndexCheck::Full)?;
    let (foreign, unenumerable): (Vec<_>, Vec<_>) = unenumerable.into_iter().partition(|u| u.foreign_version);

    Ok(PackHealthSnapshot {
        quarantined: quarantined.into_iter().map(|q| (q.index_path, q.error)).collect(),
        unenumerable: unenumerable.into_iter().map(|u| (u.index_path, u.error)).collect(),
        foreign: foreign.into_iter().map(|u| (u.index_path, u.error)).collect(),
        damaged: damaged.into_iter().map(|d| (d.index_path, d.error)).collect(),
    })
}

/// Forget the cached packs for the active warehouse, so the next read reloads them from disk.
///
/// Called after `compact`/a bundle install write new packs in this process, so a later read in
/// the same process sees them — and, the same idiom in reverse, called defensively *before*
/// deleting or renaming pack files this process may have already mmap'd. The cached [`PackSet`]
/// holds a `memmap2::Mmap` per pack; the original worry was that an open mmap blocks its own
/// pack file's deletion on Windows. **That specific claim was tested directly on
/// `windows-latest` and did not hold**: `load_pack_pair` opens the pack file with a
/// plain `std::fs::File::open`, and Rust's `std::fs::File` defaults its Windows share mode to
/// include `FILE_SHARE_DELETE`, so a file mapped from that handle stays deletable the whole time
/// it's open — see `an_mmapd_pack_does_not_block_its_own_directory_removal` in this file's own
/// tests for the confirming probe. Calling this before a delete/rename is kept anyway, here and
/// at the pre-existing `compact`/import call sites: it is cheap, and correctness-neutral either
/// way (a caller that drops the cache and then reloads it pays one extra disk read, never a
/// wrong answer), so there's no reason to remove the insurance now that the specific rationale
/// for it has been shown false. Do not re-cite the Windows-deletion-blocking claim as settled;
/// it isn't, on this platform/toolchain — if you're relying on it for a *new* decision, verify
/// it the same way this one was.
///
/// `pub` (not `pub(crate)`) so a caller in the `forklift` binary crate — `franchise`'s cleanup —
/// can call it too; every other caller here is within this crate.
pub fn invalidate_cache() {
    registry().lock().expect("the pack registry lock is poisoned")
        .remove(&file_utils::get_path_objects_root());
    // Bytes reconstructed against the old mapping may be wrong (a record since rewritten); they
    // are only ever a delta's dictionary, so keeping them is never wrong, but it would make every
    // read built on them fail its fast attempt until they age out. Only this warehouse's entries
    // go (the key's objects-root prefix), so a reload here costs another warehouse nothing.
    unverified_base_cache().remove_prefixed(&file_utils::read_cache_key(""));
}

/// Forget everything this process holds about the current warehouse's objects: its pack set
/// and reload lock, and its entries in the verified read cache, the parsed-tree cache and the
/// unverified-base cache. For a server letting a warehouse go: once it is not live another
/// process may delete objects from it (gc, a full repack), and every one of those caches answers
/// a hit without looking at the disk, so a kept entry would report a deleted object as present —
/// to a client deciding what to upload, and to the closure check of a ref update. Only this
/// warehouse's entries go, so the next one served pays nothing for it.
pub fn forget_warehouse() {
    invalidate_cache();
    RELOAD_LOCKS.get_or_init(|| Mutex::new(HashMap::new())).lock()
        .expect("the pack reload lock map is poisoned")
        .remove(&file_utils::get_path_objects_root());
    file_utils::forget_cached_objects();
    object_utils::forget_cached_trees();
}

/// The densify-pending marker's filename, in the pack folder. Presence-only — its content is
/// just a human-readable note, never parsed — set after a bulk ingest (`StoreIngest::finish`,
/// `import_transport_packs`) publishes at least one pack, and cleared after a successful
/// `compact --all --redelta`. Those bulk paths append per-object or per-pack, so their delta
/// chains can only ever see the similarity a per-path/per-pack window offers; a full `--redelta`
/// pass is the one thing that lets them see cross-path similarity too, so the marker is a hint
/// that the win is still on the table, not something anything auto-runs (redelta is a one-shot,
/// CPU-bound minutes-long pass — out of budget for auto-maintenance's cheap-and-rare contract).
const DENSIFY_MARKER_NAME: &str = "densify-pending";

/// Record that this store would benefit from a `compact --all --redelta` pass. Best-effort: a
/// failure here must never fail the ingest that just succeeded, so errors are swallowed — a
/// missed marker only costs a missed suggestion, never correctness.
fn mark_densify_pending(pack_folder: &Path) {
    let _ = std::fs::write(
        pack_folder.join(DENSIFY_MARKER_NAME),
        "This store was bulk-ingested (import-git or a franchise bundle install) and has not \
        been through a full delta-compress pass yet. Run \"forklift compact --all --redelta\" \
        to shrink it further. Safe to delete; it is only a hint.\n",
    );
}

/// Clear the densify-pending marker after a successful `compact --all --redelta` run, so the
/// suggestion does not fire again until another bulk ingest re-sets it — redelta should not be
/// repeated back-to-back on the same store (see the KNOWN GAP note on `retrieve_from_packs`).
/// Best-effort, same reasoning as `mark_densify_pending`.
fn clear_densify_pending(pack_folder: &Path) {
    let _ = std::fs::remove_file(pack_folder.join(DENSIFY_MARKER_NAME));
}

/// Whether the object store carries the densify-pending marker — surfaced by `store` as a
/// suggestion. Never consulted to decide behavior; the marker is presence-only and read here as
/// a plain existence check, not parsed.
pub fn densify_pending() -> Result<bool, String> {
    Ok(pack_folder().join(DENSIFY_MARKER_NAME).exists())
}

/// Load every pack in `pack_folder` (its data file and index mapped). A missing pack folder is
/// simply no packs. `check` decides how much of each index is validated up front: the registry
/// loads [`IndexCheck::Lazy`], a census [`IndexCheck::Full`].
///
/// A single broken pair no longer fails the whole load (FORK-47, DESIGN.html §3.1.1): a
/// `load_pack_pair` failure re-attempts the registry-independent index parse alone (the same
/// read-and-parse shape [`hashes_in_index_file`] uses, without ever touching the data file that
/// just failed). If the index itself parses, the pair is orphaned rather than corrupt — its index
/// bytes and the load's error are kept in `quarantined`, so a
/// read for a hash it names can still say something honest (`quarantine_reason`) instead of a bare
/// `Absent`. If the index does not parse either, it goes to `unenumerable` — its hash set is
/// unknown, so (I4) it gates no verdict — unless it is of another format version, which gates
/// every hash (see [`quarantine_reason`]).
/// Only a `read_dir` failure on the pack folder itself (not `NotFound`) is still a whole-load
/// `Err` — an environment failure, not a per-pack one.
///
/// The returned set carries a [`FolderStamp`] taken before the listing, and each broken entry a
/// [`PairStamp`] taken before its load, which is what [`reloaded_if_stale`] compares against.
fn load_packs_from_disk(pack_folder: &Path, check: IndexCheck) -> Result<PackSet, String> {
    PACK_SET_READ_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    let mut packs = Vec::new();
    let mut quarantined = Vec::new();
    let mut unenumerable = Vec::new();
    let mut damaged = Vec::new();
    let mut names = Vec::new();

    // Stamp first, list second (see `FolderStamp`).
    let read_at = SystemTime::now();
    let modified = folder_modified(pack_folder)?;

    let entries = match std::fs::read_dir(pack_folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let stamp = FolderStamp { names, times: Mutex::new(StampTimes { modified: None, read_at }) };
            return Ok(PackSet { packs, quarantined, unenumerable, damaged, stamp });
        }
        Err(error) => return Err(format!(
            "Error while reading the pack folder \"{}\": {}", pack_folder.to_string_lossy(), error
        )),
    };

    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while listing the pack folder: {}", e))?;
        let path = entry.path();

        if is_pack_file_name(&path) {
            names.push(entry.file_name());
        }

        if path.extension().and_then(|e| e.to_str()) != Some(PACK_INDEX_EXTENSION) {
            continue;
        }

        match classify_pack_pair(&path, check) {
            PairOutcome::Loaded(pack) => packs.push(pack),
            PairOutcome::Quarantined(pack) => quarantined.push(pack),
            PairOutcome::Unenumerable(pack) => unenumerable.push(pack),
            PairOutcome::Damaged(pack) => damaged.push(pack),
        }
    }

    // `read_dir` order is platform-arbitrary (especially on Windows); a fixed, deterministic
    // enumeration order makes which error a multi-pack failure reports reproducible, and is what
    // lets a test pin the "broken copy enumerated first" case deterministically on every platform.
    packs.sort_by(|a, b| a.data_path.file_name().cmp(&b.data_path.file_name()));
    quarantined.sort_by(|a, b| a.index_path.file_name().cmp(&b.index_path.file_name()));
    damaged.sort_by(|a, b| a.index_path.file_name().cmp(&b.index_path.file_name()));
    names.sort();

    let stamp = FolderStamp { names, times: Mutex::new(StampTimes { modified: Some(modified), read_at }) };
    Ok(PackSet { packs, quarantined, unenumerable, damaged, stamp })
}

/// Whether `path` names a pack index or pack data file — the names a [`FolderStamp`] records.
fn is_pack_file_name(path: &Path) -> bool {
    matches!(path.extension().and_then(|e| e.to_str()), Some(PACK_INDEX_EXTENSION) | Some(PACK_DATA_EXTENSION))
}

/// The pack folder's modification time: `Ok(None)` when it exists but the platform reports none,
/// `Ok(Some(..))` otherwise. A missing folder reads as `Ok(None)` too — its `read_dir` will say
/// `NotFound` next, and the caller records the absence from that. Any other `stat` failure is the
/// same environment failure a `read_dir` on the folder would be.
fn folder_modified(pack_folder: &Path) -> Result<Option<SystemTime>, String> {
    match std::fs::metadata(pack_folder) {
        Ok(metadata) => Ok(metadata.modified().ok()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(format!(
            "Error while reading the pack folder \"{}\": {}", pack_folder.to_string_lossy(), error
        )),
    }
}

/// How much of each index a pack load validates up front.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexCheck {
    /// The header and fanout table only; each bucket is validated when a lookup first reads it.
    /// What the pack registry loads, so a command pays only for the buckets it touches.
    Lazy,
    /// Every bucket and the tiling check ([`LoadedPack::validate_full`]): a pack that fails is
    /// damaged ([`DamagedPack`]). What a census loads, so it reports every corrupt index.
    Full,
}

/// What the loader makes of one index entry.
enum PairOutcome {
    Loaded(LoadedPack),
    Quarantined(QuarantinedPack),
    Unenumerable(UnenumerablePack),
    /// Loads, but fails [`LoadedPack::validate_full`]; produced only under [`IndexCheck::Full`].
    Damaged(DamagedPack),
}

/// Classify one index entry exactly as the loader does — the single classifier both
/// [`load_packs_from_disk`] and [`reloaded_if_stale`]'s retries rest on, so the two cannot
/// disagree about what a pair is. A `load_pack_pair` failure re-attempts the registry-independent
/// index parse alone (the same read-and-parse shape [`hashes_in_index_file`] uses, without
/// touching the data file that just failed): an index that parses is quarantined, one that does
/// not is unenumerable. The two checks differ in one outcome only: under [`IndexCheck::Full`] a
/// pair that loads but fails [`LoadedPack::validate_full`] is damaged, where the registry's
/// [`IndexCheck::Lazy`] loads it.
///
/// The pair is stamped before it is opened, so a change that lands during the load moves its
/// files past the stamp.
fn classify_pack_pair(index_path: &Path, check: IndexCheck) -> PairOutcome {
    let stamp = pair_stamp(index_path);
    let attempted_at = Instant::now();
    let data_path = index_path.with_extension(PACK_DATA_EXTENSION);

    let pair_failure = match load_pack_pair_classified(&data_path, index_path) {
        Ok(pack) => match check {
            IndexCheck::Lazy => return PairOutcome::Loaded(pack),
            IndexCheck::Full => return match pack.validate_full() {
                Ok(()) => PairOutcome::Loaded(pack),
                Err(error) => PairOutcome::Damaged(DamagedPack { index_path: index_path.to_path_buf(), error }),
            },
        },
        Err(failure) => failure,
    };

    match read_pack_file(index_path) {
        Ok(index) => match index_format::Layout::parse(&index, index_path) {
            Ok(layout) => PairOutcome::Quarantined(QuarantinedPack {
                index_path: index_path.to_path_buf(),
                index: CheckedIndex::new(index, layout, None),
                error: pair_failure.message,
                stamp,
                failure: pair_failure.kind,
                attempted_at,
            }),
            Err(parse_error) => PairOutcome::Unenumerable(UnenumerablePack {
                index_path: index_path.to_path_buf(),
                error: parse_error,
                foreign_version: index_format::foreign_version(&index).is_some(),
                stamp,
                failure: FailureKind::Fixed,
                attempted_at,
            }),
        },
        Err(read_failure) => PairOutcome::Unenumerable(UnenumerablePack {
            index_path: index_path.to_path_buf(),
            error: format!("Error while reading pack index \"{}\": {}", index_path.to_string_lossy(), read_failure.error),
            foreign_version: false,
            stamp,
            failure: read_failure.kind,
            attempted_at,
        }),
    }
}

/// Why a pack file could not be read, and its [`FailureKind`].
struct PackReadFailure {
    error: std::io::Error,
    kind: FailureKind,
}

/// Open a pack file, classifying a failure: not-found is [`FailureKind::Fixed`], anything else
/// [`FailureKind::Open`].
fn open_pack_file(path: &Path) -> Result<std::fs::File, PackReadFailure> {
    std::fs::File::open(path).map_err(|error| {
        let kind = if error.kind() == std::io::ErrorKind::NotFound { FailureKind::Fixed } else { FailureKind::Open };
        PackReadFailure { error, kind }
    })
}

/// Read a whole pack file, as `std::fs::read` does, but with its failure classified: a read error
/// after the `open` succeeded is [`FailureKind::Io`].
fn read_pack_file(path: &Path) -> Result<Vec<u8>, PackReadFailure> {
    let mut file = open_pack_file(path)?;
    let mut bytes = Vec::with_capacity(file.metadata().map(|m| m.len() as usize).unwrap_or(0));
    file.read_to_end(&mut bytes).map_err(|error| PackReadFailure { error, kind: FailureKind::Io })?;
    Ok(bytes)
}

/// Load one native pack/index pair and validate the structural contract shared by ordinary local
/// packs and transport-installed packs. Content hashes are checked separately: ordinary reads do
/// it on demand, while a transport import checks the whole quarantined set before publication.
fn load_pack_pair(data_path: &Path, index_path: &Path) -> Result<LoadedPack, String> {
    load_pack_pair_classified(data_path, index_path).map_err(|failure| failure.message)
}

/// A [`load_pack_pair`] failure: its message and [`FailureKind`].
struct PairFailure {
    message: String,
    kind: FailureKind,
}

impl PairFailure {
    fn fixed(message: String) -> PairFailure {
        PairFailure { message, kind: FailureKind::Fixed }
    }
}

/// [`load_pack_pair`], keeping its failure's [`FailureKind`]. Checks the index's header and fanout
/// table and the data file's header; no record is read (see [`IndexCheck`]).
fn load_pack_pair_classified(data_path: &Path, index_path: &Path) -> Result<LoadedPack, PairFailure> {
    let index_file = open_pack_file(index_path).map_err(|failure| PairFailure {
        message: format!("Error while reading pack index \"{}\": {}", index_path.to_string_lossy(), failure.error),
        kind: failure.kind,
    })?;
    let index_len = index_file.metadata().map_err(|e| PairFailure {
        message: format!("Error while reading pack index \"{}\": {}", index_path.to_string_lossy(), e),
        kind: FailureKind::Io,
    })?.len();
    // Too short to name its version: refuse it here, since mapping an empty file fails on some
    // platforms with an error that would read as an I/O fault rather than a corrupt index.
    if index_len < index_format::IDENTITY_LEN as u64 {
        return Err(PairFailure::fixed(format!(
            "Pack index \"{}\" is corrupt or has an unknown format.", index_path.to_string_lossy()
        )));
    }
    // SAFETY: as for the data file below: a published index is write-once, and a transport
    // staging index is closed and never modified again before this mapping is dropped.
    let index = unsafe { memmap2::Mmap::map(&index_file) }
        .map_err(|e| PairFailure {
            message: format!("Error while mapping pack index \"{}\": {}", index_path.to_string_lossy(), e),
            kind: FailureKind::Io,
        })?;
    let layout = index_format::Layout::parse(&index, index_path).map_err(PairFailure::fixed)?;

    let file = open_pack_file(data_path).map_err(|failure| PairFailure {
        message: format!("Error while opening pack data \"{}\": {}", data_path.to_string_lossy(), failure.error),
        kind: failure.kind,
    })?;
    // SAFETY: callers only pass immutable pack data: a published pack is write-once, and a
    // transport staging file is closed and never modified again before this mapping is dropped.
    let data = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| PairFailure {
            message: format!("Error while mapping pack data \"{}\": {}", data_path.to_string_lossy(), e),
            kind: FailureKind::Io,
        })?;

    if data.len() < PACK_DATA_HEADER_LEN as usize || &data[0..8] != PACK_DATA_MAGIC {
        return Err(PairFailure::fixed(format!(
            "Pack data \"{}\" is corrupt or has an unknown format.", data_path.to_string_lossy()
        )));
    }

    let data_version = read_u32_le(&data, 8);
    if data_version != PACK_FORMAT_VERSION {
        return Err(PairFailure::fixed(format!(
            "Pack data \"{}\" has format version {}, but this build reads only version {}.",
            data_path.to_string_lossy(), data_version, PACK_FORMAT_VERSION
        )));
    }

    let index = CheckedIndex::new(index, layout, Some(data.len() as u64));
    Ok(LoadedPack { data_path: data_path.to_path_buf(), data, index, full: OnceLock::new() })
}

/// The bits of a start offset one radix pass of [`sort_ranges_by_start`] consumes.
const RANGE_RADIX_BITS: u32 = 11;

/// Sort `(start, end)` ranges by `start`, as `sort_unstable_by_key` would, with an LSD radix sort.
///
/// Every process pays this once per pack it opens, over every record of a hash-ordered index —
/// ~400k on git.git, where the comparison sort was 5.1 of the 5.8 ms the whole validation took
/// (FORK-183). The keys are byte offsets bounded by the pack's size, so a few 11-bit passes cover
/// them: three for any pack under 8 GiB.
fn sort_ranges_by_start(ranges: &mut Vec<(u64, u64)>) {
    // Below this the counting arrays cost more than the comparison sort they replace.
    if ranges.len() < 1024 {
        ranges.sort_unstable_by_key(|(start, _)| *start);
        return;
    }

    let max_start = ranges.iter().map(|(start, _)| *start).max().unwrap_or(0);
    let key_bits = u64::BITS - max_start.leading_zeros();
    let mask = (1u64 << RANGE_RADIX_BITS) - 1;
    let mut scratch = vec![(0u64, 0u64); ranges.len()];
    let mut shift = 0;

    while shift < key_bits {
        let mut counts = [0usize; 1 << RANGE_RADIX_BITS];
        for (start, _) in ranges.iter() {
            counts[((start >> shift) & mask) as usize] += 1;
        }

        let mut position = 0;
        for count in counts.iter_mut() {
            let bucket = *count;
            *count = position;
            position += bucket;
        }

        // Each pass is stable, so the order the earlier (lower-digit) passes established survives
        // every tie on this digit.
        for range in ranges.iter() {
            let digit = ((range.0 >> shift) & mask) as usize;
            scratch[counts[digit]] = *range;
            counts[digit] += 1;
        }

        std::mem::swap(ranges, &mut scratch);
        shift += RANGE_RADIX_BITS;
    }
}

/// What installing the native-pack payload of a transport bundle added to this object store.
pub(crate) struct TransportImportStats {
    pub stored_objects: usize,
    pub skipped_objects: usize,

    /// The parcels the packs carry (hex), whether stored or already present: the only hashes the
    /// bundle's signature section may sign.
    pub parcels: HashSet<String>,
}

/// A quarantined native pack/index pair extracted from a transport bundle. Both paths live in the
/// real pack directory but retain a `.tmp` suffix, so normal readers cannot discover them before
/// verification and the index-last publication sequence below.
struct StagedTransportPack {
    data_path: PathBuf,
    index_path: PathBuf,
}

/// Import consecutive native pack/index sections from `reader`. Every section is copied to an
/// ignored temp file, the complete incoming set is structurally and content-hash verified, and
/// only then are its pack pairs made visible. A failure before publication removes every temp;
/// a crash during publication leaves at worst a valid subset, which the ordinary franchise
/// history walk heals from loose/batch fetches.
///
/// The exists/rename publication sequence assumes no *concurrent* importer of the same store:
/// `franchise` targets a directory it just created, and every other pack writer runs under the
/// store lock (`compact`) or warehouse lock.
///
/// # Returns
/// * `Ok(TransportImportStats)` - Every renamed pack pair's directory entries are durable and,
///                                once [`taint_utils::activate`](crate::util::taint_utils::activate)
///                                has been called in this process, no durability taint was found
///                                standing for this root on the re-check that runs right before
///                                this returns (see `file_utils::sync_dir_or_taint`). An
///                                unactivated process skips that re-check entirely.
/// * `Err(String)`                - A section, a pack, a sync, or the trailing directory sync
///                                failed. On the trailing directory-sync failure specifically,
///                                every pack pair actually renamed *this run* (never a pair kept
///                                because an identical layout was already published) is visible
///                                but its directory entries are not proven durable — once
///                                activated, that is recorded as a taint over exactly those paths.
///                                A later `heal_utils::heal_if_tainted` call restages exactly
///                                those paths and clears the taint once they are durable again.
pub(crate) fn import_transport_packs<R: Read>(reader: &mut R,
                                               sections: &[(u64, u64)])
                                               -> Result<TransportImportStats, String> {
    if sections.len() > MAX_TRANSPORT_PACKS {
        return Err(format!(
            "The bundle declares {} native packs, above the {}-pack limit.",
            sections.len(), MAX_TRANSPORT_PACKS
        ));
    }

    let folder = pack_folder();
    file_utils::create_folder_if_not_exists(&folder)?;
    remove_stale_temp_files(&folder);
    let mut staged: Vec<StagedTransportPack> = Vec::with_capacity(sections.len());

    let result = (|| -> Result<TransportImportStats, String> {
        for (data_len, index_len) in sections {
            if *data_len > MAX_TRANSPORT_PACK_BYTES {
                return Err(format!(
                    "A bundle pack section declares {} bytes, above the {}-byte limit.",
                    data_len, MAX_TRANSPORT_PACK_BYTES
                ));
            }
            if *index_len > MAX_TRANSPORT_INDEX_BYTES {
                return Err(format!(
                    "A bundle pack index declares {} bytes, above the {}-byte limit.",
                    index_len, MAX_TRANSPORT_INDEX_BYTES
                ));
            }

            let data_path = temp_path(&folder, PACK_DATA_EXTENSION);
            let index_path = temp_path(&folder, PACK_INDEX_EXTENSION);
            staged.push(StagedTransportPack { data_path, index_path });
            let pair = staged.last().expect("the transport staging pair was just pushed");
            copy_exact_to_file(reader, *data_len, &pair.data_path, "pack data")?;
            copy_exact_to_file(reader, *index_len, &pair.index_path, "pack index")?;
        }

        let incoming: Vec<LoadedPack> = staged.iter()
            .map(|pair| load_pack_pair(&pair.data_path, &pair.index_path))
            .collect::<Result<_, _>>()?;
        // Every byte of an incoming pack is accounted for before any of it is trusted: each index
        // is validated in full, not bucket by bucket as a local read would.
        for pack in &incoming {
            pack.validate_full()?;
        }

        let (hashes, parcels) = verify_incoming_packs(&incoming)?;
        let mut stored_objects = 0;
        let mut skipped_objects = 0;
        for hash in &hashes {
            if file_utils::does_object_exist(&sign_utils::to_hex(hash))? {
                skipped_objects += 1;
            } else {
                stored_objects += 1;
            }
        }

        // Each pack's published name, from the index just validated.
        let pack_ids: Vec<String> = incoming.iter().map(pack_id_of).collect::<Result<_, _>>()?;

        // Drop every mmap before renaming the staged files (Windows refuses to rename a mapped
        // file; POSIX permits it, but the portable contract is cheap to preserve).
        drop(incoming);

        if stored_objects == 0 {
            return Ok(TransportImportStats { stored_objects, skipped_objects, parcels });
        }

        // All pack bytes and indexes are valid. Harden the small number of aggregate files, then
        // publish each data file before its index (the index is the reader-visible commit point).
        for pair in &staged {
            sync_file(&pair.data_path, "bundle pack data")?;
            sync_file(&pair.index_path, "bundle pack index")?;
        }

        // Every final path actually renamed this run — never the `(true, true)` case's paths,
        // which are kept exactly as they already stood and never renamed — so the trailing sync
        // below taints exactly what it just made visible-but-unproven, not the whole install.
        let mut published_this_run: Vec<PathBuf> = Vec::new();

        for (pair, pack_id) in staged.iter().zip(&pack_ids) {
            let data_final = folder.join(format!("{}.{}", pack_id, PACK_DATA_EXTENSION));
            let index_final = folder.join(format!("{}.{}", pack_id, PACK_INDEX_EXTENSION));

            match (data_final.exists(), index_final.exists()) {
                (true, true) => {
                    // An identical layout is already published. Its content-addressed reads were
                    // previously validated; keep it and discard this redundant staging pair.
                }
                (true, false) => {
                    // A prior crash may have published data but not its index. It is invisible;
                    // replace it with the just-verified bytes before publishing the index.
                    std::fs::remove_file(&data_final)
                        .map_err(|e| format!("Error while replacing incomplete pack data: {}", e))?;
                    std::fs::rename(&pair.data_path, &data_final)
                        .map_err(|e| format!("Error while publishing bundle pack data: {}", e))?;
                    std::fs::rename(&pair.index_path, &index_final)
                        .map_err(|e| format!("Error while publishing bundle pack index: {}", e))?;
                    published_this_run.push(data_final.clone());
                    published_this_run.push(index_final.clone());
                }
                (false, true) => {
                    // An index without data is reader-visible corruption. Remove the stale commit
                    // point first, then publish the verified pair data-first/index-last.
                    std::fs::remove_file(&index_final)
                        .map_err(|e| format!("Error while replacing incomplete pack index: {}", e))?;
                    std::fs::rename(&pair.data_path, &data_final)
                        .map_err(|e| format!("Error while publishing bundle pack data: {}", e))?;
                    std::fs::rename(&pair.index_path, &index_final)
                        .map_err(|e| format!("Error while publishing bundle pack index: {}", e))?;
                    published_this_run.push(data_final.clone());
                    published_this_run.push(index_final.clone());
                }
                (false, false) => {
                    std::fs::rename(&pair.data_path, &data_final)
                        .map_err(|e| format!("Error while publishing bundle pack data: {}", e))?;
                    std::fs::rename(&pair.index_path, &index_final)
                        .map_err(|e| format!("Error while publishing bundle pack index: {}", e))?;
                    published_this_run.push(data_final.clone());
                    published_this_run.push(index_final.clone());
                }
            }
        }

        // A sync failure taints exactly `published_this_run`; a sync success is re-checked
        // against any taint already standing for this root before this function may report `Ok`
        // — see `file_utils::sync_dir_or_taint`.
        let published_refs: Vec<&Path> = published_this_run.iter().map(PathBuf::as_path).collect();
        file_utils::sync_dir_or_taint(&folder, &published_refs)?;
        invalidate_cache();
        // A native bundle installs whole packs verbatim from the far end's own bulk ingest —
        // the same undensified shape `--redelta` exists to fix.
        mark_densify_pending(&folder);

        Ok(TransportImportStats { stored_objects, skipped_objects, parcels })
    })();

    // Published paths were renamed away and return NotFound here; failed/unneeded staging files
    // are removed. Cleanup errors never mask the import's real result.
    for pair in staged {
        let _ = std::fs::remove_file(pair.data_path);
        let _ = std::fs::remove_file(pair.index_path);
    }

    result
}

/// Copy exactly `length` bytes into a fresh staging file without trusting the section length for
/// allocation. The bundle itself is already on disk, but this remains streaming and memory-bounded.
fn copy_exact_to_file(reader: &mut impl Read,
                      length: u64,
                      path: &Path,
                      what: &str) -> Result<(), String> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("Error while creating bundle {} staging file: {}", what, e))?;
    let copied = std::io::copy(&mut reader.take(length), &mut file)
        .map_err(|e| format!("Error while reading bundle {}: {}", what, e))?;

    if copied != length {
        return Err(format!(
            "The bundle is truncated: its {} declared {} bytes but only {} remained.",
            what, length, copied
        ));
    }

    file.flush().map_err(|e| format!("Error while flushing bundle {}: {}", what, e))
}

fn sync_file(path: &Path, what: &str) -> Result<(), String> {
    if !file_utils::fsync_enabled() {
        return Ok(());
    }
    // A *write* handle, not a read-only one: Windows refuses FlushFileBuffers on a handle
    // without write access (POSIX fsyncs any descriptor).
    std::fs::OpenOptions::new().write(true).open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("Error while syncing {}: {}", what, e))
}

/// Derive the native pack filename from the index's sorted `(hash, offset, length)` records — the
/// same layout identity `PackWriter::finalize` used when the server built it.
fn pack_id_of(pack: &LoadedPack) -> Result<String, String> {
    let records: Vec<([u8; HASH_LEN], u64, u64)> = pack.records()?.iter()
        .map(|record| {
            let mut hash = [0u8; HASH_LEN];
            hash.copy_from_slice(record.hash);
            (hash, record.offset, record.length)
        })
        .collect();

    Ok(compute_pack_id(&records))
}

/// Verify every object reachable through the incoming indexes before any pack is published.
/// Returns the distinct object hashes for import statistics, and the parcels among them, read off
/// each object's header as it is verified, at no extra decode. Bookkeeping keeps hashes in their
/// raw 32-byte index form — hex `String`s would multiply the per-object cost several times over,
/// and a large clone carries millions of objects; only the parcels, a small share of them, are
/// kept in hex, the form the signature section names them in. The cache is deliberately bounded: it
/// accelerates delta chains, but a hostile pack cannot make verification retain the whole
/// decompressed store in RAM.
fn verify_incoming_packs(packs: &[LoadedPack])
                         -> Result<(Vec<[u8; HASH_LEN]>, HashSet<String>), String> {
    let locator = IncomingLocator::build(packs)?;

    let mut hashes = Vec::with_capacity(locator.entries.len());
    let mut parcels = HashSet::new();
    let mut cache = IncomingVerificationCache::default();
    let mut visiting = HashSet::new();

    for entry in &locator.entries {
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(locator.hash_of(*entry));
        let bytes = resolve_incoming_object(&hash, &locator, &mut cache, &mut visiting, 0)?;
        if matches!(object_utils::peek_object_header(&bytes), Ok((ObjectType::Parcel, _))) {
            parcels.insert(sign_utils::to_hex(&hash));
        }
        hashes.push(hash);
    }

    Ok((hashes, parcels))
}

/// A sorted locator over every record in the quarantined incoming set, so each of the
/// per-object (and per-delta-base) lookups during verification is one binary search across the
/// whole set instead of one per pack — O(objects × log objects) total rather than
/// O(objects × packs × log). Entries are indirect `(pack, record)` positions (8 bytes per
/// object) comparing against the packs' mapped index bytes; a hash-keyed map would cost an
/// order of magnitude more transient memory on a large clone.
struct IncomingLocator<'a> {
    packs: &'a [LoadedPack],
    /// Each pack's records, validated in full.
    records: Vec<index_format::AllRecords<'a, memmap2::Mmap>>,
    /// `(pack position, record position)`, sorted by the record's hash bytes.
    entries: Vec<(u32, u32)>,
}

impl<'a> IncomingLocator<'a> {
    /// Build the locator. Sorting also surfaces cross-pack duplicates (adjacent equal hashes),
    /// which are refused: the bundle builder emits every object exactly once.
    fn build(packs: &'a [LoadedPack]) -> Result<IncomingLocator<'a>, String> {
        let records: Vec<_> = packs.iter().map(LoadedPack::records).collect::<Result<_, _>>()?;
        let total = records.iter().map(|records| records.len()).sum();
        let mut entries: Vec<(u32, u32)> = Vec::with_capacity(total);

        for (pack_position, records) in records.iter().enumerate() {
            for record_position in 0..records.len() {
                entries.push((pack_position as u32, record_position as u32));
            }
        }

        let hash_of = |entry: &(u32, u32)| -> &[u8] { records[entry.0 as usize].get(entry.1 as usize).hash };
        entries.sort_unstable_by(|a, b| hash_of(a).cmp(hash_of(b)));

        for pair in entries.windows(2) {
            if hash_of(&pair[0]) == hash_of(&pair[1]) {
                return Err(format!(
                    "The bundle's native packs contain duplicate object {}.",
                    sign_utils::to_hex(hash_of(&pair[0]))
                ));
            }
        }

        Ok(IncomingLocator { packs, records, entries })
    }

    fn hash_of(&self, entry: (u32, u32)) -> &'a [u8] {
        self.records[entry.0 as usize].get(entry.1 as usize).hash
    }

    /// Locate a hash in the incoming set: its pack and the record's `(offset, length)` there.
    fn locate(&self, hash: &[u8; HASH_LEN]) -> Option<(&'a LoadedPack, u64, u64)> {
        let position = self.entries
            .binary_search_by(|entry| self.hash_of(*entry).cmp(hash))
            .ok()?;
        let (pack_position, record_position) = self.entries[position];
        let record = self.records[pack_position as usize].get(record_position as usize);

        Some((&self.packs[pack_position as usize], record.offset, record.length))
    }

    fn contains(&self, hash: &[u8; HASH_LEN]) -> bool {
        self.locate(hash).is_some()
    }
}

const INCOMING_VERIFY_CACHE_BYTES: usize = 128 * 1024 * 1024;

#[derive(Default)]
struct IncomingVerificationCache {
    objects: HashMap<[u8; HASH_LEN], Arc<Vec<u8>>>,
    bytes: usize,
}

impl IncomingVerificationCache {
    fn get(&self, hash: &[u8; HASH_LEN]) -> Option<Arc<Vec<u8>>> {
        self.objects.get(hash).cloned()
    }

    fn insert(&mut self, hash: [u8; HASH_LEN], bytes: Arc<Vec<u8>>) {
        if bytes.len() > INCOMING_VERIFY_CACHE_BYTES {
            return;
        }
        if self.bytes.saturating_add(bytes.len()) > INCOMING_VERIFY_CACHE_BYTES {
            self.objects.clear();
            self.bytes = 0;
        }
        self.bytes += bytes.len();
        self.objects.insert(hash, bytes);
    }
}

/// Resolve one incoming native-pack object against the quarantined pack set (falling back to an
/// already-present local base for incremental compatibility), then enforce content addressing and
/// import ceilings. Delta recursion is cycle-checked and hard-bounded independently of the writer.
fn resolve_incoming_object(hash: &[u8; HASH_LEN],
                           locator: &IncomingLocator<'_>,
                           cache: &mut IncomingVerificationCache,
                           visiting: &mut HashSet<[u8; HASH_LEN]>,
                           depth: u32) -> Result<Arc<Vec<u8>>, String> {
    if let Some(bytes) = cache.get(hash) {
        return Ok(bytes);
    }

    // One transient hex form for the store API and error messages; bookkeeping stays binary.
    let hex = sign_utils::to_hex(hash);

    if depth > MAX_DELTA_CHAIN {
        return Err(format!(
            "Packed delta {} exceeds the reconstruction depth limit (corrupt bundle?).", hex
        ));
    }
    if !visiting.insert(*hash) {
        return Err(format!("The bundle's native packs contain a delta cycle at {}.", hex));
    }

    let resolved = (|| -> Result<Vec<u8>, String> {
        let Some((pack, offset, length)) = locator.locate(hash) else {
            return file_utils::retrieve_object_by_hash(&hex);
        };
        let record = pack.slice(offset, length)?;

        let (&kind, body) = record.split_first()
            .ok_or_else(|| format!("Packed object {} has an empty record.", hex))?;

        match kind {
            RECORD_FULL => decode_full_transport_record(body, &hex),
            RECORD_DELTA => {
                if body.len() < HASH_LEN {
                    return Err(format!("Packed delta {} is truncated (no base hash).", hex));
                }

                let mut base_hash = [0u8; HASH_LEN];
                base_hash.copy_from_slice(&body[..HASH_LEN]);
                let (target_len, read) = byte_utils::number_from_vlq_bytes(HASH_LEN, body)?;
                let target_len = usize::try_from(target_len)
                    .map_err(|_| format!("Packed delta {} declares an unrepresentable length.", hex))?;
                let payload_start = HASH_LEN.checked_add(read)
                    .filter(|start| *start <= body.len())
                    .ok_or_else(|| format!("Packed delta {} is truncated.", hex))?;

                let base = if locator.contains(&base_hash) {
                    resolve_incoming_object(&base_hash, locator, cache, visiting, depth + 1)?
                } else {
                    file_utils::retrieve_object_by_hash_shared(&sign_utils::to_hex(&base_hash))?
                };

                delta_utils::decompress_delta(&base, &body[payload_start..], target_len)
            }
            other => Err(format!("Packed object {} has an unknown record kind {}.", hex, other)),
        }
    })();

    visiting.remove(hash);
    let bytes = resolved?;
    object_utils::validate_imported_object(&hex, &bytes)?;
    let bytes = Arc::new(bytes);
    cache.insert(*hash, Arc::clone(&bytes));
    Ok(bytes)
}

/// Bounded full-record decompression for untrusted transport packs. Local packs predate this
/// transport role and use the ordinary read path; a new bundle writer never emits a full object
/// above the import ceiling, so producing one byte beyond it is an immediate refusal.
fn decode_full_transport_record(record: &[u8], hash: &str) -> Result<Vec<u8>, String> {
    object_utils::decode_object_bounded(record).map_err(|e| match e {
        object_utils::BoundedDecodeError::Malformed(msg) => {
            format!("Error while decompressing packed object {}: {}", hash, msg)
        }
        object_utils::BoundedDecodeError::OverCeiling => format!(
            "Packed object {} expands above the {}-byte object ceiling; refusing the bundle.",
            hash, object_utils::MAX_OBJECT_BYTES
        ),
    })
}

/// The outcome of a pack lookup for one hash — kept distinct from a bare `Option`/`Err` because a
/// pack *naming* a hash but failing to serve it must never silently collapse into "not here": the
/// classified read (`file_utils::read_object_classified`) needs "no pack has heard of this hash"
/// (`NotLocated`, which still permits the loose fallback / an eventual `Absent`) told apart from
/// "some pack has it but the record could not be resolved" (`Failed`, which can only ever
/// escalate to `Unverifiable` — it must never quietly decay to `Absent`, and it must never win
/// over a verifiable copy elsewhere).
///
/// `Err(String)` is reserved separately for "the registry itself could not be loaded" — an
/// environment failure that says nothing about this particular hash, never returned for a
/// per-record resolve failure.
#[derive(Debug)]
pub(crate) enum PackRetrieval {
    /// A located record resolved and content-verified.
    Verified(Vec<u8>),
    /// At least one loaded pack located the hash, but every located record failed to resolve.
    /// Carries the *first* such error — later, redundant failures for the same hash add no
    /// information once one is on record.
    Failed(String),
    /// No loaded pack's index names the hash.
    NotLocated,
}

/// Retrieve the decompressed, verified bytes of an object from the packs.
/// `file_utils::read_object_uncached`/`read_object_classified` consult this *first* — packs are
/// the common case at scale, not a fallback for when the loose file is absent.
///
/// Tries *every* loaded pack that locates the hash, not just the first: a located record that
/// fails to resolve (a corrupt full record, a bad delta, a delta rebuilt against the wrong base —
/// `resolve_record` hash-verifies, so "resolved" is exactly "verified") no longer suppresses a
/// good copy of the same hash sitting in a different pack. Only once every locating pack has
/// failed does this report `Failed`.
///
/// # Arguments
/// * `hash` - The hex hash of the object.
///
/// # Returns
/// * `Ok(PackRetrieval)` - See [`PackRetrieval`].
/// * `Err(String)`       - The pack registry itself could not be loaded.
pub(crate) fn retrieve_from_packs(hash: &str) -> Result<PackRetrieval, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(PackRetrieval::NotLocated);
    };

    let packs = loaded_packs()?;

    match retrieve_from(&packs, &hash_bytes, hash)? {
        PackRetrieval::NotLocated => match reloaded_if_stale(&packs)? {
            Some(fresh) => retrieve_from(&fresh, &hash_bytes, hash),
            None => Ok(PackRetrieval::NotLocated),
        },
        located => Ok(located),
    }
}

/// [`retrieve_from_packs`] against one already-loaded set, with no revalidation.
fn retrieve_from(packs: &PackSet, hash_bytes: &[u8; HASH_LEN], hash: &str) -> Result<PackRetrieval, String> {
    let mut first_error: Option<String> = None;

    for pack in packs.packs.iter() {
        let (offset, length) = match pack.locate(hash_bytes) {
            Located::Found(offset, length) => (offset, length),
            Located::Missing => continue,
            // This pack cannot say whether it holds the object. Another pack may still serve it
            // (a good copy wins); if none does, the read fails rather than reporting it absent.
            Located::CorruptBucket(bucket) => {
                if first_error.is_none() {
                    first_error = Some(pack.corrupt_bucket_error(bucket));
                }
                continue;
            }
        };

        // `offset`/`length` came from a bucket of this pack's index that passed validation
        // (`index_format`), which bounds every record in it inside `[PACK_DATA_HEADER_LEN,
        // data.len()]` — the same bound `slice` rechecks. So `slice` cannot fail on a `Found`
        // record: this `?` is a defensive anomaly channel (environment `Err`), never the
        // located-but-broken path the `first_error` accumulation exists for.
        let record = pack.slice(offset, length)?;

        match resolve_record(record, hash) {
            Ok(bytes) => return Ok(PackRetrieval::Verified(bytes)),
            Err(e) => {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
    }

    Ok(match first_error {
        Some(e) => PackRetrieval::Failed(e),
        None => PackRetrieval::NotLocated,
    })
}

/// Retrieve an object from packs after forcibly reloading this warehouse's pack registry from
/// disk.
///
/// A miss no longer needs this: [`retrieve_from_packs`] revalidates the cached set against the
/// pack folder on every miss ([`reloaded_if_stale`]). What the folder's names cannot vouch for is
/// a pack's *data* bytes — a same-name rewrite can replace a record — so the one caller left is
/// the read path's `Failed` branch: a located record that failed to resolve, which the loose
/// store also could not serve, gets one forced reload so a good copy written since is not
/// shadowed by the stale mapping. A persistently broken record therefore reloads at most once per
/// read.
pub(crate) fn retrieve_from_packs_reloading(hash: &str) -> Result<PackRetrieval, String> {
    #[cfg(test)]
    FORCED_RELOADS.with(|reloads| reloads.set(reloads.get() + 1));

    invalidate_cache();
    retrieve_from_packs(hash)
}

#[cfg(test)]
thread_local! {
    /// How many forced registry reloads ([`retrieve_from_packs_reloading`]) ran on this thread.
    static FORCED_RELOADS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A hard ceiling on how deep a delta chain may be followed when reconstructing an object.
///
/// This is a backstop only, not the mechanism that keeps chains short: `compact`'s write-time
/// depth ledger (`true_depth`, consulted by its depth-safety pre-pass) is what enforces the real
/// invariant — **every delta this run *encodes* is depth-checked before it is written**, by
/// resolving every candidate path base's *actual* current depth (from record headers, no
/// decompression) before committing to it, rather than trusting `compute_path_bases`'s own
/// bookkeeping (which only counts path hops since a reset, and is deliberately silent about a
/// reset point's own real depth — see its doc comment) at face value. A candidate that would
/// push true depth past the cap is demoted to a full record instead. A verbatim-copied delta
/// (`Source::CopyRecord`) is not encoded and is not covered by this check — it inherits its
/// base's carried depth unchecked, so a copied delta whose base gets re-encoded elsewhere in
/// the same run can be written at an unverified true depth (tracked as FORK-59).
///
/// So a well-formed pack's chains never approach this ceiling; it exists purely against a
/// corrupt or adversarial pack that chains without end, turning unbounded recursion (a crash)
/// into a clean error, never a lost or wrong object (caught before anything is written or
/// deleted). `true_depth`'s own walk is bounded by this same constant for the same reason.
///
/// It bounds one reconstruction's recursion, not a chain's length: a base already in the read
/// cache ends the recursion there, so whether an object deeper than this reads back depends on
/// what was read before it.
const MAX_RECONSTRUCT_DEPTH: u32 = 1000;

thread_local! {
    /// The current delta-reconstruction recursion depth on this thread (see [`resolve_record`]).
    static RECONSTRUCT_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Decrements the reconstruction depth when it drops, so the count is restored on every path
/// out of a delta reconstruction — the normal return and every early error alike.
struct DepthGuard;

impl Drop for DepthGuard {
    fn drop(&mut self) {
        RECONSTRUCT_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
    }
}

/// Whether this thread is inside a delta reconstruction — reading a delta's *base*, not an object
/// a caller asked for. The read path's forced registry reload on a located-but-broken record
/// (`file_utils::read_object_classified`) is skipped there: the outermost read reloads once and
/// re-walks the whole chain, whereas a reload at every level of the chain doubles the work per
/// level (2^depth reloads behind one corrupt chain root).
pub(crate) fn in_reconstruction() -> bool {
    RECONSTRUCT_DEPTH.with(|d| d.get()) > 0
}

/// How [`reconstruct_record`] fetches a delta's base.
#[derive(Clone, Copy)]
enum Bases {
    /// Through the ordinary verified object read — every base is content-verified on its own.
    Verified,
    /// Through [`unverified_delta_base`] — intermediate bases are not hashed; only the target is.
    Unverified,
}

/// Reconstruct an object's bytes from its data-file record and verify them against `hash`.
///
/// The reconstruction is delegated to [`reconstruct_record`]; this wrapper then enforces the
/// module's content-addressing guarantee — the reconstructed bytes must hash to `hash`, so a
/// corrupt full record, a bad delta, or a delta rebuilt against the wrong base fails the read
/// rather than silently returning wrong bytes (`object_utils::verify_object_bytes`).
///
/// A delta read a caller asked for (not a base inside another reconstruction) first reconstructs
/// its chain with *unverified* bases ([`Bases::Unverified`]) and hashes only the result: a base
/// is used only as the delta's dictionary, and no wrong base can yield bytes that hash to `hash`.
/// That skips one hash pass over every intermediate base, ~25% of a packed `diff --staged`.
/// If that attempt fails in any way, the read is redone with every base verified — exactly the
/// read this was before — and *its* outcome is returned, so the error for a corrupt chain still
/// names the object that is actually corrupt, and a base that only verifies through the full read
/// path (a good loose copy behind a broken pack record) still serves.
///
/// What this changes: a target whose delta was encoded against the bytes a corrupt base's record
/// reconstructs to — only a crafted pack can hold one, since every writer encodes against verified
/// bases — can now read, because its own bytes verify. Whether it does depends on which copy of
/// the base this process meets first (a verified copy in the read cache wins, then pack order),
/// so such an object is servable but not reliably so. `compact --all` never copies its delta
/// verbatim once the base is repacked from another copy (`collect_targets`): it reads the object
/// first, and either re-encodes it or fails loudly — the delta is never kept pointing at bytes
/// the repack has thrown away.
fn resolve_record(record: &[u8], hash: &str) -> Result<Vec<u8>, String> {
    if record.first() == Some(&RECORD_DELTA) && !in_reconstruction() {
        if let Ok(bytes) = reconstruct_record(record, hash, Bases::Unverified) {
            if object_utils::verify_object_bytes(hash, &bytes).is_ok() {
                return Ok(bytes);
            }
        }
    }

    let bytes = reconstruct_record(record, hash, Bases::Verified)?;
    object_utils::verify_object_bytes(hash, &bytes)?;
    Ok(bytes)
}

/// Reconstruct an object's bytes from its data-file record, *without* verifying the result —
/// its sole caller [`resolve_record`] does that.
///
/// A record starts with a kind byte: a full record's remainder is the zstd blob; a delta record's remainder is
/// `base hash (32) || target length (VLQ) || zstd delta`, reconstructed against its base —
/// which `bases` says how to fetch: either through the top-level object read (so the base may be
/// loose, in another pack, or itself a delta) or through [`unverified_delta_base`]. The chain is
/// bounded well below `MAX_RECONSTRUCT_DEPTH`, which guards only against a corrupt pack chaining
/// without end.
fn reconstruct_record(record: &[u8], hash: &str, bases: Bases) -> Result<Vec<u8>, String> {
    // Trusted bytes: a local pack record is self-written or transport-verified (see
    // `decode_full_transport_record`), and a grandfathered giant must stay readable — bounding
    // this read would make a pre-existing over-ceiling packed object unreadable.
    #[allow(clippy::disallowed_methods)]
    let decode_full = |blob: &[u8]| zstd::stream::decode_all(blob)
        .map_err(|e| format!("Error while decompressing packed object {}: {}", hash, e));

    let (&kind, body) = record.split_first()
        .ok_or_else(|| format!("Packed object {} has an empty record.", hash))?;

    match kind {
        RECORD_FULL => decode_full(body),
        RECORD_DELTA => {
            if body.len() < HASH_LEN {
                return Err(format!("Packed delta {} is truncated (no base hash).", hash));
            }

            let depth = RECONSTRUCT_DEPTH.with(|d| { let n = d.get() + 1; d.set(n); n });
            let _guard = DepthGuard;
            if depth > MAX_RECONSTRUCT_DEPTH {
                return Err(format!("Packed delta {} exceeds the reconstruction depth limit (corrupt pack?).", hash));
            }

            let base_bytes: [u8; HASH_LEN] = body[0..HASH_LEN].try_into().expect("checked above");
            let base_hash = sign_utils::to_hex(&base_bytes);
            let (target_len, read) = byte_utils::number_from_vlq_bytes(HASH_LEN, body)?;
            let payload = &body[HASH_LEN + read..];

            // Borrow-only: the delta base is only read to reconstruct against, so share the
            // cached `Arc` instead of copying the base out (hot on packed-delta reads and compact).
            let base = match bases {
                Bases::Verified => file_utils::retrieve_object_by_hash_shared(&base_hash)?,
                Bases::Unverified => unverified_delta_base(&base_hash, &base_bytes)?,
            };

            delta_utils::decompress_delta(&base, payload, target_len as usize)
        }
        other => Err(format!("Packed object {} has an unknown record kind {}.", hash, other)),
    }
}

/// A delta base for [`Bases::Unverified`]: the verified read cache's copy if it holds one, else
/// a copy reconstructed earlier by this path, else the first loaded pack record that locates it,
/// reconstructed (its own bases fetched the same way) and kept in [`unverified_base_cache`]. A
/// base no loaded pack locates (loose, absent, behind a corrupt bucket) takes the ordinary verified
/// read, with all of its fallbacks.
///
/// A located record that fails to reconstruct fails this attempt outright, with no second copy
/// tried and no verified read: either would retry the whole chain beneath it at every level (a
/// second copy per level is exponential in the depth), and the verified redo that
/// [`resolve_record`] runs on any failure already tries every copy, once.
///
/// These bytes are never handed to a caller: the only consumer is [`reconstruct_record`], as a
/// delta's dictionary, and the object built from them is hashed before anything returns it.
fn unverified_delta_base(hash: &str, hash_bytes: &[u8; HASH_LEN]) -> Result<Arc<Vec<u8>>, String> {
    if let Some(bytes) = file_utils::read_cache_get(hash) {
        return Ok(bytes);
    }

    let key = file_utils::read_cache_key(hash);
    if let Some(bytes) = unverified_base_cache().get(&key) {
        return Ok(bytes);
    }

    let packs = loaded_packs()?;
    for pack in packs.packs.iter() {
        let Some((offset, length)) = pack.locate(hash_bytes).found() else { continue };
        let bytes = Arc::new(reconstruct_record(pack.slice(offset, length)?, hash, Bases::Unverified)?);
        let weight = bytes.len();
        unverified_base_cache().put(&key, Arc::clone(&bytes), weight);
        return Ok(bytes);
    }

    file_utils::retrieve_object_by_hash_shared(hash)
}

/// The cache behind [`unverified_delta_base`], kept apart from the verified read cache so that
/// cache only ever holds bytes that hashed to their key. Keyed by `file_utils::read_cache_key`
/// (qualified by the warehouse) and bounded like the read cache, which held these bases before:
/// a smaller per-entry ceiling would leave a large base uncached and rebuilt, chain and all, for
/// every object on top of it.
fn unverified_base_cache() -> &'static super::two_gen_cache::TwoGenCache<Vec<u8>> {
    static CACHE: OnceLock<super::two_gen_cache::TwoGenCache<Vec<u8>>> = OnceLock::new();
    CACHE.get_or_init(|| super::two_gen_cache::TwoGenCache::new(
        file_utils::READ_CACHE_BUDGET, file_utils::READ_CACHE_MAX_ENTRY
    ))
}

/// Every packed object hash (hex) that begins with `prefix`. The pack-aware half of
/// resolving a revision given as a hash or hash prefix — without it, a hash reference stops
/// resolving once its object is packed. Reads only the index buckets a hash with that prefix can
/// be in, validating each first.
///
/// The cached set is revalidated ([`reloaded_if_stale`]) before every scan, not only when the
/// scan comes back empty: a prefix that matches one packed hash can still match another in a pack
/// published since, and answering "unique" from a stale set would resolve it wrongly.
///
/// # Arguments
/// * `prefix` - A lowercase hex hash or hash prefix; anything else matches nothing.
///
/// # Returns
/// * `Ok(Vec<String>)` - The full hex hashes of packed objects matching the prefix.
/// * `Err(String)`     - If the packs could not be loaded, or an index bucket the prefix falls in
///                       is corrupt, so its matches cannot be listed.
pub fn find_hashes_with_prefix(prefix: &str) -> Result<Vec<String>, String> {
    let cached = loaded_packs()?;
    let packs = reloaded_if_stale(&cached)?.unwrap_or(cached);
    let mut matches = Vec::new();

    for pack in packs.packs.iter() {
        let records = pack.index.records_for_hex_prefix(prefix)
            .map_err(|bucket| pack.corrupt_bucket_error(bucket))?;
        for record in records {
            let hash = sign_utils::to_hex(record.hash);
            if hash.starts_with(prefix) {
                matches.push(hash);
            }
        }
    }

    Ok(matches)
}

/// Enumerate every object hash a pack index file names, reading it directly off disk and
/// independent of the warehouse-wide pack registry ([`loaded_packs`]).
///
/// Used by the durability-taint recovery walk (`recovery_utils`) when a taint recorded a pack
/// **data** file as a final path and its restage found the data file vanished: the index sibling
/// (if it survives) is the only record of what that pack held, so its hashes become the closure
/// walk's search targets — a hash from here that turns out to still be present elsewhere (another
/// pack, or loose) is not actually lost; one that is not present *and* is referenced from a
/// durable ref is the dangling case the walk reports.
///
/// Deliberately does not call [`load_pack_pair`] (which also maps and validates the *data* file —
/// exactly the file this is reached because is missing) or go through [`loaded_packs`]'s
/// registry/cache (which would fail to load the whole pack set over one missing pair). Every
/// bucket is validated for crc, order and prefix; the data-length bounds are skipped since there
/// is no data file to check against — this function's only job is the hash list.
///
/// # Returns
/// * `Ok(Vec<String>)` - Every hex hash the index names, in its stored (sorted) order.
/// * `Err(String)`      - The index file could not be read, or is corrupt / an unrecognized format.
pub(crate) fn hashes_in_index_file(index_path: &Path) -> Result<Vec<String>, String> {
    let index = std::fs::read(index_path)
        .map_err(|e| format!("Error while reading pack index \"{}\": {}", index_path.to_string_lossy(), e))?;
    let layout = index_format::Layout::parse(&index, index_path)?;
    let index = CheckedIndex::new(index, layout, None);
    let records = index.all_records().map_err(|bucket| index_format::corrupt_bucket_message(index_path, bucket))?;

    Ok(records.iter().map(|record| sign_utils::to_hex(record.hash)).collect())
}

/// Whether any pack holds the object with the given hash. The existence fallback for
/// `file_utils::does_object_exist` when the loose file is absent.
///
/// Answers from **loaded packs only** — a hash named only by a quarantined or unenumerable index
/// (FORK-47, DESIGN.html §3.1.1), or one whose index bucket is corrupt, reads `false` here, never
/// `true`. This is the safe polarity: every
/// existing caller of this presence check (push/fetch dedup, prune, the closure walk) must treat
/// an unservable hash as absent-and-worth-chasing, which is what lets heal *refetch and clear* it
/// rather than silently skip it — flipping the polarity would make a quarantine-named hash
/// permanently unhealable.
///
/// A hit answers from the cached set with no syscall. A miss revalidates the set against the pack
/// folder first ([`reloaded_if_stale`]) — so a pack another process published since this one
/// cached its set is seen, and `false` means no pack on disk held the hash when this call looked,
/// not merely none this process happened to load (FORK-106: a live server used to 404 everything
/// an external `compact` had packed). A caller that also checks the loose store must ask the packs
/// again after a loose miss, since a compact can pack and sweep in between (see
/// [`is_in_cached_packs`]).
///
/// # Arguments
/// * `hash` - The hex hash of the object.
///
/// # Returns
/// * `Ok(true)`    - If a loaded pack holds the object.
/// * `Ok(false)`   - If no loaded pack holds it.
/// * `Err(String)` - If the packs could not be loaded.
pub fn is_in_packs(hash: &str) -> Result<bool, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(false);
    };

    let packs = loaded_packs()?;
    if located_in(&packs, &hash_bytes) {
        return Ok(true);
    }

    Ok(match reloaded_if_stale(&packs)? {
        Some(fresh) => located_in(&fresh, &hash_bytes),
        None => false,
    })
}

/// [`is_in_packs`] against the cached set only, with no revalidation: the first step of a
/// presence check that tries packs, then loose, then revalidated packs (`does_object_exist`).
/// Asking the cached set first keeps a loose object's check free of the revalidation; asking the
/// revalidated set last catches a compact that packed the object and swept its loose file between
/// the two looks.
pub(crate) fn is_in_cached_packs(hash: &str) -> Result<bool, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(false);
    };

    Ok(located_in(&*loaded_packs()?, &hash_bytes))
}

fn located_in(packs: &PackSet, hash_bytes: &[u8; HASH_LEN]) -> bool {
    packs.packs.iter().any(|pack| matches!(pack.locate(hash_bytes), Located::Found(..)))
}

/// Where an object to pack comes from, and how to pack it.
enum Source {
    /// A loose file: read it, delta it (path-aware / window), and delete it once packed.
    Loose(PathBuf),
    /// A record already in a pack whose delta base survives the repack *from the same copy* —
    /// **copied verbatim**,
    /// never reconstructed or re-deltated, so the original (good) delta is preserved and the
    /// repack stays a byte-copy. `pack_index` indexes the source packs kept mapped for the run
    /// (`collect_targets` returns them), so the record is a zero-copy slice out of that pack's
    /// mmap — no per-record `open`/`seek`/`read`. This is the sole-copy case; when the hash is
    /// duplicated, `collect_targets` keeps this verbatim path only if every copy's bytes are
    /// byte-identical, and demotes to `Source::Reconstruct` otherwise, so a duplicate is never
    /// carried forward unverified.
    CopyRecord { pack_index: usize, offset: u64, len: u64, is_delta: bool },
    /// A packed object whose delta base is being dropped as garbage, a duplicated hash whose
    /// surviving copy `collect_targets` could not trust verbatim (FORK-55), or a delta whose base
    /// is itself one of these (so its record may have been encoded against a copy the repack
    /// does not keep — see the pass at the end of `collect_targets`): reconstruct it
    /// (through the exhaustive verified read, `retrieve_object_by_hash`) and re-pack it
    /// path-aware, so nothing is left pointing at a dropped base and no unverified duplicate is
    /// ever carried forward. `loose_source` is `Some(path)` when this target was demoted from a
    /// `Source::Loose(path)` duplicate — its loose file is still on disk and must ride the
    /// ordinary durable-before-destructive source sweep once this target is packed (see
    /// `compact`'s `loose_path` match, `pack_utils.rs:1937-1940`); `None` for the pre-existing
    /// dropped-base case and for a duplicate demoted from a `Source::CopyRecord` (nothing loose
    /// to delete).
    Reconstruct { loose_source: Option<PathBuf> },
}

/// An object to pack, with the size that orders the packing and where it comes from.
struct PackTarget {
    hash: [u8; HASH_LEN],
    size: u64,
    source: Source,
}

/// A recently-packed object kept as a candidate delta base: its hash, decompressed bytes
/// (the zstd dictionary a delta is made against) and the length of the chain it already sits
/// on (so a chain cannot grow past `MAX_DELTA_CHAIN`).
struct WindowEntry {
    hash: [u8; HASH_LEN],
    raw: Vec<u8>,
    depth: u32,
}

/// Pack the active warehouse's objects, then delete the originals.
///
/// The caller must hold the warehouse lock (this deletes objects). Two modes:
///
/// * **Incremental** (`all = false`): pack the *loose* objects into new packs and leave
///   existing packs untouched — the cheap, common case (used after `import-git`).
/// * **Repack** (`all = true`): rewrite everything — loose *and* every existing pack — into
///   fresh packs, keeping only the **live** set (objects reachable from the GC roots) and so
///   dropping unreachable garbage that was stuck in packs, and consolidating many packs into
///   few. Unreachable *loose* objects are left alone for the grace-period-aware loose
///   collector (`gc_utils`); this only drops garbage that had already been packed. Because a
///   repack re-deltas the live set from scratch and every path base is itself live, no delta
///   is ever left pointing at a dropped object.
///
/// A repack's live *packed* objects normally take the fast path — copied verbatim, preserving
/// whatever delta each already has (see `Source::CopyRecord`) — so the common repack is a
/// byte-copy, not a re-compress. A duplicated hash keeps this fast path only when every copy is
/// byte-identical; a duplicate whose copies differ is demoted to the verified reconstruct path
/// instead (see `Source::Reconstruct`), so the byte-copy guarantee never covers unverified
/// bytes. `redelta` (only meaningful with `all`; the caller is
/// responsible for refusing the combination otherwise, see the CLI's `compact` handler) turns
/// every live packed object into a recompression candidate instead — the same treatment a
/// loose object gets (read, decompress, offer to the path base then the size window, keep the
/// delta only if it wins) — so cross-path similarity a per-object copy structurally cannot see
/// (renames, moved files) gets a chance to delta. `compute_path_bases` credits *directories* as
/// well as files with a path base (a directory's previous version at the same path), which
/// matters enormously more here than in a plain repack: a plain repack's `CopyRecord` fast path
/// preserves whatever delta a directory already had (from `import_git`'s own per-path chaining,
/// say), but `redelta` discards that and starts every directory over from a decompress — without
/// crediting directories too, every one of them (typically a third or more of a repo's objects)
/// would fall back to the size window on every redelta, and a redelta pass could easily land
/// *larger* than the store it started from. One-shot and CPU-bound: every live object is
/// re-read and re-compressed, not just the ones a plain repack would have touched anyway.
///
/// Signature sidecars (`.sig`) and temp files are left alone (sidecars are read by path).
/// Objects with no path base are packed largest-first and offered a **delta** against a small
/// size window, kept only when smaller than the full blob — the size ordering is what gives
/// that window any chance of similarity. Objects `compute_path_bases` gave a path base instead
/// are packed in that walk's own order (a base always before its dependent, whatever their
/// relative sizes), each offered a delta against the previous version of the same path — kept
/// only when it wins the same size race *and* the write-time depth ledger confirms the base's
/// true current depth leaves room for one more hop without exceeding `MAX_DELTA_CHAIN` (demoted
/// to a full record otherwise; see `MAX_RECONSTRUCT_DEPTH`'s doc comment for why that check
/// exists and cannot be skipped). Packs are written durably before any original is removed, so a
/// failure never loses an object.
///
/// # Arguments
/// * `all` - Repack existing packs too (drop packed garbage, consolidate), not just the loose set.
/// * `redelta` - Re-delta every live packed object instead of copying it verbatim (see above).
/// * `salvage` - With `all`, recover what damaged packs still hold and remove the ones whose live
///   objects are all accounted for (FORK-194, [`salvage_damaged_packs`]); without it they are left
///   exactly as found. Only an explicitly requested repack sets it, never auto-maintenance, which
///   runs unasked, reports nothing and would repeat the salvage after every command.
/// * `progress` - Where to report the run's phases and how far packing has got.
///
/// # Returns
/// * `Ok(CompactStats)` - What was packed and removed. Every new pack's directory entries are
///                        durable and, once [`taint_utils::activate`](crate::util::taint_utils::activate)
///                        has been called in this process, no durability taint was found standing
///                        for this root on the re-check that runs right before the destructive
///                        sweep below is allowed to proceed (see `file_utils::sync_dir_or_taint`).
///                        An unactivated process skips that re-check entirely.
/// * `Err(String)`      - If enumeration, writing, or deletion failed. A new-pack directory-sync
///                        failure specifically leaves every pack finalized this run
///                        (`new_pack_files`) visible but not proven durable, and — once activated
///                        — records that as a taint; either that or a standing taint found on the
///                        re-check both abort before the loose-source/old-pack removal sweep below
///                        ever runs. A later `heal_utils::heal_if_tainted` call restages exactly
///                        the affected paths and clears the taint once they are durable again —
///                        `compact`'s own destructive sweep stays refused until then either way.
pub fn compact(all: bool, redelta: bool, salvage: bool, progress: &Progress) -> Result<CompactStats, String> {
    let pack_folder = pack_folder();
    file_utils::create_folder_if_not_exists(&pack_folder)?;

    // Serialize destructive store maintenance across bays *and* processes: the object store is
    // shared (`forklift_root`), but a command's bay lock is not, so without this two bays could
    // enumerate the same loose set and race each other's deletions. Held for the whole run;
    // errors immediately if another compaction holds it — an explicit `compact` surfaces that,
    // auto-maintenance (which ignores compaction errors) simply skips the now-redundant work.
    // Taken after the folder exists so its parent (`forklift_root`) is present for `create_new`.
    let _store_lock = lock_utils::StoreLock::acquire()?;

    // A repack is serve-exclusive, like gc (FORK-149). It keeps only the live set, with no grace
    // period, so beside a live server it can drop a lift's objects after an incremental compact
    // packed them and before the lift's ref update lands — and a server whose presence check is
    // current (FORK-106) answers "present" for exactly those objects, so the client never
    // re-sends a loose copy that would survive. Incremental compaction drops nothing and stays
    // allowed. Auto-maintenance ignores a compaction error, so it simply skips the repack while
    // the root is served.
    let _serve_lock = if all {
        Some(lock_utils::ServeLock::acquire().map_err(|e| format!(
            "A full repack cannot run while this warehouse root is being served, or while a gc runs \
             against it; stop the server first. If neither is running, the lock below was left by \
             one that was killed and can be removed. {}", e
        ))?)
    } else {
        None
    };

    // A repack drops every packed object outside the live set it is about to compute, so no
    // writer may be between a dedup decision and the root that publishes it while that set is
    // computed and the old packs are dropped (FORK-150): a load in another bay that found a
    // packed-but-unreachable object present would otherwise publish a reference to it and lose it
    // here. Never waits; refused while any writer holds the store, which auto-maintenance treats
    // as any other compaction error and retries on a later command. Incremental compaction makes
    // no liveness decision, but it moves loose objects into a pack and deletes the loose copies,
    // which must not interleave with a scope-prune or gc deleting loose objects on a keep-set; so
    // it shares the store with writers, refused (never waiting) while a deleter holds it.
    let (_store_exclusive, _store_shared) = if all {
        (Some(lock_utils::StoreAccessLock::try_exclusive()?), None)
    } else {
        (None, Some(lock_utils::StoreAccessLock::try_shared()?))
    };

    remove_stale_temp_files(&pack_folder);

    progress.status("Collecting objects");

    // The objects to pack, and the old pack files a repack supersedes.
    //
    // One shared reachability pass (D/P3). Both the garbage decision (`collect_live_set`, inside
    // `collect_targets`) and the path-base selection (`compute_path_bases`) walk the reachable
    // parcel DAG, reading each parcel several times over — so hold a `ParcelReadMemo` across the
    // whole reachability phase, which collapses those (~5 per parcel) re-reads to one decode each
    // and is dropped before the parallel batch loop below. `source_packs` are the existing packs,
    // kept mapped for the run so a `CopyRecord` copies its bytes straight from the mmap.
    let (targets, old_packs, source_packs, depth_packs, path_bases, damaged_packs_dropped, damaged_packs_kept,
        damaged_packs_skipped, salvage_registration) = {
        let _parcel_memo = object_utils::ParcelReadMemo::activate();

        let CollectedTargets {
            targets, old_packs, source_packs, damaged_packs_dropped, damaged_packs_kept, damaged_packs_skipped,
            salvage_registration,
        } = collect_targets(all, redelta, salvage)?;

        // Path-aware base selection (phase 2b) is only needed to *build* new deltas — for loose
        // objects and for the rare object whose base is dropped. A repack that only copies existing
        // records (the common case) skips the whole DAG walk.
        let needs_delta = targets.iter().any(|t| !matches!(t.source, Source::CopyRecord { .. }));
        let path_bases = if needs_delta {
            compute_path_bases()?
        } else {
            PathBases { base_of: HashMap::new(), sequence: HashMap::new() }
        };

        // The depth ledger's cross-pack fallback (`true_depth`) needs the current pack registry
        // regardless of `all`: an incremental compact's `source_packs` is deliberately empty (it
        // never touches existing packs — see `collect_targets`), but a loose target's path base
        // can still be one of them. Already cached when `all` is set (`collect_targets` itself
        // called `loaded_packs`), so this is a free `Arc` clone in that case.
        let depth_packs = loaded_packs()?;

        // Split into root targets (no candidate path base: a first version at their path, or a
        // `CopyRecord` this run never re-deltates) and chain targets (a candidate from
        // `compute_path_bases`). Roots keep the largest-first order the size-window fallback
        // depends on. Chains are reordered to the walk's own order instead — a chain's base is
        // always ordered before it (`PathBases::sequence`) — so the depth ledger below can
        // resolve every chain forward in one pass, a base's true depth always already known by
        // the time its dependent asks for it, regardless of how the two compare in size.
        let (mut chain_targets, mut root_targets): (Vec<PackTarget>, Vec<PackTarget>) = targets
            .into_iter()
            .partition(|t| !matches!(t.source, Source::CopyRecord { .. }) && path_bases.base_of.contains_key(&t.hash));

        // Largest first, with the object hash as a total tie-breaker so the packing order — and
        // therefore every record's offset — is deterministic. Without it, equal-size objects kept
        // their filesystem-enumeration order, so two repacks of the *same already-packed* live set
        // produced different layouts every run; harmless under the old id (which hashed only the
        // object set) but, now that the pack id folds in offsets/lengths, this determinism is what
        // stops a steady-state repack from churning the pack onto a fresh name (rewrite + delete)
        // each run and lets it land on the same name.
        root_targets.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.hash.cmp(&b.hash)));
        chain_targets.sort_by_key(|t| *path_bases.sequence.get(&t.hash)
            .expect("a chain target's hash was recorded by the same walk that gave it a base_of entry"));

        root_targets.extend(chain_targets);
        let targets = root_targets;

        (targets, old_packs, source_packs, depth_packs, path_bases, damaged_packs_dropped, damaged_packs_kept,
            damaged_packs_skipped, salvage_registration)
    };

    let mut stats = CompactStats {
        objects_packed: 0, packs_written: 0, loose_removed: 0, deltas: 0, bytes_packed: 0,
        corrupt_skipped: 0, over_ceiling_skipped: 0, damaged_packs_dropped, damaged_packs_kept,
        damaged_packs_skipped,
    };
    let mut writer: Option<PackWriter> = None;

    // The fallback delta window: the last few packed objects, decompressed, bounded by count
    // and by resident bytes (a run of large objects must not blow the window up).
    let mut window: VecDeque<WindowEntry> = VecDeque::new();
    let mut window_bytes: usize = 0;

    // The loose files packed, deleted only after every pack is durably written. Deferring
    // deletion (and old-pack deletion) keeps every delta base readable throughout the run, so
    // fetching a path base never depends on a just-finalized pack the read cache has not seen.
    let mut packed_sources: Vec<PathBuf> = Vec::new();
    // The new packs' own paths, so a repack never deletes one as an "old" pack (the id is
    // layout-derived, so an unchanged repack writes the very same filename).
    let mut new_pack_files: HashSet<PathBuf> = HashSet::new();

    // The write-time depth ledger (see `true_depth`): every hash this run has assigned a real (or,
    // for a still-pending chain target, a sound worst-case — see the pre-pass below) depth to.
    // Consulted instead of a stale on-disk read for anything this run has already decided, since
    // `redelta` may re-encode a hash as something else entirely. Spans the whole run (unlike
    // `safe_base_of` below): a later batch's pre-pass must see an earlier batch's decisions.
    let mut known_depth: HashMap<[u8; HASH_LEN], u32> = HashMap::new();

    // Process the targets in byte-bounded batches. Each batch's *path* deltas — the CPU-heavy
    // part — are compressed in parallel by `prepare_batch`; the writer then walks the batch in
    // order doing only the sequential work (the size-window fallback and the append), so the
    // pack it produces is byte-for-byte what a single-threaded compaction would. Bounding the
    // batch bounds the decompressed bytes held in memory at once.
    const BATCH_BYTES: u64 = 16 * 1024 * 1024;
    const BATCH_COUNT: usize = 1024;

    progress.status("Packing objects");

    let mut start = 0;
    while start < targets.len() {
        // Grow the batch until it hits the byte or count budget (always at least one object).
        let mut end = start;
        let mut batch_bytes = 0u64;
        while end < targets.len()
            && (end == start || (batch_bytes < BATCH_BYTES && end - start < BATCH_COUNT))
        {
            batch_bytes = batch_bytes.saturating_add(targets[end].size);
            end += 1;
        }

        let batch = &targets[start..end];

        // Depth-safety pre-pass, sequential and header-read-only (no decompression — see
        // `true_depth`): for every chain target in this batch, resolve its candidate base's
        // *true* current depth — this run's own decision if the base was already processed
        // (an earlier-sorted root, or an earlier chain target in this same walk order — see
        // `PathBases::sequence`), otherwise a header-chain walk of whichever pack currently holds
        // it (a `CopyRecord` object this run leaves untouched, or — an incremental compact — any
        // pre-existing packed object at all). A candidate that would push true depth past
        // `MAX_DELTA_CHAIN` is rejected here, *before* `prepare_batch` ever reads or compresses
        // it — this is the hard invariant `compute_path_bases`'s own bookkeeping cannot provide
        // (see `MAX_RECONSTRUCT_DEPTH`'s doc comment): every delta this run *encodes* is
        // depth-checked before it is written. The loop below skips every `Source::CopyRecord`
        // target (it is copied verbatim, never encoded), so a verbatim-copied delta inherits its
        // base's carried depth unchecked rather than being re-verified here (FORK-59).
        //
        // A safe candidate's depth is recorded here as the *planned* value — assuming it will
        // in fact be kept as a delta — before `prepare_batch` has even computed its payload, let
        // alone learned whether that payload beats the full blob. This is sound, not a race: if
        // the size check below ends up rejecting it (kept full instead, true depth 0), the real
        // depth only ever comes in *under* what was planned for it, so anything chaining off this
        // hash that already used the planned value stays a valid (if occasionally conservative)
        // upper bound. Never the reverse: nothing here ever assumes a shallower depth than a
        // candidate can actually turn out to have.
        //
        // Scoped to this batch alone (rebuilt fresh every iteration, unlike `known_depth`):
        // `prepare_batch` below only ever looks up *this* batch's targets in it, so carrying
        // earlier batches' entries forward would just accumulate dead weight — at git.git scale,
        // most of `path_bases.base_of` restated back into a second map nothing after this batch
        // still reads.
        let mut safe_base_of: HashMap<[u8; HASH_LEN], [u8; HASH_LEN]> = HashMap::new();
        for target in batch {
            if matches!(target.source, Source::CopyRecord { .. }) {
                continue;
            }
            let Some(&base) = path_bases.base_of.get(&target.hash) else {
                continue;
            };

            let base_depth = true_depth(base, &mut known_depth, &depth_packs.packs)?;
            let planned_depth = base_depth + 1;

            if planned_depth <= MAX_DELTA_CHAIN {
                safe_base_of.insert(target.hash, base);
                known_depth.insert(target.hash, planned_depth);
            } else {
                known_depth.insert(target.hash, 0);
            }
        }

        let mut prepared = prepare_batch(batch, &safe_base_of)?;
        start = end;

        for (i, target) in batch.iter().enumerate() {
            // A corrupt loose object (see `read_target`/`PrepSlot::CorruptLoose`): skip it before
            // the chunk check and before a writer is ever fetched, exactly like the chunk-skip
            // below — for a *sole-copy* corrupt loose object, never packed, never deleted, never
            // the reason an all-corrupt batch creates an empty pack. This variant is `Source::
            // Loose`-only: a *duplicated* hash whose loose copy is corrupt is demoted by
            // `collect_targets` to `Reconstruct` before this arm is ever reached, so it is
            // instead packed from whichever copy verifies and its loose file is swept, changing
            // `stats.corrupt_skipped` for that case. A non-`Loose` target is never skipped here or
            // below (I4) — its only guaranteed surviving copy may be in a pack this run's sweep
            // deletes, so skipping it would be a data-loss path, not a preservation one.
            if matches!(prepared[i], PrepSlot::CorruptLoose) {
                stats.corrupt_skipped += 1;
                continue;
            }

            // A loose object above the whole-object ceiling (see `read_target`/
            // `PrepSlot::OverCeiling`): a grandfathered giant authored before the ceiling
            // existed, not corruption. Skipped the same way and at the same point as
            // `CorruptLoose` above — for a *sole-copy* giant, never packed, never deleted, left
            // loose and readable. This variant is likewise `Source::Loose`-only: a *duplicated*
            // over-ceiling giant is demoted to `Reconstruct` and packed via its unbounded local
            // decode instead of reaching this arm. Same I4 rule as above: a non-`Loose` target is
            // never skipped, because its only guaranteed copy may be in a pack this run deletes.
            if matches!(prepared[i], PrepSlot::OverCeiling) {
                stats.over_ceiling_skipped += 1;
                continue;
            }

            // Chunks are never packed or delta'd: leave the loose chunk file exactly where it is
            // (do not write it into a pack, do not mark it for deletion, do not seed the delta
            // window with it). Detected from the decompressed bytes already prepared for this
            // target, so no extra read; only a loose target can be a chunk (a chunk is never in an
            // existing pack, so a `CopyRecord` never is one). This is checked before the writer is
            // fetched so an all-chunk batch never creates an empty pack. Guarded on
            // `Source::Loose(_)` deliberately, per I4: a non-`Loose` target is never skipped,
            // because its only guaranteed copy may be in a pack this run deletes — a demoted
            // `Reconstruct` target that happens to be a chunk falls through and is packed instead.
            if matches!(target.source, Source::Loose(_))
                && prepared[i].as_ready().is_some_and(|prep| is_chunk(&prep.raw)) {
                continue;
            }

            let pack = match writer.as_mut() {
                Some(pack) => pack,
                None => writer.insert(PackWriter::new(&pack_folder)?),
            };

            // Copy an existing record verbatim — a repack's fast path: the original (good) delta
            // is preserved, nothing is reconstructed or re-deltated. The bytes are a zero-copy
            // slice out of the source pack's mmap rather than a fresh file read.
            if let Source::CopyRecord { pack_index, offset, len, is_delta } = &target.source {
                let record = source_packs.packs[*pack_index].slice(*offset, *len)?;
                let written = pack.append_raw_record(target.hash, record)?;
                stats.bytes_packed += written;
                if *is_delta {
                    stats.deltas += 1;
                }
                stats.objects_packed += 1;

                if pack.should_roll_over() {
                    let finalized = writer.take().unwrap().finalize()?;
                    packed_sources.extend(finalized.sources);
                    new_pack_files.extend(finalized.files);
                    stats.packs_written += 1;
                }
                continue;
            }

            // A chain target (`compute_path_bases` offered it a candidate base) never falls back
            // to the size window, whether its candidate was rejected by the pre-pass above or by
            // `prepare_target`'s own size check: the window's own depth bookkeeping only holds
            // (window bases are always already-written when chosen, so their depth is always
            // known — see `WindowEntry`) because it never has to account for a base whose true
            // depth might still be unknown. Letting a chain target compete for it would reopen
            // exactly that gap. It still gets the same fallback a root does when it has no safe
            // or winning delta at all: full.
            let is_chain_target = path_bases.base_of.contains_key(&target.hash);

            let PrepSlot::Ready(prep) = std::mem::take(&mut prepared[i]) else {
                panic!("a non-copy, non-corrupt target must have a Ready prep slot")
            };

            let loose_path = match &target.source {
                Source::Loose(path) => Some(path.clone()),
                // A duplicate demoted from `Source::Loose` (FORK-55, `collect_targets`) still has
                // a loose file on disk that must ride the same durable-before-destructive sweep
                // once this target is packed -- without this arm it would return `None` for every
                // demoted duplicate and the corrupt/redundant loose file would never be removed.
                Source::Reconstruct { loose_source: Some(path) } => Some(path.clone()),
                _ => None,
            };

            // 1. A winning path delta — the previous version of this exact file — was already
            //    computed (in parallel) off the write path.
            let mut path_delta = false;
            if let Some((base, payload)) = &prep.path_delta {
                let written = pack.append_delta(target.hash, *base, prep.raw.len() as u64, payload, loose_path.clone())?;
                stats.deltas += 1;
                stats.bytes_packed += written;
                path_delta = true;
            }

            // 2. Otherwise fall back to the size window (trees and the like) — sequential, as it
            //    deltas against the objects just packed — keeping the smallest delta only when
            //    it beats the full blob. Never for a chain target (see above): it goes straight
            //    to full instead, and its `known_depth` entry already stands from the pre-pass.
            let mut window_depth = 0;
            if !path_delta {
                let best = if !is_chain_target && prep.deltable {
                    best_delta(&prep.raw, &window)?
                } else {
                    None
                };

                match best {
                    Some((base_hash, payload, base_depth)) if payload.len() < prep.compressed.len() => {
                        let written = pack.append_delta(target.hash, base_hash, prep.raw.len() as u64, &payload, loose_path.clone())?;
                        stats.deltas += 1;
                        stats.bytes_packed += written;
                        window_depth = base_depth + 1;
                        if !is_chain_target {
                            known_depth.insert(target.hash, window_depth);
                        }
                    }
                    _ => {
                        let written = pack.append_full(target.hash, &prep.compressed, loose_path.clone())?;
                        stats.bytes_packed += written;
                        if !is_chain_target {
                            known_depth.insert(target.hash, 0);
                        }
                    }
                }
            }
            stats.objects_packed += 1;

            if pack.should_roll_over() {
                let finalized = writer.take().unwrap().finalize()?;
                packed_sources.extend(finalized.sources);
                new_pack_files.extend(finalized.files);
                stats.packs_written += 1;
            }

            // Only fallback-packed objects seed the window: a path delta fetches its base from
            // the store, so it need never be a window base — keeping path and window chains
            // separate (so reconstruction recursion stays bounded per mechanism). Parcels never
            // seed it either (nothing should delta against a parcel). Nor does a chain target
            // that fell through to full (never tried the window, `window_depth` stayed 0) — a
            // future window base's depth must be *known*, and the window's own bookkeeping can
            // only guarantee that by construction if only fallback-full/window objects ever
            // enter it (see `is_chain_target`'s doc comment above).
            if !path_delta && prep.deltable && !is_chain_target {
                window_bytes += prep.raw.len();
                window.push_back(WindowEntry { hash: target.hash, raw: prep.raw, depth: window_depth });
                while window.len() > DELTA_WINDOW || (window_bytes > DELTA_WINDOW_MEMORY && window.len() > 1) {
                    if let Some(evicted) = window.pop_front() {
                        window_bytes -= evicted.raw.len();
                    }
                }
            }
        }

        progress.fraction(start, targets.len(), || format!("{}/{} objects", start, targets.len()));
    }

    if let Some(pack) = writer.take() {
        let finalized = pack.finalize()?;
        packed_sources.extend(finalized.sources);
        new_pack_files.extend(finalized.files);
        stats.packs_written += 1;
    }

    // Release the source packs' mmaps now — their last use was a `CopyRecord` in the loop just
    // above. This must happen before the old-pack removal below: on Windows, a file opened (or
    // mapped) without `FILE_SHARE_DELETE` — the platform default — cannot be deleted while a
    // handle to it is still open, and `memmap2::Mmap` holds exactly such a handle. Before this
    // change, the equivalent per-record `File::open` in `read_pack_slice` was closed the instant
    // each copy finished, so no old pack was ever held open this late; keeping the whole-run
    // `Arc<PackSet>` for the mmap'd fast path must not silently change that. (POSIX needs
    // no such care — unlinking an open-and-mapped file is always safe there — but this drop must
    // hold on every supported platform, not just the one this was measured on.)
    drop(source_packs);

    // A corrupt-bucket error from here on could no longer say that nothing was deleted, and
    // nothing past this point reads an object: end the salvage registration.
    drop(salvage_registration);

    // Each pack's data and index bytes were fsynced in `finalize`, but the *directory entries*
    // that the renames created are themselves only durable once the pack folder is fsynced. Do it
    // once here (a single sync covers every rename this run made) before anything is deleted, so a
    // power loss between the sweep below and that metadata reaching disk cannot lose a pack whose
    // loose sources are already gone — the "durable" half of durable-before-destructive. A sync
    // failure taints `new_pack_files` (never the loose sources or old packs, which this sync says
    // nothing about); a sync success is re-checked against any taint already standing for this
    // root, and — since that check runs inside `sync_dir_or_taint`, still behind this `?` — a
    // taint found standing aborts here too, before the destructive sweep below ever runs. Durable-
    // before-destructive therefore means "durable *and no taint standing*," not durable alone.
    if !new_pack_files.is_empty() {
        let new_pack_refs: Vec<&Path> = new_pack_files.iter().map(PathBuf::as_path).collect();
        file_utils::sync_dir_or_taint(&pack_folder, &new_pack_refs)?;
    }

    // Every new pack is durable — only now remove the originals: the loose files that were
    // packed, then (for a repack) the old packs they superseded. Losing an object is
    // impossible at any interruption: it exists in a new pack before its original is deleted.
    // A file already gone is not an error: the `StoreLock` serializes compactions, but the
    // grace-period loose collector or a concurrent read-side cleanup can still have removed a
    // source first — the post-condition ("it is not loose") already holds, so tolerate NotFound.
    progress.status("Removing packed originals");

    for source in &packed_sources {
        match std::fs::remove_file(source) {
            Ok(()) => stats.loose_removed += 1,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!(
                "Error while removing loose object \"{}\": {}", source.to_string_lossy(), e
            )),
        }
    }

    invalidate_cache();

    for old_pack in &old_packs {
        // A layout-derived pack id means an unchanged repack writes the same filename it is
        // about to "delete" — never remove a file a new pack was just written to.
        if new_pack_files.contains(old_pack) {
            continue;
        }
        // As with the loose sweep, an old pack another process already removed is not an error.
        match std::fs::remove_file(old_pack) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!(
                "Error while removing old pack \"{}\": {}", old_pack.to_string_lossy(), e
            )),
        }
    }

    // A later read in this same process must see the packs we just wrote (and not the old ones).
    invalidate_cache();

    // Populate the commit-graph for the whole reachable history while the lock is held and the
    // object caches are warm, so the first ancestry query (merge base, divergence check) after
    // an import or repack is already fast. The graph is derived and self-healing, so a failure
    // here only defers that work to the first reader — never a reason to fail the compact.
    if let Ok(refs) = pallet_utils::all_pallet_refs() {
        let heads: Vec<String> = refs.into_iter().map(|(_, head)| head).collect();
        let _ = graph_utils::build_from_heads(&heads);
    }

    // A redelta pass just gave the whole live set its cross-path shot; the densify suggestion
    // (set when a bulk ingest could not) no longer applies until another bulk ingest re-earns it.
    if redelta {
        clear_densify_pending(&pack_folder);
    }

    Ok(stats)
}

/// The outcome of reading and decompressing one target's bytes for [`prepare_target`]. A loose
/// object's bytes may be corrupt (see [`PrepSlot::CorruptLoose`]); a `Reconstruct` target's
/// bytes, once this returns `Bytes`, are already store-verified (`retrieve_object_by_hash`).
enum ReadOutcome {
    Bytes(Vec<u8>, Vec<u8>),
    /// Loose only — see [`PrepSlot::CorruptLoose`].
    CorruptLoose,
    /// Loose only — see [`PrepSlot::OverCeiling`].
    OverCeiling,
}

/// Read an object being *re-deltated* (a loose object, or one whose base is dropped): its
/// decompressed bytes and its zstd blob (the full-record payload and the size guard). A loose
/// object is read from its file; a to-reconstruct object comes through the object store.
///
/// For a *sole-copy* loose object this is the one ingress its bytes take into a pack
/// (`enumerate_loose_objects` addresses it by *filename*, never by content — see the module
/// doc), so it is also the one place that content is checked against that address: a decode
/// failure or a hash mismatch is `CorruptLoose`, not an `Err` — corruption found here is
/// pre-existing damage this run did not cause, and packing it under a hash it does not hash to
/// would make it durable and outlive its last-recoverable loose form (deleted the moment its
/// corrupt packed twin lands). A *duplicated* loose object's bytes can instead enter through the
/// `Reconstruct` arm below, which serves whichever copy the exhaustive verified read finds
/// servable (possibly this same loose file, read and verified again there) — this arm's own
/// ingress checks are then bypassed in favor of that read (see `collect_targets`'s demotion).
/// A decode that would exceed the whole-object ceiling is `OverCeiling`, distinct from
/// `CorruptLoose`: this object is not corrupt, it is a legitimate object authored before the
/// ceiling existed (see [`object_utils::MAX_OBJECT_BYTES`]). For a sole-copy giant `compact`
/// leaves it loose rather than claim a decode this ingress point deliberately refuses to finish;
/// a *duplicated* over-ceiling giant is demoted the same way as a duplicated loose object and is
/// packed via the `Reconstruct` arm, whose local decode is deliberately unbounded so a
/// grandfathered giant stays readable (see `reconstruct_record`'s doc comment; only *transport*
/// decode is bounded). `Reconstruct` and `CopyRecord` are unchanged: any failure there
/// still aborts the whole compact (see `PrepSlot`'s doc comment for why that asymmetry is
/// load-bearing).
fn read_target(target: &PackTarget) -> Result<ReadOutcome, String> {
    match &target.source {
        Source::Loose(path) => {
            let compressed = std::fs::read(path)
                .map_err(|e| format!("Error while reading loose object: {}", e))?;

            let raw = match object_utils::decode_object_bounded(compressed.as_slice()) {
                Ok(raw) => raw,
                Err(object_utils::BoundedDecodeError::Malformed(_)) => return Ok(ReadOutcome::CorruptLoose),
                Err(object_utils::BoundedDecodeError::OverCeiling) => return Ok(ReadOutcome::OverCeiling),
            };

            if object_utils::verify_object_bytes(&sign_utils::to_hex(&target.hash), &raw).is_err() {
                return Ok(ReadOutcome::CorruptLoose);
            }

            Ok(ReadOutcome::Bytes(raw, compressed))
        }
        Source::Reconstruct { .. } => {
            let hash_hex = sign_utils::to_hex(&target.hash);
            // Wrapped with repack context (FORK-55): the exhaustive verified read this arm goes
            // through (`retrieve_object_by_hash`) can fail for a live duplicated hash with no
            // servable copy, aborting the whole compact before the destructive sweep -- name the
            // failing hash and point at the remedy rather than surfacing the read's bare error.
            let raw = file_utils::retrieve_object_by_hash(&hash_hex).map_err(|e| format!(
                "Error while repacking object {}: {} -- no verifiable copy could be found; \
                run \"forklift heal\" to check for a recoverable copy.",
                hash_hex, e
            ))?;
            let compressed = zstd::encode_all(raw.as_slice(), 0)
                .map_err(|e| format!("Error while compressing a repacked object: {}", e))?;
            Ok(ReadOutcome::Bytes(raw, compressed))
        }
        Source::CopyRecord { .. } => Err("a copied record is not re-deltated".to_string()),
    }
}

/// Everything about one to-be-packed object that can be computed off the sequential write
/// path: its decompressed and zstd-compressed bytes, whether it may be delta'd, and — the
/// expensive part — its *path* delta if one wins. Path deltas are independent (each is the
/// object against the previous version of the same file, fetched from the store) and never
/// seed the sliding window, so they can be computed in parallel ahead of the writer, which
/// then only has to do the sequential window fallback and the append. Produced per batch so
/// the raw bytes held in memory are bounded (see [`prepare_batch`]).
struct Prepared {
    raw: Vec<u8>,
    compressed: Vec<u8>,
    deltable: bool,

    /// A winning path delta as `(base hash, payload)` — `None` when the object has no path
    /// base or the delta did not beat the full blob.
    path_delta: Option<([u8; HASH_LEN], Vec<u8>)>,
}

/// What [`prepare_target`] found for one target. Four states, distinct *by type* — unlike a
/// `corrupt: bool` flag on [`Prepared`], the corrupt state is unappendable: nothing downstream
/// can accidentally `append_*` empty or absent bytes under a corrupt object's hash, because
/// there is no `Prepared` to reach for in that variant at all.
#[derive(Default)]
enum PrepSlot {
    /// A `CopyRecord` — byte-copied verbatim on the sequential write path, so it needs no
    /// preparation. Also what a `Ready` slot decays to once the main loop [`std::mem::take`]s
    /// it (the post-take residue every prior slot in this run has already moved past).
    #[default]
    Copy,
    /// A target ready to pack, with its bytes and any winning path delta.
    Ready(Prepared),
    /// A loose object whose bytes did not decode as zstd, or decoded but do not hash to the
    /// address their filename claims — see `read_target`. Only produced for a *sole-copy*
    /// `Source::Loose` target; a duplicated hash whose loose copy is corrupt is demoted to
    /// `Reconstruct` instead and never reaches this variant. The main loop skips a sole-copy
    /// occurrence before ever fetching a writer or touching the delta window: never packed,
    /// never deleted, left exactly as found for `forklift audit`/`heal` or a future re-fetch.
    CorruptLoose,
    /// A loose object whose decoded size would exceed [`object_utils::MAX_OBJECT_BYTES`] — a
    /// grandfathered giant authored before that ceiling existed, not corruption (see
    /// `read_target`). Only produced for a *sole-copy* `Source::Loose` target; a duplicated
    /// over-ceiling giant is demoted to `Reconstruct` and packed instead of reaching this
    /// variant. Skipped the same way `CorruptLoose` is: never packed, never deleted, left
    /// loose and fully readable via the ordinary (unbounded) read path forever.
    OverCeiling,
}

impl PrepSlot {
    /// Borrow the prepared bytes, or `None` for `Copy`/`CorruptLoose`/`OverCeiling`.
    fn as_ready(&self) -> Option<&Prepared> {
        match self {
            PrepSlot::Ready(prep) => Some(prep),
            PrepSlot::Copy | PrepSlot::CorruptLoose | PrepSlot::OverCeiling => None,
        }
    }
}

/// Prepare a batch of targets, fanning the (read + path-delta compress) across the cores. A
/// `CopyRecord` target needs no preparation (it is byte-copied on the write path), so its slot
/// is [`PrepSlot::Copy`]. Results are positionally aligned with `batch`.
fn prepare_batch(batch: &[PackTarget],
                 path_bases: &HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>) -> Result<Vec<PrepSlot>, String> {
    // Below this many objects the threads cost more than the reads/compressions they share.
    const PARALLEL_THRESHOLD: usize = 8;

    let to_prepare = batch.iter()
        .filter(|target| !matches!(target.source, Source::CopyRecord { .. }))
        .count();

    if to_prepare < PARALLEL_THRESHOLD {
        return batch.iter().map(|target| prepare_target(target, path_bases)).collect();
    }

    // See `fanout_utils::fanout_map` for the fan-out idiom (chunking, worker count, and the
    // storage-scope re-entry every worker needs). It never short-circuits, so the
    // first-index error a serial `.collect()` would report is recovered by collecting the
    // (order-preserved) results the same way here. A corrupt loose object is now an `Ok`
    // variant (`PrepSlot::CorruptLoose`), so only a genuine environment error still aborts the
    // fan-out early.
    fanout_utils::fanout_map(batch, |target| prepare_target(target, path_bases))
        .into_iter()
        .collect()
}

/// Read one target and compute its path delta if one wins — the body of the parallel prep.
/// A `CopyRecord` is [`PrepSlot::Copy`] (it is copied verbatim on the sequential write path); a
/// corrupt loose object is [`PrepSlot::CorruptLoose`], and an over-ceiling one is
/// [`PrepSlot::OverCeiling`] (see `read_target`). The window fallback is *not* done here: it
/// depends on the objects written just before, so it stays on the sequential path.
fn prepare_target(target: &PackTarget,
                  path_bases: &HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>) -> Result<PrepSlot, String> {
    if matches!(target.source, Source::CopyRecord { .. }) {
        return Ok(PrepSlot::Copy);
    }

    let (raw, compressed) = match read_target(target)? {
        ReadOutcome::Bytes(raw, compressed) => (raw, compressed),
        ReadOutcome::CorruptLoose => return Ok(PrepSlot::CorruptLoose),
        ReadOutcome::OverCeiling => return Ok(PrepSlot::OverCeiling),
    };

    // Parcels are stored full, never delta'd (the history walk reads every parcel); an
    // over-large object is not delta'd either.
    let deltable = !is_parcel(&raw) && raw.len() <= MAX_DELTA_OBJECT_SIZE;

    // Prefer the path base — the previous version of this exact file — kept only when the
    // delta beats the full blob.
    let mut path_delta = None;
    if deltable {
        if let Some(base) = path_bases.get(&target.hash) {
            // Borrow-only (the delta is computed against it): share the cached `Arc` rather than
            // copy the base blob out under the read-cache lock. The store read still takes hex
            // (the on-disk/wire address format); the map itself stays binary.
            let base_hex = sign_utils::to_hex(base);
            // A base that cannot be read verified (corrupt loose bytes, or a transient store
            // failure) must not abort the whole compact: path-delta selection is opportunistic
            // compression, never a durability promise. Leave `path_delta` unset and fall through
            // — a chain target with no winning delta packs full from its own already-verified
            // `raw`. A genuinely failing store still aborts, just via the target's own read
            // (`read_target`, above), not this base lookup.
            if let Ok(base_raw) = file_utils::retrieve_object_by_hash_shared(&base_hex) {
                let payload = delta_utils::compress_delta(&base_raw, &raw)?;

                if payload.len() < compressed.len() {
                    path_delta = Some((*base, payload));
                }
            }
        }
    }

    Ok(PrepSlot::Ready(Prepared { raw, compressed, deltable, path_delta }))
}

/// Recover what a full repack can from every loaded pack whose index fails full validation
/// (FORK-194), before the liveness walk reads anything: each such pack's data file is walked
/// record by record ([`walk_pack_records`]) and every record that decodes is stored loose through
/// the ordinary store path ([`object_utils::force_store_object_bytes`], ceilings included), named
/// by the hash of its bytes, so an object only a corrupt bucket held reads verified again. A record
/// that already reads verified is not written again.
/// Salvage writes every record that decodes, garbage included; what the walk leaves unmarked
/// waits for the loose collector.
///
/// A pack is *completely* salvaged when the walk covered its whole data body with no gap, every
/// record decoded and was stored (none refused by the object ceilings, which salvage never
/// bypasses), as many records decoded as the index header counts, and every decoded object now
/// reads verified through the uncached classified read. Every record its data file holds then has
/// a copy outside it, or is one of its sound-bucket records the drop gate probes before the pack
/// goes, so a hash outside its raw record table was never in it: the returned
/// guard registers that exoneration for [`sweep_unreadable_reason`] for as long as it lives. It
/// registers every other damaged pack as incompletely salvaged, so a corrupt-bucket error naming
/// one says that salvage could not recover it.
///
/// # Returns
/// * `Ok((complete, guard))` - The index paths of the completely salvaged packs, and the guard
///   holding every damaged pack's verdict.
/// * `Err(String)`           - A salvaged object could not be written, or the store could not be
///   consulted; nothing has been deleted.
fn salvage_damaged_packs(packs: &PackSet) -> Result<(HashSet<PathBuf>, SalvageRegistration), String> {
    let mut salvages: Vec<PackSalvage> = packs.packs.iter()
        .filter(|pack| pack.validate_full().is_err())
        .map(|pack| PackSalvage::walk(pack))
        .collect();

    // A delta can only decode once its base is readable, and its base may be a later record of
    // its own pack or a record of another damaged pack (incremental compaction deltas a loose
    // object against a packed base): retry what is still waiting, across every damaged pack,
    // until a whole pass makes no progress.
    loop {
        let mut progress = false;
        for salvage in salvages.iter_mut() {
            progress |= salvage.decode_pending()?;
        }
        if !progress {
            break;
        }
    }

    let mut verdicts = Vec::new();
    let mut complete = HashSet::new();
    for salvage in &salvages {
        let index_path = salvage.pack.index_path();
        if salvage.is_complete()? {
            complete.insert(index_path.clone());
            verdicts.push((index_path, Salvaged::Complete(Arc::new(salvage.pack.index.raw_record_hashes()))));
        } else {
            verdicts.push((index_path, Salvaged::Incomplete));
        }
    }

    Ok((complete, SalvageRegistration::register(verdicts)))
}

/// One damaged pack's salvage in progress (see [`salvage_damaged_packs`]).
struct PackSalvage<'a> {
    pack: &'a LoadedPack,
    /// Records walked whose delta base does not read yet.
    pending: Vec<&'a [u8]>,
    /// Hashes of the records decoded and stored so far.
    decoded: Vec<String>,
    /// False once anything rules complete salvage out: a gap in the walk, a record count short of
    /// the index header's, or a record that does not decode or that the ceilings refuse.
    complete: bool,
}

impl<'a> PackSalvage<'a> {
    fn walk(pack: &'a LoadedPack) -> Self {
        let data: &[u8] = &pack.data;

        // A walk that loses sync resumes only where a sound bucket says a record starts.
        let mut resync: Vec<u64> = pack.index.valid_bucket_records().iter().map(|record| record.offset).collect();
        resync.sort_unstable();
        resync.dedup();

        let (records, gap) = walk_pack_records(data, &resync);
        PackSalvage {
            pack,
            complete: !gap && records.len() == pack.index.count(),
            pending: records.into_iter().map(|range| &data[range]).collect(),
            decoded: Vec::new(),
        }
    }

    /// Try every pending record once; `Ok(true)` when at least one decoded.
    fn decode_pending(&mut self) -> Result<bool, String> {
        let mut progress = false;
        let mut waiting = Vec::new();

        for record in std::mem::take(&mut self.pending) {
            match decode_salvaged_record(record)? {
                SalvagedRecord::Decoded(bytes) => {
                    progress = true;
                    let hash = object_utils::hash_object_bytes(&bytes);
                    // Checked before the store so a ceiling refusal (an object this store may not
                    // take in) leaves the salvage incomplete instead of failing the run; a failed
                    // write still fails it.
                    if object_utils::validate_imported_object(&hash, &bytes).is_err() {
                        self.complete = false;
                        continue;
                    }
                    // Written unless a copy already reads verified. A presence check would skip
                    // the write whenever some unreadable copy is listed (a corrupt loose file, a
                    // bad record in another pack), leaving the object unrecovered with its bytes
                    // in hand; the forced write supersedes a corrupt loose file in one rename.
                    if !matches!(file_utils::read_object_classified(&hash)?, file_utils::StoreReadOutcome::Verified(_)) {
                        object_utils::force_store_object_bytes(&hash, &bytes)?;
                    }
                    self.decoded.push(hash);
                }
                SalvagedRecord::BaseUnreadable => waiting.push(record),
                SalvagedRecord::Undecodable => self.complete = false,
            }
        }

        self.pending = waiting;
        Ok(progress)
    }

    /// Whether this pack was completely salvaged, once no damaged pack makes progress.
    fn is_complete(&self) -> Result<bool, String> {
        if !self.complete || !self.pending.is_empty() {
            return Ok(false);
        }
        for hash in &self.decoded {
            if !matches!(file_utils::read_object_classified(hash)?, file_utils::StoreReadOutcome::Verified(_)) {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Walk a pack's data body record by record from its header, without consulting the index: a
/// record's extent is its kind byte, for a delta the base hash and the target length, and then
/// one zstd frame. Where a record does not parse (an unknown kind byte, a frame that does not
/// parse, a length past the end), the walk resumes at the next offset in `resync` (sorted) past
/// it, or stops when there is none.
///
/// # Returns
/// The byte range of every record walked, and whether any byte of the body was left uncovered.
fn walk_pack_records(data: &[u8], resync: &[u64]) -> (Vec<std::ops::Range<usize>>, bool) {
    let mut records = Vec::new();
    let mut gap = false;
    let mut at = PACK_DATA_HEADER_LEN as usize;

    while at < data.len() {
        if let Some(end) = record_end_at(data, at) {
            records.push(at..end);
            at = end;
            continue;
        }

        gap = true;
        match resync.get(resync.partition_point(|&offset| offset as usize <= at)) {
            Some(&offset) => at = offset as usize,
            None => break,
        }
    }

    (records, gap)
}

/// Where the record starting at `at` ends, if its framing parses (see [`walk_pack_records`]).
fn record_end_at(data: &[u8], at: usize) -> Option<usize> {
    let frame_start = match *data.get(at)? {
        RECORD_FULL => at + 1,
        RECORD_DELTA => {
            let body = data.get(at + 1..)?;
            if body.len() < HASH_LEN {
                return None;
            }
            let (_, read) = byte_utils::number_from_vlq_bytes(HASH_LEN, body).ok()?;
            at + 1 + HASH_LEN + read
        }
        _ => return None,
    };

    let length = zstd::zstd_safe::find_frame_compressed_size(data.get(frame_start..)?).ok()?;
    let end = frame_start.checked_add(length)?;
    (end <= data.len()).then_some(end)
}

/// What decoding one walked record gave (see [`decode_salvaged_record`]).
enum SalvagedRecord {
    /// The object's bytes, not yet hashed.
    Decoded(Vec<u8>),
    /// A delta whose base does not read yet; a later pass may find it salvaged.
    BaseUnreadable,
    /// The record does not decode: corrupt, or above the object ceilings.
    Undecodable,
}

/// Decode a record [`walk_pack_records`] found. The bytes are as untrusted as a damaged pack's
/// are, so a full record decodes under the bounded decoder and a delta under its own target
/// ceiling; a delta's base is read through the uncached classified read.
///
/// # Returns
/// * `Ok(record)`  - What decoding gave; a base with no copy that reads verified (yet) is
///   `BaseUnreadable`.
/// * `Err(String)` - The store could not be consulted for the base: never taken as the base being
///   unreadable, which would rule the pack's salvage incomplete on an unknown.
fn decode_salvaged_record(record: &[u8]) -> Result<SalvagedRecord, String> {
    let Some((&kind, body)) = record.split_first() else {
        return Ok(SalvagedRecord::Undecodable);
    };

    Ok(match kind {
        RECORD_FULL => match object_utils::decode_object_bounded(body) {
            Ok(bytes) => SalvagedRecord::Decoded(bytes),
            Err(_) => SalvagedRecord::Undecodable,
        },
        RECORD_DELTA if body.len() >= HASH_LEN => {
            let Ok((target_len, read)) = byte_utils::number_from_vlq_bytes(HASH_LEN, body) else {
                return Ok(SalvagedRecord::Undecodable);
            };
            let base = match file_utils::read_object_classified(&sign_utils::to_hex(&body[..HASH_LEN]))? {
                file_utils::StoreReadOutcome::Verified(base) => base,
                _ => return Ok(SalvagedRecord::BaseUnreadable),
            };
            match delta_utils::decompress_delta(&base, &body[HASH_LEN + read..], target_len as usize) {
                Ok(bytes) => SalvagedRecord::Decoded(bytes),
                Err(_) => SalvagedRecord::Undecodable,
            }
        }
        _ => SalvagedRecord::Undecodable,
    })
}

/// The result of [`collect_targets`]: what to pack, what a repack supersedes, and the packs a
/// `Source::CopyRecord` among `targets` borrows its bytes from.
struct CollectedTargets {
    /// The objects to pack (loose files and/or existing packed records).
    targets: Vec<PackTarget>,
    /// Old pack files a repack supersedes (empty for an incremental `compact`).
    old_packs: Vec<PathBuf>,
    /// The existing packs, kept mapped for the run so every `Source::CopyRecord { pack_index,
    /// .. }` in `targets` can borrow its record straight from `source_packs.packs[pack_index]`'s
    /// mmap (empty for an incremental `compact`, which has no `CopyRecord` targets).
    source_packs: Arc<PackSet>,
    /// The damaged packs scheduled for deletion (see [`CompactStats::damaged_packs_dropped`]).
    damaged_packs_dropped: Vec<String>,
    /// The damaged packs kept (see [`CompactStats::damaged_packs_kept`]).
    damaged_packs_kept: Vec<KeptDamagedPack>,
    /// The damaged packs left alone unsalvaged (see [`CompactStats::damaged_packs_skipped`]).
    damaged_packs_skipped: Vec<String>,
    /// Every damaged pack's salvage verdict, registered for the rest of the reading phase:
    /// `compact` holds it while it reads targets and drops it before anything is deleted.
    salvage_registration: Option<SalvageRegistration>,
}

/// The objects to pack and the old pack files a repack supersedes.
///
/// Incremental: every loose object, no old packs touched. Repack: the live set only — live
/// loose objects, plus live objects in existing packs, and every existing pack file to be
/// deleted once the live set is safely re-packed. A live packed record is **copied verbatim**
/// when its delta base also survives (the fast path); the rare object whose base is being
/// dropped, or whose base is repacked from a different copy than its own pack's, is
/// reconstructed and re-deltated instead. A duplicated live hash rides the same
/// verbatim path only when every copy is byte-identical — a duplicate whose copies differ is
/// demoted to the reconstruct path instead, so survival is decided by which bytes actually
/// verify, not by which copy was enumerated first. Unreachable objects are simply not
/// carried over, so packed garbage is dropped; unreachable *loose* objects are left for the
/// grace-period collector.
///
/// `redelta` (densification, only meaningful when `all` is set) makes every live packed
/// object a `Source::Reconstruct` unconditionally — the same path already used for the rare
/// dropped-base case — instead of `Source::CopyRecord`, so nothing is copied verbatim: the
/// whole live set is re-read and re-offered to path-base/window delta selection (which now
/// credits directories with a path base too, not just files — see `compute_path_bases`).
///
/// A `Reconstruct` target's `size` (the largest-first sort key `compact` uses for whichever
/// targets end up with no path base — see there for the rest of the ordering) comes from the
/// record header when the record is a delta — its raw target length, read straight from the
/// delta's own cleartext VLQ field, the same real-content-size proxy a loose object's file size
/// already stands in for — rather than the delta's own (small, previously packed) on-disk
/// length: an import-produced store is mostly deltas, so using their on-disk length here would
/// sort (and so window-batch) objects by how well they happened to already compress, not by how
/// big they are, which is close to meaningless for grouping similar-sized objects together.
fn collect_targets(all: bool, redelta: bool, salvage: bool) -> Result<CollectedTargets, String> {
    if !all {
        return Ok(CollectedTargets {
            targets: enumerate_loose_objects()?,
            old_packs: Vec::new(),
            source_packs: Arc::new(PackSet::empty()),
            damaged_packs_dropped: Vec::new(),
            damaged_packs_kept: Vec::new(),
            damaged_packs_skipped: Vec::new(),
            salvage_registration: None,
        });
    }

    // Salvage runs before the liveness walk, so a tree only a corrupt bucket held is read from
    // its salvaged copy, and a completely salvaged pack stops counting as a pack that might hold
    // a hash its record table does not name. The registration outlives this function: `compact`
    // holds it while it reads the targets, and ends it before anything is deleted.
    let packs = loaded_packs()?;
    let (completely_salvaged, salvage_registration) = if salvage {
        let (complete, registration) = salvage_damaged_packs(&packs)?;
        (complete, Some(registration))
    } else {
        (HashSet::new(), None)
    };

    let live = crate::util::gc_utils::collect_live_set()?;
    let mut targets = Vec::new();
    // Hash -> index of its already-pushed target in `targets` (FORK-55): a later duplicate of a
    // live hash resolves against the target already selected instead of being skipped outright,
    // so survival is decided by whether the bytes are servable, not by enumeration order.
    let mut seen: HashMap<[u8; HASH_LEN], usize> = HashMap::new();
    // (target index, base hash) of every delta selected for a verbatim copy — see the pass after
    // the loop below.
    let mut copied_deltas: Vec<(usize, [u8; HASH_LEN])> = Vec::new();

    // Live loose objects (read from and deleted with their files). Loose-loose duplicates are
    // impossible -- one fan-out path per hash -- so no collision handling is needed here.
    for target in enumerate_loose_objects()? {
        if live.contains(&sign_utils::to_hex(&target.hash)) {
            seen.insert(target.hash, targets.len());
            targets.push(target);
        }
    }

    // Live objects in existing packs. Copy each record verbatim when its base survives; the old
    // packs are removed at the end, which is what drops the garbage that is not carried over. The
    // mapped packs are returned to the caller and held for the whole run, so a `CopyRecord` reads
    // its bytes as a zero-copy slice out of the mmap (addressed by `pack_index`), never a fresh
    // per-record file read.
    //
    // Each pack's index is validated in full before any record is copied out of it. A damaged
    // one is never copied from: its live records are read through the verified read instead,
    // and it is deleted only when every live object it might hold is accounted for (see
    // `collect_damaged_pack`).
    let mut old_packs = Vec::new();
    let mut damaged_packs_dropped = Vec::new();
    let mut damaged_packs_kept = Vec::new();
    let mut damaged_packs_skipped = Vec::new();

    for (pack_index, pack) in packs.packs.iter().enumerate() {
        let records = match pack.records() {
            Ok(records) => records,
            Err(error) if !salvage => {
                damaged_packs_skipped.push(error);
                continue;
            }
            Err(error) => {
                let complete = completely_salvaged.contains(&pack.index_path());
                let unaccounted = collect_damaged_pack(pack, complete, &live, &mut targets, &mut seen)?;
                let index_path = pack.index_path().to_string_lossy().into_owned();
                if unaccounted.is_empty() {
                    old_packs.push(pack.data_path.clone());
                    old_packs.push(pack.index_path());
                    damaged_packs_dropped.push(index_path);
                } else {
                    damaged_packs_kept.push(KeptDamagedPack { index_path, error, unaccounted });
                }
                continue;
            }
        };

        old_packs.push(pack.data_path.clone());
        old_packs.push(pack.index_path());

        for record in records.iter() {
            let mut hash = [0u8; HASH_LEN];
            hash.copy_from_slice(record.hash);

            if !live.contains(&sign_utils::to_hex(&hash)) {
                continue;
            }

            let (offset, length) = (record.offset, record.length);

            // A duplicate of a hash a previous pack (or the loose enumeration above) already
            // selected (FORK-55). The first-enumerated copy no longer wins unconditionally --
            // resolve against the target already selected instead of skipping this one outright,
            // so which copy survives is decided by whether the bytes are servable, never by
            // enumeration order.
            if let Some(&existing_index) = seen.get(&hash) {
                match &targets[existing_index].source {
                    // Already demoted (by an earlier duplicate, a dropped base, or `redelta`):
                    // it already reads through the exhaustive verified read, so a further
                    // duplicate needs no further action.
                    Source::Reconstruct { .. } => {}
                    // A live loose copy was selected first. Demote it in place to `Reconstruct`
                    // so its bytes are proven by the verified read rather than assumed, carrying
                    // its loose file forward as `loose_source` so it still rides the ordinary
                    // durable-before-destructive sweep once packed (`compact`'s `loose_path`
                    // match).
                    Source::Loose(path) => {
                        let loose_source = Some(path.clone());
                        targets[existing_index].source = Source::Reconstruct { loose_source };
                    }
                    // A verbatim copy from an earlier pack was selected first. Byte-identical
                    // records are interchangeable (W6): keep the existing `CopyRecord` fast path
                    // only when this record matches it exactly; any difference means neither
                    // copy is trusted over the other, so demote to the verified read.
                    Source::CopyRecord {
                        pack_index: existing_pack_index, offset: existing_offset, len: existing_len, ..
                    } => {
                        let existing_record = packs.packs[*existing_pack_index].slice(*existing_offset, *existing_len)?;
                        let candidate_record = pack.slice(offset, length)?;
                        if existing_record != candidate_record {
                            targets[existing_index].source = Source::Reconstruct { loose_source: None };
                        }
                    }
                }
                continue;
            }

            // The record header (kind + base hash + a delta's raw target length) is read
            // straight from the pack's mmap, not a fresh file read — needed unconditionally now
            // (not skipped under `redelta` the way it used to be): a delta's *on-disk* length is
            // its payload size, which for a well-delta'd store is small and essentially
            // unrelated to the object's real size, so using it as the largest-first sort key
            // (below) would pair objects by how well they happened to already compress, not by
            // how big they are — scrambling the very order the size-window fallback depends on.
            let header = read_record_header(pack, offset, length)?;

            // A delta whose base is not itself live cannot be copied (the base is being
            // dropped); reconstruct and re-delta it. Everything else is copied as-is — unless
            // `redelta` asked for every live object to be a recompression candidate, in which
            // case always reconstruct.
            let base_dropped = header.is_delta && header.base.is_some_and(|b| !live.contains(&sign_utils::to_hex(&b)));
            let source = if redelta || base_dropped {
                Source::Reconstruct { loose_source: None }
            } else {
                Source::CopyRecord { pack_index, offset, len: length, is_delta: header.is_delta }
            };

            // A delta's raw target length is free (it is stored in cleartext right after the
            // base hash — see `read_record_header`); a full record has no such field without
            // decompressing it, so its on-disk (compressed) length is the best cheap proxy —
            // the same one a loose object's file size already stands in for.
            let size = header.raw_len.unwrap_or(length);

            if let (Source::CopyRecord { is_delta: true, .. }, Some(base)) = (&source, header.base) {
                copied_deltas.push((targets.len(), base));
            }

            seen.insert(hash, targets.len());
            targets.push(PackTarget { hash, size, source });
        }
    }

    // A copied delta whose base was demoted to the verified read is reconstructed too. Its record
    // was encoded against whatever the base's old copy reconstructed to, and a read that does not
    // verify intermediate bases (`resolve_record`) can serve it even when that copy is corrupt;
    // once the base is repacked from the copy that verifies, the verbatim delta would decode
    // against different bytes and the object would be lost without ever having been read. Read
    // here, it is either re-encoded from bytes that verify or fails the run loudly. One pass is
    // enough: a reconstructed object's bytes are fixed by its hash, so a delta against *it* still
    // decodes after the repack. The cost lands on a harmless duplicate too (a base both loose and
    // packed after an interrupted sweep): its deltas are re-encoded once, by the `compact --all`
    // that also removes the duplicate.
    for (index, base) in copied_deltas {
        let base_reread = seen.get(&base)
            .is_some_and(|&base_index| matches!(targets[base_index].source, Source::Reconstruct { .. }));
        if base_reread {
            targets[index].source = Source::Reconstruct { loose_source: None };
        }
    }

    Ok(CollectedTargets {
        targets, old_packs, source_packs: packs, damaged_packs_dropped, damaged_packs_kept, damaged_packs_skipped,
        salvage_registration,
    })
}

/// A full repack's handling of one damaged pack (FORK-194), after salvage: make each live object
/// its sound buckets serve a target, and decide whether the pack may be deleted. Returns the live
/// hashes the pack might hold that no other copy serves verified, sorted; the pack may be deleted
/// only when there are none.
///
/// Each live record of a sound bucket is first read through the verified read. One that reads
/// becomes a `Source::Reconstruct` target, never a verbatim copy, since the pack is not trusted
/// whole. One that does not is no target at all and keeps the pack: a target that cannot be read
/// would fail the whole run, and a healthy pack's verbatim delta against it would be demoted and
/// fail with it, where leaving it alone keeps every copy as it is. The probe runs before the
/// duplicate check, so a copy selected earlier is never demoted on behalf of a failed read.
///
/// Then the drop gate: every live hash the pack locates in a corrupt bucket must either read
/// verified through the uncached classified read, or the pack must have been completely salvaged
/// (`complete`) and its raw record table not name the hash. The liveness walk passing is not
/// enough, since it never reads a blob or a chunk.
fn collect_damaged_pack(
    pack: &LoadedPack,
    complete: bool,
    live: &HashSet<String>,
    targets: &mut Vec<PackTarget>,
    seen: &mut HashMap<[u8; HASH_LEN], usize>,
) -> Result<Vec<String>, String> {
    use crate::util::file_utils::StoreReadOutcome;

    let mut unaccounted = Vec::new();

    for record in pack.index.valid_bucket_records() {
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(record.hash);
        let hex = sign_utils::to_hex(&hash);

        if !live.contains(&hex) {
            continue;
        }

        if !matches!(file_utils::read_object_classified(&hex)?, StoreReadOutcome::Verified(_)) {
            unaccounted.push(hex);
            continue;
        }

        // The selected copy is unproven and the verified read above may have been served by this
        // pack, which is about to go: reconstruct from a verified read rather than trust it.
        if let Some(&existing_index) = seen.get(&hash) {
            let loose_source = match &targets[existing_index].source {
                Source::Reconstruct { .. } => continue,
                Source::Loose(path) => Some(path.clone()),
                Source::CopyRecord { .. } => None,
            };
            targets[existing_index].source = Source::Reconstruct { loose_source };
            continue;
        }

        // The size is only a sort key; a header that does not parse falls back to the on-disk
        // length rather than failing a record that has just read verified.
        let size = read_record_header(pack, record.offset, record.length).ok()
            .and_then(|header| header.raw_len)
            .unwrap_or(record.length);

        seen.insert(hash, targets.len());
        targets.push(PackTarget { hash, size, source: Source::Reconstruct { loose_source: None } });
    }

    let raw_table = complete.then(|| pack.index.raw_record_hashes());
    for hex in live {
        let Some(hash) = hash_to_bytes(hex) else {
            continue;
        };
        if !matches!(pack.locate(&hash), Located::CorruptBucket(_)) {
            continue;
        }
        if raw_table.as_ref().is_some_and(|raw_table| !raw_table.contains(&hash)) {
            continue;
        }
        if !matches!(file_utils::read_object_classified(hex)?, StoreReadOutcome::Verified(_)) {
            unaccounted.push(hex.clone());
        }
    }

    unaccounted.sort();
    Ok(unaccounted)
}

/// A record's kind and, for a delta, its base hash and the raw (decompressed) length of the
/// object it encodes — see [`read_record_header`].
#[derive(Debug)]
struct RecordHeader {
    is_delta: bool,
    base: Option<[u8; HASH_LEN]>,
    /// A delta's raw target length, read straight from its own cleartext VLQ field — no
    /// decompression needed. `None` for a full record: its raw length is not cheaply knowable
    /// without decompressing the zstd blob, so a caller wanting a sort-key proxy for one falls
    /// back to its on-disk (compressed) length instead (see `collect_targets`).
    raw_len: Option<u64>,
}

/// Read a record's kind and (for a delta) its base hash and raw target length, without
/// reconstructing it — just the leading kind byte, the 32-byte base that follows a delta, and
/// the VLQ length after that (`base hash (32) || target length (VLQ) || zstd delta`, the layout
/// `PackWriter::append_delta` writes).
fn read_record_header(pack: &LoadedPack, offset: u64, len: u64) -> Result<RecordHeader, String> {
    let kind = pack.slice(offset, 1)?[0];
    if kind != RECORD_DELTA {
        return Ok(RecordHeader { is_delta: false, base: None, raw_len: None });
    }

    // The whole record, bounded by its own on-disk length (from the index) — safe to hand to
    // the VLQ decoder as-is: it stops at the first byte whose high bit is clear, never reading
    // past what is passed in.
    let record = pack.slice(offset, len)?;

    // `record`'s length is the index's declared record length, not a promise it actually holds a
    // base hash: a native writer never emits a delta shorter than this, but a locally loaded pack
    // is only checked for index-level consistency (`index_format` — offsets/lengths in bounds, and
    // under full validation exactly covering the data file, no per-record shape check), not for
    // whether *this* record's declared length is enough for what the kind byte says it is. A transport-imported pack
    // is separately re-verified against every hash it claims to hold, but a pack read straight off
    // local disk is not. Bounds-check before indexing, so on-disk corruption fails this compact
    // cleanly instead of panicking on a slice out of range (the fuzz suite's posture: reject, never
    // panic, on any malformed input).
    if record.len() < 1 + HASH_LEN {
        return Err(format!(
            "Pack \"{}\" delta record at offset {} is truncated (no base hash).",
            pack.data_path.to_string_lossy(), offset
        ));
    }

    let mut base = [0u8; HASH_LEN];
    base.copy_from_slice(&record[1..1 + HASH_LEN]);

    // `number_from_vlq_bytes` is itself bounds-safe on truncated input (it stops at `content.len()`
    // and errors rather than indexing past it — see its own doc comment), so a delta record long
    // enough for its base hash but truncated mid-VLQ fails cleanly here too, without a further
    // explicit length check.
    let (raw_len, _) = byte_utils::number_from_vlq_bytes(1 + HASH_LEN, record)?;

    Ok(RecordHeader { is_delta: true, base: Some(base), raw_len: Some(raw_len) })
}

/// The first pack (if any) among `packs` that holds `hash`, with its record's `(offset, length)`
/// — the same "which pack, if any" search `retrieve_from_packs`/`is_in_packs` do, factored out
/// for the depth ledger below, which needs the pack itself (to read the record's header) rather
/// than its resolved bytes. A pack whose bucket for `hash` is corrupt is skipped, as a miss.
fn locate_in_packs<'a>(hash: &[u8; HASH_LEN], packs: &'a [LoadedPack]) -> Option<(&'a LoadedPack, u64, u64)> {
    packs.iter().find_map(|pack| pack.locate(hash).found().map(|(offset, length)| (pack, offset, length)))
}

/// The *true* current reconstruction depth of `hash` — 0 for a full record or a loose file,
/// `1 + true_depth(base)` for a delta — computed from record **headers only** (`read_record_header`,
/// no decompression), memoized in `known` across the whole compact run.
///
/// This is the real ledger `compute_path_bases`'s own bookkeeping cannot be (see
/// `MAX_RECONSTRUCT_DEPTH`'s doc comment): it does not matter whether `hash` is a chain root that
/// won a size-window delta, a `CopyRecord` object whose shape this run never re-derives, or an
/// object this run has *already* decided (via `known`, checked first and authoritative — a
/// hash's on-disk shape from an old pack is never trusted once this run has re-decided it, since
/// `redelta` may re-encode it as something else entirely). `known` is exactly `compact`'s
/// depth-safety ledger; this function is how it answers a query it does not already have memoized,
/// by resolving the on-disk chain of whichever pack currently holds `hash`.
///
/// Walks iteratively, not recursively: an *existing* store may currently hold a chain deep enough
/// (up to `MAX_RECONSTRUCT_DEPTH`, the read-side backstop) that native call-stack recursion would
/// risk overflow resolving it — exactly the kind of store this ledger exists to repair. Bounded by
/// the same backstop, so a corrupt or adversarial pack that chains without end fails this compact
/// cleanly instead of spinning forever.
fn true_depth(hash: [u8; HASH_LEN], known: &mut HashMap<[u8; HASH_LEN], u32>, packs: &[LoadedPack]) -> Result<u32, String> {
    if let Some(&depth) = known.get(&hash) {
        return Ok(depth);
    }

    // Every hop from `hash` down to (but not including) whichever node ends the walk below.
    let mut chain: Vec<[u8; HASH_LEN]> = Vec::new();
    let mut current = hash;

    let terminal_depth = loop {
        if let Some(&depth) = known.get(&current) {
            break depth;
        }

        let Some((pack, offset, length)) = locate_in_packs(&current, packs) else {
            break 0; // Not packed at all: a loose file, always stored full.
        };

        let header = read_record_header(pack, offset, length)?;

        if !header.is_delta {
            break 0;
        }

        if chain.len() as u32 >= MAX_RECONSTRUCT_DEPTH {
            return Err(format!(
                "Object {} exceeds the reconstruction depth limit while computing its true depth (corrupt pack?).",
                sign_utils::to_hex(&current)
            ));
        }

        chain.push(current);
        current = header.base.expect("a delta record's header always carries a base");
    };

    // Unwind: `current` (whatever ended the walk) has `terminal_depth`; each hop in `chain`,
    // walked oldest-to-newest in reverse (i.e. newest-recorded-first), is one level deeper than
    // the one after it. Memoizing every hop, not just `hash`, makes a later query for any of
    // them (a common case: the same base shared by several dependents) O(1).
    let mut depth = terminal_depth;
    known.insert(current, depth);
    for h in chain.into_iter().rev() {
        depth += 1;
        known.insert(h, depth);
    }

    Ok(*known.get(&hash).expect("hash was just inserted, if it was not already known"))
}

/// How many bits a Bloom filter probes per key (tuned with ~10 bits/element for ~1% false
/// positives — see [`Bloom`]).
const BLOOM_PROBES: usize = 7;

/// A Bloom filter for the path-base walk's "seen" sets, so their memory is bounded by a chosen
/// bit budget instead of growing to one entry per reachable object (which at kernel scale runs
/// to hundreds of MB). A false positive only makes the walk *skip* an object — it then gets no
/// path base and falls back to the size window: a smaller delta, never a wrong object, because
/// the content-address check is the real safety net. There are no false negatives.
struct Bloom {
    bits: Vec<u64>,
    /// The bit count minus one (the count is a power of two), for masking a probe into range.
    mask: usize,
}

impl Bloom {
    /// A filter sized for roughly `expected` elements at about a 1% false-positive rate (~10
    /// bits per element), with a floor so a tiny repo still gets a usable filter.
    fn new(expected: usize) -> Bloom {
        let want_bits = expected.max(4096).saturating_mul(10).next_power_of_two();
        let words = (want_bits / 64).max(1);
        Bloom { bits: vec![0u64; words], mask: words * 64 - 1 }
    }

    /// Two independent 64-bit hashes of a key (FNV-1a variants), for double hashing.
    fn hashes(key: &[u8]) -> (u64, u64) {
        let mut h1: u64 = 0xcbf29ce484222325;
        let mut h2: u64 = 0x100000001b3;
        for &byte in key {
            h1 = (h1 ^ byte as u64).wrapping_mul(0x100000001b3);
            h2 = (h2 ^ byte as u64).wrapping_mul(0xcbf29ce484222325);
        }
        (h1, h2 | 1)
    }

    fn contains(&self, key: &[u8]) -> bool {
        let (h1, h2) = Self::hashes(key);
        (0..BLOOM_PROBES).all(|i| {
            let position = h1.wrapping_add((i as u64).wrapping_mul(h2)) as usize & self.mask;
            self.bits[position >> 6] & (1u64 << (position & 63)) != 0
        })
    }

    fn insert(&mut self, key: &[u8]) {
        let (h1, h2) = Self::hashes(key);
        for i in 0..BLOOM_PROBES {
            let position = h1.wrapping_add((i as u64).wrapping_mul(h2)) as usize & self.mask;
            self.bits[position >> 6] |= 1u64 << (position & 63);
        }
    }
}

/// Path-aware base selection (phase 2b): for every reachable blob *and tree*, the previous
/// version at the *same path* as its delta base — the ideal base git's name-sorted packer
/// picks, which the size heuristic can only approximate. A directory changes one entry per
/// commit and so deltas against its own previous version just as well as a file does — the
/// same locality `import_git`'s pack-direct pipeline already exploits on the way in (its
/// `latest_tree_at_path`) — so a `compact --redelta` recomputing bases from scratch must credit
/// trees too, or every one of them (typically a third or more of a repo's objects) falls back
/// to the size window on every redelta, not just the rare object whose base was dropped as
/// garbage. Returns `hash → base hash` for both kinds in one map, both as raw 32-byte Blake3
/// digests (not hex) — this map is purely an in-memory intermediate consulted once per object
/// during packing, never serialized, so a fixed `[u8; HASH_LEN]` key avoids a 64-byte hex
/// `String` (allocation + `Eq`/`Hash` over 64 bytes) per entry in favour of a stack-sized array
/// — a real win at the object counts this map is sized for (one entry per reachable blob or
/// tree). An object with no entry (a parcel, a first version at its path, or an unreachable
/// object) has no path base and falls back to the size window.
///
/// This walks the reachable DAG — all parcels and their trees, but never blob *content* —
/// mirroring the bundle traversal (`bundle_utils`), and bounds each chain to `MAX_DELTA_CHAIN`
/// so reconstruction recursion stays bounded. Every "seen" set is a Bloom filter, so the walk's
/// memory is bounded (a bit budget) rather than one entry per object — what keeps it viable at
/// kernel scale (see [`Bloom`]).
fn compute_path_bases() -> Result<PathBases, String> {
    let heads: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter().map(|(_, head)| head).collect();

    let reachable = audit_utils::collect_reachable(&heads)?;
    // Oldest first, so a file's (or a directory's) earlier version is visited before the later
    // version that will delta against it.
    let order = bundle_utils::topo_order_oldest_first(&reachable)?;

    // A repo averages several objects (trees + blobs) per parcel; size the Bloom filters from
    // that estimate so they are bounded and roughly right for both small and huge histories.
    let estimate = reachable.len().saturating_mul(5);
    let mut walk = PathBaseWalk {
        seen_subtrees: Bloom::new(estimate),
        seen_blobs: Bloom::new(estimate),
        seen_trees: Bloom::new(estimate),
        latest_blob_at_path: HashMap::new(),
        latest_tree_at_path: HashMap::new(),
        base_of: HashMap::new(),
        sequence: HashMap::new(),
        next_sequence: 0,
    };

    for parcel_hash in &order {
        let tree_hash = object_utils::load_parcel(parcel_hash)?.tree_hash;
        walk_tree_for_bases(&tree_hash, "", &mut walk)?;
    }

    Ok(PathBases { base_of: walk.base_of, sequence: walk.sequence })
}

/// The output of [`compute_path_bases`]: candidate path bases, plus the walk order that makes
/// them safe to act on out of packing's largest-first order (see `compact`'s write-time depth
/// ledger).
struct PathBases {
    /// hash → its candidate delta base. Not a promise the base is depth-safe to actually delta
    /// against — `compute_path_bases`'s own `MAX_DELTA_CHAIN` bookkeeping only counts path hops
    /// since a reset, blind to a reset point's own real depth (see `MAX_RECONSTRUCT_DEPTH`'s doc
    /// comment) — so `compact` re-checks every candidate's *true* depth at write time before
    /// committing to it.
    base_of: HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>,
    /// hash → the order this walk (oldest-parcel-first) established it in. `base_of[h]`'s
    /// sequence number is always lower than `h`'s own — a base is always recorded (fixed, or
    /// re-affirmed as the path's latest) strictly before the object that names it as a base is
    /// processed — so sorting by this number, instead of by size, gives `compact` a safe order
    /// to resolve a whole chain's true depths in: every base before its dependent.
    sequence: HashMap<[u8; HASH_LEN], u64>,
}

/// Mutable state threaded through [`walk_tree_for_bases`] (see [`compute_path_bases`]).
struct PathBaseWalk {
    /// Skips re-descending into an already-fully-walked (unchanged) subtree's *children* — a
    /// pure recursion dedup (an identical subtree carries no new blob or tree versions beneath
    /// it), orthogonal to `seen_blobs`/`seen_trees` below, which decide which objects a path
    /// base is fixed for.
    seen_subtrees: Bloom,
    /// Fixes each blob's base on first encounter (see [`record_path_base`]).
    seen_blobs: Bloom,
    /// Fixes each tree's base on first encounter — a separate Bloom from `seen_blobs` so a
    /// false positive in one can never suppress the other's base.
    seen_trees: Bloom,
    /// The newest blob at each path so far, and its chain depth. Bounded by the number of
    /// distinct file paths (not by history depth).
    latest_blob_at_path: HashMap<String, ([u8; HASH_LEN], u32)>,
    /// The newest tree (directory) at each path so far, and its chain depth. Kept as a map
    /// separate from `latest_blob_at_path`: a path that holds a file in one commit and a
    /// directory in another (a rename over a deleted entry of the other kind) must never delta
    /// one kind against the other, and reusing one map keyed only on path string could pair
    /// them by coincidence.
    latest_tree_at_path: HashMap<String, ([u8; HASH_LEN], u32)>,
    /// hash → its delta base — the walk's output (see [`compute_path_bases`]). Blob and tree
    /// hashes share this one map safely: both are content addresses of disjoint byte encodings,
    /// so a tree's base is always another tree and a blob's base always another blob.
    base_of: HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>,
    /// hash → the sequence number it was last recorded under (see [`PathBases::sequence`]).
    sequence: HashMap<[u8; HASH_LEN], u64>,
    /// The next sequence number to hand out — one global counter across blobs and trees alike,
    /// so a tree and the blobs beneath it still sort correctly relative to each other (not that
    /// it matters for their own chains, which never cross kinds, but a single counter is simpler
    /// than two and costs nothing).
    next_sequence: u64,
}

/// Walk one tree's closure, recording the tree's *own* path base and then each blob's (see
/// [`compute_path_bases`]). Recursion into a subtree's children is deduplicated by tree hash: an
/// identical (unchanged) subtree carries no new versions beneath it, so descending again is
/// skipped — the same optimisation the bundle walk makes. (A Bloom false positive skips a
/// subtree that was not in fact seen; its contents then fall back to the size window.) That
/// dedup is *only* about children — the tree's own path base is fixed on every occurrence, same
/// as a blob's, since even a repeated (reverted) directory is a legitimate base for whatever
/// comes next at that path.
fn walk_tree_for_bases(tree_hash: &str, path_prefix: &str, walk: &mut PathBaseWalk) -> Result<(), String> {
    let already_expanded = walk.seen_subtrees.contains(tree_hash.as_bytes());
    if !already_expanded {
        walk.seen_subtrees.insert(tree_hash.as_bytes());
    }

    // Presence-tolerant descent, matching the live-set walk (`gc_utils::collect_live_set`). A
    // subtree object can be legitimately absent — sealed by hash in a signed parcel but never
    // fetched into this warehouse. There is no path base to compute for content we do not hold,
    // and by the store invariant nothing beneath an absent subtree is present here either, so
    // stopping loses no base for any object that will actually be packed (every pack target is a
    // present object). This guard does not fire on a full store in the only flow that reaches it:
    // `compact` holds the store lock for its entire run, so no external repack can move an object
    // mid-walk, and `compact` runs only inside a short-lived, single-shot CLI process, so the pack
    // registry this walk reads was populated fresh by this same run, never carried over stale from
    // an earlier one. For a store missing some paths the output is deterministic given the same
    // present set: the same objects are present, so the same bases are computed. Checked on every
    // occurrence, not just the first: presence is a pure property of the hash (invariant for the
    // whole run, the store lock is held throughout), so this never contradicts an earlier result —
    // and packed objects resolve it with a syscall-free resident-index lookup (`is_in_packs`), so
    // paying it again on a repeat is cheap in the common (non-sparse) case this scales for.
    if !file_utils::does_object_exist(tree_hash)? {
        return Ok(());
    }

    // A tree entry's hash is always a valid content address (it was written as one); the rare
    // unparseable case (a corrupt tree) simply gets no path base here rather than aborting the
    // walk — the same fallback a Bloom false positive already causes, and the object read/verify
    // path is what actually guards against corruption.
    if let Some(tree_hash_bytes) = hash_to_bytes(tree_hash) {
        record_path_base(tree_hash_bytes, tree_hash, path_prefix,
                         &mut walk.seen_trees, &mut walk.latest_tree_at_path, &mut walk.base_of,
                         &mut walk.sequence, &mut walk.next_sequence);
    }

    if already_expanded {
        return Ok(());
    }

    let tree = object_utils::load_tree(tree_hash)?;

    for (name, file) in tree.get_files() {
        let path = join_path(path_prefix, name);
        if let Some(blob_hash) = hash_to_bytes(&file.hash) {
            // The Bloom filter is keyed on the *hex string* bytes, not the raw digest — pinned
            // deliberately, not an oversight: a Bloom filter is probabilistic, so which bytes it
            // hashes decides its false-positive *pattern*, and a false positive here changes
            // which blob gets a path-base delta vs. falls back to the size window — i.e. it can
            // change the packed bytes. Switching this encoding would silently change that pattern
            // (and so, at a scale where a false positive actually fires, the pack's bytes) even
            // though `base_of`/`latest_blob_at_path` themselves are exact `HashMap`s and safe to
            // key on `[u8; HASH_LEN]` (see `compute_path_bases`'s doc comment) — an exact lookup
            // gives an identical answer under any equivalent key encoding, but a hash-bucketed
            // probabilistic structure does not. So this call keeps hashing exactly what the
            // pre-D/P3 code hashed.
            record_path_base(blob_hash, &file.hash, &path,
                             &mut walk.seen_blobs, &mut walk.latest_blob_at_path, &mut walk.base_of,
                             &mut walk.sequence, &mut walk.next_sequence);
        }
    }

    for (name, subtree) in tree.get_subtrees() {
        let child = join_path(path_prefix, name);
        walk_tree_for_bases(&subtree.hash, &child, walk)?;
    }

    Ok(())
}

/// Record one object's (a blob's or a tree's) path base: the most recent object of the same kind
/// seen at this path (if its chain is not yet at the limit), and update the latest-at-path so
/// the next version chains from this one. Also stamps `hash` with the current walk-order
/// sequence number, regardless of which branch below fires — see [`PathBases::sequence`] for why
/// every occurrence (not just a fresh `base_of` entry) needs one: a later object's base is
/// whatever `latest_at_path` holds *at that moment*, so the base's own sequence must always be
/// stamped strictly before, and the base is not always a fresh entry (a reverted/repeated
/// object just re-affirms the path without creating one).
///
/// Takes both `hash` (raw, for the exact `base_of`/`latest_at_path` maps) and `hash_hex` (for
/// the probabilistic `seen` Bloom filter) — see the call site's doc comment for why the Bloom
/// filter deliberately keeps hashing the hex-string bytes rather than switching to the raw ones
/// the maps now use.
// The seven state parameters are each a distinct, independently-borrowed field of `PathBaseWalk`
// (blob and tree callers pass different ones for most of them); bundling them into one struct
// parameter would just move the same borrows behind a name without shrinking anything real.
#[allow(clippy::too_many_arguments)]
fn record_path_base(hash: [u8; HASH_LEN],
                    hash_hex: &str,
                    path: &str,
                    seen: &mut Bloom,
                    latest_at_path: &mut HashMap<String, ([u8; HASH_LEN], u32)>,
                    base_of: &mut HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>,
                    sequence: &mut HashMap<[u8; HASH_LEN], u64>,
                    next_sequence: &mut u64) {
    let seq = *next_sequence;
    *next_sequence += 1;
    sequence.insert(hash, seq);

    // First time this object is seen fixes its base; a later appearance (or a Bloom false
    // positive) only advances the path. Its real chain depth is not tracked per object (that
    // would defeat the bounded-memory point), so the recorded depth restarts at 0 here — which
    // makes this bookkeeping only a coarse, approximate pre-filter on how many candidates
    // `compact` even considers (real chain length can run a small multiple of `MAX_DELTA_CHAIN`);
    // it is `compact`'s own write-time depth ledger that enforces the real bound (see
    // `MAX_RECONSTRUCT_DEPTH`'s doc comment). That approximation is still safe here regardless:
    // base pointers stay acyclic so any resolution of them terminates.
    if seen.contains(hash_hex.as_bytes()) {
        latest_at_path.insert(path.to_string(), (hash, 0));
        return;
    }
    seen.insert(hash_hex.as_bytes());

    let mut depth = 0;

    if let Some((base, base_depth)) = latest_at_path.get(path) {
        if *base_depth < MAX_DELTA_CHAIN && *base != hash {
            base_of.insert(hash, *base);
            depth = base_depth + 1;
        }
    }

    latest_at_path.insert(path.to_string(), (hash, depth));
}

/// Join a warehouse path prefix and an entry name (`""` prefix yields the bare name).
fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// A chosen delta: the base's hash, the delta payload, and the chain depth of the base.
type DeltaChoice = ([u8; HASH_LEN], Vec<u8>, u32);

/// The smallest delta of `target` against any window base whose chain is not yet at the
/// limit, as `(base hash, delta payload, base depth)` — or `None` if the window is empty.
/// Newest (most similar) bases are tried first.
fn best_delta(target: &[u8], window: &VecDeque<WindowEntry>) -> Result<Option<DeltaChoice>, String> {
    let mut best: Option<DeltaChoice> = None;

    for base in window.iter().rev() {
        if base.depth >= MAX_DELTA_CHAIN {
            continue;
        }

        let delta = delta_utils::compress_delta(&base.raw, target)?;

        if best.as_ref().is_none_or(|(_, payload, _)| delta.len() < payload.len()) {
            best = Some((base.hash, delta, base.depth));
        }
    }

    Ok(best)
}

/// Enumerate the loose objects of the active warehouse: the files under the two-hex fan-out
/// folders, excluding signature sidecars, in-progress temp files, and anything that is not a
/// valid object hash. Each carries its compressed on-disk size (for the packing order).
fn enumerate_loose_objects() -> Result<Vec<PackTarget>, String> {
    let objects_root = PathBuf::from(file_utils::get_path_objects_root());
    let mut loose = Vec::new();

    let folders = std::fs::read_dir(&objects_root)
        .map_err(|e| format!("Error while reading the objects folder: {}", e))?;

    for folder in folders {
        let folder = folder.map_err(|e| format!("Error while listing the objects folder: {}", e))?;
        let prefix = folder.file_name().to_string_lossy().to_string();

        // The object store fans out on the first two hex characters of the hash; the pack
        // folder (and any other non-fan-out entry) is not one of those, so it is skipped.
        if !file_utils::is_fan_out_folder_name(&prefix) {
            continue;
        }

        let files = std::fs::read_dir(folder.path())
            .map_err(|e| format!("Error while reading an objects folder: {}", e))?;

        for file in files {
            let file = file.map_err(|e| format!("Error while listing an objects folder: {}", e))?;
            let name = file.file_name().to_string_lossy().to_string();

            // Sidecars stay loose (read by path, not hash); temp files are half-written.
            if name.ends_with(sign_utils::FILE_SUFFIX_SIGNATURE) || name.contains(".tmp") {
                continue;
            }

            // Packed under the path builder's spelling, and only where that spelling reaches
            // the entry: elsewhere no read ever opens it.
            let Some((prefix, name)) = file_utils::canonical_loose_entry(&folder.path(), &name) else {
                continue;
            };

            let hash = format!("{}{}", prefix, name);
            let Some(hash_bytes) = hash_to_bytes(&hash) else {
                // Not a valid object hash — leave it untouched rather than pack junk.
                continue;
            };

            let size = file.metadata()
                .map_err(|e| format!("Error while reading loose object metadata: {}", e))?
                .len();

            loose.push(PackTarget { hash: hash_bytes, size, source: Source::Loose(file.path()) });
        }
    }

    Ok(loose)
}

/// Accumulates objects into one pack: an append-only data file plus the records for its
/// index, and the loose paths to delete once the pack is durable.
struct PackWriter {
    pack_folder: PathBuf,
    data_temp_path: PathBuf,
    data_writer: BufWriter<std::fs::File>,
    /// (hash, offset, length) of each blob, for the index.
    records: Vec<([u8; HASH_LEN], u64, u64)>,
    /// The loose files this pack now holds, deleted only after it is durably written.
    sources: Vec<PathBuf>,
    /// The next write offset in the data file (past the header initially).
    offset: u64,
}

impl PackWriter {
    /// Start a new pack, writing to a temp data file in the pack folder.
    fn new(pack_folder: &Path) -> Result<PackWriter, String> {
        let data_temp_path = temp_path(pack_folder, PACK_DATA_EXTENSION);
        let file = std::fs::File::create(&data_temp_path).map_err(|e| format!(
            "Error while creating pack file \"{}\": {}", data_temp_path.to_string_lossy(), e
        ))?;
        let mut data_writer = BufWriter::new(file);

        data_writer.write_all(PACK_DATA_MAGIC)
            .and_then(|_| data_writer.write_all(&PACK_FORMAT_VERSION.to_le_bytes()))
            .map_err(|e| format!("Error while writing pack header: {}", e))?;

        Ok(PackWriter {
            pack_folder: pack_folder.to_path_buf(),
            data_temp_path,
            data_writer,
            records: Vec::new(),
            sources: Vec::new(),
            offset: PACK_DATA_HEADER_LEN,
        })
    }

    /// Append a full record (`RECORD_FULL` then the object's zstd blob, as a loose file
    /// holds it). Returns the number of bytes the record occupies in the pack.
    fn append_full(&mut self, hash: [u8; HASH_LEN], compressed: &[u8], source: Option<PathBuf>) -> Result<u64, String> {
        let mut record = Vec::with_capacity(1 + compressed.len());
        record.push(RECORD_FULL);
        record.extend_from_slice(compressed);
        self.write_record(hash, &record, source)
    }

    /// Append a delta record (`RECORD_DELTA` then `base hash (32) || target length (VLQ) ||
    /// zstd delta payload`). Returns the number of bytes the record occupies in the pack.
    fn append_delta(&mut self,
                    hash: [u8; HASH_LEN],
                    base: [u8; HASH_LEN],
                    target_len: u64,
                    payload: &[u8],
                    source: Option<PathBuf>) -> Result<u64, String> {
        let length = byte_utils::number_to_vlq_bytes(target_len);

        let mut record = Vec::with_capacity(1 + HASH_LEN + length.len() + payload.len());
        record.push(RECORD_DELTA);
        record.extend_from_slice(&base);
        record.extend_from_slice(&length);
        record.extend_from_slice(payload);
        self.write_record(hash, &record, source)
    }

    /// Append an already-framed record verbatim (a repack copying a live object's existing
    /// record). Its old pack is removed at the end, so there is no loose source to track.
    fn append_raw_record(&mut self, hash: [u8; HASH_LEN], record: &[u8]) -> Result<u64, String> {
        self.write_record(hash, record, None)
    }

    /// Write a framed record to the data file, indexing it and remembering the loose file it
    /// replaces (if any — an object repacked from an existing pack has no loose file, its old
    /// pack being removed at the end instead). Returns the record's length.
    fn write_record(&mut self, hash: [u8; HASH_LEN], record: &[u8], source: Option<PathBuf>) -> Result<u64, String> {
        self.data_writer.write_all(record)
            .map_err(|e| format!("Error while writing to pack: {}", e))?;

        let length = record.len() as u64;
        self.records.push((hash, self.offset, length));
        if let Some(source) = source {
            self.sources.push(source);
        }
        self.offset += length;

        Ok(length)
    }

    /// Whether this pack has reached a rollover threshold and should be finalized.
    fn should_roll_over(&self) -> bool {
        self.offset >= PACK_ROLLOVER_BYTES || self.records.len() >= PACK_ROLLOVER_OBJECTS
    }

    /// Finish the pack: flush and fsync the data file, write the sorted index, then rename
    /// both into place. The order is **data first, index last**: readers enumerate `.idx`
    /// files and open the matching `.pack`, so the index is the commit point — it must appear
    /// only *after* its data is fully present (renaming index-before-data would let a reader
    /// see an index with no data). A same-named pack rewrite (a differently-laid-out pack of the
    /// same object set) could otherwise pair a freshly renamed data file with the not-yet-replaced
    /// index of the old one — the fix for that is a layout-derived id alone, not an
    /// index-before-data reorder: a layout-derived id (see
    /// `compute_pack_id`) means a differently-laid-out pack of the same object set gets a
    /// *different* name and is written fresh rather than overwriting this pair. A same-name
    /// rewrite therefore always has the same *layout* — the same records at the same offsets and
    /// lengths — but not necessarily the same bytes: a repack that demotes a duplicated hash
    /// re-encodes that record's content fresh (see `collect_targets`), and a fresh encode can land
    /// on the replaced record's exact length. The rewrite is safe on layout identity plus
    /// read-time verification, never byte identity: the not-yet-replaced old index tiles the new
    /// data file exactly, each record it locates is either the replaced record's own bytes (a
    /// verbatim copy of a sole or byte-identical duplicate) or bytes whose content this repack
    /// verified (`read_target`), and every pack read verifies what it resolves
    /// (`resolve_record`) — so a reader interleaving with the two renames serves verified bytes or
    /// fails cleanly, and a same-name rewrite can only ever replace a record's bytes with bytes
    /// that verify, never a verified copy with bytes that do not. Index-before-data, by contrast,
    /// would break the load-bearing new-pack invariant above.
    /// Returns the loose files this pack now holds (the caller deletes them once **every** pack
    /// is durable) and this pack's two final paths (so a repack never deletes, as an "old"
    /// pack, a file a new pack was just written to — an idempotent repack lands on that name).
    fn finalize(mut self) -> Result<Finalized, String> {
        self.data_writer.flush().map_err(|e| format!("Error while flushing pack: {}", e))?;
        if file_utils::fsync_enabled() {
            self.data_writer.get_ref().sync_all()
                .map_err(|e| format!("Error while syncing pack: {}", e))?;
        }

        // The index is sorted by hash for binary-search lookups.
        self.records.sort_by(|a, b| a.0.cmp(&b.0));

        let pack_id = compute_pack_id(&self.records);
        let index_bytes = build_index_bytes(&self.records);

        let index_temp_path = temp_path(&self.pack_folder, PACK_INDEX_EXTENSION);
        write_and_sync(&index_temp_path, &index_bytes)?;

        let data_final = self.pack_folder.join(format!("{}.{}", pack_id, PACK_DATA_EXTENSION));
        let index_final = self.pack_folder.join(format!("{}.{}", pack_id, PACK_INDEX_EXTENSION));

        std::fs::rename(&self.data_temp_path, &data_final).map_err(|e| format!(
            "Error while finalizing pack data \"{}\": {}", data_final.to_string_lossy(), e
        ))?;
        std::fs::rename(&index_temp_path, &index_final).map_err(|e| format!(
            "Error while finalizing pack index \"{}\": {}", index_final.to_string_lossy(), e
        ))?;

        Ok(Finalized { sources: self.sources, files: vec![data_final, index_final] })
    }
}

/// The outcome of finalizing a pack: the loose files it superseded (to delete) and its own
/// final paths (which a repack must not delete as "old").
struct Finalized {
    sources: Vec<PathBuf>,
    files: Vec<PathBuf>,
}

/// One native pack/index pair built for embedding in a transport bundle.
pub(crate) struct TransportPackArtifact {
    pub data_path: PathBuf,
    pub index_path: PathBuf,
}

/// A narrow facade over the native pack writer for bundle construction. It preserves the same
/// rollover, record framing, delta representation and layout-derived identity as local compaction,
/// but has no loose sources to delete: the server is producing a read-only transport artifact.
pub(crate) struct TransportPackBuilder {
    folder: PathBuf,
    writer: Option<PackWriter>,
    artifacts: Vec<TransportPackArtifact>,
}

impl TransportPackBuilder {
    pub fn new(folder: &Path) -> Result<TransportPackBuilder, String> {
        file_utils::create_folder_if_not_exists(folder)?;
        Ok(TransportPackBuilder {
            folder: folder.to_path_buf(),
            writer: None,
            artifacts: Vec::new(),
        })
    }

    /// Append raw verified object bytes as a native full record.
    pub fn append_full(&mut self, hash: &str, raw: &[u8]) -> Result<(), String> {
        let hash_bytes = hash_to_bytes(hash)
            .ok_or_else(|| format!("Cannot pack \"{}\": not a 64-character object hash.", hash))?;
        object_utils::verify_object_bytes(hash, raw)?;
        let compressed = zstd::encode_all(raw, 0)
            .map_err(|e| format!("Error while compressing bundled object {}: {}", hash, e))?;
        self.writer_mut()?.append_full(hash_bytes, &compressed, None)?;
        self.roll_if_needed()
    }

    /// Append a bundle delta in the native pack's framing. Both formats use the same zstd
    /// dictionary payload; only the base-hash and target-length encodings differ.
    pub fn append_delta(&mut self,
                        target_hash: &str,
                        base_hash: &str,
                        target_len: u64,
                        payload: &[u8]) -> Result<(), String> {
        let target = hash_to_bytes(target_hash)
            .ok_or_else(|| format!("Cannot pack \"{}\": not a 64-character object hash.", target_hash))?;
        let base = hash_to_bytes(base_hash)
            .ok_or_else(|| format!("Cannot delta against \"{}\": not a 64-character object hash.", base_hash))?;
        self.writer_mut()?.append_delta(target, base, target_len, payload, None)?;
        self.roll_if_needed()
    }

    pub fn finish(mut self) -> Result<Vec<TransportPackArtifact>, String> {
        self.finish_current()?;
        Ok(self.artifacts)
    }

    fn writer_mut(&mut self) -> Result<&mut PackWriter, String> {
        if self.writer.is_none() {
            self.writer = Some(PackWriter::new(&self.folder)?);
        }
        Ok(self.writer.as_mut().expect("the transport pack writer was just created"))
    }

    fn roll_if_needed(&mut self) -> Result<(), String> {
        if self.writer.as_ref().is_some_and(PackWriter::should_roll_over) {
            self.finish_current()?;
        }
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), String> {
        let Some(writer) = self.writer.take() else {
            return Ok(());
        };
        let finalized = writer.finalize()?;
        let mut data_path = None;
        let mut index_path = None;

        for path in finalized.files {
            match path.extension().and_then(|extension| extension.to_str()) {
                Some(PACK_DATA_EXTENSION) => data_path = Some(path),
                Some(PACK_INDEX_EXTENSION) => index_path = Some(path),
                _ => {}
            }
        }

        self.artifacts.push(TransportPackArtifact {
            data_path: data_path.ok_or_else(|| "A built transport pack has no data file.".to_string())?,
            index_path: index_path.ok_or_else(|| "A built transport pack has no index file.".to_string())?,
        });
        Ok(())
    }
}

/// How a [`StoreIngest`] recorded one object.
#[derive(Debug, PartialEq, Eq)]
pub enum IngestStored {
    /// The object was already in the store (or already appended this ingest) — nothing written.
    AlreadyPresent,
    /// Stored as a full record.
    Full,
    /// Stored as a delta whose chain depth is `depth` (its base's depth + 1). The caller feeds
    /// this back as [`IngestBase::depth`] for the next version, keeping chains bounded.
    Delta { depth: u32 },
}

/// A delta base candidate for [`StoreIngest::store_with_base`]: the previous version at the same
/// path, with its decompressed object bytes and current chain depth.
pub struct IngestBase<'a> {
    pub hash: &'a str,
    pub bytes: &'a [u8],
    pub depth: u32,
}

/// What a finished ingest wrote.
pub struct IngestStats {
    /// Objects appended (full and delta records).
    pub objects: usize,
    /// Of those, delta records.
    pub deltas: usize,
    /// Packs published.
    pub packs: usize,
}

/// Direct-to-pack ingestion for bulk imports (`import-git`): objects append straight into
/// native packs in the store's pack folder, publishing on rollover — never touching the loose
/// store. Landing hundreds of thousands of individually-written loose files (and then compacting
/// them straight back out) is the measured wall of a large import; this writes the dense form
/// once. `finish` must run before anything reads the ingested objects: it publishes the last
/// pack and refreshes the pack registry.
pub struct StoreIngest {
    folder: PathBuf,
    writer: Option<PackWriter>,
    /// Hashes appended across the whole ingest (published or not): the pack-direct equivalent
    /// of the loose store's exists-check, so convergent inputs (e.g. two git trees differing
    /// only by a skipped submodule) never append the same object twice.
    appended: HashSet<[u8; HASH_LEN]>,
    objects: usize,
    deltas: usize,
    packs: usize,
    /// Every pack's two final paths (data + index), across every rollover this ingest has
    /// published so far. `finish`'s own trailing directory sync covers all of them in one call
    /// (no intermediate rollover syncs its own directory separately — see `finish`), so this is
    /// exactly the set a failure there must taint.
    finalized_files: Vec<PathBuf>,
}

impl StoreIngest {
    pub fn new() -> Result<StoreIngest, String> {
        let folder = pack_folder();
        file_utils::create_folder_if_not_exists(&folder)?;
        remove_stale_temp_files(&folder);

        Ok(StoreIngest {
            folder,
            writer: None,
            appended: HashSet::new(),
            objects: 0,
            deltas: 0,
            packs: 0,
            finalized_files: Vec::new(),
        })
    }

    /// Store one built object as a full record, skipping it when the store (or this ingest)
    /// already holds it — the same idempotence as the loose `store()`.
    pub fn store(&mut self, object: &LooseObject) -> Result<IngestStored, String> {
        let Some(hash_bytes) = self.admit(object)? else {
            return Ok(IngestStored::AlreadyPresent);
        };
        self.append_full(hash_bytes, &object.content)?;
        Ok(IngestStored::Full)
    }

    /// Store one built object with per-path versions (a blob, or a tree of one directory) — as
    /// a delta against the previous version at its path when that pays, otherwise in full. The
    /// policy is the bundle builder's: never delta an over-large target (the read-side bomb
    /// ceiling), never extend a maximal chain, and never keep a delta that is not actually
    /// smaller than the object it encodes.
    pub fn store_with_base(&mut self,
                           object: &LooseObject,
                           base: Option<IngestBase<'_>>) -> Result<IngestStored, String> {
        let Some(hash_bytes) = self.admit(object)? else {
            return Ok(IngestStored::AlreadyPresent);
        };

        if let Some(base) = base {
            let deltable = object.content.len() <= delta_utils::MAX_DELTA_TARGET_BYTES
                && base.depth < MAX_DELTA_CHAIN
                && base.hash != object.hash;

            if deltable {
                let base_hash = hash_to_bytes(base.hash).ok_or_else(|| format!(
                    "Cannot delta against \"{}\": not a 64-character object hash.", base.hash
                ))?;
                let delta = delta_utils::compress_delta(base.bytes, &object.content)?;

                if delta.len() < object.content.len() {
                    self.writer_mut()?.append_delta(
                        hash_bytes, base_hash, object.content.len() as u64, &delta, None
                    )?;
                    self.appended.insert(hash_bytes);
                    self.objects += 1;
                    self.deltas += 1;
                    self.roll_if_needed()?;
                    return Ok(IngestStored::Delta { depth: base.depth + 1 });
                }
            }
        }

        self.append_full(hash_bytes, &object.content)?;
        Ok(IngestStored::Full)
    }

    /// Publish the final pack and make everything ingested visible to readers. Refs pointing at
    /// ingested objects must only be written after this returns `Ok`.
    ///
    /// # Returns
    /// * `Ok(IngestStats)` - Every finalized pack's directory entries are durable and, once
    ///                       [`taint_utils::activate`](crate::util::taint_utils::activate) has
    ///                       been called in this process, no durability taint was found standing
    ///                       for this root on the re-check that runs right before this returns
    ///                       (see `sync_dir_or_taint`). An unactivated process skips that
    ///                       re-check entirely.
    /// * `Err(String)`     - A pack write, `finalize`, or the trailing directory sync failed. On
    ///                       the directory-sync failure specifically, every pack finalized during
    ///                       this ingest (`finalized_files`, data + index paths, across every
    ///                       rollover) is visible but its directory entries are not proven
    ///                       durable — once activated, that is recorded as a taint over exactly
    ///                       those paths (see `taint_after_sync_failure`).
    pub fn finish(mut self) -> Result<IngestStats, String> {
        self.finish_current()?;

        if self.packs > 0 {
            let final_paths: Vec<&Path> = self.finalized_files.iter().map(PathBuf::as_path).collect();
            file_utils::sync_dir_or_taint(&self.folder, &final_paths)?;
            invalidate_cache();
            // This is exactly the shape `--redelta` densifies: objects appended straight into
            // packs, one path/window at a time, never seeing similarity across the whole store.
            mark_densify_pending(&self.folder);
        }

        Ok(IngestStats { objects: self.objects, deltas: self.deltas, packs: self.packs })
    }

    /// Gate one object into the ingest: enforce the authorship ceiling and answer `None` when
    /// the store or this ingest already holds it (nothing to write).
    fn admit(&mut self, object: &LooseObject) -> Result<Option<[u8; HASH_LEN]>, String> {
        object_utils::check_object_ceiling(&object.object_type, object.content.len())?;

        let hash_bytes = hash_to_bytes(&object.hash).ok_or_else(|| format!(
            "Cannot pack \"{}\": not a 64-character object hash.", object.hash
        ))?;

        if self.appended.contains(&hash_bytes) || file_utils::does_object_exist(&object.hash)? {
            return Ok(None);
        }

        Ok(Some(hash_bytes))
    }

    fn append_full(&mut self, hash_bytes: [u8; HASH_LEN], raw: &[u8]) -> Result<(), String> {
        let compressed = zstd::encode_all(raw, 0)
            .map_err(|e| format!("Error while compressing an ingested object: {}", e))?;
        self.writer_mut()?.append_full(hash_bytes, &compressed, None)?;
        self.appended.insert(hash_bytes);
        self.objects += 1;
        self.roll_if_needed()
    }

    fn writer_mut(&mut self) -> Result<&mut PackWriter, String> {
        if self.writer.is_none() {
            self.writer = Some(PackWriter::new(&self.folder)?);
        }
        Ok(self.writer.as_mut().expect("the ingest pack writer was just created"))
    }

    fn roll_if_needed(&mut self) -> Result<(), String> {
        if self.writer.as_ref().is_some_and(PackWriter::should_roll_over) {
            self.finish_current()?;
        }
        Ok(())
    }

    fn finish_current(&mut self) -> Result<(), String> {
        let Some(writer) = self.writer.take() else {
            return Ok(());
        };
        let finalized = writer.finalize()?;
        self.finalized_files.extend(finalized.files);
        self.packs += 1;
        Ok(())
    }
}

/// Build the on-disk index bytes for the (already sorted) records — see [`index_format`].
fn build_index_bytes(records: &[([u8; HASH_LEN], u64, u64)]) -> Vec<u8> {
    index_format::build(records)
}

/// A pack's id: the Blake3 over its sorted records — each object hash **and its offset and
/// length** — so the name is derived from the on-disk *layout*, not just the object set.
///
/// The property the finalize/repack paths lean on: same records at the same offsets ⇒ same
/// name; any difference in layout ⇒ a different name. So re-packing an already-packed live set
/// reproduces the same name and is idempotent — no duplicate pile-up, and the old-pack sweep
/// recognizes it as already-written (this needs the packing order to be deterministic, which
/// the hash tie-break on the `sort_by` in `compact` provides). A pack that lays the *same*
/// objects out differently (a genuinely changed set, or the one-time loose→packed transition
/// whose size metric differs) gets a *different* id and is written to a fresh name instead of
/// overwriting an existing pair in place. Hashing only the object hashes (the old behavior)
/// gave a differently-laid-out pack the *same* name, and the non-atomic two-rename that
/// followed could momentarily pair a freshly renamed data file with the not-yet-replaced index
/// of that pack — a torn read. Deriving the id from the layout is what closes that window.
fn compute_pack_id(records: &[([u8; HASH_LEN], u64, u64)]) -> String {
    let mut hasher = blake3::Hasher::new();
    for (hash, offset, length) in records {
        hasher.update(hash);
        hasher.update(&offset.to_le_bytes());
        hasher.update(&length.to_le_bytes());
    }
    hasher.finalize().to_hex().to_string()
}

/// A unique temp path in the pack folder for an in-progress write.
fn temp_path(pack_folder: &Path, extension: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let sequence = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

    pack_folder.join(format!(".compact-{}-{}.{}.tmp", std::process::id(), sequence, extension))
}

/// How old an orphaned `.tmp` staging file must be before a later run reclaims it. Normal runs
/// remove their staging on every exit path; only a hard kill (SIGKILL, power loss) leaves one
/// behind. The generous age keeps any plausibly-live writer's files out of reach.
const STALE_TEMP_SECONDS: u64 = 24 * 60 * 60;

/// Best-effort removal of staging debris (`temp_path` names) that a killed writer left in the
/// pack folder — otherwise it accumulates forever, and a recycled PID could even collide with it.
/// Errors are ignored: reclaiming debris must never fail the operation that triggered it.
fn remove_stale_temp_files(pack_folder: &Path) {
    let Ok(entries) = std::fs::read_dir(pack_folder) else { return };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_temp = name.to_str()
            .is_some_and(|name| name.starts_with(".compact-") && name.ends_with(".tmp"));
        let is_stale = entry.metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() >= STALE_TEMP_SECONDS);

        if is_temp && is_stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Write a file and fsync it (used for the index, which must be durable before rename).
fn write_and_sync(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("Error while creating \"{}\": {}", path.to_string_lossy(), e))?;
    file.write_all(bytes)
        .map_err(|e| format!("Error while writing \"{}\": {}", path.to_string_lossy(), e))?;
    if file_utils::fsync_enabled() {
        file.sync_all()
            .map_err(|e| format!("Error while syncing \"{}\": {}", path.to_string_lossy(), e))?;
    }
    Ok(())
}

/// Whether an object's raw bytes are a parcel, read from the type in its header (`VLQ version,
/// VLQ type, …`) without a full parse. Parcels are stored full, never delta'd — the history
/// walk reads every one, so a delta chain per parcel would make it reconstruct-bound.
fn is_parcel(raw: &[u8]) -> bool {
    let Ok((_version, after_version)) = byte_utils::number_from_vlq_bytes(0, raw) else {
        return false;
    };
    matches!(
        byte_utils::number_from_vlq_bytes(after_version, raw),
        Ok((code, _)) if code == crate::enums::object_type::ObjectType::Parcel.get_code()
    )
}

/// Whether an object's raw bytes are a chunk, read from the type in its header without a full
/// parse. Chunks are **never** packed or delta'd: each must stay an individually addressable
/// loose object (a hosted head serves each chunk as its own presigned GET, and loose chunks give
/// O(1) ranged reads); a chunk also has no "path" for the path-base delta grouping, and CDC
/// already captured the dedup a delta would chase. So a chunk is left loose by compaction.
fn is_chunk(raw: &[u8]) -> bool {
    let Ok((_version, after_version)) = byte_utils::number_from_vlq_bytes(0, raw) else {
        return false;
    };
    matches!(
        byte_utils::number_from_vlq_bytes(after_version, raw),
        Ok((code, _)) if code == crate::enums::object_type::ObjectType::Chunk.get_code()
    )
}

/// Decode a 64-character hex object hash into its 32 raw bytes, or `None` if it is not a
/// valid Blake3 hex hash. Non-hashes never match a pack, so they map to `None` (not an error).
///
/// Lowercase only, like the loose path ([`file_utils::is_object_hash_or_prefix`]): decoding an
/// uppercase spelling would find a packed object that the same spelling misses while it is loose
/// on a case-sensitive file system, so presence would depend on the platform and on whether a
/// `compact` had run.
fn hash_to_bytes(hash: &str) -> Option<[u8; HASH_LEN]> {
    if !object_utils::is_object_hash(hash) {
        return None;
    }

    sign_utils::from_hex(hash).ok()?.try_into().ok()
}

/// Read a little-endian u64 at `offset` in `bytes` (offset is always in bounds by construction).
fn read_u64_le(bytes: &[u8], offset: usize) -> u64 {
    let mut value = [0u8; 8];
    value.copy_from_slice(&bytes[offset..offset + 8]);
    u64::from_le_bytes(value)
}

/// Read a little-endian u32 at `offset` in `bytes`.
fn read_u32_le(bytes: &[u8], offset: usize) -> u32 {
    let mut value = [0u8; 4];
    value.copy_from_slice(&bytes[offset..offset + 4]);
    u32::from_le_bytes(value)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    /// How [`pack_as_unreadable`] makes an object's only pack unreadable.
    #[derive(Debug, Clone, Copy)]
    pub(crate) enum Unreadable {
        /// The index survives, its data file is gone.
        Quarantined,
        /// The index's bucket holding the object fails validation.
        CorruptBucket,
        /// The index carries another format version.
        ForeignVersion,
        /// The index does not parse at all, so its hash set is unknown.
        Unparseable,
    }

    /// Move the loose object `hash` into a pack of its own (deleting the loose file), so a
    /// test can place one object in a pack and the objects around it elsewhere.
    pub(crate) fn pack_alone(hash: &str) -> TransportPackArtifact {
        let raw = file_utils::retrieve_object_by_hash(hash).unwrap();
        let mut builder = TransportPackBuilder::new(&pack_folder()).unwrap();
        builder.append_full(hash, &raw).unwrap();
        let mut artifacts = builder.finish().unwrap();
        assert_eq!(artifacts.len(), 1, "one object must make exactly one pack pair");

        let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
        std::fs::remove_file(Path::new(&folder).join(file_name)).unwrap();
        invalidate_cache();

        artifacts.remove(0)
    }

    /// Run `case` once per [`Unreadable`] kind and fail with every arm's failure together, so a red
    /// run reports each arm rather than stopping at the first.
    pub(crate) fn for_each_unreadable(case: impl Fn(Unreadable) -> Option<String>) {
        let all = [Unreadable::Quarantined, Unreadable::CorruptBucket, Unreadable::ForeignVersion, Unreadable::Unparseable];
        let failures: Vec<String> = all
            .into_iter()
            .filter_map(case)
            .collect();
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    /// Leave the loose object `hash` held only by a pack index this store cannot serve it from,
    /// and assert that it reads not present, yet not absent to a sweep.
    pub(crate) fn pack_as_unreadable(hash: &str, how: Unreadable) {
        let artifact = pack_alone(hash);
        let rewrite_header = |offset: usize, bytes: &[u8]| {
            let mut index = std::fs::read(&artifact.index_path).unwrap();
            index[offset..offset + bytes.len()].copy_from_slice(bytes);
            std::fs::write(&artifact.index_path, index).unwrap();
        };

        match how {
            Unreadable::Quarantined => std::fs::remove_file(&artifact.data_path).unwrap(),
            Unreadable::CorruptBucket => corrupt_record_of(&artifact.index_path, &hash_to_bytes(hash).unwrap()),
            Unreadable::ForeignVersion => rewrite_header(8, &(PACK_FORMAT_VERSION + 1).to_le_bytes()),
            Unreadable::Unparseable => rewrite_header(0, b"NOTAPACK"),
        }
        invalidate_cache();

        // Each kind refuses for its own reason, so an arm cannot pass on another kind's gate.
        let reason = match how {
            Unreadable::Quarantined => "whose data file could not be loaded",
            Unreadable::CorruptBucket => "is corrupt (bucket",
            Unreadable::ForeignVersion => "which this build cannot read",
            Unreadable::Unparseable => "which could not be parsed",
        };
        assert!(!file_utils::does_object_exist(hash).unwrap(), "{:?}: the object must not read present", how);
        match file_utils::is_present_for_sweep(hash) {
            Err(error) => assert!(error.contains(reason), "{:?}: the refusal must give its own reason: {}", how, error),
            Ok(present) => panic!("{:?}: a sweep must not read the object absent (read {})", how, present),
        }
    }

    /// Test-only convenience: assert `retrieve_from_packs(hash)` reports `Verified` and return
    /// its bytes, so a byte-comparison assertion at the call site stays a single line as it was
    /// before `PackRetrieval` replaced the bare `Option`.
    fn packs_verified(hash: &str) -> Vec<u8> {
        match retrieve_from_packs(hash).unwrap() {
            PackRetrieval::Verified(bytes) => bytes,
            other => panic!("expected a verified pack record for {}, got {:?}", hash, other),
        }
    }

    /// A deterministic pseudo-random stream (an LCG), so the shuffles below are reproducible.
    fn lcg(seed: &mut u64) -> u64 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *seed >> 11
    }

    /// The records of an index over `count` objects that tile a data body in a shuffled order,
    /// in strictly increasing hash order and spread across the fanout buckets, as the native
    /// writer lays them out. Lengths vary so that offsets need several radix digits. Returns the
    /// records and the data length they tile.
    fn tiled_records(count: usize) -> (Vec<([u8; HASH_LEN], u64, u64)>, usize) {
        let mut seed = 7;
        let mut order: Vec<usize> = (0..count).collect();
        for i in (1..count).rev() {
            order.swap(i, (lcg(&mut seed) % (i as u64 + 1)) as usize);
        }

        let lengths: Vec<u64> = (0..count).map(|_| 1 + lcg(&mut seed) % 5000).collect();
        let mut placed = vec![(0u64, 0u64); count];
        let mut offset = PACK_DATA_HEADER_LEN;
        for &record in &order {
            placed[record] = (offset, lengths[record]);
            offset += lengths[record];
        }

        let records = placed.iter().enumerate().map(|(position, (offset, length))| {
            let mut hash = [0u8; HASH_LEN];
            hash[..8].copy_from_slice(&((position as u64) << 48).to_be_bytes());
            (hash, *offset, *length)
        }).collect();

        (records, offset as usize)
    }

    /// An index over `records`, checked against a data file of `data_len` bytes.
    fn checked_index(records: &[([u8; HASH_LEN], u64, u64)], data_len: usize) -> CheckedIndex<Vec<u8>> {
        let bytes = build_index_bytes(records);
        let layout = index_format::Layout::parse(&bytes, Path::new("test.idx")).expect("a built index must parse");
        CheckedIndex::new(bytes, layout, Some(data_len as u64))
    }

    /// Whether every bucket of the index over `records` validates and its records tile `data_len`.
    fn tiles(records: &[([u8; HASH_LEN], u64, u64)], data_len: usize) -> bool {
        checked_index(records, data_len).all_records().is_ok_and(|all| all.tile(data_len as u64))
    }

    #[test]
    fn the_radix_range_sort_orders_like_a_comparison_sort() {
        // Starts spread over 41 bits take four radix passes, and repeated starts check that ties
        // keep a valid order; small inputs take the comparison-sort branch.
        for count in [0, 1, 5, 1023, 1024, 5000] {
            let mut seed = count as u64 + 1;
            let mut ranges: Vec<(u64, u64)> = (0..count).map(|_| {
                let start = lcg(&mut seed) % (1 << 41);
                let start = if start % 7 == 0 { 4096 } else { start };
                (start, lcg(&mut seed))
            }).collect();

            let mut expected = ranges.clone();
            expected.sort_by_key(|(start, _)| *start);
            sort_ranges_by_start(&mut ranges);

            let first_misplaced = ranges.iter().zip(&expected).position(|(got, want)| got.0 != want.0);
            assert_eq!(first_misplaced, None, "starts out of order for {} ranges", count);

            let mut got_sorted = ranges.clone();
            got_sorted.sort();
            expected.sort();
            assert!(got_sorted == expected, "the sort lost or duplicated a range for {} ranges", count);
        }
    }

    #[test]
    fn index_validation_accepts_a_shuffled_tiling_and_refuses_a_gap_or_an_overlap() {
        // 5000 records: the radix branch. The records are in hash order and their offsets are a
        // shuffle of the body, so only a correct sort by offset sees the exact tiling.
        let (records, data_len) = tiled_records(5000);
        assert!(tiles(&records, data_len));

        // A record that starts one byte late leaves a gap before it and overlaps its successor.
        let record = records.iter()
            .position(|(_, offset, length)| *length > 1 && offset + length < data_len as u64).unwrap();
        let mut shifted = records.clone();
        shifted[record].1 += 1;
        assert!(!tiles(&shifted, data_len), "a record shifted one byte must not tile the body");

        // A record one byte short leaves a gap after it that no record accounts for.
        let mut short = records.clone();
        short[record].2 -= 1;
        assert!(!tiles(&short, data_len), "a record one byte short must not tile the body");

        // Two records swapping their offsets but keeping their lengths no longer tile the body when
        // the lengths differ, although the lengths still sum to it.
        let (a, b) = (0..records.len()).flat_map(|a| (a + 1..records.len()).map(move |b| (a, b)))
            .find(|&(a, b)| records[a].2 != records[b].2).unwrap();
        let mut swapped = records.clone();
        swapped[a].1 = records[b].1;
        swapped[b].1 = records[a].1;
        assert!(!tiles(&swapped, data_len), "swapped offsets must not tile the body");
    }

    #[test]
    fn pack_id_depends_on_the_layout_not_just_the_object_set() {
        // The id must fold in each record's offset and length, so two packs holding the
        // same object *set* but a different byte layout get different names — otherwise the
        // finalize renames overwrite an existing pair in place and a concurrent reader can
        // pair new data with the old index (a torn read).
        let hash = |b: u8| [b; HASH_LEN];
        let layout_a = [(hash(1), 12u64, 100u64), (hash(2), 112, 50)];
        let layout_b = [(hash(1), 12u64, 90u64), (hash(2), 102, 60)];

        assert_ne!(
            compute_pack_id(&layout_a), compute_pack_id(&layout_b),
            "same objects laid out differently must not collide on one pack name"
        );

        // Idempotency is preserved: an unchanged repack produces byte-identical records, so it
        // still lands on the very same name rather than piling up a duplicate.
        let layout_a_again = [(hash(1), 12u64, 100u64), (hash(2), 112, 50)];
        assert_eq!(
            compute_pack_id(&layout_a), compute_pack_id(&layout_a_again),
            "an identical repack must be idempotent (same name)"
        );
    }

    #[test]
    fn reads_are_cached_and_stay_valid_across_compaction() {
        let temp = std::env::temp_dir().join(format!("forklift-read-cache-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"read cache content".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);

        // First read populates the cache; the second is served from it — both correct.
        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), content);
        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), content);

        // Compaction relocates the bytes (loose → pack), but the content for a hash is
        // immutable, so the cached value stays valid — no stale reads.
        compact(false, false, false, &Progress::NONE).unwrap();
        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), content);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Mechanism-level probe for a held pack mapping blocking a franchise's cleanup, isolated
    /// from `franchise` entirely:
    /// does an `Mmap`'d pack file actually block `remove_dir_all` on its own containing
    /// directory? Windows-only by construction — POSIX permits unlinking (and therefore
    /// recursively removing a directory over) a file that is still mapped, verified empirically
    /// on macOS, so this question has no meaning on a non-Windows
    /// target and the test is gated accordingly rather than asserting something platform-
    /// specific as if it were universal.
    ///
    /// This was written after the franchise-level regression test for the same worry turned out
    /// not to discriminate: it passed identically with and without `invalidate_cache()` in
    /// `franchise.rs`'s cleanup path, on Windows CI. Run directly on `windows-latest`, this probe
    /// answered the question the other test couldn't: **the mechanism is not real** in this
    /// configuration. `remove_dir_all` succeeds on a directory containing a still-mapped pack.
    ///
    /// Why: `load_pack_pair` opens the pack file with a plain `std::fs::File::open` before
    /// mapping it, and Rust's `std::fs::File` on Windows defaults its share mode to
    /// `FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE` — the file (and, transitively,
    /// anything mapped from that same handle) stays deletable the whole time it's open. This
    /// contradicts the comment on `compact`'s own `drop(source_packs)` a few thousand lines up
    /// (`"a file opened (or mapped) without FILE_SHARE_DELETE — the platform default — cannot be
    /// deleted"`), which was never actually verified on Windows either (its own test, right below
    /// this one, explicitly says so: "unobservable as a failure on POSIX... this test instead
    /// pins the outcome"). That's a pre-existing, out-of-scope question this PR does not resolve
    /// — flagging it for a follow-up, not fixing it here.
    ///
    /// So: this test pins the *actual*, now-confirmed platform behavior (removal succeeds
    /// regardless), so a future change cannot silently reintroduce the disproven assumption
    /// without this test calling it out.
    #[cfg(windows)]
    #[test]
    fn an_mmapd_pack_does_not_block_its_own_directory_removal() {
        let temp = std::env::temp_dir().join(format!("forklift-mmap-block-probe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"mmap block probe content".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);

        // Packs it; loose is removed, so the only path to this hash is now through a pack.
        // `compact`'s own trailing `invalidate_cache()` leaves the registry empty afterward —
        // nothing is mapped yet.
        compact(false, false, false, &Progress::NONE).unwrap();

        // A read through the real (uncached-content) path: this is what actually populates
        // `PACK_REGISTRY` with a live `Mmap` for the pack just written.
        assert_eq!(file_utils::retrieve_object_by_hash_uncached(&hash).unwrap(), content);

        // The mmap is still held in the process-global registry, with no `invalidate_cache()`
        // call anywhere in this test — yet removal succeeds anyway (see the doc comment above
        // for why: `FILE_SHARE_DELETE` is Rust's own Windows default for `std::fs::File`).
        let removal_while_mapped = std::fs::remove_dir_all(&temp);
        assert!(
            removal_while_mapped.is_ok(),
            "expected remove_dir_all to succeed even while a pack from this directory is still \
            mmap'd (the confirmed Windows behavior) — got {:?}; if this now fails, the platform \
            or toolchain behavior this test documents has changed and `invalidate_cache`'s \
            pre-deletion role needs re-examining, not this assertion silently flipped back",
            removal_while_mapped
        );
    }

    #[test]
    fn is_parcel_reads_the_type_from_the_object_header() {
        use crate::enums::object_type::ObjectType;

        // A loose object header is `VLQ(version) VLQ(type) VLQ(length) NUL` then the content.
        let object = |type_code: u64| {
            let mut raw = byte_utils::number_to_vlq_bytes(1);
            raw.extend(byte_utils::number_to_vlq_bytes(type_code));
            raw.extend(byte_utils::number_to_vlq_bytes(3));
            raw.push(0);
            raw.extend_from_slice(b"abc");
            raw
        };

        assert!(is_parcel(&object(ObjectType::Parcel.get_code())), "a parcel must be detected");
        assert!(!is_parcel(&object(ObjectType::Blob.get_code())), "a blob is not a parcel");
        assert!(!is_parcel(&object(ObjectType::Tree.get_code())), "a tree is not a parcel");
        assert!(!is_parcel(b""), "empty bytes are not a parcel");
    }

    #[test]
    fn bloom_has_no_false_negatives_and_a_low_false_positive_rate() {
        let count = 5000;
        let mut bloom = Bloom::new(count);

        let key = |i: usize| blake3::hash(format!("in-{i}").as_bytes()).to_hex().to_string();
        for i in 0..count {
            bloom.insert(key(i).as_bytes());
        }

        // No false negatives — every inserted key is reported present.
        for i in 0..count {
            assert!(bloom.contains(key(i).as_bytes()), "inserted key must be present");
        }

        // Low false-positive rate for keys never inserted (~1% by design; allow slack).
        let trials = 5000;
        let positives = (0..trials)
            .filter(|i| bloom.contains(blake3::hash(format!("out-{i}").as_bytes()).to_hex().as_bytes()))
            .count();
        assert!(positives * 100 < trials * 5, "false-positive rate too high: {}/{}", positives, trials);
    }

    /// Store a loose object the way the real store does (zstd, fanned out by hash prefix).
    fn store_loose(hash: &str, content: &[u8]) {
        let compressed = zstd::encode_all(content, 0).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
        file_utils::write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();
    }

    /// Deterministic, non-trivially-compressible filler bytes (an LCG stream, not real
    /// entropy) — repeat-free enough that zstd cannot compress it away for free the way it
    /// would a run of zeros, so a path delta against a shared prefix of it is a meaningfully
    /// smaller payload than compressing the same bytes standalone.
    fn deterministic_fill(len: usize) -> Vec<u8> {
        let mut buf = Vec::with_capacity(len);
        let mut state: u64 = 0x9E3779B97F4A7C15;
        while buf.len() < len {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            buf.extend_from_slice(&state.to_le_bytes());
        }
        buf.truncate(len);
        buf
    }

    /// Whether `hash`'s packed record is specifically a delta (`RECORD_DELTA`) — reads the
    /// record's own kind byte directly, rather than inferring it from a whole-run delta count
    /// (which a fixture with more than one delta-eligible pair can't attribute to any one of
    /// them). `false` if the hash is not packed at all.
    fn is_stored_as_delta(hash: &str) -> bool {
        let Some(hash_bytes) = hash_to_bytes(hash) else { return false; };
        let packs = loaded_packs().unwrap();
        for pack in packs.packs.iter() {
            if let Some((offset, length)) = pack.locate(&hash_bytes).found() {
                if let Ok(record) = pack.slice(offset, length) {
                    return record.first() == Some(&RECORD_DELTA);
                }
            }
        }
        false
    }

    /// The pack-direct import sink, end to end: full and delta records land in one published
    /// pack, read back byte-correct through the ordinary object path, with no loose files.
    #[test]
    fn store_ingest_writes_full_and_delta_records_readable_after_finish() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;

        let temp = std::env::temp_dir().join(format!("forklift-ingest-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Two versions of one file: the second differs by a suffix, so a delta clearly pays.
        let base_object = LooseObjectBuilder::build_blob(&Blob {
            content: b"a line of file content\n".repeat(500),
        });
        let target_object = LooseObjectBuilder::build_blob(&Blob {
            content: [b"a line of file content\n".repeat(500), b"one more line\n".to_vec()].concat(),
        });

        let mut ingest = StoreIngest::new().unwrap();
        assert_eq!(ingest.store_with_base(&base_object, None).unwrap(), IngestStored::Full);

        let stored = ingest.store_with_base(&target_object, Some(IngestBase {
            hash: &base_object.hash,
            bytes: &base_object.content,
            depth: 0,
        })).unwrap();
        assert_eq!(stored, IngestStored::Delta { depth: 1 });

        // Re-ingesting either version is a no-op (the loose store's exists-check equivalent).
        assert_eq!(ingest.store_with_base(&base_object, None).unwrap(), IngestStored::AlreadyPresent);

        let stats = ingest.finish().unwrap();
        assert_eq!(stats.objects, 2);
        assert_eq!(stats.deltas, 1);
        assert_eq!(stats.packs, 1);

        let status = store_status().unwrap();
        assert_eq!(status.loose_objects, 0, "ingest must never create loose files");
        assert_eq!(status.packed_objects, 2);

        assert_eq!(
            file_utils::retrieve_object_by_hash(&base_object.hash).unwrap(),
            base_object.content
        );
        assert_eq!(
            file_utils::retrieve_object_by_hash(&target_object.hash).unwrap(),
            target_object.content
        );
    }

    /// An object already in the store is skipped, and an ingest that wrote nothing publishes
    /// nothing.
    #[test]
    fn store_ingest_skips_objects_the_store_already_holds() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;

        let temp = std::env::temp_dir().join(format!("forklift-ingest-skip-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let mut object = LooseObjectBuilder::build_blob(&Blob { content: b"already here".to_vec() });
        object.store().unwrap();

        let mut ingest = StoreIngest::new().unwrap();
        assert_eq!(ingest.store(&object).unwrap(), IngestStored::AlreadyPresent);

        let stats = ingest.finish().unwrap();
        assert_eq!(stats.objects, 0);
        assert_eq!(stats.packs, 0, "an empty ingest must not publish an empty pack");
    }

    /// Fire-site c (the design's durable-taint wiring): `StoreIngest::finish`'s own trailing
    /// directory sync. Reverting the `file_utils::sync_dir_or_taint` wiring in `finish` back to a
    /// bare `file_utils::sync_dir(&self.folder)?` call kills this test — no taint file would
    /// exist to read back.
    #[test]
    #[cfg(unix)]
    fn store_ingest_finish_dir_sync_failure_taints_the_finalized_pack_files() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;
        use crate::util::taint_utils;
        use crate::util::file_utils::SyncDirFaultGuard;

        let _serial = taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-ingest-taint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = crate::globals::forklift_root();

        let object = LooseObjectBuilder::build_blob(&Blob { content: b"pack taint content".to_vec() });
        let mut ingest = StoreIngest::new().unwrap();
        assert_eq!(ingest.store(&object).unwrap(), IngestStored::Full);

        let _guard = SyncDirFaultGuard::failing("pack");
        let error = match ingest.finish() {
            Err(e) => e,
            Ok(_) => panic!("a blocked pack directory must fail `finish`'s trailing sync"),
        };
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.recorded.is_empty(), "a pack directory-sync failure must record a taint");
        for path in &state.recorded {
            assert!(path.starts_with("objects/pack"), "expected a pack path, got {:?}", path);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Fire-site e (durable-taint wiring): `import_transport_packs`'s own trailing directory
    /// sync — the same fire-site class as `StoreIngest::finish` above (a rename-then-sync
    /// publishing new pack pairs, feeding `does_object_exist`), reached from the far end of a
    /// network transfer rather than a local ingest. Reverting the `file_utils::sync_dir_or_taint`
    /// wiring back to a bare `file_utils::sync_dir(&folder)?` call kills this test.
    #[test]
    #[cfg(unix)]
    fn import_transport_packs_dir_sync_failure_taints_the_published_pack_files() {
        use crate::util::taint_utils;
        use crate::util::file_utils::SyncDirFaultGuard;

        let _serial = taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-transport-taint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = crate::globals::forklift_root();

        // Build one native pack pair in a throwaway staging folder — `import_transport_packs`
        // takes a byte *stream* (as a network transfer would deliver), not a folder, so this
        // just produces the bytes it will read.
        let staging = temp.join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let mut builder = TransportPackBuilder::new(&staging).unwrap();
        let content = b"import-transport-taint content".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        builder.append_full(&hash, &content).unwrap();
        let artifacts = builder.finish().unwrap();
        assert_eq!(artifacts.len(), 1, "one full record must finalize into exactly one pack pair");

        let data_bytes = std::fs::read(&artifacts[0].data_path).unwrap();
        let index_bytes = std::fs::read(&artifacts[0].index_path).unwrap();
        let sections = vec![(data_bytes.len() as u64, index_bytes.len() as u64)];
        let mut combined = data_bytes;
        combined.extend_from_slice(&index_bytes);
        let mut reader = std::io::Cursor::new(combined);

        let _guard = SyncDirFaultGuard::failing("pack");
        let error = match import_transport_packs(&mut reader, &sections) {
            Err(e) => e,
            Ok(_) => panic!("a blocked pack directory must fail the trailing sync"),
        };
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.recorded.is_empty(),
            "a transport-pack install directory-sync failure must record a taint");
        for path in &state.recorded {
            assert!(path.starts_with("objects/pack"), "expected a pack path, got {:?}", path);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_packs_loose_objects_and_reads_them_back() {
        let temp = std::env::temp_dir().join(format!("forklift-pack-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Three objects with real Blake3 hashes so the fan-out and index keys are valid.
        let contents: Vec<Vec<u8>> = vec![b"first object".to_vec(), b"second".to_vec(), vec![7u8; 5000]];
        let hashes: Vec<String> = contents.iter()
            .map(|c| blake3::hash(c).to_hex().to_string())
            .collect();

        for (hash, content) in hashes.iter().zip(&contents) {
            store_loose(hash, content);
        }

        let stats = compact(false, false, false, &Progress::NONE).unwrap();
        assert_eq!(stats.objects_packed, 3);
        assert_eq!(stats.packs_written, 1);
        assert_eq!(stats.loose_removed, 3);
        // Regression guard: the corrupt-loose classifier must never fire on healthy objects.
        assert_eq!(stats.corrupt_skipped, 0);

        // The loose files are gone...
        for hash in &hashes {
            let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
            assert!(!Path::new(&folder).join(&file_name).exists(), "loose object should be removed after packing");
        }

        // ...but every object still reads back byte-for-byte from the packs.
        for (hash, content) in hashes.iter().zip(&contents) {
            assert!(is_in_packs(hash).unwrap(), "packed object should be found");
            assert_eq!(packs_verified(hash), *content);
        }

        // A hash in no pack is a clean miss, not an error.
        let absent = blake3::hash(b"absent").to_hex().to_string();
        assert!(!is_in_packs(&absent).unwrap());
        assert!(matches!(retrieve_from_packs(&absent).unwrap(), PackRetrieval::NotLocated));

        std::fs::remove_dir_all(&temp).ok();
    }

    /// An uppercase spelling of a real hash gets the same answer whether the object is loose or
    /// packed, on every file system: refused, because the store answers for the lowercase
    /// spelling alone. Before, the pack index decoded it case-insensitively (present), and the
    /// loose path used its characters as-is (present on a case-insensitive file system, absent on
    /// a case-sensitive one).
    ///
    /// Revert: `file_utils::is_object_hash_or_prefix` accepts uppercase → the loose probe answers
    /// `Ok` (true on macOS, false on Linux) → red at the loose assertion. Revert: `hash_to_bytes`
    /// decodes uppercase → the packed probe answers `Ok(true)` → red at the packed assertion.
    #[test]
    fn an_uppercase_hash_is_refused_alike_loose_and_packed() {
        let temp = std::env::temp_dir().join(format!("forklift-pack-case-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"a hash spelled two ways".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        let upper = hash.to_ascii_uppercase();
        assert_ne!(upper, hash, "the hash must carry a letter for the probe to mean anything");

        store_loose(&hash, &content);
        assert!(file_utils::does_object_exist(&hash).unwrap(), "the loose object is present");
        let loose = file_utils::does_object_exist(&upper);
        assert!(loose.is_err(), "loose: an uppercase spelling is refused, got {:?}", loose);

        compact(false, false, false, &Progress::NONE).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        assert!(!Path::new(&folder).join(&file_name).exists(), "the object is now packed only");
        assert!(file_utils::does_object_exist(&hash).unwrap(), "the packed object is present");
        assert!(!is_in_packs(&upper).unwrap(), "the pack index does not decode an uppercase spelling");
        let packed = file_utils::does_object_exist(&upper);
        assert!(packed.is_err(), "packed: an uppercase spelling is refused, got {:?}", packed);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A loose object listed under a respelled fan-out folder (`AB/…`) is packed under its
    /// lowercase hash wherever reads open it there (a case-insensitive file system), and left
    /// alone wherever they do not.
    ///
    /// Revert: the enumeration drops respelled entries (`file_utils::canonical_loose_entry` →
    /// `None`) → it is never packed → red on macOS and Windows. Revert: the enumeration lowercases
    /// without the identity check → a stranger is packed → red on Linux.
    #[test]
    fn compact_packs_a_respelled_loose_object_exactly_where_reads_reach_it() {
        let temp = std::env::temp_dir().join(format!("forklift-pack-respelled-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let (hash, content) = (0..)
            .map(|i| format!("respelled {}", i).into_bytes())
            .map(|content| (blake3::hash(&content).to_hex().to_string(), content))
            .find(|(hash, _)| hash[..2].bytes().any(|b| b.is_ascii_alphabetic()))
            .unwrap();
        store_loose(&hash, &content);

        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        let upper = Path::new(&folder).with_file_name(hash[..2].to_ascii_uppercase());
        std::fs::rename(&folder, &upper).unwrap();
        let folds = Path::new(&folder).join(&file_name).exists();

        compact(false, false, false, &Progress::NONE).unwrap();

        assert_eq!(is_in_packs(&hash).unwrap(), folds, "packed exactly where reads reach it (folds: {})", folds);
        assert_eq!(upper.join(&file_name).exists(), !folds, "the loose entry goes once packed");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_skips_a_loose_object_whose_content_does_not_hash_to_its_filename() {
        // Pins P1 (the ingress invariant): nothing enters a pack under a hash it does not hash
        // to. `enumerate_loose_objects` addresses a loose object by its *filename*; this test
        // writes bytes at a good address that decode cleanly but hash to something else
        // entirely (the class `object_utils::verify_object_bytes` exists to catch).
        let temp = std::env::temp_dir().join(format!("forklift-pack-corrupt-loose-hash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // The plaintext must be neither chunk- nor parcel-shaped, or the chunk-skip (unrelated
        // to this fix) would swallow it under a revert of the fix and this test would go falsely
        // green. Self-checking rather than assumed.
        let wrong_plaintext = b"wrong bytes entirely".to_vec();
        assert!(!is_chunk(&wrong_plaintext) && !is_parcel(&wrong_plaintext),
            "the corrupt plaintext must not look like a chunk or a parcel, or the chunk-skip masks this test");

        // `good_hash` is a genuine hash (of content this test never actually stores) -- what
        // matters is only that it is a valid, well-formed loose-object address whose file holds
        // the wrong content.
        let good_hash = blake3::hash(b"the content this address actually belongs to").to_hex().to_string();
        let (folder, file_name) = file_utils::get_path_for_object(&good_hash).unwrap();
        let corrupt_path = Path::new(&folder).join(&file_name);
        let compressed = zstd::encode_all(wrong_plaintext.as_slice(), 0).unwrap();
        file_utils::write_object_to_file(Path::new(&folder), &file_name, compressed).unwrap();

        let genuine_content = b"a genuine sibling object".to_vec();
        let genuine_hash = blake3::hash(&genuine_content).to_hex().to_string();
        store_loose(&genuine_hash, &genuine_content);

        let stats = compact(false, false, false, &Progress::NONE).unwrap();

        assert!(corrupt_path.exists(), "a corrupt loose object must be left in place, never deleted");
        assert!(!is_in_packs(&good_hash).unwrap(),
            "corrupt bytes must never enter a pack under an address they do not hash to");
        assert_eq!(stats.corrupt_skipped, 1);

        // The genuine sibling in the same batch is unaffected.
        assert_eq!(stats.objects_packed, 1);
        assert!(is_in_packs(&genuine_hash).unwrap());
        assert_eq!(packs_verified(&genuine_hash), genuine_content);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_skips_a_loose_object_that_does_not_even_decode_as_zstd() {
        // Pins clause (a) (a decode failure is a skip, not an abort) and the auto-maintenance
        // silent-fail for a standalone corrupt object: before this fix, one garbage loose object
        // aborted the *entire* compact via `?` -- and auto-maintenance drops that error
        // (`crates/forklift/src/commands/maintenance.rs`), so one bad file silently wedged
        // background housekeeping for the whole store.
        let temp = std::env::temp_dir().join(format!("forklift-pack-corrupt-loose-decode-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // A valid 64-hex address (so it is enumerated as a loose object at all) whose file holds
        // bytes that are not a zstd frame in the first place.
        let garbage_hash = "a".repeat(64);
        let (folder, file_name) = file_utils::get_path_for_object(&garbage_hash).unwrap();
        let garbage_path = Path::new(&folder).join(&file_name);
        file_utils::write_object_to_file(
            Path::new(&folder), &file_name, b"not a zstd frame at all".to_vec()
        ).unwrap();

        let genuine_content = b"a second genuine object".to_vec();
        let genuine_hash = blake3::hash(&genuine_content).to_hex().to_string();
        store_loose(&genuine_hash, &genuine_content);

        let result = compact(false, false, false, &Progress::NONE);
        assert!(result.is_ok(), "one corrupt loose object must not abort the whole compact, got {:?}", result);
        let stats = result.unwrap();
        assert_eq!(stats.corrupt_skipped, 1);

        assert!(garbage_path.exists(), "the undecodable file must be left in place, never deleted");
        assert!(!is_in_packs(&garbage_hash).unwrap());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_skips_a_correctly_addressed_over_ceiling_giant_loose_object() {
        // INV-5 (site B wiring): a blob of MAX_OBJECT_BYTES + 1 zero bytes, stored at the exact
        // fan-out path its own (uncompressed) content's hash encodes — a genuine, correctly-
        // addressed object, not a corrupt one. It is not corruption (`corrupt_skipped` must stay
        // 0), and it must never enter a pack: a bounded decode cannot produce the bytes to pack
        // it in the first place. Mutation: revert *only* `read_target`'s Loose-branch mapping so
        // `OverCeiling` folds back into decoding-and-packing (as it did before this fix) — the
        // giant fully decodes, verifies, gets packed, and its loose file is swept once the pack
        // is durable — every assertion below flips.
        let temp = std::env::temp_dir().join(format!("forklift-pack-over-ceiling-loose-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let giant = vec![0u8; object_utils::MAX_OBJECT_BYTES + 1];
        let giant_hash = blake3::hash(&giant).to_hex().to_string();
        store_loose(&giant_hash, &giant);
        let (folder, file_name) = file_utils::get_path_for_object(&giant_hash).unwrap();
        let giant_path = Path::new(&folder).join(&file_name);

        let stats = compact(false, false, false, &Progress::NONE).unwrap();

        assert_eq!(stats.over_ceiling_skipped, 1);
        assert_eq!(stats.corrupt_skipped, 0, "a grandfathered giant is not corruption");
        assert!(giant_path.exists(), "an over-ceiling loose object must be left in place, never deleted");
        assert!(!is_in_packs(&giant_hash).unwrap(),
            "a bounded decode must never produce the bytes to pack an over-ceiling object");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_redelta_aborts_on_a_corrupt_packed_record_leaving_the_old_pack_in_place() {
        // Pins I3: the corrupt-loose skip is scoped to loose objects only, never to a packed
        // record reached through `Source::Reconstruct`. A corrupt loose object is a safe skip
        // (the file survives, still fetchable); an already-packed live object hit by `--redelta`
        // may have no other recoverable copy at all, so silently dropping it would be real data
        // loss, not a safe skip. That path must still abort the whole compact.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-pack-redelta-abort-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // A minimal *reachable* history -- one blob, its tree, one parentless parcel, `main`'s
        // head pointed at it -- so `compact --all`'s live-set walk (`gc_utils::collect_live_set`)
        // keeps these objects instead of collecting them as garbage.
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: b"a small tracked file\n".to_vec() });
        blob.store().unwrap();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("file.txt".to_string(), blob.hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // Incremental compact: no live-set filtering, packs everything loose. This also runs
        // `compact`'s own commit-graph build (unconditional, every call) over `main`'s new head,
        // which reads (and process-globally caches) the parcel and tree bytes -- so the object
        // this test corrupts below must be the blob: the one object in this fixture the graph
        // build never reads, so the read cache is never warmed with its pre-corruption bytes and
        // the corruption below is guaranteed to actually reach disk on the next read.
        let stats = compact(false, false, false, &Progress::NONE).unwrap();
        assert_eq!(stats.objects_packed, 3);

        let pack_path = std::fs::read_dir(pack_folder()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .expect("compact should have written one pack file");

        // Locate the blob's own record (index lookup only -- never reads/decodes it, so this
        // cannot itself warm the read cache) and flip a byte in its *data* region: past the
        // record's 1-byte kind tag, i.e. inside its zstd payload, never in the pack header or
        // the index.
        let blob_hash_bytes = hash_to_bytes(&blob.hash).unwrap();
        let (blob_offset, blob_len) = loaded_packs().unwrap().packs.iter()
            .find_map(|pack| pack.locate(&blob_hash_bytes).found())
            .expect("the blob must be packed");

        let mut data = std::fs::read(&pack_path).unwrap();
        let flip_at = blob_offset as usize + 1;
        assert!((flip_at as u64) < blob_offset + blob_len, "the flip must land inside the blob's own record");
        data[flip_at] ^= 0xFF;
        // Drop the mmap'd pack registry *before* rewriting the pack file: on Windows a file with a
        // user-mapped section open cannot be written (os error 1224) — the same handle constraint
        // `compact`'s own drop-before-delete guards against. `compact(true, true)` below reloads.
        invalidate_cache();
        std::fs::write(&pack_path, &data).unwrap();

        // `--all --redelta` forces every live packed object through `Source::Reconstruct` (see
        // `collect_targets`) -- the same path a dropped-base repack already uses -- never
        // `Source::Loose`, so this can never become `CorruptLoose`.
        let result = compact(true, true, true, &Progress::NONE);
        assert!(result.is_err(), "a corrupt packed record reached via Reconstruct must abort, not skip");
        assert!(pack_path.exists(), "abort must precede old-pack removal -- the corrupt pack must survive");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-55 pinning test (spike — expected RED on `main`, no fix applied yet). `collect_targets`'s
    /// first-pack-wins dedup (its `!seen.insert(hash)` check) takes whichever pack enumerates first
    /// for a live object present in more than one pack, and that record is copied **verbatim** —
    /// `Source::CopyRecord`, resolved via `compact`'s own `Source::CopyRecord` arm (`slice` then
    /// `append_raw_record`) — never reconstructed, never hash-checked (see `read_target`'s
    /// `Source::CopyRecord => Err(...)` arm: a `CopyRecord` target is never even routed through the
    /// read/verify path `Source::Loose` and `Source::Reconstruct` both go through). If the winning
    /// copy happens to be a corrupt one and a byte-correct copy exists in a different pack, the
    /// corrupt copy is carried into the new pack and *every* old pack — including the one holding
    /// the good copy — is deleted once the repack "succeeds" (the old-pack list `collect_targets`
    /// builds, removed by `compact`'s own old-pack sweep at the end). Silent, permanent data loss.
    ///
    /// The fixture: a live blob X, packed normally by a first `compact --all` into a genuine pack
    /// (pack A, a full record — X is the only object of its kind here, so there is nothing for it
    /// to delta against). A byte-for-byte duplicate of pack A is then made under a stem
    /// (`0000...-dup`) that is lexicographically before pack A's own content-hashed (blake3 hex)
    /// stem, forcing the duplicate to enumerate first (`load_packs_from_disk`'s file-name sort,
    /// whose own comment calls out this exact test shape). One byte is flipped inside the
    /// duplicate's copy of X's record *payload*, strictly past the record's 1-byte kind tag — so
    /// the kind byte (`RECORD_FULL`) survives and this stays a full record, never a delta. A delta
    /// corruption would instead trip the dropped-base rescue in `collect_targets` (the base is not
    /// in the live set, so the target is demoted to `Source::Reconstruct`, which *is*
    /// read-verified) and PR #76's exhaustive-verified-read fall-through — passing for the wrong
    /// reason. The duplicate's index tiles are left untouched, so it still loads and validates as
    /// an ordinary pack (index validation never inspects record shape) — modeling honest
    /// bitrot, not an unloadable pack.
    ///
    /// Contract asserted here — deliberately, not just "no crash": this fixture has a verifiable
    /// byte-correct copy of X sitting in pack A, so the repack is required to succeed *and*
    /// consolidate down to one pack, with X reading back correctly afterward. This test would fail
    /// just as loudly if the remedy were a fail-closed abort that left both packs in place: a good
    /// copy was available, so refusing to repack would mean leaving recoverable data unconsolidated,
    /// not avoiding data loss. Abort-and-preserve is the correct response only when *no* copy
    /// verifies at all — see
    /// `compact_redelta_aborts_on_a_corrupt_packed_record_leaving_the_old_pack_in_place` above,
    /// where there is exactly one copy and it is corrupt. Do not relax this test to accept a
    /// refusal.
    ///
    /// Every readback below is checked against `expected` -- the *framed* bytes
    /// `LooseObjectBuilder::build_blob` actually hashed and stored (its header, then the raw
    /// content), never the raw bytes handed to `Blob { content }` directly. Those are not the same:
    /// `build_blob` writes a header ahead of the content before hashing, so what a correct read
    /// returns, and what the hash actually addresses, is the framed form. This is the same
    /// convention `object_utils`'s large-object streaming round-trip test already follows (it
    /// captures the built object's own `.content`, not its caller-supplied input, as the value
    /// every readback is compared against) -- diverging from it here once already produced a test
    /// that could never go green: on `main` this fixture is red at the terminal `.expect()` (an
    /// `Err`, the FORK-55 symptom), which happens to short-circuit *before* a since-removed
    /// raw-bytes comparison would have also failed, on unrelated grounds, every single time --
    /// including against a correct fix. A pinning test that cannot go green even against a correct
    /// fix is not a falsifier at all.
    ///
    /// Expected outcome on `main`: a second `compact --all` picks the corrupt duplicate's record for
    /// X (it enumerates first), carries it forward unverified, and deletes pack A along with the
    /// duplicate — so X is unreadable afterward even though a byte-correct copy existed moments
    /// before the second `compact --all` ran, and `compact` itself reports success throughout (no
    /// error, no diagnostic).
    #[test]
    fn a_repack_must_not_carry_a_corrupt_duplicate_record_over_a_good_copy() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-dup-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        // Minimal reachable history: one blob (the object under test), its tree, one parentless
        // parcel, `main`'s head pointed at it. The blob is the object this fixture corrupts below
        // -- `compact`'s own unconditional commit-graph build reads (and process-globally caches)
        // the parcel and tree bytes on every call, so those two must stay untouched for the
        // corruption to actually be observed on the final read; see the identical reasoning in
        // `compact_redelta_aborts_on_a_corrupt_packed_record_leaving_the_old_pack_in_place`, just
        // above.
        let original_content = b"the true original content of X -- only these exact bytes are correct\n".to_vec();
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: original_content });
        blob.store().unwrap();
        let x_hash = blob.hash.clone();
        // What a correct read of X actually returns and hashes to -- see the doc comment above for
        // why this, not the raw bytes given to `Blob { content }` above, is every readback's
        // comparand.
        let expected = blob.content.clone();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("x.txt".to_string(), x_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // First `compact --all`: X is live (reachable from `main`'s head), so it is packed into a
        // genuine pack -- pack A. No existing packs yet, so nothing here is a `CopyRecord`; this
        // just gets X (and its tree/parcel) off loose and into a real pack the way any first
        // `compact --all` would.
        let first = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(first.objects_packed, 3, "sanity: blob + tree + parcel all packed");
        assert!(!is_stored_as_delta(&x_hash),
            "sanity: X must land as a full record for this fixture -- a delta would exercise a \
            different (already-guarded) path, see the doc comment above");

        let folder = pack_folder();
        let pack_a_data = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .expect("compact should have written one pack file");
        let pack_a_stem = pack_a_data.file_stem().unwrap().to_str().unwrap().to_string();
        let pack_a_index = pack_a_data.with_extension(PACK_INDEX_EXTENSION);

        // Byte-copy pack A's data+index to a stem that sorts before pack A's own (content-hashed,
        // lowercase-hex) stem -- both files copied verbatim, so the duplicate is initially a valid,
        // loadable pack, byte-identical to A.
        let dup_stem = "0000000000000000000000000000000000000000000000000000000000000000-dup";
        assert!(dup_stem < pack_a_stem.as_str(),
            "sanity: the duplicate's stem must sort before pack A's for the load order this test needs");
        let dup_data = folder.join(format!("{}.{}", dup_stem, PACK_DATA_EXTENSION));
        let dup_index = folder.join(format!("{}.{}", dup_stem, PACK_INDEX_EXTENSION));
        std::fs::copy(&pack_a_data, &dup_data).unwrap();
        std::fs::copy(&pack_a_index, &dup_index).unwrap();

        // Corrupt the duplicate: flip one byte inside X's record *payload*, strictly past its
        // 1-byte kind tag (so the kind byte -- RECORD_FULL -- survives; see the doc comment above
        // for why this must stay a full record, never a delta). The index tiles are left untouched,
        // so the duplicate still loads and validates normally.
        let x_hash_bytes = hash_to_bytes(&x_hash).unwrap();
        invalidate_cache();
        let (x_offset, x_len) = loaded_packs().unwrap().packs.iter()
            .find_map(|pack| pack.locate(&x_hash_bytes).found())
            .expect("X must be packed");
        let mut dup_bytes = std::fs::read(&dup_data).unwrap();
        let flip_at = x_offset as usize + 1;
        assert!((flip_at as u64) < x_offset + x_len, "the flip must land inside X's own record");
        dup_bytes[flip_at] ^= 0xFF;
        // Drop the mmap'd pack registry *before* rewriting the pack file: on Windows a file with a
        // user-mapped section open cannot be written -- the same handle constraint `compact`'s own
        // drop-before-delete guards against (see the redelta-abort test above).
        invalidate_cache();
        std::fs::write(&dup_data, &dup_bytes).unwrap();

        // Sanity: the duplicate pack loads normally (honest bitrot, not an unloadable pack) and
        // enumerates before pack A, so the fixture actually forces the ordering this test needs.
        invalidate_cache();
        {
            let loaded = loaded_packs().unwrap();
            assert_eq!(loaded.packs.len(), 2, "sanity: both packs must load");
            assert_eq!(loaded.packs[0].data_path, dup_data, "sanity: the corrupt duplicate must enumerate first");
            assert!(loaded.quarantined.is_empty(), "sanity: the corrupt duplicate must still load, not quarantine");
            // `loaded` holds an `Arc<PackSet>` whose `LoadedPack`s mmap both `dup_data` and
            // `pack_a_data`. It must not outlive this block: the second `compact` call below
            // renames over pack A's path and removes the old packs, and on Windows a file with a
            // mapped section still open cannot be renamed or deleted -- the same handle constraint
            // `compact`'s own `drop(source_packs)` guards against (see its comment). Scoping the
            // block, rather than a bare `drop(loaded)` at the end of the function, is what keeps
            // that true regardless of what gets added below in the future.
        }

        // Central premise, asserted directly rather than left implicit: X is still readable, with
        // its original bytes, right up to the moment the second `compact --all` runs -- both packs
        // present, corruption already written to the duplicate. Without this, a final-read failure
        // below could not distinguish "the repack destroyed the good copy" (the FORK-55 defect
        // this test pins) from "the corruption already broke reads with both packs present" (a
        // regression in the exhaustive verified read PR #76 added) -- this fixture would then pass
        // for the wrong reason if that read path ever regressed. Uncached for the same reason the
        // final read below is: see its own comment.
        let pre_repack = file_utils::retrieve_object_by_hash_uncached(&x_hash)
            .expect("X must still be readable before the repack, with both packs (good and corrupt) present");
        assert_eq!(pre_repack, expected, "X must read back its original bytes before the repack runs");

        // Second `compact --all`: re-enumerates both packs. X is live and present in both;
        // `collect_targets`'s first-pack-wins dedup takes the corrupt duplicate's record (it
        // enumerates first) and copies it verbatim -- never reconstructed, never hash-checked on
        // this path. Every old pack (the duplicate *and* pack A, the good copy) is deleted once
        // this repack completes.
        let second = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(second.objects_packed, 3, "sanity: the repack still carries all three live objects forward");

        // The repack did happen: the duplicate's own (distinctly-named) files are gone, and only
        // one pack file remains. Note this does *not* assert `!pack_a_data.exists()`: the new
        // pack's filename is layout-*derived* (`compute_pack_id`, hashed from each record's
        // `(hash, offset, length)` triple, never its bytes -- see the doc comment there), and this
        // fixture's corrupted byte changes neither hash, offset, nor length, so the repack's
        // deterministic layout lands the new pack on the exact same filename pack A already had --
        // the corrupted content is written straight over pack A's own path (`compact`'s old-pack
        // sweep has its own idempotent-rename skip, which exists for the *legitimate*
        // unchanged-repack case, and is what leaves that filename looking untouched while its
        // bytes underneath are now the corrupt copy's). A different fixture shape could easily
        // land the new pack on a fresh name instead -- either way, the good bytes are gone the
        // moment this repack completes.
        assert!(!dup_data.exists(), "sanity: the duplicate pack's own files must be gone after the repack");
        let packs_remaining: Vec<PathBuf> = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .collect();
        assert_eq!(packs_remaining.len(), 1,
            "sanity: the repack must have consolidated down to exactly one pack file, got {:?}", packs_remaining);

        // The claim under test: X must still read back correctly after the repack consolidated the
        // duplicate and the good pack together. Uncached on purpose -- X's *loose* bytes were read
        // (and so cached) while packing it in the first `compact --all` above; going through the
        // ordinary cached `retrieve_object_by_hash` here could serve that stale-but-correct cache
        // entry instead of actually exercising the post-repack pack read this test is pinning.
        let read_back = file_utils::retrieve_object_by_hash_uncached(&x_hash)
            .expect("X must still be readable after the repack consolidated the duplicate and the good pack together");
        assert_eq!(read_back, expected, "X must read back its original, correct bytes after the repack");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-55, T1b (§5 I1) — loose-vs-pack: a corrupt *loose* duplicate of a live packed hash
    /// must not shadow the good packed copy. Mechanism (iii)'s falsifier: before the fix, the
    /// loose loop in `collect_targets` seeds `seen` first, so a corrupt loose duplicate wins the
    /// dedup unconditionally, the packed (good) record is skipped, and the old pack holding it is
    /// deleted -- `CorruptLoose` then also skips the loose copy itself (never packed, never
    /// deleted), so the net result is a store whose only surviving copy of X is the corrupt loose
    /// file.
    ///
    /// The fixture: X packed good into pack A by a first `compact --all` (a full record, so this
    /// exercises the same shape as T1a rather than the already-guarded delta/dropped-base path).
    /// A corrupt loose file is then planted at X's own fan-out path with different content --
    /// `store_loose` writes bytes under whatever hash it is given, regardless of whether the
    /// bytes actually hash to it, so this reproduces a loose object whose content does not match
    /// its address without needing real bitrot.
    ///
    /// Under the fix, `collect_targets`'s loose loop still seeds `seen` with X's loose target
    /// first, but the pack loop's collision handling then demotes it in place to
    /// `Source::Reconstruct { loose_source: Some(path) }` -- the loose file's bytes are never
    /// trusted, X is read through the exhaustive verified read instead (which finds pack A's good
    /// record), and the demoted target still carries its loose file into the durable-before-
    /// destructive sweep, so the corrupt loose file is deleted once the (correct) repacked copy
    /// is durable.
    ///
    /// This is the only detector of the wrong-site plumbing defect (§2 item 4 / §8): X stays
    /// servable either way (it is packed from A's good copy), so servability alone cannot catch
    /// `loose_source` wired into the sweep (`pack_utils.rs:2051-2059`) instead of the loose-path
    /// registration site (`pack_utils.rs:1937-1940`) -- only "the corrupt loose file is gone"
    /// can.
    ///
    /// **Mutation that reddens it:** revert only the `Source::Loose(path)` collision arm back to
    /// a `continue` (the old first-pack-wins skip) -- the corrupt loose file then survives
    /// (`CorruptLoose`, never packed, never deleted) while pack A is deleted by the old-pack
    /// sweep, leaving X permanently unreadable. This is mechanism (iii)'s exact shape (§1.2).
    #[test]
    fn a_repack_must_not_let_a_corrupt_loose_duplicate_shadow_a_good_packed_copy() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-loose-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let original_content = b"the true original content of X -- loose-vs-pack duplicate\n".to_vec();
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: original_content });
        blob.store().unwrap();
        let x_hash = blob.hash.clone();
        // See T1a's doc comment for why this -- the framed bytes `build_blob` actually hashed
        // and stored -- is the correct comparand, never the raw bytes given to `Blob { content }`.
        let expected = blob.content.clone();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("x.txt".to_string(), x_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // First `compact --all`: X is packed good into pack A. No existing packs yet, so nothing
        // here is a `CopyRecord` -- this is a genuine, verified pack.
        let first = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(first.objects_packed, 3, "sanity: blob + tree + parcel all packed");

        let (folder, file_name) = file_utils::get_path_for_object(&x_hash).unwrap();
        let corrupt_loose_path = Path::new(&folder).join(&file_name);
        assert!(!corrupt_loose_path.exists(), "sanity: X's loose file was removed by the first compact");

        // Plant a corrupt loose duplicate at X's own address: valid zstd, wrong content -- a
        // hash mismatch, not a decode failure, so this models bitrot rather than an unloadable
        // file.
        store_loose(&x_hash, b"wrong content entirely -- this is not X");
        assert!(corrupt_loose_path.exists(), "sanity: the corrupt duplicate was planted");

        // Second `compact --all`: X is live and present both loose (corrupt) and packed (good,
        // in pack A). Must consolidate to the good copy and delete the corrupt loose duplicate.
        let second = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(second.objects_packed, 3, "sanity: the repack still carries all three live objects forward");

        assert!(!corrupt_loose_path.exists(),
            "the corrupt loose duplicate must be gone after the repack -- it must have ridden \
            `loose_source` into the durable-before-destructive sweep once the verified copy was \
            packed");

        // Uncached, per the read-cache polarity rule (§5): a hash proven *servable* must be read
        // uncached, so this exercises the actual post-repack pack read rather than a cache entry
        // some earlier step happened to warm.
        let read_back = file_utils::retrieve_object_by_hash_uncached(&x_hash)
            .expect("X must still be readable after the repack consolidated the loose duplicate and the good pack");
        assert_eq!(read_back, expected, "X must read back its original, correct bytes after the repack");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-55, T2 (§5 I2) — no *obtainable* verifiable copy of a live duplicated hash must abort
    /// `compact --all` before the destructive sweep, never carry a corrupt copy forward or
    /// silently drop the only recoverable state. Also pins §7's requirement that `read_target`'s
    /// `Reconstruct` abort names the failing hash and points at `heal` -- a "must" with no
    /// falsifier before this test (`is_err` alone would be satisfied by an abort on *any* hash,
    /// including the wrong one).
    ///
    /// The fixture: X packed good into pack A by a first `compact --all` (full record). Pack A is
    /// then byte-copied to a duplicate stem sorting before A's own, and *both* copies are
    /// corrupted -- differently, at different byte offsets inside X's record payload -- so no
    /// copy of X verifies anywhere in the store, and there is no loose X to fall back on.
    ///
    /// Under the fix, `collect_targets`'s pack-loop collision handling compares the two records'
    /// bytes (`slice`): they differ, so the duplicate is demoted to
    /// `Source::Reconstruct`. `read_target`'s `Reconstruct` arm then reads through the exhaustive
    /// verified read (`retrieve_object_by_hash`), which tries every locating pack and finds
    /// neither copy verifies -- the read errs, `prepare_target` propagates it via `?`, and the
    /// whole compact aborts *before* any pack is written this run (three objects is well under
    /// the parallel-prep threshold, so a serial `.collect()` aborts on the first error) --
    /// therefore before the destructive old-pack sweep, so both original pack pairs (the
    /// duplicate's and pack A's own) remain exactly as they were.
    ///
    /// **The registry must be invalidated before each in-place pack rewrite** (`loaded_packs`
    /// holds an `Arc` in a process-global registry; on Windows a mapped file cannot be rewritten)
    /// -- same pattern as T1a.
    ///
    /// **The read-cache polarity guard**, immediately before the compact under test: proving a
    /// hash *unservable* must read it through the *cached* path and prove the cache is cold --
    /// the process read cache can only ever err toward "servable" (a failed read inserts
    /// nothing), so this guard cannot itself produce a false negative, but without it a warm
    /// cache left over from setup could rescue the read and silently turn this into a
    /// non-falsifier.
    ///
    /// **Mutation that reddens it:** revert the pack-loop collision handling for a `CopyRecord`
    /// collision back to `continue` (keep the first-enumerated copy unconditionally) -- `compact`
    /// then *succeeds*, carrying the first corrupt copy forward and deleting both old packs, so
    /// `assert!(result.is_err())` reddens.
    #[test]
    fn a_repack_must_abort_before_the_sweep_when_no_copy_of_a_duplicated_hash_verifies() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-both-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let original_content = b"the true original content of X -- both-corrupt duplicate fixture\n".to_vec();
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: original_content });
        blob.store().unwrap();
        let x_hash = blob.hash.clone();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("x.txt".to_string(), x_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let first = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(first.objects_packed, 3, "sanity: blob + tree + parcel all packed");
        assert!(!is_stored_as_delta(&x_hash),
            "sanity: X must land as a full record for this fixture, see T1a's doc comment above");

        let folder = pack_folder();
        let pack_a_data = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .expect("compact should have written one pack file");
        let pack_a_stem = pack_a_data.file_stem().unwrap().to_str().unwrap().to_string();
        let pack_a_index = pack_a_data.with_extension(PACK_INDEX_EXTENSION);

        let dup_stem = "0000000000000000000000000000000000000000000000000000000000000000-dup";
        assert!(dup_stem < pack_a_stem.as_str(),
            "sanity: the duplicate's stem must sort before pack A's for the load order this test needs");
        let dup_data = folder.join(format!("{}.{}", dup_stem, PACK_DATA_EXTENSION));
        let dup_index = folder.join(format!("{}.{}", dup_stem, PACK_INDEX_EXTENSION));
        std::fs::copy(&pack_a_data, &dup_data).unwrap();
        std::fs::copy(&pack_a_index, &dup_index).unwrap();

        let x_hash_bytes = hash_to_bytes(&x_hash).unwrap();
        invalidate_cache();
        let (x_offset, x_len) = loaded_packs().unwrap().packs.iter()
            .find_map(|pack| pack.locate(&x_hash_bytes).found())
            .expect("X must be packed");

        // Corrupt the duplicate at one offset...
        let mut dup_bytes = std::fs::read(&dup_data).unwrap();
        let dup_flip_at = x_offset as usize + 1;
        assert!((dup_flip_at as u64) < x_offset + x_len, "the flip must land inside X's own record");
        dup_bytes[dup_flip_at] ^= 0xFF;
        invalidate_cache();
        std::fs::write(&dup_data, &dup_bytes).unwrap();

        // ...and pack A itself at a *different* offset, with a *different* value, so the two
        // corrupt copies are not just corrupt but corrupt differently -- ruling out a fix that
        // only detects a byte-identical pair.
        invalidate_cache();
        let mut a_bytes = std::fs::read(&pack_a_data).unwrap();
        let a_flip_at = x_offset as usize + 2;
        assert!((a_flip_at as u64) < x_offset + x_len, "the flip must land inside X's own record");
        a_bytes[a_flip_at] ^= 0x0F;
        invalidate_cache();
        std::fs::write(&pack_a_data, &a_bytes).unwrap();

        invalidate_cache();
        {
            let loaded = loaded_packs().unwrap();
            assert_eq!(loaded.packs.len(), 2, "sanity: both packs must load");
            assert_eq!(loaded.packs[0].data_path, dup_data, "sanity: the corrupt duplicate must enumerate first");
            assert!(loaded.quarantined.is_empty(), "sanity: both corrupt packs must still load, not quarantine");
        }

        // The read-cache polarity guard (§5): proving X unservable must go through the *cached*
        // path and prove the cache is cold, or a warm cache left over from setup could rescue the
        // read and silently turn this test into a non-falsifier.
        assert!(file_utils::retrieve_object_by_hash(&x_hash).is_err(),
            "fixture defect: X is still servable (warm read cache or corruption did not take) -- \
            the compact under test would be rescued, not exercised");

        let result = compact(true, false, true, &Progress::NONE);
        let error = match result {
            Err(e) => e,
            Ok(_) => panic!("compact must abort when no copy of a duplicated live hash verifies, got Ok"),
        };
        assert!(error.contains(&x_hash), "the abort must name the failing hash X, got: {}", error);
        assert!(error.to_lowercase().contains("heal"),
            "the abort must point the operator at `forklift heal`, got: {}", error);

        assert!(dup_data.exists() && dup_index.exists(),
            "abort must precede the destructive sweep -- the duplicate pack must survive");
        assert!(pack_a_data.exists() && pack_a_index.exists(),
            "abort must precede the destructive sweep -- pack A must survive");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-55, T3′ (§5 I3) — the falsifier for the byte-identical-duplicate fast path: when
    /// every duplicate copy of a live hash is byte-identical (even though all of them are
    /// corrupt), the fix must keep the verbatim `Source::CopyRecord` path rather than demoting to
    /// `Source::Reconstruct` -- there is no *choice* to make (W6: byte-identical framed records
    /// are interchangeable), and demoting anyway would turn an already-unservable hash into an
    /// aborted `compact --all` for no correctness benefit (a repack is not an audit).
    ///
    /// The fixture: X packed good into pack A by a first `compact --all` (full record). A single
    /// byte inside X's record payload is then flipped **in pack A itself**, and only *then* is
    /// the (now-corrupted) pack byte-copied to a duplicate stem sorting before A's own -- so both
    /// copies are corrupt, and byte-identically so (a genuine memcmp match, not merely the same
    /// length).
    ///
    /// Under the fix, `collect_targets`'s pack-loop collision handling compares the two records'
    /// framed bytes and finds them identical, so the `CopyRecord` is kept -- `compact --all`
    /// succeeds (it always did for a byte-identical duplicate; that is the whole point of the
    /// fast path), X remains exactly as unservable as it was before the repack, and I1 holds
    /// vacuously (repack never claimed to fix unservable data, only never to *destroy* servable
    /// data).
    ///
    /// **The read-cache polarity guard is what makes this a falsifier at all.** `read_target`'s
    /// `Reconstruct` arm reads through the *cached* `retrieve_object_by_hash`; under the
    /// demote-all mutation below, a warm cache left over from setup would serve X's correct
    /// cached bytes, `compact` would return `Ok` for the wrong reason, and
    /// `assert!(result.is_ok())` would pass whether or not the mutation was applied -- silently
    /// not a falsifier. The guard immediately below rules that out by proving the cache cold
    /// through the same cached path `Reconstruct` itself uses.
    ///
    /// **Mutation that reddens it:** remove the byte-identical (`CopyRecord`-preserving) branch
    /// from the pack-loop collision handling, demoting every duplicate unconditionally -- X
    /// becomes `Source::Reconstruct`, the exhaustive verified read finds no verifiable copy (both
    /// byte-identical copies are still corrupt), `compact` errs, and
    /// `assert!(result.is_ok())` reddens. Deterministic on every platform, given the guard above.
    #[test]
    fn a_repack_must_keep_the_verbatim_copy_for_byte_identical_corrupt_duplicates() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-identical-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let original_content = b"the true original content of X -- identical-corrupt duplicate fixture\n".to_vec();
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: original_content });
        blob.store().unwrap();
        let x_hash = blob.hash.clone();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("x.txt".to_string(), x_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let first = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(first.objects_packed, 3, "sanity: blob + tree + parcel all packed");
        assert!(!is_stored_as_delta(&x_hash),
            "sanity: X must land as a full record for this fixture, see T1a's doc comment above");

        let folder = pack_folder();
        let pack_a_data = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .expect("compact should have written one pack file");
        let pack_a_stem = pack_a_data.file_stem().unwrap().to_str().unwrap().to_string();
        let pack_a_index = pack_a_data.with_extension(PACK_INDEX_EXTENSION);

        let x_hash_bytes = hash_to_bytes(&x_hash).unwrap();
        invalidate_cache();
        let (x_offset, x_len) = loaded_packs().unwrap().packs.iter()
            .find_map(|pack| pack.locate(&x_hash_bytes).found())
            .expect("X must be packed");

        // Corrupt pack A itself first...
        let mut a_bytes = std::fs::read(&pack_a_data).unwrap();
        let flip_at = x_offset as usize + 1;
        assert!((flip_at as u64) < x_offset + x_len, "the flip must land inside X's own record");
        a_bytes[flip_at] ^= 0xFF;
        invalidate_cache();
        std::fs::write(&pack_a_data, &a_bytes).unwrap();

        // ...*then* byte-copy the now-corrupted pack, so the duplicate is byte-identical to A's
        // corrupt record, not merely a second independent corruption.
        let dup_stem = "0000000000000000000000000000000000000000000000000000000000000000-dup";
        assert!(dup_stem < pack_a_stem.as_str(),
            "sanity: the duplicate's stem must sort before pack A's for the load order this test needs");
        let dup_data = folder.join(format!("{}.{}", dup_stem, PACK_DATA_EXTENSION));
        let dup_index = folder.join(format!("{}.{}", dup_stem, PACK_INDEX_EXTENSION));
        std::fs::copy(&pack_a_data, &dup_data).unwrap();
        std::fs::copy(&pack_a_index, &dup_index).unwrap();

        invalidate_cache();
        {
            let loaded = loaded_packs().unwrap();
            assert_eq!(loaded.packs.len(), 2, "sanity: both packs must load");
            assert_eq!(loaded.packs[0].data_path, dup_data, "sanity: the duplicate must enumerate first");
            assert!(loaded.quarantined.is_empty(), "sanity: both corrupt packs must still load, not quarantine");
        }

        // The read-cache polarity guard (§5) -- see this test's doc comment above for why it is
        // load-bearing here specifically (silent, not loud, without it).
        assert!(file_utils::retrieve_object_by_hash(&x_hash).is_err(),
            "fixture defect: X is still servable (warm read cache or corruption did not take) -- \
            the compact under test would be rescued, not exercised");

        let result = compact(true, false, true, &Progress::NONE);
        assert!(result.is_ok(),
            "a byte-identical duplicate (even a corrupt one) must keep the verbatim CopyRecord \
            fast path and never abort the repack, got {:?}", result);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-55, T3 (§5 I3) — property assertion, not a falsifier (its redness under the
    /// demote-all mutation is likely but not guaranteed, since
    /// the encode pipeline is deterministic and a re-encode may reproduce the original layout and
    /// filename anyway). Retained for the crash-recovery idempotency story this fix must not
    /// regress: the common duplicate shape in practice is the crash-window self-duplicate --
    /// publishing new pack pairs by rename means an interrupted publish followed by a retry can
    /// leave two byte-identical copies of the same pack pair on disk. A clean (uncorrupted)
    /// self-duplicate must still take the fast path and land the repack on pack A's own original
    /// filename.
    #[test]
    fn a_clean_self_duplicate_repacks_onto_the_original_packs_own_filename() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-clean-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let original_content = b"the true original content of X -- clean self-duplicate fixture\n".to_vec();
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: original_content });
        blob.store().unwrap();
        let x_hash = blob.hash.clone();
        let expected = blob.content.clone();

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("x.txt".to_string(), x_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let first = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(first.objects_packed, 3, "sanity: blob + tree + parcel all packed");

        let folder = pack_folder();
        let pack_a_data = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .expect("compact should have written one pack file");
        let pack_a_stem = pack_a_data.file_stem().unwrap().to_str().unwrap().to_string();
        let pack_a_index = pack_a_data.with_extension(PACK_INDEX_EXTENSION);

        // A clean (uncorrupted) byte-copy under a stem sorting before A's own -- the crash-window
        // self-duplicate shape, without any corruption.
        let dup_stem = "0000000000000000000000000000000000000000000000000000000000000000-dup";
        assert!(dup_stem < pack_a_stem.as_str(),
            "sanity: the duplicate's stem must sort before pack A's for the load order this test needs");
        let dup_data = folder.join(format!("{}.{}", dup_stem, PACK_DATA_EXTENSION));
        let dup_index = folder.join(format!("{}.{}", dup_stem, PACK_INDEX_EXTENSION));
        std::fs::copy(&pack_a_data, &dup_data).unwrap();
        std::fs::copy(&pack_a_index, &dup_index).unwrap();

        invalidate_cache();
        {
            let loaded = loaded_packs().unwrap();
            assert_eq!(loaded.packs.len(), 2, "sanity: both packs must load");
            assert_eq!(loaded.packs[0].data_path, dup_data, "sanity: the duplicate must enumerate first");
        }

        let second = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(second.objects_packed, 3, "sanity: the repack still carries all three live objects forward");

        let packs_remaining: Vec<PathBuf> = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .collect();
        assert_eq!(packs_remaining.len(), 1,
            "the repack must have consolidated down to exactly one pack file, got {:?}", packs_remaining);
        assert_eq!(packs_remaining[0], pack_a_data,
            "a clean self-duplicate's repack must land on pack A's own original filename \
            (W5: pack ids are layout-derived, and a byte copy reproduces the layout)");

        let read_back = file_utils::retrieve_object_by_hash_uncached(&x_hash)
            .expect("X must still be readable after the repack consolidated the clean duplicate");
        assert_eq!(read_back, expected, "X must read back its original, correct bytes after the repack");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_duplicated_grandfathered_giant_is_packed_and_its_corrupt_loose_copy_is_swept() {
        // T4 (the D3 policy narrowing): unlike a *sole-copy* over-ceiling giant (see
        // `compact_skips_a_correctly_addressed_over_ceiling_giant_loose_object`, above -- left
        // loose and untouched, exactly as on `main`), a *duplicated* giant is demoted to
        // `Reconstruct` by `collect_targets` and packed via its unbounded local decode, and any
        // corrupt loose duplicate rides the ordinary durable-before-destructive sweep once the
        // repack is durable. I1 wins over "never packed, never deleted" for this legacy state.
        //
        // No shipped writer ever packs a giant's *first* copy -- the `OverCeiling` skip is the
        // only writer-side disposition for a sole-copy giant, and the only way this fix packs one
        // is by demoting an already-*duplicated* giant, which needs a packed copy to already
        // exist. So this fixture hand-builds pack A's record directly with the crate's own
        // private `PackWriter` -- the exact primitive `compact` itself drives, called the same
        // way `compact` would have called it had a duplicate existed at write time. This is not a
        // production hook: no test-only escape hatch is added to production code, and the pack
        // produced is byte-for-byte what a real write path emits (magic, version, framing,
        // layout-derived id via `finalize`). The pack loader never validates a pack's stem against
        // `compute_pack_id` of its own index (`load_pack_pair` and index validation check
        // only the data/index framing and record layout, never the filename), so no stem surgery
        // is needed either.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-fork55-dup-giant-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let giant = deterministic_fill(object_utils::MAX_OBJECT_BYTES + 1);
        let giant_hash_hex = blake3::hash(&giant).to_hex().to_string();
        let giant_hash_bytes = hash_to_bytes(&giant_hash_hex).unwrap();

        // Hand-built pack A: the giant's one and only full record, written with the same
        // `PackWriter` primitive `compact` uses internally.
        file_utils::create_folder_if_not_exists(&pack_folder()).unwrap();
        let compressed = zstd::encode_all(giant.as_slice(), 0).unwrap();
        let mut writer = PackWriter::new(&pack_folder()).unwrap();
        writer.append_full(giant_hash_bytes, &compressed, None).unwrap();
        writer.finalize().unwrap();

        // A corrupt loose duplicate at the giant's own fan-out path -- honest bitrot (wrong
        // content entirely), not a re-encode, so it fails verification outright rather than
        // merely differing in bytes.
        let (folder, file_name) = file_utils::get_path_for_object(&giant_hash_hex).unwrap();
        let giant_loose_path = Path::new(&folder).join(&file_name);
        file_utils::write_object_to_file(
            Path::new(&folder), &file_name, b"not the giant at all -- honest bitrot".to_vec()
        ).unwrap();

        // Reachable history: a tree referencing the giant blob's hash, a parentless parcel,
        // `main`'s head -- so `collect_live_set`'s walk marks the giant live (a blob leaf is
        // marked live by hash alone, its bytes never read by that walk -- see
        // `gc_utils::collect_live_set`) instead of collecting it as garbage.
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("giant.bin".to_string(), giant_hash_hex.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        invalidate_cache();
        // Sanity: the giant really is duplicated (a live packed copy plus a corrupt loose file at
        // its own address) and the packed copy is the one that verifies -- both true before the
        // repack under test ever runs.
        assert!(is_in_packs(&giant_hash_hex).unwrap(), "sanity: the giant must be packed already");
        assert!(giant_loose_path.exists(), "sanity: the corrupt loose duplicate must be on disk");
        let pre_repack = file_utils::retrieve_object_by_hash_uncached(&giant_hash_hex)
            .expect("the giant must already be servable, from its packed copy, before the repack");
        assert_eq!(pre_repack, giant, "the pre-repack read must return the giant's real bytes");

        let folder = pack_folder();
        let stats = compact(true, false, true, &Progress::NONE).unwrap();

        assert_eq!(stats.objects_packed, 3, "sanity: giant + tree + parcel all carried forward");
        assert_eq!(stats.over_ceiling_skipped, 0,
            "a duplicated giant must be packed, not skipped the way a sole-copy giant is");
        assert!(!giant_loose_path.exists(),
            "the corrupt loose duplicate must be swept once the repack is durable (loose_source)");

        let packs_remaining: Vec<PathBuf> = std::fs::read_dir(&folder).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .collect();
        assert_eq!(packs_remaining.len(), 1,
            "the repack must have consolidated down to exactly one pack file, got {:?}", packs_remaining);

        let servable = file_utils::retrieve_object_by_hash_uncached(&giant_hash_hex)
            .expect("the giant must still be servable after the repack packed its duplicated copy");
        assert_eq!(servable, giant, "the giant must read back its real bytes after the repack");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Fire-site f (durable-taint wiring): `compact`'s own new-pack directory sync — the last of
    /// the three write paths added to the durable-taint wiring. Reverting the
    /// `file_utils::sync_dir_or_taint` wiring back to a bare `file_utils::sync_dir(&pack_folder)?`
    /// call kills the taint half of this test; making the destructive sweep below run
    /// unconditionally (moving it ahead of the `?`, or dropping the `?` on the sync call) kills
    /// the durable-before-destructive half — the loose source would then be gone despite the
    /// new pack's own directory entry never having been proven durable.
    #[test]
    #[cfg(unix)]
    fn compact_new_pack_dir_sync_failure_taints_the_new_packs_and_skips_the_destructive_sweep() {
        use crate::util::taint_utils;
        use crate::util::file_utils::SyncDirFaultGuard;

        let _serial = taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        taint_utils::activate();

        let temp = std::env::temp_dir().join(format!("forklift-compact-taint-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let forklift = crate::globals::forklift_root();

        let content = b"compact-taint content".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);

        let (folder, file_name) = file_utils::get_path_for_object(&hash).unwrap();
        let loose_path = Path::new(&folder).join(&file_name);
        assert!(loose_path.exists(), "sanity: the loose source exists before compacting");

        let _guard = SyncDirFaultGuard::failing("pack");
        let error = match compact(false, false, false, &Progress::NONE) {
            Err(e) => e,
            Ok(_) => panic!("a blocked pack directory must fail compact's new-pack sync"),
        };
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        // Durable-before-destructive, extended: the destructive sweep (removing packed loose
        // sources) must never run when the new pack's own directory entry was not proven durable
        // — a standing taint (or the sync failure that caused it) blocks it exactly like an
        // unsynced pack always has.
        assert!(loose_path.exists(),
            "compact must not remove a loose source before its new pack's directory sync succeeds");

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.recorded.is_empty(), "a new-pack directory-sync failure must record a taint");
        for path in &state.recorded {
            assert!(path.starts_with("objects/pack"), "expected a pack path, got {:?}", path);
        }

        drop(_guard);
        // `test_remove_taint_files`, not the production `resolve_taints`, deliberately: this
        // clears the durable taint files while leaving the in-memory gate exactly as `compact`'s
        // own internal failure path set it, because the next assertion (a second, hand-planted
        // taint file below) pins the durable-file re-check path, not the gate.
        taint_utils::test_remove_taint_files(&forklift, &state.files).unwrap();

        // The second, more precise half of the durable-before-destructive claim: even with the
        // directory sync itself succeeding cleanly (no fault armed below), a taint standing from
        // *something else* must still block the sweep — the re-check runs before `Ok`, not the
        // sync alone.
        let content_b = b"compact-taint content, round two".to_vec();
        let hash_b = blake3::hash(&content_b).to_hex().to_string();
        store_loose(&hash_b, &content_b);
        let (folder_b, file_name_b) = file_utils::get_path_for_object(&hash_b).unwrap();
        let loose_path_b = Path::new(&folder_b).join(&file_name_b);
        assert!(loose_path_b.exists(), "sanity: the second loose source exists before compacting");

        let taint_dir = forklift.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), crate::util::taint_utils::taint_file_bytes(["objects/zz/preexisting"])).unwrap();

        let error = match compact(false, false, false, &Progress::NONE) {
            Err(e) => e,
            Ok(_) => panic!("a standing taint must fail compact's re-check even with a clean sync"),
        };
        assert!(error.contains(taint_utils::GATE_TAINT_MARKER), "got: {}", error);
        assert!(loose_path_b.exists(),
            "compact must not remove a loose source while a taint is standing, even with a \
            clean directory sync");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_stores_similar_objects_as_deltas_that_round_trip() {
        let temp = std::env::temp_dir().join(format!("forklift-pack-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // A large high-entropy body (so its *full* zstd blob stays large and a delta genuinely
        // wins), derived deterministically by chaining Blake3 — no rng needed.
        let mut body: Vec<u8> = Vec::new();
        let mut seed = blake3::hash(b"delta-test-body").as_bytes().to_vec();
        while body.len() < 20_000 {
            seed = blake3::hash(&seed).as_bytes().to_vec();
            body.extend_from_slice(&seed);
        }

        // 30 "versions" of one file: the same body plus a small unique tail — exactly the
        // version-to-version redundancy deltas are meant to collapse.
        let contents: Vec<Vec<u8>> = (0..30).map(|i| {
            let mut v = body.clone();
            v.extend_from_slice(format!("\nunique tail for version {i}\n").as_bytes());
            v
        }).collect();
        let hashes: Vec<String> = contents.iter().map(|c| blake3::hash(c).to_hex().to_string()).collect();
        for (hash, content) in hashes.iter().zip(&contents) {
            store_loose(hash, content);
        }

        let full_size: u64 = contents.iter().map(|c| c.len() as u64).sum();

        let stats = compact(false, false, false, &Progress::NONE).unwrap();
        assert_eq!(stats.objects_packed, 30);
        assert!(stats.deltas > 0, "similar objects should be stored as deltas (got {})", stats.deltas);

        // Every version reconstructs byte-for-byte from the packs — through its delta chain,
        // whose base is fetched recursively from the store.
        for (hash, content) in hashes.iter().zip(&contents) {
            assert_eq!(packs_verified(hash), *content, "a delta must reconstruct exactly");
        }

        // And the deltas actually shrank the store far below storing every version in full
        // (a body of high-entropy bytes barely compresses on its own, so this is all delta).
        assert!(stats.bytes_packed < full_size / 3,
            "deltas should shrink the store: packed {} vs full {}", stats.bytes_packed, full_size);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_pack_of_any_other_format_version_is_refused_by_name() {
        // Versions 1 and 2 had no fanout table; a later version may have anything. Neither may be
        // read as version 3, and neither may be silently ignored: the load names the version it
        // found, so the operator knows the warehouse needs re-creating rather than repair.
        let temp = std::env::temp_dir().join(format!("forklift-pack-old-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();

        let content = vec![9u8; 3000];
        let hash_bytes = *blake3::hash(&content).as_bytes();
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();

        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        let offset = data.len() as u64;
        data.push(RECORD_FULL);
        data.extend_from_slice(&compressed);
        let length = data.len() as u64 - offset;

        let data_path = temp.join("p.pack");
        let index_path = temp.join("p.idx");
        std::fs::write(&data_path, &data).unwrap();

        // The current version loads, so the refusals below are about the version alone.
        std::fs::write(&index_path, build_index_bytes(&[(hash_bytes, offset, length)])).unwrap();
        load_pack_pair(&data_path, &index_path).expect("a version-3 pair must load");

        for version in [1u32, 2, 4] {
            // An old-layout index: magic, version, count, then the record with no fanout table.
            let mut index = Vec::new();
            index.extend_from_slice(PACK_INDEX_MAGIC);
            index.extend_from_slice(&version.to_le_bytes());
            index.extend_from_slice(&1u32.to_le_bytes());
            index.extend_from_slice(&hash_bytes);
            index.extend_from_slice(&offset.to_le_bytes());
            index.extend_from_slice(&length.to_le_bytes());
            std::fs::write(&index_path, &index).unwrap();

            let error = load_pack_pair(&data_path, &index_path).err().expect("another index version must be refused");
            assert!(error.contains(&format!("format version {}", version)) && error.contains("reads only version 3"),
                "the refusal must name the version found: {}", error);
        }

        // A version-3 index over a data file of another version is refused by the data's version.
        std::fs::write(&index_path, build_index_bytes(&[(hash_bytes, offset, length)])).unwrap();
        let mut old_data = data.clone();
        old_data[8..12].copy_from_slice(&2u32.to_le_bytes());
        std::fs::write(&data_path, &old_data).unwrap();
        let error = load_pack_pair(&data_path, &index_path).err().expect("another data version must be refused");
        assert!(error.contains("Pack data") && error.contains("format version 2"), "{}", error);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_pack_record_that_decompresses_to_the_wrong_bytes_fails_the_read() {
        // The silent-corruption case the read-side hash check guards against: a pack whose record decompresses
        // *cleanly* but to bytes that are not the object its index is addressed by (a damaged
        // record, or a delta rebuilt against the wrong base). Without the read-side hash check
        // this returns wrong bytes silently; with it, the read must error.
        let temp = std::env::temp_dir().join(format!("forklift-pack-corrupt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let pack_folder = temp.join(".forklift/objects/pack");
        std::fs::create_dir_all(&pack_folder).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Address the record by hash(A), but store a valid zstd blob of a *different* content B.
        let content_a = vec![1u8; 4000];
        let content_b = vec![2u8; 4000];
        let hash_a = blake3::hash(&content_a).to_hex().to_string();
        let hash_a_bytes = hash_to_bytes(&hash_a).unwrap();
        let blob_b = zstd::encode_all(content_b.as_slice(), 0).unwrap();

        // Data: header, then one RECORD_FULL whose payload is B's blob.
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        let offset = data.len() as u64;
        data.push(RECORD_FULL);
        data.extend_from_slice(&blob_b);
        let length = 1 + blob_b.len() as u64;

        // Index: one (hash A, offset, length) record.
        let index = build_index_bytes(&[(hash_a_bytes, offset, length)]);

        std::fs::write(pack_folder.join("corrupt.pack"), &data).unwrap();
        std::fs::write(pack_folder.join("corrupt.idx"), &index).unwrap();
        invalidate_cache();

        // The record is present and decompresses fine, but to the wrong bytes — the read fails
        // instead of returning content B under hash A.
        assert!(is_in_packs(&hash_a).unwrap(), "the record is indexed under hash A");
        let result = retrieve_from_packs(&hash_a).unwrap();
        match &result {
            PackRetrieval::Failed(e) => assert!(e.contains("corrupt"), "the error should name the corruption: {}", e),
            other => panic!("a record decompressing to the wrong bytes must fail the read, got {:?}", other),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A full record of `content`.
    fn full_record(content: &[u8]) -> Vec<u8> {
        let mut record = vec![RECORD_FULL];
        record.extend_from_slice(&zstd::encode_all(content, 0).unwrap());
        record
    }

    /// A delta record that decodes to `target` against `base` (whatever hash it names).
    fn delta_record(base_hash: &str, base: &[u8], target: &[u8]) -> Vec<u8> {
        let mut record = vec![RECORD_DELTA];
        record.extend_from_slice(&hash_to_bytes(base_hash).unwrap());
        record.extend_from_slice(&byte_utils::number_to_vlq_bytes(target.len() as u64));
        record.extend_from_slice(&delta_utils::compress_delta(base, target).unwrap());
        record
    }

    /// Write one pack, `<stem>.pack`/`<stem>.idx`, holding `records` addressed by the given hex
    /// hashes — whatever the records decode to — and drop the cached registry.
    fn write_raw_pack(pack_folder: &Path, stem: &str, records: &[(&str, Vec<u8>)]) {
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        let mut index_records = Vec::new();
        for (hash, record) in records {
            index_records.push((hash_to_bytes(hash).unwrap(), data.len() as u64, record.len() as u64));
            data.extend_from_slice(record);
        }
        index_records.sort_by(|a, b| a.0.cmp(&b.0));
        std::fs::create_dir_all(pack_folder).unwrap();
        std::fs::write(pack_folder.join(format!("{}.pack", stem)), &data).unwrap();
        std::fs::write(pack_folder.join(format!("{}.idx", stem)), build_index_bytes(&index_records)).unwrap();
        invalidate_cache();
    }

    /// Three related contents (c → b → t, each a small edit of the one before), and their hashes.
    fn chain_contents(tag: &str) -> [(Vec<u8>, String); 3] {
        let c = [tag.as_bytes(), &deterministic_fill(6000)].concat();
        let b = [c.as_slice(), b"\nthe second version adds a line\n"].concat();
        let t = [b.as_slice(), b"\nand the third another\n"].concat();
        [c, b, t].map(|content| {
            let hash = blake3::hash(&content).to_hex().to_string();
            (content, hash)
        })
    }

    fn verify_calls() -> usize {
        object_utils::VERIFY_CALLS.with(|calls| calls.get())
    }

    /// A cold read of an object at the end of a delta chain hashes only that object, not every
    /// base on the way; each base still hashes once when it is read directly, and never again.
    ///
    /// Mutation: drop `resolve_record`'s unverified attempt → the target's read hashes three
    /// times (target and both bases) → red.
    #[test]
    fn a_cold_delta_chain_read_hashes_only_the_object_asked_for() {
        let temp = fresh_root("delta-chain-verify-once");
        let _scope = StorageRootScope::enter(&temp);
        let [(c, c_hash), (b, b_hash), (t, t_hash)] = chain_contents("verify-once");
        write_raw_pack(&temp.join(".forklift/objects/pack"), "chain", &[
            (&c_hash, full_record(&c)),
            (&b_hash, delta_record(&c_hash, &c, &b)),
            (&t_hash, delta_record(&b_hash, &b, &t)),
        ]);

        let before = verify_calls();
        assert_eq!(file_utils::retrieve_object_by_hash(&t_hash).unwrap(), t);
        assert_eq!(verify_calls() - before, 1, "a cold chain read hashes the target only");

        let before = verify_calls();
        assert_eq!(file_utils::retrieve_object_by_hash(&b_hash).unwrap(), b);
        assert_eq!(file_utils::retrieve_object_by_hash(&b_hash).unwrap(), b);
        assert_eq!(verify_calls() - before, 1, "a base read directly hashes once, then serves from the cache");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// On a thread with `WORKER_STACK_SIZE`, a cold read of a chain one delta past
    /// `MAX_RECONSTRUCT_DEPTH` fails with the depth-limit error, and one exactly at the limit reads
    /// back: the recursion to the limit fits that stack, so the backstop is reached as an error and
    /// not as a stack overflow, which aborts the process. Both reads start beneath 4 MiB of used
    /// stack, standing in for the frames above a real read (the async entry point, the command,
    /// clap's), so the test pins headroom and not only a fit at the top of a fresh thread. The
    /// at-limit read is the deeper one: it recurses all the way and decodes on the way back. The
    /// past-limit read goes first because the limit bounds one reconstruction's recursion: once the
    /// at-limit read has cached the bases, the deeper object is a single delta away. The
    /// past-limit read fails before decoding anything, so the at-limit read after it is still cold.
    ///
    /// Mutation (debug build, which is what `cargo test` and CI run): `WORKER_STACK_SIZE` back to
    /// 8 MiB → the read (about 7.5 MiB) overflows beneath the 4 MiB → red. In a release build the
    /// read takes under 2 MiB and the mutation stays green.
    #[test]
    fn a_chain_at_the_depth_limit_reads_and_one_past_it_fails_cleanly_on_the_worker_stack() {
        let temp = fresh_root("delta-chain-depth-limit");
        let links = MAX_RECONSTRUCT_DEPTH as usize + 1;
        let contents: Vec<Vec<u8>> = (0..=links)
            .map(|i| [b"depth limit chain".as_slice(), &deterministic_fill(64), format!("{}", i).as_bytes()].concat())
            .collect();
        let hashes: Vec<String> = contents.iter().map(|c| blake3::hash(c).to_hex().to_string()).collect();
        let records: Vec<(&str, Vec<u8>)> = (0..=links)
            .map(|i| (hashes[i].as_str(), match i {
                0 => full_record(&contents[0]),
                _ => delta_record(&hashes[i - 1], &contents[i - 1], &contents[i]),
            }))
            .collect();
        write_raw_pack(&temp.join(".forklift/objects/pack"), "deep", &records);

        let root = temp.clone();
        let (at_limit, past_limit) = crate::util::fanout_utils::run_on_worker_stack("depth-limit-probe", move || {
            let _scope = StorageRootScope::enter(&root);
            let top = 0u8;
            let past_limit = beneath_used_stack(&top as *const u8 as usize, 4 * 1024 * 1024,
                                                &|| file_utils::retrieve_object_by_hash(&hashes[links]));
            let at_limit = beneath_used_stack(&top as *const u8 as usize, 4 * 1024 * 1024,
                                              &|| file_utils::retrieve_object_by_hash(&hashes[links - 1]));
            (at_limit, past_limit)
        });

        let error = past_limit.expect_err("a cold read of a chain past the depth limit must fail");
        assert!(error.contains("depth limit"), "the failure must name the depth limit: {}", error);
        assert_eq!(at_limit.as_deref(), Ok(contents[links - 1].as_slice()),
                   "a chain exactly at the depth limit must read back");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Call `f` once the stack in use, measured from `top` (an address in an outer frame), is at
    /// least `bytes`.
    #[inline(never)]
    fn beneath_used_stack<R>(top: usize, bytes: usize, f: &dyn Fn() -> R) -> R {
        let frame = std::hint::black_box([0u8; 1024]);
        let here = std::hint::black_box(&frame as *const _ as usize);
        if here.abs_diff(top) >= bytes { return f(); }
        let result = beneath_used_stack(top, bytes, f);
        std::hint::black_box(&frame);
        result
    }

    /// A chain whose middle record decodes to the wrong bytes fails its target's read with the
    /// error naming the corrupt middle object — the attribution the fully verified read gives —
    /// and the middle object fails its own read.
    ///
    /// Mutation: return the unverified attempt's error instead of redoing the read with every
    /// base verified → the error names the target, not the corrupt base → red.
    #[test]
    fn a_corrupt_intermediate_base_fails_the_read_naming_the_base() {
        let temp = fresh_root("delta-chain-corrupt-base");
        let _scope = StorageRootScope::enter(&temp);
        let [(c, c_hash), (b, b_hash), (t, t_hash)] = chain_contents("corrupt-base");
        let wrong_b = [b.as_slice(), b"not what b holds"].concat();
        write_raw_pack(&temp.join(".forklift/objects/pack"), "chain", &[
            (&c_hash, full_record(&c)),
            (&b_hash, delta_record(&c_hash, &c, &wrong_b)),
            (&t_hash, delta_record(&b_hash, &b, &t)),
        ]);

        let error = file_utils::retrieve_object_by_hash(&t_hash).unwrap_err();
        assert!(error.contains(&format!("Object {} is corrupt", b_hash)),
            "the error must name the corrupt base, got: {}", error);
        let error = file_utils::retrieve_object_by_hash(&b_hash).unwrap_err();
        assert!(error.contains(&format!("Object {} is corrupt", b_hash)), "got: {}", error);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A target encoded against the bytes a corrupt base's record actually decodes to reads,
    /// because its own bytes verify — but those base bytes are never served as the base, before
    /// or after the target's read put them in the unverified base cache.
    ///
    /// Mutation: have the direct read take a cached unverified base (look `hash` up in
    /// `unverified_base_cache` in `retrieve_object_by_hash_shared`) → the base's read returns the
    /// wrong bytes → red.
    #[test]
    fn bytes_reconstructed_for_a_corrupt_base_are_never_served_as_that_base() {
        let temp = fresh_root("delta-chain-unverified-base");
        let _scope = StorageRootScope::enter(&temp);
        let [(c, c_hash), (b, b_hash), (t, t_hash)] = chain_contents("unverified-base");
        let wrong_b = [b.as_slice(), b"not what b holds"].concat();
        write_raw_pack(&temp.join(".forklift/objects/pack"), "chain", &[
            (&c_hash, full_record(&c)),
            (&b_hash, delta_record(&c_hash, &c, &wrong_b)),
            (&t_hash, delta_record(&b_hash, &wrong_b, &t)),
        ]);

        assert_eq!(file_utils::retrieve_object_by_hash(&t_hash).unwrap(), t);
        let error = file_utils::retrieve_object_by_hash(&b_hash).unwrap_err();
        assert!(error.contains(&format!("Object {} is corrupt", b_hash)), "got: {}", error);
        assert!(matches!(retrieve_from_packs(&b_hash).unwrap(), PackRetrieval::Failed(_)));

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Reading the end of a long chain whose root is corrupt reloads the pack registry once, for
    /// the read that was asked for — not at every level of the chain, which doubled per level.
    ///
    /// Mutation: drop the two `in_reconstruction()` arms in `read_object_classified` → 1023
    /// (2^10 − 1) forced reloads → red.
    #[test]
    fn a_corrupt_chain_root_costs_one_registry_reload_not_one_per_level() {
        let temp = fresh_root("delta-chain-corrupt-root");
        let _scope = StorageRootScope::enter(&temp);

        let versions: Vec<Vec<u8>> = (0..10)
            .map(|i| [deterministic_fill(4000).as_slice(), format!("\nversion {}\n", i).as_bytes()].concat())
            .collect();
        let hashes: Vec<String> = versions.iter().map(|v| blake3::hash(v).to_hex().to_string()).collect();
        let mut records = vec![(hashes[0].as_str(), full_record(b"the root's record holds other bytes"))];
        for i in 1..versions.len() {
            records.push((hashes[i].as_str(), delta_record(&hashes[i - 1], &versions[i - 1], &versions[i])));
        }
        write_raw_pack(&temp.join(".forklift/objects/pack"), "chain", &records);

        let before = FORCED_RELOADS.with(|reloads| reloads.get());
        let error = file_utils::retrieve_object_by_hash(hashes.last().unwrap()).unwrap_err();
        assert!(error.contains(&format!("Object {} is corrupt", hashes[0])), "got: {}", error);
        assert_eq!(FORCED_RELOADS.with(|reloads| reloads.get()) - before, 1);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// `compact --all` never copies a delta verbatim once its base is repacked from a different
    /// copy than the one the delta was encoded against. The pack that sorts first holds a corrupt
    /// base record and a target encoded against what that record decodes to; a later pack holds a
    /// good copy of the base. So the target reads before the repack: its read meets the corrupt
    /// copy first. The repack keeps the base's good copy alone, against which a verbatim copy of
    /// the target could never decode, so it must read the target rather than copy it. That read
    /// may meet either copy, depending on what the caches hold, so the run may either fail loudly
    /// naming the target, with the old packs left in place, or succeed with the target still
    /// readable. What it must never do is succeed and lose the target.
    ///
    /// Mutation: drop the copied-delta pass at the end of `collect_targets` → the compact succeeds
    /// and the target no longer reads → red.
    #[test]
    fn a_repack_never_copies_a_delta_whose_base_it_repacks_from_another_copy() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = fresh_root("delta-copy-reread-base");
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let body = deterministic_fill(6000);
        let base = LooseObjectBuilder::build_blob(&Blob { content: [body.as_slice(), b"\nv1\n"].concat() });
        let target = LooseObjectBuilder::build_blob(&Blob { content: [body.as_slice(), b"\nv1\nv2\n"].concat() });
        let wrong_base = [base.content.as_slice(), b"not what the base holds"].concat();

        let pack_folder = temp.join(".forklift/objects/pack");
        write_raw_pack(&pack_folder, "a-crafted", &[
            (&base.hash, full_record(&wrong_base)),
            (&target.hash, delta_record(&base.hash, &wrong_base, &target.content)),
        ]);
        write_raw_pack(&pack_folder, "z-good-base", &[(&base.hash, full_record(&base.content))]);

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("base.txt".to_string(), base.hash.clone(), DirEntryType::Normal));
        tree.add_child(TreeItem::new("target.txt".to_string(), target.hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();
        let mut parcel = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("crafted".to_string()),
        });
        parcel.store().unwrap();
        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        assert!(matches!(retrieve_from_packs(&target.hash).unwrap(), PackRetrieval::Verified(_)),
            "the fixture's target must read before the repack");

        match compact(true, false, true, &Progress::NONE) {
            Ok(_) => {
                invalidate_cache();
                assert!(matches!(retrieve_from_packs(&target.hash).unwrap(), PackRetrieval::Verified(_)),
                    "a repack that succeeds must leave the target readable");
            }
            Err(error) => {
                assert!(error.contains(&target.hash), "the failed repack must name the target: {}", error);
                assert!(pack_folder.join("a-crafted.pack").exists() && pack_folder.join("z-good-base.pack").exists(),
                    "a failed repack must leave the old packs in place");
            }
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Rename a just-finalized pack pair's stem, so file-name enumeration order is deterministic
    /// in a test. Pack stems are otherwise layout-derived (`compute_pack_id`, layout-hashed) and
    /// so arbitrary from a test's point of view; `load_pack_pair` validates magic/version/record
    /// structure, never the stem (see the load-time sort's own precondition note on
    /// `retrieve_from_packs`'s I5 falsifier), so any stem is safe to give it.
    fn rename_pack_stem(artifacts: &[TransportPackArtifact], pack_folder: &Path, new_stem: &str) {
        assert_eq!(artifacts.len(), 1, "test fixture expects exactly one pack artifact pair");
        for artifact_path in [&artifacts[0].data_path, &artifacts[0].index_path] {
            let extension = artifact_path.extension().unwrap().to_str().unwrap();
            let new_path = pack_folder.join(format!("{}.{}", new_stem, extension));
            std::fs::rename(artifact_path, &new_path).unwrap();
        }
    }

    /// (I5, falsifies `pack_utils.rs:1148`'s old first-locate-wins `return`) A located pack
    /// record that fails to resolve must never suppress a good, verifying record for the same
    /// hash sitting in a *different* pack — `retrieve_from_packs` must try every locating pack,
    /// not just the first. The load-time file-name sort (§2c) forces the broken copy to be
    /// enumerated first on every platform, so this pins the worst order deterministically rather
    /// than depending on `read_dir`'s platform-arbitrary order to happen to hit it.
    ///
    /// Mutation: restore the unconditional `return resolve_record(...).map(Some)` on the first
    /// `locate` hit (the pre-fix body of `retrieve_from_packs`) → the broken pack (enumerated
    /// first) wins and the good copy is never even tried → red.
    #[test]
    fn a_broken_record_falls_through_to_a_good_copy_in_a_later_pack() {
        let temp = std::env::temp_dir()
            .join(format!("forklift-pack-broken-then-good-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let content = b"a genuinely good copy of X, sitting in the second, sorted-later pack";
        let hash = blake3::hash(content).to_hex().to_string();

        // Pack sorted first: a delta against a base that was never stored — located, but
        // unresolvable.
        let never_stored_base = "9".repeat(64);
        let mut broken_builder = TransportPackBuilder::new(&folder).unwrap();
        broken_builder.append_delta(&hash, &never_stored_base, 10, b"not a real delta payload").unwrap();
        let broken_artifacts = broken_builder.finish().unwrap();
        rename_pack_stem(&broken_artifacts, &folder, "0000-broken-first");

        // Pack sorted after: a genuine full record for the same hash.
        let mut good_builder = TransportPackBuilder::new(&folder).unwrap();
        good_builder.append_full(&hash, content.as_slice()).unwrap();
        let good_artifacts = good_builder.finish().unwrap();
        rename_pack_stem(&good_artifacts, &folder, "zzzz-good-second");

        invalidate_cache();

        assert_eq!(packs_verified(&hash), content);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (I5, both-broken companion to the test above) When *every* locating pack's record fails to
    /// resolve, the terminal outcome is `Failed`, carrying the first error — never a silent
    /// success and never a panic from trying to keep scanning past the last pack.
    #[test]
    fn when_every_located_copy_is_broken_the_outcome_is_failed_not_verified() {
        let temp = std::env::temp_dir()
            .join(format!("forklift-pack-broken-then-broken-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let hash = "c".repeat(64);

        let never_stored_base_1 = "d".repeat(64);
        let mut first_builder = TransportPackBuilder::new(&folder).unwrap();
        first_builder.append_delta(&hash, &never_stored_base_1, 10, b"not a real delta payload").unwrap();
        let first_artifacts = first_builder.finish().unwrap();
        rename_pack_stem(&first_artifacts, &folder, "0000-broken-first");

        let never_stored_base_2 = "e".repeat(64);
        let mut second_builder = TransportPackBuilder::new(&folder).unwrap();
        second_builder.append_delta(&hash, &never_stored_base_2, 10, b"also not a real delta payload").unwrap();
        let second_artifacts = second_builder.finish().unwrap();
        rename_pack_stem(&second_artifacts, &folder, "zzzz-broken-second");

        invalidate_cache();

        assert!(matches!(retrieve_from_packs(&hash).unwrap(), PackRetrieval::Failed(_)),
            "every locating pack failed to resolve — the outcome must be Failed");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_short_delta_record_is_rejected_not_panicked() {
        // Index-level validation (`index_format`) only checks that offsets/lengths are in bounds
        // and exactly cover the data file — it has no notion of what a record's *kind*
        // byte requires, so it happily accepts a record too short to hold a delta's base hash. A
        // native writer never emits one, but a locally loaded pack is not re-verified against
        // that shape (unlike a transport-imported one, checked hash-by-hash on the way in), so a
        // disk-corrupted or hand-crafted index can still produce this. `read_record_header` must
        // reject it cleanly rather than panic indexing past the record's declared length.
        let temp = std::env::temp_dir().join(format!("forklift-pack-short-delta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let pack_folder = temp.join(".forklift/objects/pack");
        std::fs::create_dir_all(&pack_folder).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Data: header, then one RECORD_DELTA record with only 2 bytes after the
        // kind byte — nowhere near the 32-byte base hash the kind promises, let alone a VLQ
        // length after it.
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        let offset = data.len() as u64;
        data.push(RECORD_DELTA);
        data.extend_from_slice(&[0xAAu8, 0xBB]);
        let length = 3u64;

        // Index: header, then one record whose (offset, length) exactly covers the short
        // record above — so index-level validation passes and the pack loads.
        let index = build_index_bytes(&[([0u8; HASH_LEN], offset, length)]);

        let data_path = pack_folder.join("short-delta.pack");
        let index_path = pack_folder.join("short-delta.idx");
        std::fs::write(&data_path, &data).unwrap();
        std::fs::write(&index_path, &index).unwrap();

        let pack = load_pack_pair(&data_path, &index_path).expect("index-level validation must accept this pack");
        pack.validate_full().expect("the record tiles the data file, so full validation must accept it");

        let result = read_record_header(&pack, offset, length);
        assert!(result.is_err(), "a delta record too short for a base hash must be rejected, got {:?}", result);
        let message = result.unwrap_err();
        assert!(message.contains("truncated"), "the error should name the truncation: {}", message);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn true_depth_walks_a_delta_chain_iteratively_and_memoizes_every_hop() {
        // `true_depth` exists because `compute_path_bases`'s own path-hop counter cannot see a
        // chain's *real* depth (see `MAX_RECONSTRUCT_DEPTH`'s doc comment) — this pins its core
        // correctness in isolation, against a hand-built chain of delta records (hash 0 full,
        // then every later hash a delta against the one before it). Deliberately longer than
        // `MAX_DELTA_CHAIN` (a cap `true_depth` itself does not enforce — that is `compact`'s
        // write-time pre-pass's job, using this function's answer) to prove it reports the *true*
        // depth regardless, not just up to some cap, and walks iteratively rather than
        // recursively: a real store this ledger exists to repair can already hold a chain deep
        // enough that native call-stack recursion would risk overflow resolving it.
        let temp = std::env::temp_dir().join(format!("forklift-true-depth-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let pack_folder = temp.join(".forklift/objects/pack");
        std::fs::create_dir_all(&pack_folder).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        const CHAIN_LEN: usize = 60;
        let hashes: Vec<[u8; HASH_LEN]> = (0..CHAIN_LEN)
            .map(|i| *blake3::hash(&(i as u64).to_le_bytes()).as_bytes())
            .collect();

        // `read_record_header` never touches a record's payload bytes (only the kind byte and,
        // for a delta, the base hash and the VLQ length right after it), so placeholder bytes
        // stand in for them — no real zstd/delta encoding needed to exercise this.
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());

        let mut records: Vec<([u8; HASH_LEN], u64, u64)> = Vec::new();
        for (i, hash) in hashes.iter().enumerate() {
            let offset = data.len() as u64;
            if i == 0 {
                data.push(RECORD_FULL);
                data.extend_from_slice(&[0u8; 4]);
            } else {
                data.push(RECORD_DELTA);
                data.extend_from_slice(&hashes[i - 1]);
                data.extend_from_slice(&byte_utils::number_to_vlq_bytes(4));
                data.extend_from_slice(&[0u8; 2]);
            }
            records.push((*hash, offset, data.len() as u64 - offset));
        }

        // Index: every record sorted by hash (the index format requires it).
        let mut sorted = records.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        let index = build_index_bytes(&sorted);

        let data_path = pack_folder.join("chain.pack");
        let index_path = pack_folder.join("chain.idx");
        std::fs::write(&data_path, &data).unwrap();
        std::fs::write(&index_path, &index).unwrap();

        let pack = load_pack_pair(&data_path, &index_path).expect("a well-formed hand-built pack must load");
        let packs = vec![pack];

        let mut known = HashMap::new();
        let tip_depth = true_depth(hashes[CHAIN_LEN - 1], &mut known, &packs)
            .expect("a well-formed chain must resolve");
        assert_eq!(tip_depth, (CHAIN_LEN - 1) as u32, "the tip's depth must equal its distance from the full base");

        // Every hop was memoized along the way at its *own* correct depth, not just the tip's.
        for (i, hash) in hashes.iter().enumerate() {
            assert_eq!(
                *known.get(hash).unwrap(), i as u32,
                "hop {} must be memoized at its own true depth, not the tip's", i
            );
        }

        // A hash already resolved (in `known`) needs no pack access at all — the memoization
        // that makes a repeated query in the same run O(1). An empty pack slice proves it: if
        // this were not served from `known`, the lookup would find nothing and misreport 0.
        let mid_depth = true_depth(hashes[CHAIN_LEN / 2], &mut known, &[])
            .expect("a memoized hash needs no pack lookup");
        assert_eq!(mid_depth, (CHAIN_LEN / 2) as u32);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_read_reloads_the_pack_registry_when_an_external_compact_moved_the_object() {
        // A long-running process (a live server) whose cached pack registry predates an
        // *external* compact would miss an object that compact moved into a new pack and whose
        // loose source it swept — both the cached packs and the loose fallback come up empty.
        let temp = std::env::temp_dir().join(format!("forklift-reload-miss-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"an object another process packs out from under us".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);

        // Pack it (loose -> pack, loose file deleted). In-process this also invalidated the cache.
        compact(false, false, false, &Progress::NONE).unwrap();

        // Simulate the stale peer: poison this process's registry back to "no packs" even though
        // the pack is on disk and the loose file is gone. A naive read now misses on both paths.
        registry().lock().expect("registry lock")
            .insert(file_utils::get_path_objects_root(), Arc::new(PackSet::empty()));

        // The read must reload the registry on the miss and still return the object.
        assert_eq!(
            file_utils::retrieve_object_by_hash(&hash).unwrap(), content,
            "a read must reload the pack registry and find an externally-packed object",
        );

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_refuses_to_run_while_the_shared_store_lock_is_held() {
        // compact serializes on the shared store lock, so a second compaction (another bay or
        // process) cannot race its deletions.
        let temp = std::env::temp_dir().join(format!("forklift-compact-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"content to compact".to_vec();
        store_loose(&blake3::hash(&content).to_hex().to_string(), &content);

        let held = lock_utils::StoreLock::acquire().expect("hold the store lock");
        assert!(compact(false, false, false, &Progress::NONE).is_err(), "compact must refuse while the shared store lock is held");
        drop(held);
        assert!(compact(false, false, false, &Progress::NONE).is_ok(), "compact runs once the store lock is free");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_repack_physically_removes_old_pack_files_it_supersedes() {
        // D/P3: `collect_targets` now keeps the old packs mmap'd for the whole `compact` call
        // (`source_packs`, for the verbatim-copy fast path) rather than the previous per-record
        // `File::open` that closed the instant each record was read. That mmap must still be
        // released *before* the old-pack deletion sweep — `compact` drops `source_packs`
        // explicitly right after the write loop, ahead of `sync_dir`/removal — or deleting a
        // still-mapped file could fail on a platform that does not allow it (Windows, without
        // `FILE_SHARE_DELETE`). This is unobservable as a failure on POSIX (unlinking an open or
        // mapped file always succeeds there), so this test instead pins the *outcome* the drop
        // exists to protect: the old pack files are actually gone from disk afterward, not
        // merely superseded in the index. No pallet/parcel scaffolding is reachable here (no
        // pallet refs exist), so every packed object is legitimately unreachable garbage and a
        // repack must drop it all — exercising the exact "mmap it, then delete it" sequence.
        let temp = std::env::temp_dir().join(format!("forklift-repack-removes-old-packs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        // Two separate incremental packs, so the repack has more than one old pack to supersede.
        let content_a = b"first pack content".to_vec();
        store_loose(&blake3::hash(&content_a).to_hex().to_string(), &content_a);
        compact(false, false, false, &Progress::NONE).unwrap();

        let content_b = b"second pack content".to_vec();
        store_loose(&blake3::hash(&content_b).to_hex().to_string(), &content_b);
        compact(false, false, false, &Progress::NONE).unwrap();

        let pack_paths_before: Vec<PathBuf> = std::fs::read_dir(pack_folder()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .collect();
        assert_eq!(pack_paths_before.len(), 2, "two incremental packs should exist before the repack");

        // No pallet refs exist in this bare store, so the live set is empty: a repack must drop
        // every packed object as garbage and remove both old packs entirely.
        let stats = compact(true, false, true, &Progress::NONE).unwrap();
        assert_eq!(stats.objects_packed, 0, "nothing is reachable, so nothing should be repacked");

        for old_pack in &pack_paths_before {
            assert!(!old_pack.exists(),
                "old pack \"{}\" must be physically removed once superseded", old_pack.to_string_lossy());
            let old_index = old_pack.with_extension(PACK_INDEX_EXTENSION);
            assert!(!old_index.exists(),
                "old index \"{}\" must be physically removed once superseded", old_index.to_string_lossy());
        }

        // And the content is genuinely gone — not just re-pointed at.
        assert!(!is_in_packs(&blake3::hash(&content_a).to_hex().to_string()).unwrap());
        assert!(!is_in_packs(&blake3::hash(&content_b).to_hex().to_string()).unwrap());

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_falls_back_to_a_full_blob_when_its_delta_base_is_corrupt() {
        // Pins I4 (the corrupt-base fallback): a corrupt loose object that is *another*
        // target's path-delta base must not abort the whole compact. This needs a real
        // two-version history — a made-up "loose object with a made-up base hash" would never
        // reach the fetch this test targets, since only `compute_path_bases`'s real tree/parcel
        // walk populates the base map at all. So build one file changed across two parcels
        // (v1 -> v2, `main` at v2) the way the store actually authors history, then corrupt
        // v1's blob on disk (a valid zstd frame, wrong bytes — a hash mismatch, not a decode
        // failure, so it exercises the same store-verified fetch the fix guards).
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-pack-corrupt-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // v1: "file.txt" holds content A, in a parentless root parcel.
        let mut blob_v1 = LooseObjectBuilder::build_blob(
            &Blob { content: b"version one content of file.txt".to_vec() }
        );
        blob_v1.store().unwrap();
        let blob_v1_hash = blob_v1.hash.clone();

        let mut tree_v1 = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree_v1.add_child(TreeItem::new("file.txt".to_string(), blob_v1_hash.clone(), DirEntryType::Normal));
        let mut tree_v1_obj = LooseObjectBuilder::build_tree(&tree_v1);
        tree_v1_obj.store().unwrap();

        let mut parcel_v1_obj = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree_v1_obj.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("v1".to_string()),
        });
        parcel_v1_obj.store().unwrap();

        // v2: "file.txt" holds content B, parented on v1 — its blob's path base is v1's blob.
        let mut blob_v2 = LooseObjectBuilder::build_blob(
            &Blob { content: b"version two content of file.txt, now different".to_vec() }
        );
        blob_v2.store().unwrap();
        let blob_v2_hash = blob_v2.hash.clone();
        let blob_v2_content = blob_v2.content.clone();

        let mut tree_v2 = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree_v2.add_child(TreeItem::new("file.txt".to_string(), blob_v2_hash.clone(), DirEntryType::Normal));
        let mut tree_v2_obj = LooseObjectBuilder::build_tree(&tree_v2);
        tree_v2_obj.store().unwrap();

        let mut parcel_v2_obj = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree_v2_obj.hash.clone(),
            parents: vec![parcel_v1_obj.hash.clone()],
            actions: Vec::new(),
            description: Some("v2".to_string()),
        });
        parcel_v2_obj.store().unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_v2_obj.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        // Corrupt v1's blob loose file in place: a well-formed zstd frame (so it decodes) whose
        // plaintext hashes to a different address (so it fails `verify_object_bytes`) — the
        // hash-mismatch flavor of `PrepSlot::CorruptLoose`, not the decode-failure one.
        let (folder, file_name) = file_utils::get_path_for_object(&blob_v1_hash).unwrap();
        let wrong_bytes = zstd::encode_all(b"not the bytes that belong at this address".as_slice(), 0).unwrap();
        file_utils::write_object_to_file(Path::new(&folder), &file_name, wrong_bytes).unwrap();

        let stats = compact(false, false, false, &Progress::NONE).unwrap();

        // v1's blob is corrupt and is skipped, not packed — same invariant clause (a)/(b) pin,
        // just reached as someone else's delta base this time.
        assert_eq!(stats.corrupt_skipped, 1, "the corrupt path-base blob must be skipped, not abort the run");
        let (folder, file_name) = file_utils::get_path_for_object(&blob_v1_hash).unwrap();
        assert!(Path::new(&folder).join(&file_name).exists(), "the corrupt base must be left in place");
        assert!(!is_in_packs(&blob_v1_hash).unwrap());

        // v2 still packs — full, since its path base could not be read verified — and reads
        // back byte-exact.
        assert!(is_in_packs(&blob_v2_hash).unwrap());
        assert_eq!(packs_verified(&blob_v2_hash), blob_v2_content);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn compact_reconstructs_a_delta_whose_path_base_is_a_skipped_over_ceiling_giant() {
        // `compute_path_bases` never checks object size, so a
        // grandfathered over-ceiling giant is just as eligible a path-delta *base* as any other
        // object at the same path — being skipped as a *target* (`PrepSlot::OverCeiling`, never
        // packed) does not make it ineligible as someone else's base. Verified this is already
        // safe: a base's bytes are fetched through `file_utils::retrieve_object_by_hash_shared`,
        // which falls back to the loose read `file_utils.rs`'s `read_object_classified` — the
        // *other* exempted, deliberately unbounded site (`zstd::stream::decode_all`, never
        // `decode_object_bounded`), precisely so a grandfathered giant stays readable. That read
        // is used both when `prepare_target` computes the delta at compact time and when
        // `reconstruct_record`'s own delta branch resolves the same base at read time — so
        // fetching a loose over-ceiling base for a path delta never hits the ceiling either way.
        //
        // This needs a real two-version history — a made-up base hash would never reach
        // `compute_path_bases`'s real tree/parcel walk — built the same way
        // `compact_falls_back_to_a_full_blob_when_its_delta_base_is_corrupt` does, but with a
        // genuine, correctly-addressed over-ceiling giant as v1 instead of a corrupt one, and a
        // v2 that shares a long prefix with it so the path delta actually wins (not a
        // vacuous pass where v2 happens to pack full anyway).
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir().join(format!("forklift-pack-over-ceiling-base-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // v1: "file.txt" holds a MAX_OBJECT_BYTES + 1 grandfathered giant, planted directly the
        // way a pre-ceiling bundle import would have left it (the ordinary write path refuses to
        // author one; `store_loose` bypasses that, exactly like the INV-4/INV-5 fixtures).
        let giant_content = deterministic_fill(object_utils::MAX_OBJECT_BYTES + 1);
        let giant_hash = blake3::hash(&giant_content).to_hex().to_string();
        store_loose(&giant_hash, &giant_content);

        let mut tree_v1 = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree_v1.add_child(TreeItem::new("file.txt".to_string(), giant_hash.clone(), DirEntryType::Normal));
        let mut tree_v1_obj = LooseObjectBuilder::build_tree(&tree_v1);
        tree_v1_obj.store().unwrap();

        let mut parcel_v1_obj = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree_v1_obj.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("v1".to_string()),
        });
        parcel_v1_obj.store().unwrap();

        // v2: "file.txt" holds a small object sharing a long prefix with the giant (so a path
        // delta against it is real and wins on size) plus a short unique tail.
        // The shared bytes come from the *end* of the giant, not the start: a dictionary
        // compressor treats the dictionary as if it immediately precedes the input, so content
        // near its end is the cheapest to reference (closest in window distance) — a share from
        // the far start of a 64 MiB dictionary may not be found as an economical match at all.
        let mut small_content = giant_content[giant_content.len() - 200_000..].to_vec();
        small_content.extend_from_slice(b"a short unique tail for version two");
        let mut blob_v2 = LooseObjectBuilder::build_blob(&Blob { content: small_content });
        blob_v2.store().unwrap();
        let blob_v2_hash = blob_v2.hash.clone();
        // The stored/hashed bytes (header + content), not the raw `Blob.content` passed in above
        // — mirrors `compact_falls_back_to_a_full_blob_when_its_delta_base_is_corrupt`'s own
        // `blob_v2_content` capture.
        let blob_v2_content = blob_v2.content.clone();

        let mut tree_v2 = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree_v2.add_child(TreeItem::new("file.txt".to_string(), blob_v2_hash.clone(), DirEntryType::Normal));
        let mut tree_v2_obj = LooseObjectBuilder::build_tree(&tree_v2);
        tree_v2_obj.store().unwrap();

        let mut parcel_v2_obj = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree_v2_obj.hash.clone(),
            parents: vec![parcel_v1_obj.hash.clone()],
            actions: Vec::new(),
            description: Some("v2".to_string()),
        });
        parcel_v2_obj.store().unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_v2_obj.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let stats = compact(false, false, false, &Progress::NONE).unwrap();

        // The giant is skipped, not corrupt, and left loose exactly as INV-5 already pins in
        // isolation.
        assert_eq!(stats.over_ceiling_skipped, 1);
        assert_eq!(stats.corrupt_skipped, 0, "a grandfathered giant used as someone else's base is still not corruption");
        let (folder, file_name) = file_utils::get_path_for_object(&giant_hash).unwrap();
        assert!(Path::new(&folder).join(&file_name).exists(), "the over-ceiling base must be left in place");
        assert!(!is_in_packs(&giant_hash).unwrap());

        // v2 specifically (not merely "some object in this run") won its path delta against the
        // loose giant — checked by inspecting v2's own packed record kind directly, not by
        // `stats.deltas` alone: the fixture's v1/v2 *trees* also delta against each other (a
        // harmless, unrelated pair), so a whole-run delta count can't tell which pair actually
        // produced it.
        assert!(is_in_packs(&blob_v2_hash).unwrap());
        assert!(is_stored_as_delta(&blob_v2_hash),
            "v2 must be packed as a delta against the loose giant base, not full");

        // And it reconstructs byte-exact: the delta-base fetch really did resolve the loose,
        // over-ceiling base through the unbounded read path, rather than failing or aborting.
        assert_eq!(packs_verified(&blob_v2_hash), blob_v2_content);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (I1, FORK-47, DESIGN.html §3.1.1) An orphaned pack pair — a parseable index whose data
    /// file is missing — must not fail the whole registry load: an entirely unrelated pack's
    /// hashes stay fully readable, and the orphan itself is quarantined (kept, not silently
    /// dropped) rather than wedging every read behind one bad pair.
    ///
    /// Fixture: pack A holds X, pack B holds Y, each its own pack/index pair; delete B's `.pack`
    /// and keep its `.idx` — the orphan shape.
    ///
    /// Revert: restore the bare `?` on `load_pack_pair` in `load_packs_from_disk` (drop the
    /// quarantine fallback, back to `packs.push(load_pack_pair(&data_path, &path)?);`) → the
    /// whole load fails on B's missing data file → X's read (which never touches B) also fails →
    /// red.
    #[test]
    fn an_orphaned_pack_index_no_longer_fails_the_registry_load() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-orphan-load-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let x_content = b"pack A's object, unrelated to B's orphan".to_vec();
        let x_hash = blake3::hash(&x_content).to_hex().to_string();
        let mut builder_a = TransportPackBuilder::new(&folder).unwrap();
        builder_a.append_full(&x_hash, &x_content).unwrap();
        builder_a.finish().unwrap();

        let y_content = b"pack B's object, about to be orphaned".to_vec();
        let y_hash = blake3::hash(&y_content).to_hex().to_string();
        let mut builder_b = TransportPackBuilder::new(&folder).unwrap();
        builder_b.append_full(&y_hash, &y_content).unwrap();
        let artifacts_b = builder_b.finish().unwrap();
        assert_eq!(artifacts_b.len(), 1, "test fixture expects exactly one pack pair for B");
        std::fs::remove_file(&artifacts_b[0].data_path).unwrap();

        // X (an entirely unrelated pack) must still read, despite B's orphan.
        match crate::util::file_utils::read_object_classified(&x_hash).unwrap() {
            StoreReadOutcome::Verified(bytes) => assert_eq!(bytes, x_content),
            StoreReadOutcome::Unverifiable(e) => panic!("expected X Verified, got Unverifiable: {}", e),
            StoreReadOutcome::Absent => panic!("expected X Verified, got Absent"),
        }

        // The uncached load itself is `Ok`, with B quarantined and A loaded normally.
        let loaded = load_packs_from_disk(&folder, IndexCheck::Lazy).expect("an orphaned pair must not fail the whole load");
        assert_eq!(loaded.packs.len(), 1, "only A's pair loaded cleanly");
        assert_eq!(loaded.quarantined.len(), 1, "B's orphaned index must be quarantined, not dropped");
        assert!(loaded.unenumerable.is_empty(), "B's index parses fine; it is not unenumerable");

        // `store` (the read-only census) must not be wedged either.
        assert!(store_status().is_ok(), "store_status must survive an orphaned pack pair");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Read a pack index file off disk as a [`CheckedIndex`] with no data bound — a test's view of
    /// where records and buckets sit.
    fn index_on_disk(index_path: &Path) -> CheckedIndex<Vec<u8>> {
        let bytes = std::fs::read(index_path).unwrap();
        let layout = index_format::Layout::parse(&bytes, index_path).expect("the test index must parse");
        CheckedIndex::new(bytes, layout, None)
    }

    /// Flip one bit of `hash`'s record offset in the index at `index_path`, which fails its
    /// bucket's crc and nothing else: the header and fanout table still parse.
    fn corrupt_record_of(index_path: &Path, hash: &[u8; HASH_LEN]) {
        let index = index_on_disk(index_path);
        let position = index.all_records().unwrap().iter().position(|record| record.hash == hash.as_slice())
            .expect("the hash to corrupt must be in the index");
        let at = index.record_byte_position(position) + HASH_LEN;
        let mut bytes = std::fs::read(index_path).unwrap();
        bytes[at] ^= 0x01;
        std::fs::write(index_path, &bytes).unwrap();
    }

    /// Contents for `count` distinct objects, with their raw and hex hashes.
    fn numbered_objects(tag: &str, count: usize) -> Vec<(Vec<u8>, [u8; HASH_LEN], String)> {
        (0..count).map(|i| {
            let content = format!("{} object {}", tag, i).into_bytes();
            let hash = *blake3::hash(&content).as_bytes();
            (content, hash, sign_utils::to_hex(&hash))
        }).collect()
    }

    /// The variant of a read outcome, for a failure message (the outcome carries object bytes).
    fn outcome_name(outcome: &crate::util::file_utils::StoreReadOutcome) -> String {
        use crate::util::file_utils::StoreReadOutcome;
        match outcome {
            StoreReadOutcome::Verified(_) => "Verified".to_string(),
            StoreReadOutcome::Unverifiable(message) => format!("Unverifiable({})", message),
            StoreReadOutcome::Absent => "Absent".to_string(),
        }
    }

    /// Content whose hash falls in `bucket` of `index`.
    fn content_in_bucket(index: &CheckedIndex<Vec<u8>>, bucket: usize, tag: &str) -> (Vec<u8>, String) {
        (0..).map(|i| format!("{} candidate {}", tag, i).into_bytes())
            .find(|content| index.bucket_of_hash(blake3::hash(content).as_bytes()) == bucket)
            .map(|content| { let hash = blake3::hash(&content).to_hex().to_string(); (content, hash) })
            .unwrap()
    }

    /// (FORK-118) A corrupt index bucket costs exactly its own prefix. Every hash that falls in
    /// it — the corrupted record's and its sound neighbours' alike — reads `Unverifiable` naming
    /// the index and the bucket, is not reported present, and cannot be listed by prefix; a hash
    /// in any other bucket of the same pack reads `Verified`. Storing an object into that prefix
    /// afterwards, the corrupted one itself or one the pack never held, reads it back `Verified`:
    /// the store's own write path is not blocked by the bad bucket.
    ///
    /// Mutations: make `CheckedIndex::locate` skip `bucket_is_valid` → the corrupted record's
    /// offset is off by one, so its read fails as a decode error rather than naming the bucket,
    /// and the neighbour reads `Verified` (the "bucket" assertions go red). Make `retrieve_from`
    /// treat `CorruptBucket` as `Missing` → the hashes classify `Absent` (red). Make `located_in`
    /// count `CorruptBucket` as present → `is_in_packs` reports the corrupted hash (red), and the
    /// store would skip writing it.
    #[test]
    fn a_corrupt_index_bucket_makes_only_its_prefix_unverifiable_and_a_new_store_there_reads_back() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-corrupt-bucket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        // 200 objects: eight buckets of about 25 records each.
        let objects = numbered_objects("bucket", 200);
        let mut builder = TransportPackBuilder::new(&folder).unwrap();
        for (content, _, hex) in &objects {
            builder.append_full(hex, content).unwrap();
        }
        let artifacts = builder.finish().unwrap();
        assert_eq!(artifacts.len(), 1, "test fixture expects exactly one pack pair");
        let index_path = artifacts[0].index_path.clone();
        let index_name = index_path.file_name().unwrap().to_str().unwrap().to_string();

        let index = index_on_disk(&index_path);
        let (bad_content, bad_hash, bad_hex) = objects[0].clone();
        let bad_bucket = index.bucket_of_hash(&bad_hash);
        let neighbour = objects.iter().skip(1).find(|(_, hash, _)| index.bucket_of_hash(hash) == bad_bucket)
            .expect("the fixture must put a second object in the corrupted bucket");
        let elsewhere = objects.iter().find(|(_, hash, _)| index.bucket_of_hash(hash) != bad_bucket)
            .expect("the fixture must put an object outside the corrupted bucket");

        corrupt_record_of(&index_path, &bad_hash);
        invalidate_cache();

        for hex in [&bad_hex, &neighbour.2] {
            match file_utils::read_object_classified(hex).unwrap() {
                StoreReadOutcome::Unverifiable(message) => assert!(
                    message.contains(&index_name) && message.contains(&format!("bucket {}", bad_bucket)),
                    "the message must name the index and the bucket: {}", message
                ),
                StoreReadOutcome::Verified(_) => panic!("a hash in a corrupt bucket must not read Verified"),
                StoreReadOutcome::Absent => panic!("a hash in a corrupt bucket must not read Absent"),
            }
            assert!(!is_in_packs(hex).unwrap(), "a hash in a corrupt bucket must not be reported present");
        }

        match file_utils::read_object_classified(&elsewhere.2).unwrap() {
            StoreReadOutcome::Verified(bytes) => assert_eq!(bytes, elsewhere.0),
            other => panic!("a hash outside the corrupt bucket must read Verified, got {}", outcome_name(&other)),
        }

        let error = find_hashes_with_prefix(&bad_hex[..8]).expect_err("a corrupt bucket's prefix cannot be listed");
        assert!(error.contains(&format!("bucket {}", bad_bucket)), "{}", error);
        assert!(find_hashes_with_prefix(&elsewhere.2[..8]).unwrap().contains(&elsewhere.2));

        // Store the corrupted object again, and one the pack never held, into the bad prefix.
        let (new_content, new_hex) = content_in_bucket(&index, bad_bucket, "never packed");
        assert!(object_utils::store_object_bytes(&bad_hex, &bad_content).unwrap(),
            "a hash only a corrupt bucket names is not present, so storing it must write it");
        assert!(object_utils::store_object_bytes(&new_hex, &new_content).unwrap());
        for (hex, content) in [(&bad_hex, &bad_content), (&new_hex, &new_content)] {
            match file_utils::read_object_classified(hex).unwrap() {
                StoreReadOutcome::Verified(bytes) => assert_eq!(&bytes, content),
                other => panic!("an object stored into a corrupt bucket's prefix must read back Verified, got {}", outcome_name(&other)),
            }
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-118) A quarantined index (its data file gone) with a corrupt bucket: a hash in that
    /// bucket reads `Unverifiable` even when the index never held it, because the bucket cannot
    /// rule it out; a never-stored hash in a sound bucket still reads `Absent`, and a stored one
    /// there `Unverifiable`.
    ///
    /// Mutation: make `QuarantinedPack::contains` treat `CorruptBucket` as `Missing` → the
    /// never-stored hash in the corrupt bucket classifies `Absent` (red).
    #[test]
    fn a_quarantined_index_with_a_corrupt_bucket_cannot_rule_out_any_hash_in_it() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-quarantine-bucket-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let objects = numbered_objects("quarantine", 200);
        let mut builder = TransportPackBuilder::new(&folder).unwrap();
        for (content, _, hex) in &objects {
            builder.append_full(hex, content).unwrap();
        }
        let artifacts = builder.finish().unwrap();
        let index_path = artifacts[0].index_path.clone();
        std::fs::remove_file(&artifacts[0].data_path).unwrap();

        let index = index_on_disk(&index_path);
        let bad_bucket = index.bucket_of_hash(&objects[0].1);
        let sound = objects.iter().find(|(_, hash, _)| index.bucket_of_hash(hash) != bad_bucket).unwrap();
        let sound_bucket = index.bucket_of_hash(&sound.1);
        corrupt_record_of(&index_path, &objects[0].1);
        invalidate_cache();

        let (_, unknown_in_bad) = content_in_bucket(&index, bad_bucket, "unknown bad");
        let (_, unknown_in_sound) = content_in_bucket(&index, sound_bucket, "unknown sound");

        for hex in [&objects[0].2, &unknown_in_bad, &sound.2] {
            assert!(matches!(file_utils::read_object_classified(hex).unwrap(), StoreReadOutcome::Unverifiable(_)),
                "{} must read Unverifiable: the quarantined index names it or cannot rule it out", hex);
        }
        assert!(matches!(file_utils::read_object_classified(&unknown_in_sound).unwrap(), StoreReadOutcome::Absent),
            "a never-stored hash in a sound bucket must still read Absent");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A pack index of another format version — a store written by another build — fails every
    /// read it might answer by name: a hash no readable copy holds reads `Unverifiable` naming the
    /// version, never `Absent`, while an object stored loose reads `Verified`. Short indexes name
    /// their version too: a 16-byte version-2 index has no fanout header to be measured against.
    ///
    /// Mutations: drop the foreign-version loop from `quarantine_reason` → the never-stored hash
    /// reads `Absent` (red). Leave foreign-version indexes among the unenumerable ones in the
    /// census → it lists the index as a file to move aside (red).
    #[test]
    fn a_pack_index_of_another_version_fails_reads_by_name_instead_of_reading_absent() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-foreign-version-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();
        std::fs::create_dir_all(&folder).unwrap();

        let mut old_index = PACK_INDEX_MAGIC.to_vec();
        old_index.extend_from_slice(&2u32.to_le_bytes());
        old_index.extend_from_slice(&0u32.to_le_bytes());
        std::fs::write(folder.join(format!("{}.{}", "ab".repeat(32), PACK_INDEX_EXTENSION)), &old_index).unwrap();
        invalidate_cache();

        let never_stored = blake3::hash(b"held only by the old pack, perhaps").to_hex().to_string();
        match file_utils::read_object_classified(&never_stored).unwrap() {
            StoreReadOutcome::Unverifiable(message) => assert!(
                message.contains("format version 2, but this build reads only version 3"), "{}", message
            ),
            other => panic!("a read an old-version index might answer must fail by name, got {}", outcome_name(&other)),
        }
        assert!(!is_in_packs(&never_stored).unwrap());

        let (content, hex) = (b"stored loose".to_vec(), blake3::hash(b"stored loose").to_hex().to_string());
        store_loose(&hex, &content);
        match file_utils::read_object_classified(&hex).unwrap() {
            StoreReadOutcome::Verified(bytes) => assert_eq!(bytes, content),
            other => panic!("a readable copy must still read Verified, got {}", outcome_name(&other)),
        }

        let census = store_status().unwrap();
        assert!(census.unenumerable_indexes.is_empty(), "an old-version index is not reported as a stray file to move aside");
        // A version field too large for any build to have written is rot, not another build.
        let mut rotted = old_index.clone();
        rotted[8..12].copy_from_slice(&0x0100_0003u32.to_le_bytes());
        assert_eq!(index_format::foreign_version(&rotted), None);
        assert_eq!(index_format::foreign_version(&old_index), Some(2));
        // Nor is a zeroed one, which no build ever wrote. And a well-formed index carrying either
        // version, its header crc intact, is refused as corrupt rather than read as version 3.
        for version in [0u32, 0x0100_0003] {
            let mut index = build_index_bytes(&[]);
            index[8..12].copy_from_slice(&version.to_le_bytes());
            assert_eq!(index_format::foreign_version(&index), None, "version {version}");
            let mut crc = crc32fast::Hasher::new();
            crc.update(&index[..20]);
            crc.update(&index[24..]);
            index[20..24].copy_from_slice(&crc.finalize().to_le_bytes());
            let refused = index_format::Layout::parse(&index, Path::new("v.idx")).err();
            assert!(refused.is_some_and(|error| error.contains("is corrupt")), "version {version}");
        }
        assert_eq!(census.foreign_version_indexes.len(), 1);
        assert!(census.foreign_version_indexes[0].error.contains("format version 2"), "{}", census.foreign_version_indexes[0].error);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194) A full repack run without salvage, as auto-maintenance runs it, leaves a pack
    /// whose index fails full validation exactly as found: reported as skipped, neither copied
    /// from, salvaged nor deleted, and the objects its sound buckets hold stay readable; the other
    /// packs are repacked as usual.
    ///
    /// Mutations: salvage regardless of the switch → a salvaged loose copy appears and the pack is
    /// dropped (red). Make a census load a pack that fails full validation instead of listing it
    /// as damaged → `store_status` fails on it (red).
    #[test]
    fn a_full_repack_without_salvage_leaves_a_damaged_pack_in_place() {
        use crate::util::file_utils::StoreReadOutcome;

        let (temp, _scope) = salvage_root("repack-no-salvage");
        let folder = pack_folder();

        let mut pairs = Vec::new();
        let mut objects = Vec::new();
        for tag in ["repack sound", "repack corrupt"] {
            let batch = numbered_objects(tag, 200);
            let mut builder = TransportPackBuilder::new(&folder).unwrap();
            for (content, _, hex) in &batch {
                builder.append_full(hex, content).unwrap();
            }
            pairs.push(builder.finish().unwrap().remove(0));
            objects.push(batch);
        }
        let (sound_pair, corrupt_pair) = (&pairs[0], &pairs[1]);

        let index = index_on_disk(&corrupt_pair.index_path);
        let bad_bucket = index.bucket_of_hash(&objects[1][0].1);
        let survivor = objects[1].iter().find(|(_, hash, _)| index.bucket_of_hash(hash) != bad_bucket).unwrap();
        corrupt_record_of(&corrupt_pair.index_path, &objects[1][0].1);
        invalidate_cache();

        // The census tells the damaged pack apart: neither quarantined nor counted.
        let census = store_status().unwrap();
        assert_eq!(census.damaged_packs.len(), 1, "the corrupt pack must be listed as damaged");
        assert!(census.damaged_packs[0].error.contains(&format!("bucket {}", bad_bucket)), "{}", census.damaged_packs[0].error);
        assert!(census.quarantined_packs.is_empty(), "a damaged pack's data file loads, so it is not quarantined");
        assert_eq!(census.packs.len(), 1, "only the sound pack is counted");

        let stats = compact(true, false, false, &Progress::NONE).unwrap();

        let (loose_folder, loose_name) = file_utils::get_path_for_object(&objects[1][0].2).unwrap();
        assert!(!Path::new(&loose_folder).join(&loose_name).exists(), "a run without salvage must write no salvaged copy");
        assert_eq!(stats.damaged_packs_skipped.len(), 1, "{:?}", stats.damaged_packs_skipped);
        let index_name = corrupt_pair.index_path.file_name().unwrap().to_str().unwrap();
        assert!(stats.damaged_packs_skipped[0].contains(index_name), "{:?}", stats.damaged_packs_skipped);
        assert!(stats.damaged_packs_dropped.is_empty() && stats.damaged_packs_kept.is_empty(), "{:?}", stats);
        assert!(corrupt_pair.index_path.exists() && corrupt_pair.data_path.exists(),
            "a damaged pack must not be deleted without salvage");
        // Nothing is reachable in this fixture, so the sound pack is dropped as garbage — the
        // repack ran for it as usual.
        assert!(!sound_pair.index_path.exists(), "the sound pack must be repacked as usual");
        match file_utils::read_object_classified(&survivor.2).unwrap() {
            StoreReadOutcome::Verified(bytes) => assert_eq!(bytes, survivor.0),
            other => panic!("an object in a sound bucket of the kept pack must stay readable, got {}", outcome_name(&other)),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// A salvage fixture's warehouse: a fresh storage root with a bay position, entered.
    fn salvage_root(name: &str) -> (PathBuf, StorageRootScope) {
        let temp = fresh_root(name);
        let scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();
        (temp, scope)
    }

    /// A blob built for a salvage fixture but not stored.
    fn salvage_blob(content: &[u8]) -> crate::model::object::loose_object::LooseObject {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;

        let blob = LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() });
        assert_eq!(object_utils::hash_object_bytes(&blob.content), blob.hash, "a built object is named by its bytes");
        blob
    }

    /// A blob built for a salvage fixture, `prefix` followed by a counter, whose hash satisfies
    /// `wanted` for the bucket it falls in, in the index the writer produces for `count` records.
    fn salvage_blob_where(prefix: &[u8], count: usize, wanted: impl Fn(usize) -> bool)
        -> crate::model::object::loose_object::LooseObject {
        (0..).map(|i| salvage_blob(&[prefix, format!("\nvariant {}\n", i).as_bytes()].concat()))
            .find(|blob| wanted(salvage_bucket(count, &blob.hash)))
            .unwrap()
    }

    /// The bucket `hash` falls in, in the index the writer produces for `count` records.
    fn salvage_bucket(count: usize, hash: &str) -> usize {
        index_format::bucket_in_index_of(count, &hash_to_bytes(hash).unwrap())
    }

    /// A root tree built for a salvage fixture but not stored, naming each `(name, hash, kind)`.
    fn salvage_tree(entries: &[(&str, &str, crate::enums::dir_entry_type::DirEntryType)])
        -> crate::model::object::loose_object::LooseObject {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::tree_item::TreeItem;

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash, kind) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *kind));
        }
        LooseObjectBuilder::build_tree(&tree)
    }

    /// Point `main` at a parentless parcel over `tree_hash`, stored loose, so a full repack's
    /// liveness walk keeps the tree and everything it names.
    fn commit_salvage_tree(tree_hash: &str) {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;

        let parcel = Parcel {
            tree_hash: tree_hash.to_string(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("salvage fixture".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::update_pallet_head("main", pallet_utils::get_pallet_head("main").unwrap().as_deref(), &parcel_object.hash, &pallet_utils::RefPreconditions::NONE)
            .unwrap();
    }

    /// Write the raw pack `<stem>` holding `records` in the given order (see [`write_raw_pack`]).
    /// Returns its index path and each record's offset in its data file.
    fn write_salvage_pack(stem: &str, records: &[(String, Vec<u8>)]) -> (PathBuf, Vec<usize>) {
        let borrowed: Vec<(&str, Vec<u8>)> = records.iter().map(|(hash, record)| (hash.as_str(), record.clone())).collect();
        write_raw_pack(&pack_folder(), stem, &borrowed);

        let mut offsets = Vec::new();
        let mut at = PACK_DATA_HEADER_LEN as usize;
        for (_, record) in records {
            offsets.push(at);
            at += record.len();
        }
        (pack_folder().join(format!("{}.{}", stem, PACK_INDEX_EXTENSION)), offsets)
    }

    /// The full records of `count` numbered objects that nothing names, to spread a pack over
    /// several index buckets.
    fn salvage_fillers(tag: &str, count: usize) -> Vec<(String, Vec<u8>)> {
        numbered_objects(tag, count).into_iter().map(|(content, _, hex)| (hex, full_record(&content))).collect()
    }

    /// Destroy the record at `offset` of pack `<stem>`'s data file: its kind byte becomes one no
    /// record has, so the record neither reads nor walks.
    fn destroy_salvage_record(stem: &str, offset: usize) {
        let path = pack_folder().join(format!("{}.{}", stem, PACK_DATA_EXTENSION));
        let mut data = std::fs::read(&path).unwrap();
        assert!(data[offset] == RECORD_FULL || data[offset] == RECORD_DELTA, "offset {} must start a record", offset);
        data[offset] = 0xEE;
        std::fs::write(&path, data).unwrap();
        invalidate_cache();
    }

    /// Fail validation of the index bucket `hash` falls in, at `index_path`.
    fn corrupt_salvage_bucket(index_path: &Path, hash: &str) {
        corrupt_record_of(index_path, &hash_to_bytes(hash).unwrap());
        invalidate_cache();
    }

    /// The variant of `hash`'s uncached classified read.
    fn salvage_read(hash: &str) -> String {
        outcome_name(&file_utils::read_object_classified(hash).unwrap())
    }

    /// Whether a loose file is stored under `hash`.
    fn is_loose(hash: &str) -> bool {
        let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
        Path::new(&folder).join(file_name).exists()
    }

    fn salvage_compact() -> Result<CompactStats, String> {
        compact(true, false, true, &Progress::NONE)
    }

    /// (FORK-194, T1) A pack whose index has a corrupt bucket but whose data is intact is
    /// salvaged by a full repack: every live object, those in the corrupt bucket included, reads
    /// verified afterwards, the damaged pack is gone, and the census reports no damaged pack.
    ///
    /// Revert: salvage nothing → the liveness walk refuses on the tree in the corrupt bucket (red).
    #[test]
    fn a_full_repack_salvages_a_pack_with_a_corrupt_bucket_and_drops_it() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-recovers");
        let blob = salvage_blob(b"salvage recovers this file\n");
        let tree = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &blob.hash);
        commit_salvage_tree(&tree.hash);
        assert!(salvage_read(&blob.hash).starts_with("Unverifiable"), "{}", salvage_read(&blob.hash));

        let stats = salvage_compact().unwrap();

        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        assert!(stats.damaged_packs_dropped[0].contains("a-crafted.idx"), "{:?}", stats.damaged_packs_dropped);
        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert!(!index.exists(), "a completely recovered damaged pack must be deleted");
        for hash in [&blob.hash, &tree.hash] {
            assert_eq!(salvage_read(hash), "Verified", "{}", hash);
        }
        assert!(store_status().unwrap().damaged_packs.is_empty(), "no damaged pack may remain");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T2) A live blob in the corrupt bucket whose record is destroyed has no copy
    /// salvage can recover: the repack keeps the pack, names the blob, and the blob still reads
    /// unverifiable, never absent.
    ///
    /// Revert: drop the gate → the pack is deleted and the blob reads absent (red).
    #[test]
    fn a_full_repack_keeps_a_damaged_pack_holding_the_only_copy_of_a_live_object() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-gate");
        let blob = salvage_blob(b"only the damaged pack held this file\n");
        let tree = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let (index, offsets) = write_salvage_pack("a-crafted", &[
            (tree.hash.clone(), full_record(&tree.content)),
            (blob.hash.clone(), full_record(&blob.content)),
        ]);
        destroy_salvage_record("a-crafted", offsets[1]);
        corrupt_salvage_bucket(&index, &tree.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_dropped.is_empty(), "{:?}", stats.damaged_packs_dropped);
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![blob.hash.clone()]);
        assert!(stats.damaged_packs_kept[0].index_path.contains("a-crafted.idx"), "{:?}", stats.damaged_packs_kept);
        assert!(index.exists(), "the damaged pack holds the only copy and must be kept");
        assert!(salvage_read(&blob.hash).starts_with("Unverifiable"), "{}", salvage_read(&blob.hash));
        assert_eq!(salvage_read(&tree.hash), "Verified", "the tree was salvaged");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T3) A delta chain inside the corrupt bucket whose records precede their bases
    /// is recovered over several salvage passes, and the pack is dropped.
    ///
    /// Revert: allow a single pass → the pack is kept naming the waiting deltas (red).
    #[test]
    fn salvage_recovers_a_delta_chain_whose_records_precede_their_bases() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-chain");
        let [c, b, t] = chain_contents("salvage chain").map(|(content, _)| salvage_blob(&content));
        let tree = salvage_tree(&[
            ("b.txt", &b.hash, DirEntryType::Normal),
            ("c.txt", &c.hash, DirEntryType::Normal),
            ("t.txt", &t.hash, DirEntryType::Normal),
        ]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (t.hash.clone(), delta_record(&b.hash, &b.content, &t.content)),
            (b.hash.clone(), delta_record(&c.hash, &c.content, &b.content)),
            (c.hash.clone(), full_record(&c.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &t.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        for hash in [&t.hash, &b.hash, &c.hash] {
            assert_eq!(salvage_read(hash), "Verified", "{}", hash);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T3b) A delta in one damaged pack whose base only another damaged pack holds is
    /// recovered even when its own pack is salvaged first, and both packs are dropped.
    ///
    /// Revert: retry pending deltas only within their own pack → the first pack is kept naming the
    /// delta (red).
    #[test]
    fn salvage_recovers_a_delta_whose_base_another_damaged_pack_holds() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-cross-pack");
        let [_, b, t] = chain_contents("salvage cross pack").map(|(content, _)| salvage_blob(&content));
        let tree = salvage_tree(&[("b.txt", &b.hash, DirEntryType::Normal), ("t.txt", &t.hash, DirEntryType::Normal)]);
        let (dependent, _) = write_salvage_pack("a-dependent", &[
            (t.hash.clone(), delta_record(&b.hash, &b.content, &t.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        let (base, _) = write_salvage_pack("b-base", &[(b.hash.clone(), full_record(&b.content))]);
        corrupt_salvage_bucket(&dependent, &t.hash);
        corrupt_salvage_bucket(&base, &b.hash);
        commit_salvage_tree(&tree.hash);

        let order: Vec<PathBuf> = loaded_packs().unwrap().packs.iter().map(|pack| pack.index_path()).collect();
        assert_eq!(order, vec![dependent.clone(), base.clone()], "the dependent pack must be salvaged first");

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 2, "{:?}", stats);
        assert!(!dependent.exists() && !base.exists(), "both damaged packs must be deleted");
        for hash in [&t.hash, &b.hash] {
            assert_eq!(salvage_read(hash), "Verified", "{}", hash);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T4) A live delta in a sound bucket whose base lay in the corrupt bucket survives
    /// the repack: salvage recovers the base before the delta is probed.
    ///
    /// Revert: salvage nothing → the delta fails its probe and the pack is kept naming it (red).
    #[test]
    fn a_sound_bucket_delta_survives_when_its_base_lay_in_the_corrupt_bucket() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-sound-delta");
        let count = 203;
        let base = salvage_blob(&[b"salvage sound delta base\n".as_slice(), &deterministic_fill(4000)].concat());
        let base_bucket = salvage_bucket(count, &base.hash);
        let delta = salvage_blob_where(&base.content[8..], count, |bucket| bucket != base_bucket);
        let tree = salvage_tree(&[("delta.txt", &delta.hash, DirEntryType::Normal)]);
        let mut records = salvage_fillers("salvage sound delta", 200);
        records.push((base.hash.clone(), full_record(&base.content)));
        records.push((delta.hash.clone(), delta_record(&base.hash, &base.content, &delta.content)));
        records.push((tree.hash.clone(), full_record(&tree.content)));
        assert_eq!(records.len(), count);
        let (index, _) = write_salvage_pack("a-crafted", &records);
        corrupt_salvage_bucket(&index, &base.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        assert_eq!(salvage_read(&delta.hash), "Verified");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T5) A pack whose buckets are all sound but whose records do not tile its data
    /// file (junk appended) fails full validation; with every live record readable, the repack
    /// drops it and every live object it served survives.
    #[test]
    fn a_full_repack_drops_a_pack_that_fails_only_the_tile_check() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-tile");
        let blob = salvage_blob(b"a tile-only damaged pack serves this file\n");
        let tree = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        let data_path = index.with_extension(PACK_DATA_EXTENSION);
        let mut data = std::fs::read(&data_path).unwrap();
        data.extend_from_slice(b"\xEEtrailing junk");
        std::fs::write(&data_path, data).unwrap();
        invalidate_cache();
        commit_salvage_tree(&tree.hash);
        assert_eq!(store_status().unwrap().damaged_packs.len(), 1, "the pack must fail full validation");

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        assert!(!index.exists());
        for hash in [&blob.hash, &tree.hash] {
            assert_eq!(salvage_read(hash), "Verified", "{}", hash);
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T6) A live blob that was never fetched hashes into the corrupt bucket of a
    /// completely salvaged pack. Every record the pack held is recovered and its raw table does
    /// not name the blob, so the pack never held it and is dropped.
    ///
    /// Revert: remove arm (b) of the drop gate → the pack is kept naming the blob (red).
    #[test]
    fn a_completely_salvaged_pack_is_dropped_despite_a_live_hash_it_never_held() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-sparse");
        let blob = salvage_blob(b"a stored file\n");
        let never_fetched = salvage_blob(b"a file this store never fetched\n");
        let tree = salvage_tree(&[
            ("file.txt", &blob.hash, DirEntryType::Normal),
            ("sparse.txt", &never_fetched.hash, DirEntryType::Normal),
        ]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &blob.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        assert_eq!(salvage_read(&never_fetched.hash), "Absent");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T7) As T6, but the damaged index names the never-fetched hash at another record's
    /// bytes, so the pack's raw table claims it: salvage recovers every record and the record
    /// count matches, yet the pack is kept naming the hash.
    ///
    /// Revert: remove the raw-table condition from arm (b) → the pack is dropped (red).
    #[test]
    fn a_completely_salvaged_pack_is_kept_while_its_raw_table_names_a_live_hash() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-raw-table");
        let blob = salvage_blob(b"a stored file, recorded twice\n");
        let claimed = salvage_blob(b"a file the index claims\n");
        let tree = salvage_tree(&[
            ("file.txt", &blob.hash, DirEntryType::Normal),
            ("claimed.txt", &claimed.hash, DirEntryType::Normal),
        ]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
            (claimed.hash.clone(), full_record(&blob.content)),
        ]);
        corrupt_salvage_bucket(&index, &blob.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_dropped.is_empty(), "{:?}", stats.damaged_packs_dropped);
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![claimed.hash.clone()]);
        assert!(index.exists());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T9) A legitimately absent subtree (a shallow or narrowed store) hashes into the
    /// corrupt bucket of a completely salvaged pack: the liveness walk accepts its absence, and the
    /// repack succeeds and drops the pack.
    ///
    /// Revert: remove the exoneration from the sweep predicate → the walk refuses on the subtree
    /// (red).
    #[test]
    fn the_liveness_walk_accepts_an_absent_tree_a_completely_salvaged_pack_never_held() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-absent-tree");
        let blob = salvage_blob(b"a file beside a narrowed-away directory\n");
        let absent = salvage_tree(&[("elsewhere.txt", &blob.hash, DirEntryType::Normal)]);
        let tree = salvage_tree(&[
            ("file.txt", &blob.hash, DirEntryType::Normal),
            ("narrowed", &absent.hash, DirEntryType::Tree),
        ]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &blob.hash);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();
        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T10) An over-ceiling object sits behind a damaged hash entry: the index names it
    /// under a hash one bit off, so only its decoded bytes could say what it is, and the bounded
    /// decode refuses to produce them. Salvage is incomplete, so the pack is kept naming the live
    /// object.
    ///
    /// Revert: let a refused record leave the salvage complete → the pack is dropped and the
    /// object reads absent (red).
    #[test]
    fn a_record_the_bounded_decode_refuses_leaves_the_salvage_incomplete() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-over-ceiling");
        let giant = vec![0u8; object_utils::MAX_OBJECT_BYTES + 1];
        let giant_hash = object_utils::hash_object_bytes(&giant);
        let mut damaged_entry = hash_to_bytes(&giant_hash).unwrap();
        damaged_entry[HASH_LEN - 1] ^= 0x01;
        let damaged_entry = sign_utils::to_hex(&damaged_entry);
        let tree = salvage_tree(&[("giant.bin", &giant_hash, DirEntryType::Normal)]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (damaged_entry.clone(), full_record(&giant)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &damaged_entry);
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_dropped.is_empty(), "{:?}", stats.damaged_packs_dropped);
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![giant_hash.clone()]);
        assert!(salvage_read(&giant_hash).starts_with("Unverifiable"), "{}", salvage_read(&giant_hash));

        std::fs::remove_dir_all(&temp).ok();
    }

    /// An eight-bucket damaged pack `<stem>` for the probe tests: 200 fillers, then `base` (whose
    /// record is destroyed and whose bucket is corrupted), then `delta` against it in another
    /// bucket. Every record in the pack is written after the fillers, so the walk resumes at the
    /// delta past the destroyed base. Returns the index path.
    fn write_probe_pack(stem: &str, base: &crate::model::object::loose_object::LooseObject,
        delta: &crate::model::object::loose_object::LooseObject) -> PathBuf {
        let mut records = salvage_fillers(stem, 200);
        records.push((base.hash.clone(), full_record(&base.content)));
        records.push((delta.hash.clone(), delta_record(&base.hash, &base.content, &delta.content)));
        let (index, offsets) = write_salvage_pack(stem, &records);
        destroy_salvage_record(stem, offsets[200]);
        corrupt_salvage_bucket(&index, &base.hash);
        index
    }

    /// A base and a delta for [`write_probe_pack`], the delta in a different bucket of its
    /// 202-record index.
    fn probe_pair(tag: &str) -> (crate::model::object::loose_object::LooseObject, crate::model::object::loose_object::LooseObject) {
        let base = salvage_blob(&[tag.as_bytes(), &deterministic_fill(4000)].concat());
        let base_bucket = salvage_bucket(202, &base.hash);
        let delta = salvage_blob_where(&base.content[8..], 202, |bucket| bucket != base_bucket);
        (base, delta)
    }

    /// (FORK-194, T11) A sound-bucket delta whose base is unrecoverable fails the up-front probe:
    /// the pack is kept naming it, and the run succeeds rather than failing on a target it cannot
    /// read.
    ///
    /// Revert: make a failed probe a target → `compact --all` fails (red).
    #[test]
    fn a_sound_bucket_record_that_fails_its_probe_keeps_the_pack_without_failing_the_run() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-probe");
        let (base, delta) = probe_pair("salvage probe base");
        let index = write_probe_pack("a-damaged", &base, &delta);
        let mut tree = salvage_tree(&[("delta.txt", &delta.hash, DirEntryType::Normal)]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![delta.hash.clone()]);
        assert!(index.exists());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T11, a corrupt loose copy) As T11, with a corrupt loose copy of the failing
    /// delta: the loose copy stays a skipped target and the run still succeeds.
    ///
    /// Revert: probe after the duplicate check → the loose target is demoted to a reconstruction
    /// that cannot read, and the run aborts (red).
    #[test]
    fn a_failed_probe_does_not_demote_a_corrupt_loose_copy() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-probe-loose");
        let (base, delta) = probe_pair("salvage probe loose base");
        let index = write_probe_pack("a-damaged", &base, &delta);
        store_loose(&delta.hash, b"not the delta's bytes");
        let mut tree = salvage_tree(&[("delta.txt", &delta.hash, DirEntryType::Normal)]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![delta.hash.clone()]);
        assert!(index.exists());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T11, a healthy copy) As T11, with a verbatim copy of the failing delta in a
    /// healthy pack that sorts first, and its base live: that copy stays verbatim and the run
    /// still succeeds.
    ///
    /// Revert: probe after the duplicate check → the healthy copy is demoted to a reconstruction
    /// that cannot read, and the run aborts (red).
    #[test]
    fn a_failed_probe_does_not_demote_a_healthy_packs_verbatim_copy() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-probe-healthy");
        let (base, delta) = probe_pair("salvage probe healthy base");
        let healthy_delta = delta_record(&base.hash, &base.content, &delta.content);
        let (healthy, _) = write_salvage_pack("a-healthy", &[(delta.hash.clone(), healthy_delta)]);
        let damaged = write_probe_pack("b-damaged", &base, &delta);
        let mut tree = salvage_tree(&[
            ("base.txt", &base.hash, DirEntryType::Normal),
            ("delta.txt", &delta.hash, DirEntryType::Normal),
        ]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        let order: Vec<PathBuf> = loaded_packs().unwrap().packs.iter().map(|pack| pack.index_path()).collect();
        assert_eq!(order, vec![healthy.clone(), damaged.clone()], "the healthy pack must be collected first");

        let stats = salvage_compact().unwrap();
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        let mut unaccounted = vec![base.hash.clone(), delta.hash.clone()];
        unaccounted.sort();
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, unaccounted);
        assert!(damaged.exists());
        assert!(is_stored_as_delta(&delta.hash), "the healthy copy must be carried verbatim");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T12) A healthy pack's live delta whose base fails the probe in a damaged pack's
    /// sound bucket is copied verbatim, the run succeeds, and the kept damaged pack still holds
    /// the base.
    ///
    /// Revert: make the failed base a reconstruction target → the run aborts (red).
    #[test]
    fn a_healthy_delta_on_a_base_that_fails_its_probe_is_copied_verbatim() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-healthy-dependent");
        let count = 201;
        let base = salvage_blob(&[b"salvage healthy dependent base\n".as_slice(), &deterministic_fill(4000)].concat());
        let base_bucket = salvage_bucket(count, &base.hash);
        let dependent = salvage_blob(&[base.content.as_slice(), b"\nthe dependent adds a line\n"].concat());
        let dependent_bucket = salvage_bucket(count, &dependent.hash);
        let mut records = salvage_fillers("salvage healthy dependent", 200);
        // The corrupt bucket holds neither the base nor the dependent, so the base fails only its
        // probe and the dependent is not named.
        let corrupted = records.iter()
            .find(|(hash, _)| ![base_bucket, dependent_bucket].contains(&salvage_bucket(count, hash)))
            .unwrap().0.clone();
        records.push((base.hash.clone(), full_record(&base.content)));
        let (damaged, offsets) = write_salvage_pack("a-damaged", &records);
        destroy_salvage_record("a-damaged", offsets[200]);
        corrupt_salvage_bucket(&damaged, &corrupted);
        write_salvage_pack("b-healthy", &[(dependent.hash.clone(), delta_record(&base.hash, &base.content, &dependent.content))]);
        let mut tree = salvage_tree(&[
            ("base.txt", &base.hash, DirEntryType::Normal),
            ("dependent.txt", &dependent.hash, DirEntryType::Normal),
        ]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();
        assert_eq!(stats.damaged_packs_kept.len(), 1, "{:?}", stats);
        assert_eq!(stats.damaged_packs_kept[0].unaccounted, vec![base.hash.clone()]);
        assert!(damaged.exists(), "the damaged pack still holds the base");
        assert!(is_stored_as_delta(&dependent.hash), "the dependent must be carried verbatim");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T13) A live object in the corrupt bucket that also has a corrupt loose copy is
    /// recovered over that copy: it reads verified afterwards and the pack is dropped.
    ///
    /// Revert: store through the presence-gated `store_object_bytes` → the write is skipped, the
    /// object stays unverifiable and the pack is kept (red).
    #[test]
    fn salvage_writes_over_an_unreadable_loose_copy() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-corrupt-loose");
        let blob = salvage_blob(b"a file with a corrupt loose copy\n");
        let tree = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let (index, _) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        corrupt_salvage_bucket(&index, &blob.hash);
        store_loose(&blob.hash, b"not the blob's bytes");
        commit_salvage_tree(&tree.hash);

        let stats = salvage_compact().unwrap();

        assert!(stats.damaged_packs_kept.is_empty(), "{:?}", stats.damaged_packs_kept);
        assert_eq!(stats.damaged_packs_dropped.len(), 1, "{:?}", stats);
        assert_eq!(salvage_read(&blob.hash), "Verified");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T14) A live tree in the corrupt bucket whose record is destroyed: the liveness
    /// walk refuses, its error names the pack as not completely salvaged and says nothing was
    /// deleted, and the pack and the salvaged copies are all still on disk.
    ///
    /// Revert: register no incompletely salvaged pack → the error does not name the salvage (red).
    #[test]
    fn a_walk_refusal_on_an_incompletely_salvaged_pack_says_so() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-refusal");
        let blob = salvage_blob(b"a file beside a destroyed tree\n");
        let tree = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let (index, offsets) = write_salvage_pack("a-crafted", &[
            (blob.hash.clone(), full_record(&blob.content)),
            (tree.hash.clone(), full_record(&tree.content)),
        ]);
        destroy_salvage_record("a-crafted", offsets[1]);
        corrupt_salvage_bucket(&index, &blob.hash);
        commit_salvage_tree(&tree.hash);

        let error = salvage_compact().unwrap_err();

        assert!(error.contains("could not completely salvage"), "{}", error);
        assert!(error.contains("Nothing was deleted"), "{}", error);
        assert!(error.contains("a-crafted.idx"), "{}", error);
        assert!(index.exists() && index.with_extension(PACK_DATA_EXTENSION).exists(), "nothing may be deleted");
        assert!(is_loose(&blob.hash), "the salvaged copy must stay on disk");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T14b) The walk refuses on an unparseable index while an incompletely salvaged pack
    /// also exists: the error carries the unparseable index's own reason and no salvage context.
    ///
    /// Revert: add the salvage context to every walk error while an incompletely salvaged pack
    /// exists → it appears here (red).
    #[test]
    fn an_unrelated_walk_refusal_carries_no_salvage_context() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-unrelated-refusal");
        let mut records = salvage_fillers("salvage unrelated refusal", 200);
        let corrupted = records[0].0.clone();
        let corrupt_bucket = salvage_bucket(200, &corrupted);
        let (damaged, offsets) = write_salvage_pack("a-damaged", &records);
        destroy_salvage_record("a-damaged", offsets[0]);
        corrupt_salvage_bucket(&damaged, &corrupted);

        let blob = salvage_blob(b"a file beside an unparseable index\n");
        let absent = salvage_blob_where(b"an absent subtree's stand-in", 200, |bucket| bucket != corrupt_bucket);
        let mut tree = salvage_tree(&[
            ("file.txt", &blob.hash, DirEntryType::Normal),
            ("narrowed", &absent.hash, DirEntryType::Tree),
        ]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        // Written last: while an index does not parse, nothing can be stored.
        records = salvage_fillers("salvage unrelated unparseable", 1);
        let (unparseable, _) = write_salvage_pack("b-unparseable", &records);
        let mut index = std::fs::read(&unparseable).unwrap();
        index[..8].copy_from_slice(b"NOTAPACK");
        std::fs::write(&unparseable, index).unwrap();
        invalidate_cache();

        let error = salvage_compact().unwrap_err();

        assert!(error.contains("could not be parsed"), "{}", error);
        assert!(!error.contains("could not completely salvage"), "{}", error);

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T14c) A live tree is a sound-bucket delta of a damaged pack whose base sits,
    /// destroyed, in the corrupt bucket: the walk fails while loading the tree, and the load
    /// error carries the salvage context.
    ///
    /// Revert: add the context only to the sweep predicate's answer → the load error lacks it (red).
    #[test]
    fn a_load_through_an_incompletely_salvaged_corrupt_bucket_says_so() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-load");
        let blob = salvage_blob(b"a file named by both trees\n");
        let base = salvage_tree(&[("file.txt", &blob.hash, DirEntryType::Normal)]);
        let base_bucket = salvage_bucket(202, &base.hash);
        let tree = (0..).map(|i| salvage_tree(&[
                ("file.txt", &blob.hash, DirEntryType::Normal),
                (&format!("copy-{}.txt", i), &blob.hash, DirEntryType::Normal),
            ]))
            .find(|tree| salvage_bucket(202, &tree.hash) != base_bucket)
            .unwrap();
        let index = write_probe_pack("a-damaged", &base, &tree);
        commit_salvage_tree(&tree.hash);

        let error = salvage_compact().unwrap_err();

        assert!(error.contains("could not completely salvage"), "{}", error);
        assert!(index.exists());

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (FORK-194, T14d) A healthy pack's live delta has a dead base whose only copy sits, destroyed,
    /// in a damaged pack's corrupt bucket: the repack fails while packing reads the delta, the
    /// error carries the salvage context, and the damaged pack and the salvaged copies are still
    /// on disk.
    ///
    /// Revert: end the salvage registration with target collection → the packing error lacks the
    /// context (red).
    #[test]
    fn a_packing_read_through_an_incompletely_salvaged_corrupt_bucket_says_so() {
        use crate::enums::dir_entry_type::DirEntryType;

        let (temp, _scope) = salvage_root("salvage-packing");
        let [_, base, dependent] = chain_contents("salvage packing").map(|(content, _)| salvage_blob(&content));
        let salvaged = salvage_blob(b"a record salvage recovers\n");
        let (damaged, offsets) = write_salvage_pack("a-damaged", &[
            (salvaged.hash.clone(), full_record(&salvaged.content)),
            (base.hash.clone(), full_record(&base.content)),
        ]);
        destroy_salvage_record("a-damaged", offsets[1]);
        corrupt_salvage_bucket(&damaged, &base.hash);
        write_salvage_pack("b-healthy", &[(dependent.hash.clone(), delta_record(&base.hash, &base.content, &dependent.content))]);
        let mut tree = salvage_tree(&[("dependent.txt", &dependent.hash, DirEntryType::Normal)]);
        tree.store().unwrap();
        commit_salvage_tree(&tree.hash);

        let error = salvage_compact().unwrap_err();

        assert!(error.contains(&dependent.hash), "{}", error);
        assert!(error.contains("could not completely salvage"), "{}", error);
        assert!(damaged.exists(), "nothing may be deleted");
        assert!(is_loose(&salvaged.hash), "the salvaged copy must stay on disk");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (I2, FORK-47, DESIGN.html §3.1.1) A hash named only by a quarantined (data-missing) index
    /// classifies `Unverifiable`, naming the index file — not a bare `Absent` indistinguishable
    /// from a hash that never existed anywhere.
    ///
    /// Fixture: the same orphan shape as I1 — pack B holds Y, its `.pack` deleted, its `.idx`
    /// survives — so Y is named only by B's now-quarantined index.
    ///
    /// Revert: drop the quarantine consult at the `Absent` gate (`read_object_classified`'s
    /// `PackRetrieval::NotLocated` arm reverts to unconditional `Ok(StoreReadOutcome::Absent)`) →
    /// Y classifies `Absent`, whose ordinary-read message is the generic NotFound wording — the
    /// name assertions go red (an `Absent`-derived message never names an index file).
    ///
    /// Also pins the negative half: a hash the quarantined index does
    /// *not* name must still classify `Absent`, not `Unverifiable` — `QuarantinedPack::contains`'s
    /// binary search must never over-match. Mutation: hardwire `QuarantinedPack::contains` to
    /// return `true` unconditionally → this half goes red (the never-stored hash below wrongly
    /// classifies `Unverifiable` too).
    #[test]
    fn a_hash_named_only_by_a_quarantined_index_is_unverifiable_and_names_the_index() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-quarantine-read-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let y_content = b"named only by a quarantined index".to_vec();
        let y_hash = blake3::hash(&y_content).to_hex().to_string();
        let mut builder = TransportPackBuilder::new(&folder).unwrap();
        builder.append_full(&y_hash, &y_content).unwrap();
        let artifacts = builder.finish().unwrap();
        assert_eq!(artifacts.len(), 1, "test fixture expects exactly one pack pair");
        let index_name = artifacts[0].index_path.file_name().unwrap().to_str().unwrap().to_string();
        std::fs::remove_file(&artifacts[0].data_path).unwrap();

        match crate::util::file_utils::read_object_classified(&y_hash).unwrap() {
            StoreReadOutcome::Unverifiable(message) => assert!(
                message.contains(&index_name),
                "expected the Unverifiable message to name \"{}\", got: {}", index_name, message
            ),
            StoreReadOutcome::Verified(_) => panic!("a quarantine-named hash must never classify Verified"),
            StoreReadOutcome::Absent => panic!(
                "a quarantine-named hash must classify Unverifiable, not Absent (I2)"
            ),
        }

        let error = file_utils::retrieve_object_by_hash_uncached(&y_hash)
            .expect_err("a quarantine-named hash must not read successfully");
        assert!(
            error.contains(&index_name),
            "expected the ordinary read's error to name \"{}\", got: {}", index_name, error
        );

        // Negative membership: a hash the quarantined index does NOT
        // name must still classify `Absent` — a `QuarantinedPack::contains` over-match would
        // wrongly turn every never-stored hash `Unverifiable` too.
        let never_stored_hash = "9".repeat(64);
        match crate::util::file_utils::read_object_classified(&never_stored_hash).unwrap() {
            StoreReadOutcome::Absent => {}
            StoreReadOutcome::Unverifiable(message) => panic!(
                "a hash the quarantined index does not name must classify Absent, not \
                Unverifiable (QuarantinedPack::contains over-matched): {}", message
            ),
            StoreReadOutcome::Verified(_) => panic!("a never-stored hash must never classify Verified"),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// (I4, the read/presence half — FORK-47, DESIGN.html §3.1.1) An unenumerable (unparseable)
    /// index is inert: it changes no read or presence verdict anywhere — a sibling pack keeps
    /// serving, and a hash never stored anywhere still classifies `Absent`, never `Err`. (The
    /// `store_status`/heal-warning surfacing half of I4 is a later leg; not asserted here.)
    ///
    /// Fixture: pack A holds X; pack B's `.idx` magic is corrupted in place (both of B's files
    /// stay present, but its index no longer parses).
    ///
    /// Revert: make the `Absent` gate poison on any `unenumerable` entry (e.g. return `Err` when
    /// `loaded_packs()?.unenumerable` is non-empty, ahead of the `quarantine_reason` consult) →
    /// the never-stored hash's classified read becomes `Err` instead of `Absent` → red.
    #[test]
    fn an_unenumerable_index_is_inert() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = std::env::temp_dir().join(format!("forklift-unenumerable-inert-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);
        let folder = pack_folder();

        let x_content = b"pack A's object, unrelated to B's corrupt index".to_vec();
        let x_hash = blake3::hash(&x_content).to_hex().to_string();
        let mut builder_a = TransportPackBuilder::new(&folder).unwrap();
        builder_a.append_full(&x_hash, &x_content).unwrap();
        builder_a.finish().unwrap();

        let y_content = b"pack B's object, about to become unenumerable".to_vec();
        let y_hash = blake3::hash(&y_content).to_hex().to_string();
        let mut builder_b = TransportPackBuilder::new(&folder).unwrap();
        builder_b.append_full(&y_hash, &y_content).unwrap();
        let artifacts_b = builder_b.finish().unwrap();
        assert_eq!(artifacts_b.len(), 1, "test fixture expects exactly one pack pair for B");

        // Corrupt B's index magic in place — both files present, but the index no longer parses.
        let mut index_bytes = std::fs::read(&artifacts_b[0].index_path).unwrap();
        index_bytes[0..8].copy_from_slice(b"NOTAPACK");
        std::fs::write(&artifacts_b[0].index_path, &index_bytes).unwrap();

        // A sibling pack keeps serving.
        match crate::util::file_utils::read_object_classified(&x_hash).unwrap() {
            StoreReadOutcome::Verified(bytes) => assert_eq!(bytes, x_content),
            StoreReadOutcome::Unverifiable(e) => panic!("expected X Verified, got Unverifiable: {}", e),
            StoreReadOutcome::Absent => panic!("expected X Verified, got Absent"),
        }

        // A hash never stored anywhere still classifies `Absent`, not `Err` — the unenumerable
        // index must never poison the gate.
        let never_stored = "e".repeat(64);
        match crate::util::file_utils::read_object_classified(&never_stored).unwrap() {
            StoreReadOutcome::Absent => {}
            StoreReadOutcome::Verified(_) => panic!("a never-stored hash must never classify Verified"),
            StoreReadOutcome::Unverifiable(e) => panic!(
                "a never-stored hash must classify Absent, not Unverifiable (an unenumerable index must not poison it): {}", e
            ),
        }

        // The uncached load places B in `unenumerable`, not `quarantined`, and A still loads.
        let loaded = load_packs_from_disk(&folder, IndexCheck::Lazy).expect("an unparseable index must not fail the whole load");
        assert_eq!(loaded.packs.len(), 1, "only A's pair loaded cleanly");
        assert!(loaded.quarantined.is_empty(), "B's index does not parse; it cannot be quarantined");
        assert_eq!(loaded.unenumerable.len(), 1, "B's unparseable index must be recorded as unenumerable");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// The pack set this process has cached for the active warehouse (panics if none is cached).
    fn cached_pack_set() -> Arc<PackSet> {
        Arc::clone(registry().lock().expect("registry lock").get(&file_utils::get_path_objects_root())
            .expect("a pack set is cached for this warehouse"))
    }

    /// Set a file's mtime. Through a write handle: Windows needs one to set file times.
    fn set_mtime(path: &Path, modified: SystemTime) {
        std::fs::OpenOptions::new().write(true).open(path).unwrap().set_modified(modified).unwrap();
    }

    /// A fresh, empty warehouse root for one test.
    fn fresh_root(name: &str) -> PathBuf {
        let temp = std::env::temp_dir().join(format!("forklift-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        temp
    }

    /// Publish one pack holding `content` into the active warehouse's pack folder, as another
    /// process would — no `invalidate_cache()`. Returns the object's hash and the pair's paths.
    fn publish_external_pack(content: &[u8]) -> (String, TransportPackArtifact) {
        let hash = blake3::hash(content).to_hex().to_string();
        let mut builder = TransportPackBuilder::new(&pack_folder()).unwrap();
        builder.append_full(&hash, content).unwrap();
        let mut artifacts = builder.finish().unwrap();
        assert_eq!(artifacts.len(), 1, "test fixture expects exactly one pack pair");
        (hash, artifacts.remove(0))
    }

    /// FORK-106: prefix resolution sees a pack published after this process cached its set — the
    /// same staleness the presence check had, on the `find_hashes_with_prefix` path (which never
    /// misses in the lookup sense, so it revalidates before every scan).
    ///
    /// Revert: drop the `reloaded_if_stale` call from `find_hashes_with_prefix` → the second
    /// lookup scans the stale (empty) set and returns nothing → red.
    #[test]
    fn prefix_resolution_sees_a_pack_published_after_caching() {
        let temp = fresh_root("prefix-after-caching");
        let _scope = StorageRootScope::enter(&temp);

        let content = b"an object whose short hash is resolved after an external compact".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        let prefix = &hash[..12];

        assert!(find_hashes_with_prefix(prefix).unwrap().is_empty(), "sanity: nothing matches yet, and the set is cached");
        publish_external_pack(&content);

        assert_eq!(find_hashes_with_prefix(prefix).unwrap(), vec![hash.clone()],
            "a pack published after caching must be seen by prefix resolution");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the racy half of the folder check: a publish that leaves the folder's mtime equal
    /// to the stamped one (a coarse-mtime filesystem, two changes in one tick) is still caught,
    /// because a stamp taken within the granularity margin compares the folder's names.
    ///
    /// Fixture: cache the set, publish a pack, then set the folder's mtime back to exactly the
    /// stamped value — so only the name compare can notice.
    ///
    /// Revert: make `StampTimes::is_racy` return `false` → an equal mtime is trusted, the miss
    /// reports absent → red.
    #[cfg(unix)]
    #[test]
    fn a_publish_hidden_under_an_equal_folder_mtime_is_caught_by_the_name_compare() {
        let temp = fresh_root("racy-name-compare");
        let _scope = StorageRootScope::enter(&temp);
        std::fs::create_dir_all(pack_folder()).unwrap();

        let content = b"published within the same mtime tick as the stamp".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();

        assert!(!is_in_packs(&hash).unwrap(), "sanity: absent, and the set is cached");
        let stamped = cached_pack_set().stamp.times.lock().unwrap().modified
            .expect("the pack folder existed when the set was stamped")
            .expect("this platform reports directory mtimes");
        assert!(cached_pack_set().stamp.times.lock().unwrap().is_racy(), "sanity: a just-taken stamp is racy");

        publish_external_pack(&content);
        std::fs::File::open(pack_folder()).unwrap().set_modified(stamped).unwrap();
        assert_eq!(std::fs::metadata(pack_folder()).unwrap().modified().unwrap(), stamped,
            "sanity: the folder's mtime is back at the stamped value");

        assert!(is_in_packs(&hash).unwrap(), "the name compare must catch a publish the mtime hides");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the `open`-failure kind: a pack whose data file could not be opened (here,
    /// permissions) is retried on the next miss once it opens, even though neither file's stamp
    /// changed — a `chmod` moves neither length, mtime nor inode.
    ///
    /// Revert: classify every `open` failure as `FailureKind::Fixed` (or make
    /// `broken_pair_changed` return `false` for `Open`) → the pair stays quarantined until the
    /// process restarts, the second presence check says absent → red.
    #[cfg(unix)]
    #[test]
    fn a_pack_whose_data_could_not_be_opened_is_retried_once_it_opens() {
        use std::os::unix::fs::PermissionsExt;
        use crate::util::file_utils::StoreReadOutcome;

        let temp = fresh_root("open-failure-retry");
        let _scope = StorageRootScope::enter(&temp);

        let (hash, artifact) = publish_external_pack(b"a pack whose data file is briefly unreadable");
        let readable = std::fs::metadata(&artifact.data_path).unwrap().permissions();
        std::fs::set_permissions(&artifact.data_path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::File::open(&artifact.data_path).is_ok() {
            // Running as root: permissions cannot make the open fail, so there is nothing to test.
            std::fs::set_permissions(&artifact.data_path, readable).unwrap();
            std::fs::remove_dir_all(&temp).ok();
            return;
        }

        invalidate_cache();
        match file_utils::read_object_classified(&hash).unwrap() {
            StoreReadOutcome::Unverifiable(_) => {}
            StoreReadOutcome::Verified(_) => panic!("an unopenable pack must not read Verified"),
            StoreReadOutcome::Absent => panic!("a quarantine-named hash must read Unverifiable, not Absent"),
        }
        let cached = cached_pack_set();
        assert_eq!(cached.quarantined.len(), 1, "sanity: the pair is quarantined");
        assert_eq!(cached.quarantined[0].failure, FailureKind::Open, "sanity: as an open failure");

        std::fs::set_permissions(&artifact.data_path, readable).unwrap();
        assert!(is_in_packs(&hash).unwrap(), "once the data file opens, the next miss must find the object");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the retry rule per failure kind, on unchanged files: a post-`open` I/O failure is
    /// retried only once `IO_FAILURE_RETRY_INTERVAL` has passed since it was attempted; a fixed
    /// failure never; an `open` failure as soon as both files open.
    ///
    /// Revert: retry `Io` on every miss (drop the interval) → the fresh-attempt assertion goes red;
    /// never retry it → the aged-attempt assertion goes red.
    #[test]
    fn each_failure_kind_is_retried_on_its_own_schedule() {
        let temp = fresh_root("failure-kind-schedule");
        let _scope = StorageRootScope::enter(&temp);

        let (_, artifact) = publish_external_pack(b"a healthy pair standing in for a broken one");
        let index = &artifact.index_path;
        let stamp = PairStamp { settles_at: None, ..pair_stamp(index) };
        let now = Instant::now();
        let aged = now.checked_sub(IO_FAILURE_RETRY_INTERVAL + Duration::from_secs(1))
            .expect("the monotonic clock is past the retry interval");

        assert!(!broken_pair_changed(index, &stamp, FailureKind::Io, now, index),
            "an I/O failure just attempted must not be retried yet");
        assert!(broken_pair_changed(index, &stamp, FailureKind::Io, aged, index),
            "an I/O failure attempted longer ago than the interval must be retried");
        assert!(!broken_pair_changed(index, &stamp, FailureKind::Fixed, aged, index),
            "a fixed failure on unchanged files is never retried");
        assert!(broken_pair_changed(index, &stamp, FailureKind::Open, now, index),
            "an open failure is retried as soon as its probe opens");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106: a change to a broken pair's own files is seen even when the folder's names and
    /// mtime do not move. Fixture: pack B's data is gone and its index corrupt (unenumerable, so a
    /// read of Y is `Absent`); the index is then repaired *in place* — same name, same inode, no
    /// folder entry created — while the data stays missing. Y is now named by a quarantined index,
    /// so its read must turn `Unverifiable`, not stay `Absent`.
    ///
    /// Revert: drop the broken-entries loop from `pack_set_is_current` → the folder check alone
    /// finds the set current, Y stays `Absent` → red.
    #[test]
    fn an_index_repaired_in_place_turns_an_absent_read_unverifiable() {
        use crate::util::file_utils::StoreReadOutcome;

        let temp = fresh_root("repaired-in-place");
        let _scope = StorageRootScope::enter(&temp);

        let (y_hash, artifact) = publish_external_pack(b"named by an index that is corrupt, then repaired");
        std::fs::remove_file(&artifact.data_path).unwrap();
        let good_index = std::fs::read(&artifact.index_path).unwrap();
        let mut corrupt = good_index.clone();
        corrupt[0..8].copy_from_slice(b"NOTAPACK");
        std::fs::write(&artifact.index_path, &corrupt).unwrap();

        invalidate_cache();
        assert!(matches!(file_utils::read_object_classified(&y_hash).unwrap(), StoreReadOutcome::Absent),
            "sanity: an unenumerable index is inert, so Y reads Absent");
        assert_eq!(cached_pack_set().unenumerable.len(), 1, "sanity: B is unenumerable");

        // Rewrite in place, and move the mtime past the stamp explicitly, so the change is
        // visible to the pair stamp on any mtime granularity (the same-tick case is
        // `a_pair_stamped_inside_an_mtime_tick_is_rechecked_once_the_tick_passes`).
        let stamped_mtime = std::fs::metadata(&artifact.index_path).unwrap().modified().unwrap();
        std::fs::write(&artifact.index_path, &good_index).unwrap();
        set_mtime(&artifact.index_path, stamped_mtime + Duration::from_secs(5));

        match file_utils::read_object_classified(&y_hash).unwrap() {
            StoreReadOutcome::Unverifiable(_) => {}
            StoreReadOutcome::Absent => panic!("a repaired index naming Y must turn its read Unverifiable, not leave it Absent"),
            StoreReadOutcome::Verified(_) => panic!("Y's data is missing; it must not read Verified"),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the cost half: a fixed-state broken pair (an orphaned index) whose files do not
    /// change does not make every miss reload the pack set — the cached set survives repeated
    /// misses.
    ///
    /// Revert: make `broken_pair_changed` return `true` for `FailureKind::Fixed` → every miss
    /// reloads and replaces the cached set → red.
    #[test]
    fn an_unchanged_orphaned_index_does_not_reload_on_every_miss() {
        let temp = fresh_root("orphan-no-reload");
        let _scope = StorageRootScope::enter(&temp);

        let (_, artifact) = publish_external_pack(b"an orphaned index that stays orphaned");
        std::fs::remove_file(&artifact.data_path).unwrap();
        // Backdate the index so its stamp is settled: a stamp inside the file's mtime tick is
        // re-checked once when the tick passes, which a slow runner could hit inside the loop.
        set_mtime(&artifact.index_path, SystemTime::now() - Duration::from_secs(60));

        invalidate_cache();
        let never_stored = "7".repeat(64);
        assert!(!is_in_packs(&never_stored).unwrap(), "sanity: absent");
        let first = cached_pack_set();
        assert_eq!(first.quarantined.len(), 1, "sanity: the orphan is quarantined");
        assert_eq!(first.quarantined[0].failure, FailureKind::Fixed, "sanity: as a fixed failure");

        for _ in 0..3 {
            assert!(!is_in_packs(&never_stored).unwrap());
        }
        assert!(Arc::ptr_eq(&first, &cached_pack_set()), "an unchanged orphan must not reload the set on a miss");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-149: a full repack is serve-exclusive — refused while the serve lock is held (a live
    /// server on this root, or a gc), with an error that says why; incremental compaction drops
    /// nothing and still runs.
    ///
    /// Revert: drop the serve-lock acquire from `compact` → the repack runs beside the "server"
    /// → red.
    #[test]
    fn a_repack_refuses_while_the_root_is_served_but_incremental_compaction_runs() {
        let temp = fresh_root("repack-serve-lock");
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let content = b"content packed beside a live server".to_vec();
        store_loose(&blake3::hash(&content).to_hex().to_string(), &content);

        let served = lock_utils::ServeLock::acquire().expect("hold the serve lock, as a live server does");
        let error = compact(true, false, true, &Progress::NONE).expect_err("a repack must refuse while the root is served");
        assert!(error.contains("being served"), "the refusal should say the root is served: {}", error);
        assert!(compact(false, false, false, &Progress::NONE).is_ok(), "incremental compaction runs beside a server");
        drop(served);
        assert!(compact(true, false, true, &Progress::NONE).is_ok(), "a repack runs once the root is no longer served");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-150: a full repack is refused while any writer holds the store, and the packed
    /// unreachable object it would drop survives; with no holder the same repack drops it. A
    /// writer in another bay is stood in for by a shared hold on a second open file, which the
    /// kernel treats exactly like another process's. Incremental compaction drops nothing on a
    /// liveness decision and still runs beside the writer.
    ///
    /// Revert: drop the store-access acquire from `compact` → the guarded repack drops the
    /// object → red.
    #[test]
    fn a_repack_refuses_while_a_writer_holds_the_store_and_keeps_the_object_it_would_drop() {
        let temp = fresh_root("repack-store-access");
        let _scope = StorageRootScope::enter(&temp);
        crate::util::pallet_utils::create_position_if_not_exists().unwrap();

        let content = b"unreachable, packed, and found present by another bay's dedup".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);
        compact(false, false, false, &Progress::NONE).unwrap();
        assert!(is_in_packs(&hash).unwrap(), "sanity: the object is packed before the repack");

        let writer = lock_utils::StoreAccessLock::shared(|| panic!("nothing holds the store exclusively"))
            .expect("hold the store shared, as a writer in another bay does");
        let error = compact(true, false, true, &Progress::NONE).expect_err("a repack must refuse beside a writer");
        assert!(error.contains("using the object store"), "the refusal should say the store is in use: {}", error);
        assert!(is_in_packs(&hash).unwrap(), "the refused repack must leave the unreachable object packed");
        assert!(compact(false, false, false, &Progress::NONE).is_ok(), "incremental compaction runs beside a writer");
        drop(writer);

        compact(true, false, true, &Progress::NONE).expect("a repack runs once no writer holds the store");
        assert!(!is_in_packs(&hash).unwrap(), "with no writer, the repack drops the unreachable object");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// Incremental compaction moves loose objects into a pack and deletes the loose copies, so it
    /// must not interleave with a scope-prune or gc deleting loose objects on a keep-set: refused
    /// (never waiting) while a deleter holds the store, leaving the loose object where the deleter
    /// planned to find it.
    ///
    /// Revert: incremental compaction takes nothing again → it packs the object beside the deleter
    /// → red.
    #[test]
    fn incremental_compaction_refuses_while_a_deleter_holds_the_store() {
        let temp = fresh_root("compact-beside-deleter");
        let _scope = StorageRootScope::enter(&temp);

        let content = b"loose, and planned for freeing by a scope-prune".to_vec();
        let hash = blake3::hash(&content).to_hex().to_string();
        store_loose(&hash, &content);

        let deleter = lock_utils::StoreAccessLock::try_exclusive()
            .expect("hold the store exclusively, as a scope-prune in another bay does");
        let error = compact(false, false, false, &Progress::NONE)
            .expect_err("incremental compaction must refuse beside a deleter");
        assert!(error.contains("maintenance"), "the refusal should name the maintenance: {}", error);
        assert!(!is_in_packs(&hash).unwrap(), "the refused compaction must leave the object loose");
        drop(deleter);

        compact(false, false, false, &Progress::NONE).expect("incremental compaction runs once the deleter is done");
        assert!(is_in_packs(&hash).unwrap(), "with no deleter, the object is packed");

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the open probe: an unenumerable pair whose index could not be opened is retried
    /// once the *index* opens, even while its data file still cannot be — re-classifying it
    /// quarantined, so a hash its index names reads `Unverifiable`, never `Absent`.
    ///
    /// Revert: probe the data file for an unenumerable pair (or require both files to open) → the
    /// pair stays unenumerable, Y keeps reading `Absent` → red.
    #[cfg(unix)]
    #[test]
    fn an_unenumerable_pair_is_retried_when_its_index_opens_though_its_data_does_not() {
        use std::os::unix::fs::PermissionsExt;
        use crate::util::file_utils::StoreReadOutcome;

        let temp = fresh_root("open-probe-index");
        let _scope = StorageRootScope::enter(&temp);

        let (y_hash, artifact) = publish_external_pack(b"named by an index that briefly cannot be opened");
        let readable = std::fs::metadata(&artifact.index_path).unwrap().permissions();
        let locked = std::fs::Permissions::from_mode(0o000);
        std::fs::set_permissions(&artifact.index_path, locked.clone()).unwrap();
        std::fs::set_permissions(&artifact.data_path, locked).unwrap();
        if std::fs::File::open(&artifact.index_path).is_ok() {
            // Running as root: permissions cannot make the open fail, so there is nothing to test.
            std::fs::set_permissions(&artifact.index_path, readable.clone()).unwrap();
            std::fs::set_permissions(&artifact.data_path, readable).unwrap();
            std::fs::remove_dir_all(&temp).ok();
            return;
        }

        invalidate_cache();
        assert!(matches!(file_utils::read_object_classified(&y_hash).unwrap(), StoreReadOutcome::Absent),
            "sanity: an unenumerable index is inert, so Y reads Absent");
        let cached = cached_pack_set();
        assert_eq!(cached.unenumerable.len(), 1, "sanity: the pair is unenumerable");
        assert_eq!(cached.unenumerable[0].failure, FailureKind::Open, "sanity: as an open failure");

        std::fs::set_permissions(&artifact.index_path, readable.clone()).unwrap();
        let outcome = file_utils::read_object_classified(&y_hash).unwrap();
        std::fs::set_permissions(&artifact.data_path, readable).unwrap();
        match outcome {
            StoreReadOutcome::Unverifiable(_) => {}
            StoreReadOutcome::Absent => panic!("once the index opens, Y is quarantine-named and must read Unverifiable, not Absent"),
            StoreReadOutcome::Verified(_) => panic!("Y's data cannot be opened; it must not read Verified"),
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// FORK-106, the coarse-mtime half of a pair stamp: a stamp taken within `MTIME_GRANULARITY`
    /// of a file's mtime may miss a same-length, in-place rewrite in that tick, so once the tick
    /// has passed the pair is re-classified once; a stamp taken after it is trusted.
    ///
    /// Revert: drop the `settles_at` check from `broken_pair_changed` → the unsettled stamp is
    /// trusted forever → red.
    #[test]
    fn a_pair_stamped_inside_an_mtime_tick_is_rechecked_once_the_tick_passes() {
        let temp = fresh_root("pair-stamp-settles");
        let _scope = StorageRootScope::enter(&temp);

        let (_, artifact) = publish_external_pack(b"a pair whose stamp may sit inside an mtime tick");
        let index = &artifact.index_path;
        assert!(pair_stamp(index).settles_at.is_some(), "sanity: a file written just now stamps unsettled");

        let past = SystemTime::now() - Duration::from_secs(60);
        for path in [index, &artifact.data_path] {
            set_mtime(path, past);
        }
        let settled = pair_stamp(index);
        assert!(settled.settles_at.is_none(), "sanity: files last modified a minute ago stamp settled");

        let unsettled = PairStamp { settles_at: Some(past + MTIME_GRANULARITY), ..settled.clone() };
        let now = Instant::now();
        assert!(broken_pair_changed(index, &unsettled, FailureKind::Fixed, now, index),
            "a stamp whose tick has passed must be re-checked once");
        assert!(!broken_pair_changed(index, &settled, FailureKind::Fixed, now, index),
            "a settled stamp on unchanged files is trusted");

        std::fs::remove_dir_all(&temp).ok();
    }
}
