//! Packing the loose object store into bounded packs (DESIGN.html §4.5, object-store
//! scaling phase 1 — see `docs/OBJECT_STORE_SCALING.md`).
//!
//! Every object is normally its own zstd-compressed file (`file_utils::write_object_to_file`).
//! At git.git scale that is ~400k tiny files: each pays filesystem slack, and a whole-history
//! walk does that many random `open`+`read`s. `compact` sweeps the loose set into a handful of
//! **packs** — an append-only data file plus a sorted index — so a read is a binary search in a
//! resident index and one `read` at an offset, and the store is a few large files instead of a
//! sea of small ones.
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
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::model::object::loose_object::LooseObject;

use crate::util::{
    audit_utils, bundle_utils, byte_utils, delta_utils, fanout_utils, file_utils, graph_utils,
    lock_utils, object_utils, pallet_utils, sign_utils,
};

/// The folder under the object store that holds packs.
const PACK_FOLDER_NAME: &str = "pack";

/// The extension of a pack's data file (concatenated object blobs).
const PACK_DATA_EXTENSION: &str = "pack";

/// The extension of a pack's index file (sorted hash → offset/length records).
const PACK_INDEX_EXTENSION: &str = "idx";

/// Magic + version prefixing a pack data file (so a truncated or foreign file is rejected).
const PACK_DATA_MAGIC: &[u8; 8] = b"FORKPACK";

/// Magic + version prefixing a pack index file.
const PACK_INDEX_MAGIC: &[u8; 8] = b"FORKPIDX";

/// The current pack format version. Version 1 stored each record as a bare zstd blob;
/// version 2 (phase 2) frames every record with a one-byte kind so a record can be a delta.
/// Version-1 packs are still read (their records have no kind byte); new packs are written
/// at the current version. Bump on any further incompatible layout change.
const PACK_FORMAT_VERSION: u32 = 2;

/// The first framed pack version — at or above this, a data record starts with a kind byte;
/// below it (version 1) the record is a bare zstd blob.
const FIRST_FRAMED_VERSION: u32 = 2;

/// Record kinds in a framed (version ≥ 2) pack.
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

/// The length of a pack index header: magic (8) + version (4) + record count (4).
const INDEX_HEADER_LEN: usize = 16;

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
const PACK_ROLLOVER_OBJECTS: usize = 100_000;

/// The largest native-pack data section a transport bundle may declare. A writer rolls after
/// crossing `PACK_ROLLOVER_BYTES`, so the record that crosses it may itself be one maximal object.
/// Keeping the import cap derived from those two writer bounds prevents a hostile bundle from
/// turning one declared section length into an unbounded disk-fill stream.
pub(crate) const MAX_TRANSPORT_PACK_BYTES: u64 =
    PACK_ROLLOVER_BYTES + object_utils::MAX_OBJECT_BYTES as u64 + 2 * 1024 * 1024;

