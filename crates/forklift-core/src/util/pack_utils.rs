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
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::util::{
    audit_utils, bundle_utils, byte_utils, delta_utils, file_utils, graph_utils, lock_utils,
    object_utils, pallet_utils, sign_utils,
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
/// same bound bundles use (`bundle_utils::MAX_DELTA_CHAIN`).
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

        let index = std::fs::read(&path)
            .map_err(|e| format!("Error while reading pack index \"{}\": {}", path.to_string_lossy(), e))?;
        let (count, version) = parse_index_header(&index, &path)?;

        let data_path = path.with_extension(PACK_DATA_EXTENSION);
        let file = std::fs::File::open(&data_path)
            .map_err(|e| format!("Error while opening pack data \"{}\": {}", data_path.to_string_lossy(), e))?;
        // SAFETY: a pack data file is immutable for its whole life — written once under a
        // temporary name, fsynced, atomically renamed into place, and thereafter only ever
        // deleted whole (never modified or truncated). So the mapped bytes cannot change or
        // shrink under us, which is the invariant `Mmap` requires.
        let data = unsafe { memmap2::Mmap::map(&file) }
            .map_err(|e| format!("Error while mapping pack data \"{}\": {}", data_path.to_string_lossy(), e))?;

        packs.push(LoadedPack {
            data_path,
            data,
            index,
            count,
            version,
        });
    }

    Ok(packs)
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
/// Chains are bounded far below this at write time (`MAX_DELTA_CHAIN`, an approximate bound
/// since the path walk restarts recorded depth on repeats — real chains can run a small
/// multiple of it), so this is only a backstop against a corrupt or adversarial pack that
/// chains without end: it turns unbounded recursion (a crash) into a clean error.
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

/// Read `length` bytes at `offset` from a pack data file.
fn read_pack_slice(data_path: &Path, offset: u64, length: u64) -> Result<Vec<u8>, String> {
    let mut file = std::fs::File::open(data_path)
        .map_err(|e| format!("Error while opening pack \"{}\": {}", data_path.to_string_lossy(), e))?;

    file.seek(SeekFrom::Start(offset))
        .map_err(|e| format!("Error while seeking in pack \"{}\": {}", data_path.to_string_lossy(), e))?;

    let mut buffer = vec![0u8; length as usize];
    file.read_exact(&mut buffer)
        .map_err(|e| format!("Error while reading from pack \"{}\": {}", data_path.to_string_lossy(), e))?;

    Ok(buffer)
}