/// The largest native-pack index section a transport bundle may declare. One writer can carry at
/// most `PACK_ROLLOVER_OBJECTS` records before it rolls; the fixed header and fixed-width records
/// make the exact upper bound cheap to state and enforce before reading the section.
pub(crate) const MAX_TRANSPORT_INDEX_BYTES: u64 =
    (INDEX_HEADER_LEN + PACK_ROLLOVER_OBJECTS * INDEX_RECORD_LEN) as u64;

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

    if let Some((value, _)) = config_utils::get_effective_value(config_utils::KEY_MAINTENANCE_AUTO)? {
        let value = value.trim().to_ascii_lowercase();
        if value == "false" || value == "0" || value == "off" || value == "no" {
            return Ok(AutoCompaction::None);
        }
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
    /// Of `objects`, how many are stored as deltas against a base (0 in a version-1 pack).
    pub deltas: usize,
    /// On-disk bytes of the pack: its data file plus its index file.
    pub bytes: u64,
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

    // Packs: reuse the reader (mmaps each data file, holds each index resident). Per pack we
    // report its object count (the index), its delta count (the framed record kinds), and its
    // on-disk size (data file + index file, both already sized once loaded).
    let mut packs = Vec::new();
    for pack in load_packs_from_disk(&pack_folder())? {
        let framed = pack.version >= FIRST_FRAMED_VERSION;
        let mut deltas = 0;

        if framed {
            for record in 0..pack.count {
                let record_offset = INDEX_HEADER_LEN + record * INDEX_RECORD_LEN;
                let data_offset = read_u64_le(&pack.index, record_offset + HASH_LEN) as usize;
                // A framed record leads with its kind byte; a delta is `RECORD_DELTA`. An
                // out-of-bounds offset (corruption) reads as `None` — simply not a delta.
                if pack.data.get(data_offset) == Some(&RECORD_DELTA) {
                    deltas += 1;
                }
            }
        }

        let id = pack.data_path.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();

        packs.push(PackSummary {
            id,
            objects: pack.count,
            deltas,
            bytes: pack.data.len() as u64 + pack.index.len() as u64,
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
    })
}

/// Whether background object-store maintenance is enabled (`maintenance.auto`, on unless set to
/// a falsey value). Mirrors the check [`auto_compaction_action`] makes.
fn maintenance_auto_enabled() -> Result<bool, String> {
    use crate::util::config_utils;

    if let Some((value, _)) = config_utils::get_effective_value(config_utils::KEY_MAINTENANCE_AUTO)? {
        let value = value.trim().to_ascii_lowercase();
        if value == "false" || value == "0" || value == "off" || value == "no" {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Read a numeric maintenance threshold from configuration, falling back to `default` when it
/// is unset (an unparseable value also falls back rather than failing maintenance).
fn config_threshold(key: &str, default: usize) -> Result<usize, String> {
    Ok(crate::util::config_utils::get_effective_value(key)?
        .and_then(|(value, _)| value.trim().parse().ok())
        .unwrap_or(default))
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
}

/// A pack loaded for reading: its data file memory-mapped, plus its index bytes held resident
/// (header + sorted records), binary-searched in place — no per-record allocation.
struct LoadedPack {
    data_path: PathBuf,
    /// The data file mapped into memory for the life of the loaded pack, so a read is a slice
    /// into mapped pages — no `open`/`seek`/`read` syscall and no buffer copy per object, which
    /// on a history or blame walk is tens of thousands of syscalls and copies saved. A pack is
    /// immutable once written (write-once, then deleted whole; never truncated) and a `compact`
    /// invalidates the whole registry, so the mapping never goes stale.
    data: memmap2::Mmap,
    index: Vec<u8>,
    count: usize,
    /// The pack's format version — decides whether a data record carries a kind byte.
    version: u32,
}

impl LoadedPack {
    /// The `(offset, length)` of the object with `hash_bytes` in this pack's data file, or
    /// `None` if this pack does not hold it. Binary search over the sorted index records.
    fn locate(&self, hash_bytes: &[u8; HASH_LEN]) -> Option<(u64, u64)> {
        let mut low = 0usize;
        let mut high = self.count;

        while low < high {
            let mid = low + (high - low) / 2;
            let record = INDEX_HEADER_LEN + mid * INDEX_RECORD_LEN;
            let record_hash = &self.index[record..record + HASH_LEN];

            match record_hash.cmp(hash_bytes.as_slice()) {
                std::cmp::Ordering::Less => low = mid + 1,
                std::cmp::Ordering::Greater => high = mid,
                std::cmp::Ordering::Equal => {
                    let offset = read_u64_le(&self.index, record + HASH_LEN);
                    let length = read_u64_le(&self.index, record + HASH_LEN + 8);
                    return Some((offset, length));
                }
            }
        }

        None
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

/// The read cache: each warehouse's object store maps to the packs loaded for it. Keyed by
/// the objects-root path so one process serving several warehouse roots (the server, via a
/// storage-root scope) never mixes their packs. Loaded once per root on first miss of the
/// loose store; `compact` invalidates its own root's entry so a same-process read sees new
/// packs.
static PACK_REGISTRY: OnceLock<Mutex<HashMap<String, Arc<Vec<LoadedPack>>>>> = OnceLock::new();

fn registry() -> &'static Mutex<HashMap<String, Arc<Vec<LoadedPack>>>> {
    PACK_REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

/// The pack folder for the active warehouse's object store.
fn pack_folder() -> PathBuf {
    PathBuf::from(file_utils::get_path_objects_root()).join(PACK_FOLDER_NAME)
}

/// Read the packs for the active warehouse, loading and caching them on first use. The
/// returned `Arc` lets a lookup search without holding the registry lock.
fn loaded_packs() -> Result<Arc<Vec<LoadedPack>>, String> {
    let key = file_utils::get_path_objects_root();

    if let Some(packs) = registry().lock().expect("the pack registry lock is poisoned").get(&key) {
        return Ok(Arc::clone(packs));
    }

    let packs = Arc::new(load_packs_from_disk(&pack_folder())?);

    registry().lock().expect("the pack registry lock is poisoned")
        .insert(key, Arc::clone(&packs));

    Ok(packs)
}

/// Forget the cached packs for the active warehouse, so the next read reloads them from
/// disk. Called after `compact` writes new packs in this process.
fn invalidate_cache() {
    registry().lock().expect("the pack registry lock is poisoned")
        .remove(&file_utils::get_path_objects_root());
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

/// Load every pack in `pack_folder` (its index resident, its data file left on disk for
/// per-object reads). A missing pack folder is simply no packs.
fn load_packs_from_disk(pack_folder: &Path) -> Result<Vec<LoadedPack>, String> {
    let mut packs = Vec::new();

    let entries = match std::fs::read_dir(pack_folder) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(packs),
        Err(error) => return Err(format!(
            "Error while reading the pack folder \"{}\": {}", pack_folder.to_string_lossy(), error
        )),
    };

    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while listing the pack folder: {}", e))?;
        let path = entry.path();

        if path.extension().and_then(|e| e.to_str()) != Some(PACK_INDEX_EXTENSION) {
            continue;
        }

        let data_path = path.with_extension(PACK_DATA_EXTENSION);
        packs.push(load_pack_pair(&data_path, &path)?);
    }

    Ok(packs)
}

/// Load one native pack/index pair and validate the structural contract shared by ordinary local
/// packs and transport-installed packs. Content hashes are checked separately: ordinary reads do
/// it on demand, while a transport import checks the whole quarantined set before publication.
fn load_pack_pair(data_path: &Path, index_path: &Path) -> Result<LoadedPack, String> {
    let index = std::fs::read(index_path)
        .map_err(|e| format!("Error while reading pack index \"{}\": {}", index_path.to_string_lossy(), e))?;
    let (count, version) = parse_index_header(&index, index_path)?;

    let file = std::fs::File::open(data_path)
        .map_err(|e| format!("Error while opening pack data \"{}\": {}", data_path.to_string_lossy(), e))?;
    // SAFETY: callers only pass immutable pack data: a published pack is write-once, and a
    // transport staging file is closed and never modified again before this mapping is dropped.
    let data = unsafe { memmap2::Mmap::map(&file) }
        .map_err(|e| format!("Error while mapping pack data \"{}\": {}", data_path.to_string_lossy(), e))?;

    if data.len() < PACK_DATA_HEADER_LEN as usize || &data[0..8] != PACK_DATA_MAGIC {
        return Err(format!(
            "Pack data \"{}\" is corrupt or has an unknown format.", data_path.to_string_lossy()
        ));
    }

    let data_version = read_u32_le(&data, 8);
    if data_version != version {
        return Err(format!(
            "Pack data \"{}\" has format version {}, but its index has version {}.",
            data_path.to_string_lossy(), data_version, version
        ));
    }

    validate_index_records(&index, count, data.len(), index_path)?;

    Ok(LoadedPack { data_path: data_path.to_path_buf(), data, index, count, version })
}

/// Validate a pack index header and return its `(record count, format version)`.
fn parse_index_header(index: &[u8], path: &Path) -> Result<(usize, u32), String> {
    let corrupt = || format!("Pack index \"{}\" is corrupt or has an unknown format.", path.to_string_lossy());

    if index.len() < INDEX_HEADER_LEN || &index[0..8] != PACK_INDEX_MAGIC {
        return Err(corrupt());
    }

    // A newer version is refused (this build cannot understand it); older ones are read.
    let version = read_u32_le(index, 8);
    if version == 0 || version > PACK_FORMAT_VERSION {
        return Err(format!(
            "Pack index \"{}\" has format version {}, but this build understands up to {}.",
            path.to_string_lossy(), version, PACK_FORMAT_VERSION
        ));
    }

    let count = read_u32_le(index, 12) as usize;

    if index.len() != INDEX_HEADER_LEN + count * INDEX_RECORD_LEN {
        return Err(corrupt());
    }

    Ok((count, version))
}

/// Validate the index beyond its header: hashes are strictly sorted, record ranges are non-empty
/// and in bounds, and the offset-sorted records cover the data body exactly once with no hidden
/// gaps or overlaps. The native writer always has this shape; enforcing it on transport input
/// makes every byte in a published pack accountable to exactly one hash.
fn validate_index_records(index: &[u8], count: usize, data_len: usize, path: &Path) -> Result<(), String> {
    let corrupt = || format!("Pack index \"{}\" is corrupt.", path.to_string_lossy());
    let mut previous_hash: Option<&[u8]> = None;
    let mut ranges: Vec<(u64, u64)> = Vec::with_capacity(count);

    for position in 0..count {
        let record = INDEX_HEADER_LEN + position * INDEX_RECORD_LEN;
        let hash = &index[record..record + HASH_LEN];

        if previous_hash.is_some_and(|previous| previous >= hash) {
            return Err(corrupt());
        }
        previous_hash = Some(hash);

        let offset = read_u64_le(index, record + HASH_LEN);
        let length = read_u64_le(index, record + HASH_LEN + 8);
        let end = offset.checked_add(length).ok_or_else(&corrupt)?;

        if length == 0 || offset < PACK_DATA_HEADER_LEN || end > data_len as u64 {
            return Err(corrupt());
        }
        ranges.push((offset, end));
    }

    ranges.sort_unstable_by_key(|(start, _)| *start);
    let mut expected = PACK_DATA_HEADER_LEN;
    for (start, end) in ranges {
        if start != expected {
            return Err(corrupt());
        }
        expected = end;
    }

    if expected != data_len as u64 {
        return Err(corrupt());
    }

    Ok(())
}

/// What installing the native-pack payload of a transport bundle added to this object store.
pub(crate) struct TransportImportStats {
    pub stored_objects: usize,
    pub skipped_objects: usize,
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

        let hashes = verify_incoming_packs(&incoming)?;
        let mut stored_objects = 0;
        let mut skipped_objects = 0;
        for hash in &hashes {
            if file_utils::does_object_exist(&sign_utils::to_hex(hash))? {
                skipped_objects += 1;
            } else {
                stored_objects += 1;
            }
        }

        // Drop every mmap before renaming the staged files (Windows refuses to rename a mapped
        // file; POSIX permits it, but the portable contract is cheap to preserve).
        drop(incoming);

        if stored_objects == 0 {
            return Ok(TransportImportStats { stored_objects, skipped_objects });
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

        for pair in &staged {
            let index = std::fs::read(&pair.index_path)
                .map_err(|e| format!("Error while reading a verified bundle pack index: {}", e))?;
            let pack_id = pack_id_from_index(&index)?;
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

        Ok(TransportImportStats { stored_objects, skipped_objects })
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
fn pack_id_from_index(index: &[u8]) -> Result<String, String> {
    let (count, _) = parse_index_header(index, Path::new("<bundle pack index>"))?;
    let mut records = Vec::with_capacity(count);

    for position in 0..count {
        let record = INDEX_HEADER_LEN + position * INDEX_RECORD_LEN;
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(&index[record..record + HASH_LEN]);
        records.push((
            hash,
            read_u64_le(index, record + HASH_LEN),
            read_u64_le(index, record + HASH_LEN + 8),
        ));
    }

    Ok(compute_pack_id(&records))
}

/// Verify every object reachable through the incoming indexes before any pack is published.
/// Returns the distinct object hashes for import statistics. Bookkeeping keeps hashes in their
/// raw 32-byte index form — hex `String`s would multiply the per-object cost several times over,
/// and a large clone carries millions of objects. The cache is deliberately bounded: it
/// accelerates delta chains, but a hostile pack cannot make verification retain the whole
/// decompressed store in RAM.
fn verify_incoming_packs(packs: &[LoadedPack]) -> Result<Vec<[u8; HASH_LEN]>, String> {
    let locator = IncomingLocator::build(packs)?;

    let mut hashes = Vec::with_capacity(locator.entries.len());
    let mut cache = IncomingVerificationCache::default();
    let mut visiting = HashSet::new();

    for entry in &locator.entries {
        let mut hash = [0u8; HASH_LEN];
        hash.copy_from_slice(locator.hash_of(*entry));
        resolve_incoming_object(&hash, &locator, &mut cache, &mut visiting, 0)?;
        hashes.push(hash);
    }

    Ok(hashes)
}

/// A sorted locator over every record in the quarantined incoming set, so each of the
/// per-object (and per-delta-base) lookups during verification is one binary search across the
/// whole set instead of one per pack — O(objects × log objects) total rather than
/// O(objects × packs × log). Entries are indirect `(pack, record)` positions (8 bytes per
/// object) comparing against the packs' resident index bytes; a hash-keyed map would cost an
/// order of magnitude more transient memory on a large clone.
struct IncomingLocator<'a> {
    packs: &'a [LoadedPack],
    /// `(pack position, record position)`, sorted by the record's hash bytes.
    entries: Vec<(u32, u32)>,
}

impl<'a> IncomingLocator<'a> {
    /// Build the locator. Sorting also surfaces cross-pack duplicates (adjacent equal hashes),
    /// which are refused: the bundle builder emits every object exactly once.
    fn build(packs: &'a [LoadedPack]) -> Result<IncomingLocator<'a>, String> {
        let total = packs.iter().map(|pack| pack.count).sum();
        let mut entries: Vec<(u32, u32)> = Vec::with_capacity(total);

        for (pack_position, pack) in packs.iter().enumerate() {
            for record_position in 0..pack.count {
                entries.push((pack_position as u32, record_position as u32));
            }
        }

        let hash_of = |entry: &(u32, u32)| -> &[u8] {
            let record = INDEX_HEADER_LEN + entry.1 as usize * INDEX_RECORD_LEN;
            &packs[entry.0 as usize].index[record..record + HASH_LEN]
        };
        entries.sort_unstable_by(|a, b| hash_of(a).cmp(hash_of(b)));

        for pair in entries.windows(2) {
            if hash_of(&pair[0]) == hash_of(&pair[1]) {
                return Err(format!(
                    "The bundle's native packs contain duplicate object {}.",
                    sign_utils::to_hex(hash_of(&pair[0]))
                ));
            }
        }

        Ok(IncomingLocator { packs, entries })
    }

    fn hash_of(&self, entry: (u32, u32)) -> &'a [u8] {
        let record = INDEX_HEADER_LEN + entry.1 as usize * INDEX_RECORD_LEN;
        &self.packs[entry.0 as usize].index[record..record + HASH_LEN]
    }

    /// Locate a hash in the incoming set: its pack and the record's `(offset, length)` there.
    fn locate(&self, hash: &[u8; HASH_LEN]) -> Option<(&'a LoadedPack, u64, u64)> {
        let position = self.entries
            .binary_search_by(|entry| self.hash_of(*entry).cmp(hash))
            .ok()?;
        let (pack_position, record_position) = self.entries[position];
        let pack = &self.packs[pack_position as usize];
        let record = INDEX_HEADER_LEN + record_position as usize * INDEX_RECORD_LEN;

        Some((
            pack,
            read_u64_le(&pack.index, record + HASH_LEN),
            read_u64_le(&pack.index, record + HASH_LEN + 8),
        ))
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

        if pack.version < FIRST_FRAMED_VERSION {
            return decode_full_transport_record(record, &hex);
        }

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
    let mut decoder = zstd::stream::read::Decoder::new(record)
        .map_err(|e| format!("Error while opening packed object {}: {}", hash, e))?;
    let mut bytes = Vec::new();
    decoder.by_ref()
        .take(object_utils::MAX_OBJECT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("Error while decompressing packed object {}: {}", hash, e))?;

    if bytes.len() > object_utils::MAX_OBJECT_BYTES {
        return Err(format!(
            "Packed object {} expands above the {}-byte object ceiling; refusing the bundle.",
            hash, object_utils::MAX_OBJECT_BYTES
        ));
    }

    Ok(bytes)
}

/// Retrieve the decompressed bytes of an object from the packs, or `None` if no pack holds
/// it. The read fallback for `file_utils::retrieve_object_by_hash` when the loose file is
/// absent.
///
/// # Arguments
/// * `hash` - The hex hash of the object.
///
/// # Returns
/// * `Ok(Some(Vec<u8>))` - The decompressed object bytes.
/// * `Ok(None)`          - If the object is in no pack.
/// * `Err(String)`       - If a pack could be read but the blob was unreadable.
pub fn retrieve_from_packs(hash: &str) -> Result<Option<Vec<u8>>, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(None);
    };

    let packs = loaded_packs()?;

    for pack in packs.iter() {
        let Some((offset, length)) = pack.locate(&hash_bytes) else {
            continue;
        };

        let record = pack.slice(offset, length)?;

        return resolve_record(record, pack.version, hash).map(Some);
    }

    Ok(None)
}

/// Retrieve an object from packs after forcibly reloading this warehouse's pack registry from
/// disk — the reload-on-miss retry a long-running process needs.
///
/// The mmap pack registry is otherwise only refreshed by a `compact` in *this* process. A live
/// server whose registry predates an *external* `compact` would miss an object that was moved
/// into a new pack — and whose loose source that same compact already swept — so both the cached
/// pack lookup and the loose fallback come up empty even though the object is present on disk. One
/// forced reload closes that window before the read is declared a miss. Called only from the
/// read path's last-resort branch, so a genuinely absent object reloads at most once per read.
pub fn retrieve_from_packs_reloading(hash: &str) -> Result<Option<Vec<u8>>, String> {
    invalidate_cache();
    retrieve_from_packs(hash)
}

/// A hard ceiling on how deep a delta chain may be followed when reconstructing an object.
///
/// This is a backstop only, not the mechanism that keeps chains short: `compact`'s write-time
/// depth ledger (`true_depth`, consulted by its depth-safety pre-pass) is what enforces the real
/// invariant — **no written delta's true chain depth may ever exceed `MAX_DELTA_CHAIN`, on any
/// pass, from any starting store** — by resolving every candidate path base's *actual* current
/// depth (from record headers, no decompression) before committing to it, rather than trusting
/// `compute_path_bases`'s own bookkeeping (which only counts path hops since a reset, and is
/// deliberately silent about a reset point's own real depth — see its doc comment) at face value.
/// A candidate that would push true depth past the cap is demoted to a full record instead.
///
/// So a well-formed pack's chains never approach this ceiling; it exists purely against a
/// corrupt or adversarial pack that chains without end, turning unbounded recursion (a crash)
/// into a clean error, never a lost or wrong object (caught before anything is written or
/// deleted). `true_depth`'s own walk is bounded by this same constant for the same reason.
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

/// Reconstruct an object's bytes from its data-file record and verify them against `hash`.
///
/// The reconstruction is delegated to [`reconstruct_record`]; this wrapper then enforces the
/// module's content-addressing guarantee — the reconstructed bytes must hash to `hash`, so a
/// corrupt full record, a bad delta, or a delta rebuilt against the wrong base fails the read
/// rather than silently returning wrong bytes (`object_utils::verify_object_bytes`).
fn resolve_record(record: &[u8], version: u32, hash: &str) -> Result<Vec<u8>, String> {
    let bytes = reconstruct_record(record, version, hash)?;
    object_utils::verify_object_bytes(hash, &bytes)?;
    Ok(bytes)
}

/// Reconstruct an object's bytes from its data-file record, *without* verifying the result —
/// its sole caller [`resolve_record`] does that.
///
/// A version-1 record is a bare zstd blob. A version ≥ 2 record starts with a kind byte:
/// a full record's remainder is the zstd blob; a delta record's remainder is
/// `base hash (32) || target length (VLQ) || zstd delta`, reconstructed against its base —
/// which is fetched through the top-level object read, so the base may itself be loose, in
/// another pack, or itself a delta. The chain is bounded well below `MAX_RECONSTRUCT_DEPTH`,
/// which guards only against a corrupt pack chaining without end.
fn reconstruct_record(record: &[u8], version: u32, hash: &str) -> Result<Vec<u8>, String> {
    let decode_full = |blob: &[u8]| zstd::stream::decode_all(blob)
        .map_err(|e| format!("Error while decompressing packed object {}: {}", hash, e));

    if version < FIRST_FRAMED_VERSION {
        return decode_full(record);
    }

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

            let base_hash = sign_utils::to_hex(&body[0..HASH_LEN]);
            let (target_len, read) = byte_utils::number_from_vlq_bytes(HASH_LEN, body)?;
            let payload = &body[HASH_LEN + read..];

            // Borrow-only: the delta base is only read to reconstruct against, so share the
            // cached `Arc` instead of copying the base out (hot on packed-delta reads and compact).
            let base = file_utils::retrieve_object_by_hash_shared(&base_hash)?;

            delta_utils::decompress_delta(&base, payload, target_len as usize)
        }
        other => Err(format!("Packed object {} has an unknown record kind {}.", hash, other)),
    }
}

/// Every packed object hash (hex) that begins with `prefix`. The pack-aware half of
/// resolving a revision given as a hash or hash prefix — without it, a hash reference stops
/// resolving once its object is packed. A linear scan of the resident indexes; resolution is
/// interactive and rare, so the simple form is fine.
///
/// # Arguments
/// * `prefix` - A hex hash or hash prefix.
///
/// # Returns
/// * `Ok(Vec<String>)` - The full hex hashes of packed objects matching the prefix.
/// * `Err(String)`     - If the packs could not be loaded.
pub fn find_hashes_with_prefix(prefix: &str) -> Result<Vec<String>, String> {
    let packs = loaded_packs()?;
    let mut matches = Vec::new();

    for pack in packs.iter() {
        for index in 0..pack.count {
            let record = INDEX_HEADER_LEN + index * INDEX_RECORD_LEN;
            let hash = sign_utils::to_hex(&pack.index[record..record + HASH_LEN]);
            if hash.starts_with(prefix) {
                matches.push(hash);
            }
        }
    }

    Ok(matches)
}

/// Whether any pack holds the object with the given hash. The existence fallback for
/// `file_utils::does_object_exist` when the loose file is absent.
///
/// # Arguments
/// * `hash` - The hex hash of the object.
///
/// # Returns
/// * `Ok(true)`    - If a pack holds the object.
/// * `Ok(false)`   - If no pack holds it.
/// * `Err(String)` - If the packs could not be loaded.
pub fn is_in_packs(hash: &str) -> Result<bool, String> {
    let Some(hash_bytes) = hash_to_bytes(hash) else {
        return Ok(false);
    };

    let packs = loaded_packs()?;

    Ok(packs.iter().any(|pack| pack.locate(&hash_bytes).is_some()))
}

/// Where an object to pack comes from, and how to pack it.
enum Source {
    /// A loose file: read it, delta it (path-aware / window), and delete it once packed.
    Loose(PathBuf),
    /// A record already in a pack whose delta base survives the repack — **copied verbatim**,
    /// never reconstructed or re-deltated, so the original (good) delta is preserved and the
    /// repack stays a byte-copy. `pack_index` indexes the source packs kept mapped for the run
    /// (`collect_targets` returns them), so the record is a zero-copy slice out of that pack's
    /// mmap — no per-record `open`/`seek`/`read`. `framed` is false for a version-1 (unframed)
    /// record, which is wrapped in a full-record kind byte on the way into the version-2 pack.
    CopyRecord { pack_index: usize, offset: u64, len: u64, framed: bool, is_delta: bool },
    /// A packed object whose delta base is being dropped as garbage: reconstruct it and re-pack
    /// it path-aware, so nothing is left pointing at the dropped base. Rare.
    Reconstruct,
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
/// byte-copy, not a re-compress. `redelta` (only meaningful with `all`; the caller is
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
pub fn compact(all: bool, redelta: bool) -> Result<CompactStats, String> {
    let pack_folder = pack_folder();
    file_utils::create_folder_if_not_exists(&pack_folder)?;

    // Serialize destructive store maintenance across bays *and* processes: the object store is
    // shared (`forklift_root`), but a command's bay lock is not, so without this two bays could
    // enumerate the same loose set and race each other's deletions. Held for the whole run;
    // errors immediately if another compaction holds it — an explicit `compact` surfaces that,
    // auto-maintenance (which ignores compaction errors) simply skips the now-redundant work.
    // Taken after the folder exists so its parent (`forklift_root`) is present for `create_new`.
    let _store_lock = lock_utils::StoreLock::acquire()?;

    remove_stale_temp_files(&pack_folder);

    // The objects to pack, and the old pack files a repack supersedes.
    //
    // One shared reachability pass (D/P3). Both the garbage decision (`collect_live_set`, inside
    // `collect_targets`) and the path-base selection (`compute_path_bases`) walk the reachable
    // parcel DAG, reading each parcel several times over — so hold a `ParcelReadMemo` across the
    // whole reachability phase, which collapses those (~5 per parcel) re-reads to one decode each
    // and is dropped before the parallel batch loop below. `source_packs` are the existing packs,
    // kept mapped for the run so a `CopyRecord` copies its bytes straight from the mmap.
    let (targets, old_packs, source_packs, depth_packs, path_bases) = {
        let _parcel_memo = object_utils::ParcelReadMemo::activate();

        let CollectedTargets { targets, old_packs, source_packs } = collect_targets(all, redelta)?;

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

        (targets, old_packs, source_packs, depth_packs, path_bases)
    };

    let mut stats = CompactStats {
        objects_packed: 0, packs_written: 0, loose_removed: 0, deltas: 0, bytes_packed: 0,
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
    // content-derived, so an unchanged repack writes the very same filename).
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
        // (see `MAX_RECONSTRUCT_DEPTH`'s doc comment): no written delta's true chain depth may
        // exceed `MAX_DELTA_CHAIN`, ever, on any pass, from any starting store.
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

            let base_depth = true_depth(base, &mut known_depth, &depth_packs)?;
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
            // Chunks are never packed or delta'd: leave the loose chunk file exactly where it is
            // (do not write it into a pack, do not mark it for deletion, do not seed the delta
            // window with it). Detected from the decompressed bytes already prepared for this
            // target, so no extra read; only a loose target can be a chunk (a chunk is never in an
            // existing pack, so a `CopyRecord` never is one). This is checked before the writer is
            // fetched so an all-chunk batch never creates an empty pack.
            if matches!(target.source, Source::Loose(_))
                && prepared[i].as_ref().is_some_and(|prep| is_chunk(&prep.raw)) {
                continue;
            }

            let pack = match writer.as_mut() {
                Some(pack) => pack,
                None => writer.insert(PackWriter::new(&pack_folder)?),
            };

            // Copy an existing record verbatim — a repack's fast path: the original (good) delta
            // is preserved, nothing is reconstructed or re-deltated. The bytes are a zero-copy
            // slice out of the source pack's mmap (framed case) rather than a fresh file read.
            if let Source::CopyRecord { pack_index, offset, len, framed, is_delta } = &target.source {
                let record = framed_record(&source_packs[*pack_index], *offset, *len, *framed)?;
                let written = pack.append_raw_record(target.hash, &record)?;
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

            let prep = prepared[i].take().expect("a non-copy target was prepared");

            let loose_path = match &target.source {
                Source::Loose(path) => Some(path.clone()),
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
    // `Arc<Vec<LoadedPack>>` for the mmap'd fast path must not silently change that. (POSIX needs
    // no such care — unlinking an open-and-mapped file is always safe there — but this drop must
    // hold on every supported platform, not just the one this was measured on.)
    drop(source_packs);

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
        // A content-derived pack id means an unchanged repack writes the same filename it is
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

/// Read an object being *re-deltated* (a loose object, or one whose base is dropped): its
/// decompressed bytes and its zstd blob (the full-record payload and the size guard). A loose
/// object is read from its file; a to-reconstruct object comes through the object store.
fn read_target(target: &PackTarget) -> Result<(Vec<u8>, Vec<u8>), String> {
    match &target.source {
        Source::Loose(path) => {
            let compressed = std::fs::read(path)
                .map_err(|e| format!("Error while reading loose object: {}", e))?;
            let raw = zstd::stream::decode_all(compressed.as_slice())
                .map_err(|e| format!("Error while decompressing a loose object: {}", e))?;
            Ok((raw, compressed))
        }
        Source::Reconstruct => {
            let raw = file_utils::retrieve_object_by_hash(&sign_utils::to_hex(&target.hash))?;
            let compressed = zstd::encode_all(raw.as_slice(), 0)
                .map_err(|e| format!("Error while compressing a repacked object: {}", e))?;
            Ok((raw, compressed))
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

/// Prepare a batch of targets, fanning the (read + path-delta compress) across the cores. A
/// `CopyRecord` target needs no preparation (it is byte-copied on the write path), so its slot
/// is `None`. Results are positionally aligned with `batch`.
fn prepare_batch(batch: &[PackTarget],
                 path_bases: &HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>) -> Result<Vec<Option<Prepared>>, String> {
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
    // (order-preserved) results the same way here.
    fanout_utils::fanout_map(batch, |target| prepare_target(target, path_bases))
        .into_iter()
        .collect()
}

/// Read one target and compute its path delta if one wins — the body of the parallel prep.
/// A `CopyRecord` is `None` (it is copied verbatim on the sequential write path). The window
/// fallback is *not* done here: it depends on the objects written just before, so it stays on
/// the sequential path.
fn prepare_target(target: &PackTarget,
                  path_bases: &HashMap<[u8; HASH_LEN], [u8; HASH_LEN]>) -> Result<Option<Prepared>, String> {
    if matches!(target.source, Source::CopyRecord { .. }) {
        return Ok(None);
    }

    let (raw, compressed) = read_target(target)?;

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
            let base_raw = file_utils::retrieve_object_by_hash_shared(&base_hex)?;
            let payload = delta_utils::compress_delta(&base_raw, &raw)?;

            if payload.len() < compressed.len() {
                path_delta = Some((*base, payload));
            }
        }
    }

    Ok(Some(Prepared { raw, compressed, deltable, path_delta }))
}

/// The result of [`collect_targets`]: what to pack, what a repack supersedes, and the packs a
/// `Source::CopyRecord` among `targets` borrows its bytes from.
struct CollectedTargets {
    /// The objects to pack (loose files and/or existing packed records).
    targets: Vec<PackTarget>,
    /// Old pack files a repack supersedes (empty for an incremental `compact`).
    old_packs: Vec<PathBuf>,
    /// The existing packs, kept mapped for the run so every `Source::CopyRecord { pack_index,
    /// .. }` in `targets` can borrow its record straight from `source_packs[pack_index]`'s mmap
    /// (empty for an incremental `compact`, which has no `CopyRecord` targets).
    source_packs: Arc<Vec<LoadedPack>>,
}

/// The objects to pack and the old pack files a repack supersedes.
///
/// Incremental: every loose object, no old packs touched. Repack: the live set only — live
/// loose objects, plus live objects in existing packs, and every existing pack file to be
/// deleted once the live set is safely re-packed. A live packed record is **copied verbatim**
/// when its delta base also survives (the fast path); the rare object whose base is being
/// dropped is reconstructed and re-deltated instead. Unreachable objects are simply not
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
fn collect_targets(all: bool, redelta: bool) -> Result<CollectedTargets, String> {
    if !all {
        return Ok(CollectedTargets {
            targets: enumerate_loose_objects()?,
            old_packs: Vec::new(),
            source_packs: Arc::new(Vec::new()),
        });
    }

    let live = crate::util::gc_utils::collect_live_set()?;
    let mut targets = Vec::new();
    let mut seen: HashSet<[u8; HASH_LEN]> = HashSet::new();

    // Live loose objects (read from and deleted with their files).
    for target in enumerate_loose_objects()? {
        if live.contains(&sign_utils::to_hex(&target.hash)) && seen.insert(target.hash) {
            targets.push(target);
        }
    }

    // Live objects in existing packs. Copy each record verbatim when its base survives; the old
    // packs are removed at the end, which is what drops the garbage that is not carried over. The
    // mapped packs are returned to the caller and held for the whole run, so a `CopyRecord` reads
    // its bytes as a zero-copy slice out of the mmap (addressed by `pack_index`), never a fresh
    // per-record file read.
    let packs = loaded_packs()?;
    let mut old_packs = Vec::new();

    for (pack_index, pack) in packs.iter().enumerate() {
        old_packs.push(pack.data_path.clone());
        old_packs.push(pack.data_path.with_extension(PACK_INDEX_EXTENSION));

        for index in 0..pack.count {
            let record = INDEX_HEADER_LEN + index * INDEX_RECORD_LEN;
            let mut hash = [0u8; HASH_LEN];
            hash.copy_from_slice(&pack.index[record..record + HASH_LEN]);

            if !live.contains(&sign_utils::to_hex(&hash)) || !seen.insert(hash) {
                continue;
            }

            let offset = read_u64_le(&pack.index, record + HASH_LEN);
            let length = read_u64_le(&pack.index, record + HASH_LEN + 8);
            let framed = pack.version >= FIRST_FRAMED_VERSION;

            // The record header (kind + base hash + a delta's raw target length) is read
            // straight from the pack's mmap, not a fresh file read — needed unconditionally now
            // (not skipped under `redelta` the way it used to be): a delta's *on-disk* length is
            // its payload size, which for a well-delta'd store is small and essentially
            // unrelated to the object's real size, so using it as the largest-first sort key
            // (below) would pair objects by how well they happened to already compress, not by
            // how big they are — scrambling the very order the size-window fallback depends on.
            let header = read_record_header(pack, offset, length, framed)?;

            // A delta whose base is not itself live cannot be copied (the base is being
            // dropped); reconstruct and re-delta it. Everything else is copied as-is — unless
            // `redelta` asked for every live object to be a recompression candidate, in which
            // case always reconstruct.
            let base_dropped = header.is_delta && header.base.is_some_and(|b| !live.contains(&sign_utils::to_hex(&b)));
            let source = if redelta || base_dropped {
                Source::Reconstruct
            } else {
                Source::CopyRecord { pack_index, offset, len: length, framed, is_delta: header.is_delta }
            };

            // A delta's raw target length is free (it is stored in cleartext right after the
            // base hash — see `read_record_header`); a full record has no such field without
            // decompressing it, so its on-disk (compressed) length is the best cheap proxy —
            // the same one a loose object's file size already stands in for.
            let size = header.raw_len.unwrap_or(length);

            targets.push(PackTarget { hash, size, source });
        }
    }

    Ok(CollectedTargets { targets, old_packs, source_packs: packs })
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
/// `PackWriter::append_delta` writes). A version-1 (unframed) record is always a full object.
fn read_record_header(pack: &LoadedPack, offset: u64, len: u64, framed: bool) -> Result<RecordHeader, String> {
    if !framed {
        return Ok(RecordHeader { is_delta: false, base: None, raw_len: None });
    }

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
    // is only checked for index-level consistency (`validate_index_records` — offsets/lengths in
    // bounds, exactly covering the data file, no per-record shape check), not for whether *this*
    // record's declared length is enough for the kind byte says it is. A transport-imported pack
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

/// A live record carried verbatim into the new pack, framed for a version-2 pack: a version-2
/// record is the source pack's mmap slice as-is (it already carries its kind byte); a version-1
/// record (a bare zstd blob) is wrapped in a `RECORD_FULL` kind byte. Borrows straight from the
/// source pack's mmap — no `open`/`seek`/`read`, and for the common framed case no copy at all;
/// the bytes are byte-identical to what a fresh file read produced, so the pack output is unchanged.
fn framed_record(pack: &LoadedPack, offset: u64, len: u64, framed: bool) -> Result<std::borrow::Cow<'_, [u8]>, String> {
    let bytes = pack.slice(offset, len)?;
    if framed {
        Ok(std::borrow::Cow::Borrowed(bytes))
    } else {
        let mut record = Vec::with_capacity(1 + bytes.len());
        record.push(RECORD_FULL);
        record.extend_from_slice(bytes);
        Ok(std::borrow::Cow::Owned(record))
    }
}

/// The first pack (if any) among `packs` that holds `hash`, with its record's `(offset, length)`
/// — the same "which pack, if any" search `retrieve_from_packs`/`is_in_packs` do, factored out
/// for the depth ledger below, which needs the pack itself (to read the record's header) rather
/// than its resolved bytes.
fn locate_in_packs<'a>(hash: &[u8; HASH_LEN], packs: &'a [LoadedPack]) -> Option<(&'a LoadedPack, u64, u64)> {
    packs.iter().find_map(|pack| pack.locate(hash).map(|(offset, length)| (pack, offset, length)))
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

        let framed = pack.version >= FIRST_FRAMED_VERSION;
        let header = read_record_header(pack, offset, length, framed)?;

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
        if prefix.len() != file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS
            || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
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
    /// *different* name and is written fresh rather than overwriting this pair, so the only
    /// remaining same-name rewrite is a byte-identical idempotent repack — harmless in either
    /// order, while index-before-data would break the load-bearing new-pack invariant above.
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

/// Build the on-disk index bytes: header, then the (already sorted) records.
fn build_index_bytes(records: &[([u8; HASH_LEN], u64, u64)]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(INDEX_HEADER_LEN + records.len() * INDEX_RECORD_LEN);

    bytes.extend_from_slice(PACK_INDEX_MAGIC);
    bytes.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
    bytes.extend_from_slice(&(records.len() as u32).to_le_bytes());

    for (hash, offset, length) in records {
        bytes.extend_from_slice(hash);
        bytes.extend_from_slice(&offset.to_le_bytes());
        bytes.extend_from_slice(&length.to_le_bytes());
    }

    bytes
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
fn hash_to_bytes(hash: &str) -> Option<[u8; HASH_LEN]> {
    let bytes = sign_utils::from_hex(hash).ok()?;
    if bytes.len() != HASH_LEN {
        return None;
    }

    let mut array = [0u8; HASH_LEN];
    array.copy_from_slice(&bytes);
    Some(array)
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
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

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
        compact(false, false).unwrap();
        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), content);

        std::fs::remove_dir_all(&temp).ok();
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

        let stats = compact(false, false).unwrap();
        assert_eq!(stats.objects_packed, 3);
        assert_eq!(stats.packs_written, 1);
        assert_eq!(stats.loose_removed, 3);

        // The loose files are gone...
        for hash in &hashes {
            let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
            assert!(!Path::new(&folder).join(&file_name).exists(), "loose object should be removed after packing");
        }

        // ...but every object still reads back byte-for-byte from the packs.
        for (hash, content) in hashes.iter().zip(&contents) {
            assert!(is_in_packs(hash).unwrap(), "packed object should be found");
            assert_eq!(retrieve_from_packs(hash).unwrap().unwrap(), *content);
        }

        // A hash in no pack is a clean miss, not an error.
        let absent = blake3::hash(b"absent").to_hex().to_string();
        assert!(!is_in_packs(&absent).unwrap());
        assert!(retrieve_from_packs(&absent).unwrap().is_none());

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
        let error = match compact(false, false) {
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
        taint_utils::remove_taint_files(&forklift).unwrap();

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
        std::fs::write(taint_dir.join("taint-99999-0"), b"objects/zz/preexisting\nEND\n").unwrap();

        let error = match compact(false, false) {
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

        let stats = compact(false, false).unwrap();
        assert_eq!(stats.objects_packed, 30);
        assert!(stats.deltas > 0, "similar objects should be stored as deltas (got {})", stats.deltas);

        // Every version reconstructs byte-for-byte from the packs — through its delta chain,
        // whose base is fetched recursively from the store.
        for (hash, content) in hashes.iter().zip(&contents) {
            assert_eq!(retrieve_from_packs(hash).unwrap().unwrap(), *content, "a delta must reconstruct exactly");
        }

        // And the deltas actually shrank the store far below storing every version in full
        // (a body of high-entropy bytes barely compresses on its own, so this is all delta).
        assert!(stats.bytes_packed < full_size / 3,
            "deltas should shrink the store: packed {} vs full {}", stats.bytes_packed, full_size);

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_version_1_pack_is_still_read() {
        // A version-1 pack (phase 1) stored each record as a bare zstd blob with no kind
        // byte. The current (framed) reader must still read one, or upgrading would strand
        // objects packed by an earlier build.
        let temp = std::env::temp_dir().join(format!("forklift-pack-v1-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        let pack_folder = temp.join(".forklift/objects/pack");
        std::fs::create_dir_all(&pack_folder).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = vec![9u8; 3000];
        let hash = blake3::hash(&content).to_hex().to_string();
        let hash_bytes = hash_to_bytes(&hash).unwrap();
        let compressed = zstd::encode_all(content.as_slice(), 0).unwrap();

        // v1 data: header (magic + version 1) then the bare zstd blob — no kind byte.
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&1u32.to_le_bytes());
        let offset = data.len() as u64;
        data.extend_from_slice(&compressed);

        // v1 index: header (magic + version 1 + count) then one (hash, offset, len) record.
        let mut index = Vec::new();
        index.extend_from_slice(PACK_INDEX_MAGIC);
        index.extend_from_slice(&1u32.to_le_bytes());
        index.extend_from_slice(&1u32.to_le_bytes());
        index.extend_from_slice(&hash_bytes);
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&(compressed.len() as u64).to_le_bytes());

        std::fs::write(pack_folder.join("legacy.pack"), &data).unwrap();
        std::fs::write(pack_folder.join("legacy.idx"), &index).unwrap();
        invalidate_cache();

        // The framed reader reads the unframed v1 record transparently.
        assert!(is_in_packs(&hash).unwrap());
        assert_eq!(retrieve_from_packs(&hash).unwrap().unwrap(), content);

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

        // Framed (v2) data: header, then one RECORD_FULL whose payload is B's blob.
        let mut data = Vec::new();
        data.extend_from_slice(PACK_DATA_MAGIC);
        data.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        let offset = data.len() as u64;
        data.push(RECORD_FULL);
        data.extend_from_slice(&blob_b);
        let length = 1 + blob_b.len() as u64;

        // Index: header (magic + version + count), then one (hash A, offset, length) record.
        let mut index = Vec::new();
        index.extend_from_slice(PACK_INDEX_MAGIC);
        index.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        index.extend_from_slice(&1u32.to_le_bytes());
        index.extend_from_slice(&hash_a_bytes);
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&length.to_le_bytes());

        std::fs::write(pack_folder.join("corrupt.pack"), &data).unwrap();
        std::fs::write(pack_folder.join("corrupt.idx"), &index).unwrap();
        invalidate_cache();

        // The record is present and decompresses fine, but to the wrong bytes — the read fails
        // instead of returning content B under hash A.
        assert!(is_in_packs(&hash_a).unwrap(), "the record is indexed under hash A");
        let result = retrieve_from_packs(&hash_a);
        assert!(result.is_err(), "a record decompressing to the wrong bytes must fail the read, got {:?}", result);
        assert!(result.unwrap_err().contains("corrupt"), "the error should name the corruption");

        std::fs::remove_dir_all(&temp).ok();
    }

    #[test]
    fn a_short_delta_record_is_rejected_not_panicked() {
        // Index-level validation (`validate_index_records`) only checks that offsets/lengths are
        // in bounds and exactly cover the data file — it has no notion of what a record's *kind*
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

        // Framed (v2) data: header, then one RECORD_DELTA record with only 2 bytes after the
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
        let mut index = Vec::new();
        index.extend_from_slice(PACK_INDEX_MAGIC);
        index.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        index.extend_from_slice(&1u32.to_le_bytes());
        index.extend_from_slice(&[0u8; HASH_LEN]);
        index.extend_from_slice(&offset.to_le_bytes());
        index.extend_from_slice(&length.to_le_bytes());

        let data_path = pack_folder.join("short-delta.pack");
        let index_path = pack_folder.join("short-delta.idx");
        std::fs::write(&data_path, &data).unwrap();
        std::fs::write(&index_path, &index).unwrap();

        let pack = load_pack_pair(&data_path, &index_path).expect("index-level validation must accept this pack");

        let result = read_record_header(&pack, offset, length, true);
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

        // Index: header, then every record sorted by hash (`validate_index_records` requires it).
        let mut sorted = records.clone();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));

        let mut index = Vec::new();
        index.extend_from_slice(PACK_INDEX_MAGIC);
        index.extend_from_slice(&PACK_FORMAT_VERSION.to_le_bytes());
        index.extend_from_slice(&(sorted.len() as u32).to_le_bytes());
        for (hash, offset, length) in &sorted {
            index.extend_from_slice(hash);
            index.extend_from_slice(&offset.to_le_bytes());
            index.extend_from_slice(&length.to_le_bytes());
        }

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
        compact(false, false).unwrap();

        // Simulate the stale peer: poison this process's registry back to "no packs" even though
        // the pack is on disk and the loose file is gone. A naive read now misses on both paths.
        registry().lock().expect("registry lock")
            .insert(file_utils::get_path_objects_root(), Arc::new(Vec::new()));

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
        assert!(compact(false, false).is_err(), "compact must refuse while the shared store lock is held");
        drop(held);
        assert!(compact(false, false).is_ok(), "compact runs once the store lock is free");

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

        // Two separate incremental packs, so the repack has more than one old pack to supersede.
        let content_a = b"first pack content".to_vec();
        store_loose(&blake3::hash(&content_a).to_hex().to_string(), &content_a);
        compact(false, false).unwrap();

        let content_b = b"second pack content".to_vec();
        store_loose(&blake3::hash(&content_b).to_hex().to_string(), &content_b);
        compact(false, false).unwrap();

        let pack_paths_before: Vec<PathBuf> = std::fs::read_dir(pack_folder()).unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some(PACK_DATA_EXTENSION))
            .collect();
        assert_eq!(pack_paths_before.len(), 2, "two incremental packs should exist before the repack");

        // No pallet refs exist in this bare store, so the live set is empty: a repack must drop
        // every packed object as garbage and remove both old packs entirely.
        let stats = compact(true, false).unwrap();
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
}