/// Where an object to pack comes from, and how to pack it.
enum Source {
    /// A loose file: read it, delta it (path-aware / window), and delete it once packed.
    Loose(PathBuf),
    /// A record already in a pack whose delta base survives the repack — **copied verbatim**,
    /// never reconstructed or re-deltated, so the original (good) delta is preserved and the
    /// repack stays a byte-copy. `framed` is false for a version-1 (unframed) record, which is
    /// wrapped in a full-record kind byte on the way into the version-2 pack.
    CopyRecord { data_path: PathBuf, offset: u64, len: u64, framed: bool, is_delta: bool },
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
/// Signature sidecars (`.sig`) and temp files are left alone (sidecars are read by path).
/// Objects are packed largest-first and offered as a **delta** — against the previous version
/// of the same file (path-aware) or, failing that, a small size window — kept only when
/// smaller than the full blob. Packs are written durably before any original is removed, so a
/// failure never loses an object.
///
/// # Arguments
/// * `all` - Repack existing packs too (drop packed garbage, consolidate), not just the loose set.
///
/// # Returns
/// * `Ok(CompactStats)` - What was packed and removed.
/// * `Err(String)`      - If enumeration, writing, or deletion failed.
pub fn compact(all: bool) -> Result<CompactStats, String> {
    let pack_folder = pack_folder();
    file_utils::create_folder_if_not_exists(&pack_folder)?;

    // Serialize destructive store maintenance across bays *and* processes: the object store is
    // shared (`forklift_root`), but a command's bay lock is not, so without this two bays could
    // enumerate the same loose set and race each other's deletions. Held for the whole run;
    // errors immediately if another compaction holds it — an explicit `compact` surfaces that,
    // auto-maintenance (which ignores compaction errors) simply skips the now-redundant work.
    // Taken after the folder exists so its parent (`forklift_root`) is present for `create_new`.
    let _store_lock = lock_utils::StoreLock::acquire()?;

    // The objects to pack, and the old pack files a repack supersedes. Largest-first so the
    // fallback window holds similar-sized neighbours (git's heuristic).
    let (mut targets, old_packs) = collect_targets(all)?;
    // Largest first (the delta/window heuristic), with the object hash as a total tie-breaker so
    // the packing order — and therefore every record's offset — is deterministic. Without it,
    // equal-size objects kept their filesystem-enumeration order, so two repacks of the *same
    // already-packed* live set produced different layouts every run; harmless under the old id
    // (which hashed only the object set) but, now that the pack id folds in offsets/lengths (D5),
    // this determinism is what stops a steady-state repack from churning the pack onto a fresh
    // name (rewrite + delete) each run and lets it land on the very same name instead.
    targets.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.hash.cmp(&b.hash)));

    // Path-aware base selection (phase 2b) is only needed to *build* new deltas — for loose
    // objects and for the rare object whose base is dropped. A repack that only copies existing
    // records (the common case) skips the whole DAG walk.
    let needs_delta = targets.iter().any(|t| !matches!(t.source, Source::CopyRecord { .. }));
    let path_bases = if needs_delta { compute_path_bases()? } else { HashMap::new() };

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
        let mut prepared = prepare_batch(batch, &path_bases)?;
        start = end;

        for (i, target) in batch.iter().enumerate() {
            let pack = match writer.as_mut() {
                Some(pack) => pack,
                None => writer.insert(PackWriter::new(&pack_folder)?),
            };

            // Copy an existing record verbatim — a repack's fast path: the original (good) delta
            // is preserved, nothing is reconstructed or re-deltated.
            if let Source::CopyRecord { data_path, offset, len, framed, is_delta } = &target.source {
                let record = read_framed_record(data_path, *offset, *len, *framed)?;
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
            //    it beats the full blob.
            let mut window_depth = 0;
            if !path_delta {
                let best = if prep.deltable {
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
                    }
                    _ => {
                        let written = pack.append_full(target.hash, &prep.compressed, loose_path.clone())?;
                        stats.bytes_packed += written;
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
            // seed it either (nothing should delta against a parcel).
            if !path_delta && prep.deltable {
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

    // Each pack's data and index bytes were fsynced in `finalize`, but the *directory entries*
    // that the renames created are themselves only durable once the pack folder is fsynced. Do it
    // once here (a single sync covers every rename this run made) before anything is deleted, so a
    // power loss between the sweep below and that metadata reaching disk cannot lose a pack whose
    // loose sources are already gone — the "durable" half of durable-before-destructive.
    if !new_pack_files.is_empty() {
        file_utils::sync_dir(&pack_folder)?;
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
                 path_bases: &HashMap<String, String>) -> Result<Vec<Option<Prepared>>, String> {
    // Below this many objects the threads cost more than the reads/compressions they share.
    const PARALLEL_THRESHOLD: usize = 8;

    let to_prepare = batch.iter()
        .filter(|target| !matches!(target.source, Source::CopyRecord { .. }))
        .count();

    if to_prepare < PARALLEL_THRESHOLD {
        return batch.iter().map(|target| prepare_target(target, path_bases)).collect();
    }

    let workers = num_cpus::get().max(1).min(batch.len());
    let chunk = batch.len().div_ceil(workers);

    // Storage-root scopes are thread-local and not inherited by spawned threads; capture the
    // caller's so each worker resolves its object reads (of the delta bases) under the same
    // warehouse root.
    let scope_root = crate::globals::current_scope_root();

    std::thread::scope(|scope| {
        let handles: Vec<_> = batch
            .chunks(chunk)
            .map(|slice| {
                let scope_root = scope_root.as_deref();
                scope.spawn(move || {
                    let _scope = scope_root.map(crate::globals::StorageRootScope::enter);

                    slice.iter()
                        .map(|target| prepare_target(target, path_bases))
                        .collect::<Vec<Result<Option<Prepared>, String>>>()
                })
            })
            .collect();

        handles.into_iter()
            .flat_map(|handle| handle.join().expect("a compaction worker panicked"))
            .collect()
    })
}

/// Read one target and compute its path delta if one wins — the body of the parallel prep.
/// A `CopyRecord` is `None` (it is copied verbatim on the sequential write path). The window
/// fallback is *not* done here: it depends on the objects written just before, so it stays on
/// the sequential path.
fn prepare_target(target: &PackTarget,
                  path_bases: &HashMap<String, String>) -> Result<Option<Prepared>, String> {
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
        if let Some(base_hex) = path_bases.get(&sign_utils::to_hex(&target.hash)) {
            // Borrow-only (the delta is computed against it): share the cached `Arc` rather than
            // copy the base blob out under the read-cache lock.
            let base_raw = file_utils::retrieve_object_by_hash_shared(base_hex)?;
            let payload = delta_utils::compress_delta(&base_raw, &raw)?;

            if payload.len() < compressed.len() {
                let base = hash_to_bytes(base_hex)
                    .ok_or_else(|| format!("Path base {} is not a valid hash.", base_hex))?;
                path_delta = Some((base, payload));
            }
        }
    }

    Ok(Some(Prepared { raw, compressed, deltable, path_delta }))
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
fn collect_targets(all: bool) -> Result<(Vec<PackTarget>, Vec<PathBuf>), String> {
    if !all {
        return Ok((enumerate_loose_objects()?, Vec::new()));
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
    // packs are removed at the end, which is what drops the garbage that is not carried over.
    let packs = loaded_packs()?;
    let mut old_packs = Vec::new();

    for pack in packs.iter() {
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

            // A delta whose base is not itself live cannot be copied (the base is being
            // dropped); reconstruct and re-delta it. Everything else is copied as-is.
            let (is_delta, base) = read_record_header(&pack.data_path, offset, framed)?;
            let source = if is_delta && base.is_some_and(|b| !live.contains(&sign_utils::to_hex(&b))) {
                Source::Reconstruct
            } else {
                Source::CopyRecord { data_path: pack.data_path.clone(), offset, len: length, framed, is_delta }
            };

            targets.push(PackTarget { hash, size: length, source });
        }
    }

    Ok((targets, old_packs))
}

/// Read a record's kind and (for a delta) its base hash, without reconstructing it — just the
/// leading kind byte and, for a delta, the 32-byte base that follows. A version-1 (unframed)
/// record is always a full object.
fn read_record_header(data_path: &Path, offset: u64, framed: bool) -> Result<(bool, Option<[u8; HASH_LEN]>), String> {
    if !framed {
        return Ok((false, None));
    }

    let kind = read_pack_slice(data_path, offset, 1)?[0];
    if kind != RECORD_DELTA {
        return Ok((false, None));
    }

    let base = read_pack_slice(data_path, offset + 1, HASH_LEN as u64)?;
    let mut array = [0u8; HASH_LEN];
    array.copy_from_slice(&base);
    Ok((true, Some(array)))
}

/// Read a record and return it framed for a version-2 pack: a version-2 record is copied
/// verbatim (it already carries its kind byte); a version-1 record (a bare zstd blob) is
/// wrapped in a `RECORD_FULL` kind byte.
fn read_framed_record(data_path: &Path, offset: u64, len: u64, framed: bool) -> Result<Vec<u8>, String> {
    let bytes = read_pack_slice(data_path, offset, len)?;
    if framed {
        Ok(bytes)
    } else {
        let mut record = Vec::with_capacity(1 + bytes.len());
        record.push(RECORD_FULL);
        record.extend_from_slice(&bytes);
        Ok(record)
    }
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

/// Path-aware base selection (phase 2b): for every reachable blob, the previous version of
/// the *same file* (by path) as its delta base — the ideal base git's name-sorted packer
/// picks, which the size heuristic can only approximate. Returns `blob hash → base hash`
/// (both hex); an object with no entry (a tree, a parcel, a first version, or an unreachable
/// blob) has no path base and falls back to the size window.
///
/// This walks the reachable DAG — all parcels and their trees, but never blob *content* —
/// mirroring the bundle traversal (`bundle_utils`), and bounds each chain to `MAX_DELTA_CHAIN`
/// so reconstruction recursion stays bounded. Its "seen trees" and "seen blobs" sets are Bloom
/// filters, so the walk's memory is bounded (a bit budget) rather than one entry per object —
/// what keeps it viable at kernel scale (see [`Bloom`]).
fn compute_path_bases() -> Result<HashMap<String, String>, String> {
    let heads: Vec<String> = pallet_utils::all_pallet_refs()?
        .into_iter().map(|(_, head)| head).collect();

    let reachable = audit_utils::collect_reachable(&heads)?;
    // Oldest first, so a file's earlier version is visited before the later version that
    // will delta against it.
    let order = bundle_utils::topo_order_oldest_first(&reachable)?;

    // A repo averages several objects (trees + blobs) per parcel; size the Bloom filters from
    // that estimate so they are bounded and roughly right for both small and huge histories.
    let estimate = reachable.len().saturating_mul(5);
    let mut seen_trees = Bloom::new(estimate);
    let mut seen_blobs = Bloom::new(estimate);
    // Bounded by the number of distinct paths (not by history depth); carries each path's
    // latest blob and that blob's chain depth (so `depth_of` need not be a per-object map).
    let mut latest_at_path: HashMap<String, (String, u32)> = HashMap::new();
    let mut base_of: HashMap<String, String> = HashMap::new();

    for parcel_hash in &order {
        let tree_hash = object_utils::load_parcel(parcel_hash)?.tree_hash;
        walk_tree_for_bases(&tree_hash, "", &mut seen_trees, &mut seen_blobs,
                            &mut latest_at_path, &mut base_of)?;
    }

    Ok(base_of)
}

/// Walk one tree's closure, recording each blob's path base (see [`compute_path_bases`]).
/// Deduplicated by tree hash: an identical (unchanged) subtree carries no new blob versions,
/// so it is skipped — the same optimisation the bundle walk makes. (A Bloom false positive
/// skips a tree that was not in fact seen; its blobs then fall back to the size window.)
fn walk_tree_for_bases(tree_hash: &str,
                       path_prefix: &str,
                       seen_trees: &mut Bloom,
                       seen_blobs: &mut Bloom,
                       latest_at_path: &mut HashMap<String, (String, u32)>,
                       base_of: &mut HashMap<String, String>) -> Result<(), String> {
    if seen_trees.contains(tree_hash.as_bytes()) {
        return Ok(());
    }
    seen_trees.insert(tree_hash.as_bytes());

    let tree = object_utils::load_tree(tree_hash)?;

    for (name, file) in tree.get_files() {
        let path = join_path(path_prefix, name);
        record_path_base(&file.hash, &path, seen_blobs, latest_at_path, base_of);
    }

    for (name, subtree) in tree.get_subtrees() {
        let child = join_path(path_prefix, name);
        walk_tree_for_bases(&subtree.hash, &child, seen_trees, seen_blobs, latest_at_path, base_of)?;
    }

    Ok(())
}

/// Record a blob's path base: the most recent blob seen at this path (if its chain is not yet
/// at the limit), and update the latest-at-path so the next version chains from this one.
fn record_path_base(blob_hash: &str,
                    path: &str,
                    seen_blobs: &mut Bloom,
                    latest_at_path: &mut HashMap<String, (String, u32)>,
                    base_of: &mut HashMap<String, String>) {
    // First time this blob is seen fixes its base; a later appearance (or a Bloom false
    // positive) only advances the path. Its real chain depth is not tracked per object (that
    // would defeat the bounded-memory point), so the recorded depth restarts at 0 here — which
    // makes the `MAX_DELTA_CHAIN` bound approximate (a real chain can run a small multiple of
    // it). That is safe: base pointers stay acyclic so reconstruction always terminates, and
    // `MAX_RECONSTRUCT_DEPTH` is the hard backstop.
    if seen_blobs.contains(blob_hash.as_bytes()) {
        latest_at_path.insert(path.to_string(), (blob_hash.to_string(), 0));
        return;
    }
    seen_blobs.insert(blob_hash.as_bytes());

    let mut depth = 0;

    if let Some((base, base_depth)) = latest_at_path.get(path) {
        if *base_depth < MAX_DELTA_CHAIN && base != blob_hash {
            base_of.insert(blob_hash.to_string(), base.clone());
            depth = base_depth + 1;
        }
    }

    latest_at_path.insert(path.to_string(), (blob_hash.to_string(), depth));
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
    /// see an index with no data). This is why the D5 fix is the layout-derived id alone and
    /// not the review's floated index-before-data reorder: a layout-derived id (see
    /// `compute_pack_id`) means a differently-laid-out pack of the same object set gets a
    /// *different* name and is written fresh rather than overwriting this pair, so the only
    /// remaining same-name rewrite is a byte-identical idempotent repack — harmless in either
    /// order, while index-before-data would break the load-bearing new-pack invariant above.
    /// Returns the loose files this pack now holds (the caller deletes them once **every** pack
    /// is durable) and this pack's two final paths (so a repack never deletes, as an "old"
    /// pack, a file a new pack was just written to — an idempotent repack lands on that name).
    fn finalize(mut self) -> Result<Finalized, String> {
        self.data_writer.flush().map_err(|e| format!("Error while flushing pack: {}", e))?;
        self.data_writer.get_ref().sync_all()
            .map_err(|e| format!("Error while syncing pack: {}", e))?;

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
/// length** — so the name is derived from the on-disk *layout*, not just the object set (D5).
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

/// Write a file and fsync it (used for the index, which must be durable before rename).
fn write_and_sync(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = std::fs::File::create(path)
        .map_err(|e| format!("Error while creating \"{}\": {}", path.to_string_lossy(), e))?;
    file.write_all(bytes)
        .map_err(|e| format!("Error while writing \"{}\": {}", path.to_string_lossy(), e))?;
    file.sync_all()
        .map_err(|e| format!("Error while syncing \"{}\": {}", path.to_string_lossy(), e))
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
        // D5: the id must fold in each record's offset and length, so two packs holding the
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
        compact(false).unwrap();
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

        let stats = compact(false).unwrap();
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

        let stats = compact(false).unwrap();
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
        // The silent-corruption case D1 guards against: a pack whose record decompresses
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
    fn a_read_reloads_the_pack_registry_when_an_external_compact_moved_the_object() {
        // D3: a long-running process (a live server) whose cached pack registry predates an
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
        compact(false).unwrap();

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
        // D4: compact serializes on the shared store lock, so a second compaction (another bay or
        // process) cannot race its deletions.
        let temp = std::env::temp_dir().join(format!("forklift-compact-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        let content = b"content to compact".to_vec();
        store_loose(&blake3::hash(&content).to_hex().to_string(), &content);

        let held = lock_utils::StoreLock::acquire().expect("hold the store lock");
        assert!(compact(false).is_err(), "compact must refuse while the shared store lock is held");
        drop(held);
        assert!(compact(false).is_ok(), "compact runs once the store lock is free");

        std::fs::remove_dir_all(&temp).ok();
    }
}
