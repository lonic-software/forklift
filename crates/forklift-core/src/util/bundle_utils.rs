//! Bundles — a transport envelope around native indexed packs
//! (`docs/format/BUNDLE_FORMAT.md`). A bundle is a clone-time optimization, never a source of
//! truth: every packed object is reconstructed and hash-verified in quarantine before any pack
//! is published, and anything a bundle lacks is fetched through the incremental object path.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use crate::error::CoreError;
use crate::globals::forklift_root;
use crate::model::remote::MAX_MISSING_BATCH;
use crate::model::tree_item::TreeItem;
use crate::util::{
    audit_utils, delta_utils, fanout_utils, file_utils, object_utils, pack_utils, pallet_utils,
    scope_utils, sign_utils,
};

/// The uncompressed ASCII header line (without the newline) that opens every whole-warehouse
/// bundle this build writes. Version 4 made the object payload one or more native `.pack`/`.idx`
/// pairs: transport bytes become the client's permanent indexed store instead of being exploded
/// into one durably-synced loose file per object. Version 5 ends the file with a digest of every
/// byte before it ([`BUNDLE_DIGEST_BYTES`]), so a file assembled from two downloads is refused
/// before import stores anything.
pub const BUNDLE_HEADER: &str = "forklift-bundle 2026-10-25";

/// The length of the blake3 digest that ends a whole-warehouse bundle: the digest of every byte
/// before it, the header line included.
pub const BUNDLE_DIGEST_BYTES: usize = 32;

/// Version 3: one outer zstd stream of `O` records. The format of partial bundles
/// (`objects/batch`, subtree fetches) only, which are imported as they stream in, each record
/// admitted against what the client asked for (see [`StreamAdmission`]). A whole-warehouse bundle
/// file in this format is no longer read: nothing writes one.
const BUNDLE_HEADER_V3: &str = "forklift-bundle 2026-07-11";

/// The longest a bundle's opening header line may run before a newline, before the import is
/// refused outright rather than searching further. Every header this build recognizes
/// (`BUNDLE_HEADER` and `BUNDLE_HEADER_V3`) is under 30 bytes; 128 leaves headroom for a
/// longer future header while staying a small, fixed bound — a hostile bundle with no newline (or
/// a very long run before one) cannot grow the header buffer past it, regardless of how long the
/// underlying stream actually is.
const MAX_HEADER_BYTES: usize = 128;

/// The record kind of a raw (full) object.
const KIND_OBJECT: u8 = b'O';

/// The longest delta chain the builder forms at one path: after this many successive
/// deltas, the next version is stored in full. This caps both the reconstruction cost of a
/// blob and the blast radius of a missing base (git's pack chain limit is the same order).
const MAX_DELTA_CHAIN: u32 = 50;

/// How many records the build's workers may load ahead of the one being appended, and the
/// compressed bytes held ahead at which they pause (see [`fanout_utils::prefetch_in_order`]). The
/// same bounds `shift` prefetches blobs under: deep enough to keep every core busy, small next to
/// the build's own working set.
const EMIT_WINDOW: usize = 256;
const EMIT_BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// How many parcels the build's workers may load trees for ahead of the walk, the entries of the
/// parsed trees held ahead at which they pause, and the most trees loaded for any one parcel (the
/// walk reads the rest itself). A parcel usually changes a handful of trees, so the window binds
/// first; the caps bound a run of large merges or wide directories, and a first parcel over a
/// tree of many directories.
const PLAN_WINDOW: usize = 256;
const PLAN_BUDGET_ENTRIES: usize = 256 * 1024;
const PLAN_PARCEL_TREES: usize = 1024;

#[cfg(test)]
thread_local! {
    /// How many trees the plan walk read itself because no worker had loaded them.
    static PLAN_WALK_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The folder (inside the forklift root) where bundles live.
pub(crate) const FOLDER_NAME_BUNDLES: &str = "bundles";

/// The file name of the most recent bundle.
pub(crate) const FILE_NAME_LATEST: &str = "latest";

/// The file name of a remote's bundle while it is downloaded and imported: kept across franchise
/// attempts, and published as `latest` only once imported.
pub(crate) const FILE_NAME_DOWNLOAD: &str = "partial";

/// Stable discriminator for the one bundle failure on which a clone can safely fall back to its
/// ordinary incremental-object walk. Corruption in a *known* format remains fatal; silently
/// treating that as "no bundle" would hide a broken or hostile server artifact.
const UNKNOWN_BUNDLE_HEADER_PREFIX: &str = "Unknown bundle header ";

/// Stable discriminator for a bundle file whose bytes are not the ones its builder wrote: its
/// digest does not match, or it ends before one. Nothing of such a file has been stored.
const BUNDLE_DIGEST_PREFIX: &str = "The bundle does not match its digest";

/// Stable discriminators for a file whose header could not be read as any bundle's.
const NOT_A_BUNDLE_PREFIX: &str = "This does not look like a forklift bundle";
const BUNDLE_HEADER_READ_PREFIX: &str = "Error while reading the bundle header";

/// What a bundle build packed.
pub struct BuildStats {
    /// Objects stored in full (`'O'` records).
    pub objects: usize,

    /// Blob versions stored as a delta against an earlier version (pack delta entries).
    pub deltas: usize,

    pub signatures: usize,
    pub path: PathBuf,
}

/// What a bundle import actually stored (records already present are skipped).
#[derive(Debug, Default)]
pub struct ImportStats {
    pub stored_objects: usize,
    pub stored_signatures: usize,
    pub skipped_records: usize,
    /// Whether this import installed native packs straight from the bundle (only the whole-
    /// warehouse native format does this — see `import_native_bundle_reader`). `franchise` uses
    /// this to gate its densify suggestion: those packs are exactly the far end's own bulk-
    /// ingest shape, never having seen cross-path similarity.
    pub installed_packs: bool,
}

/// The path of the latest bundle of the current warehouse.
pub fn get_latest_bundle_path() -> PathBuf {
    forklift_root()
        .join(FOLDER_NAME_BUNDLES)
        .join(FILE_NAME_LATEST)
}

/// The path a remote's bundle is downloaded to in the current warehouse (see
/// [`publish_bundle_download`]).
pub fn get_bundle_download_path() -> PathBuf {
    forklift_root()
        .join(FOLDER_NAME_BUNDLES)
        .join(FILE_NAME_DOWNLOAD)
}

/// Remove the current warehouse's bundle download, if there is one, without following it.
pub fn discard_bundle_download() -> Result<(), String> {
    file_utils::remove_unfollowed(&get_bundle_download_path())
}

/// Publish the current warehouse's imported bundle download as its own latest bundle, which
/// `forklift serve` hands out as it is. The rename is made durable like any other rename into the
/// store: a folder sync that fails records a taint instead of leaving it unproven.
pub fn publish_bundle_download() -> Result<(), String> {
    let download = get_bundle_download_path();
    let latest = get_latest_bundle_path();

    file_utils::rename(&download, &latest).map_err(|e| format!(
        "Error while moving the bundle into place at \"{}\": {}", latest.to_string_lossy(), e
    ))?;

    match latest.parent() {
        Some(parent) => file_utils::sync_dir_or_taint(parent, &[&latest]),
        None => Ok(()),
    }
}

/// Whether an import failure means the file's header could not be read as a bundle's in any
/// version, or names one this build does not read. Nothing of such a file was stored.
pub fn is_bundle_header_error(error: &str) -> bool {
    is_unsupported_bundle_error(error)
        || error.starts_with(NOT_A_BUNDLE_PREFIX)
        || error.starts_with(BUNDLE_HEADER_READ_PREFIX)
}

/// Whether an import failure means only that this build does not understand the envelope version.
/// Clone callers use this narrow case to skip the optimization and continue fetching verified
/// objects through the version-independent API.
pub fn is_unsupported_bundle_error(error: &str) -> bool {
    error.starts_with(UNKNOWN_BUNDLE_HEADER_PREFIX)
}

/// Whether an import failure means the file is not the bytes its builder wrote (see
/// [`BUNDLE_DIGEST_BYTES`]). The check runs before import writes anything, so nothing of the file
/// was stored.
pub fn is_bundle_digest_error(error: &str) -> bool {
    error.starts_with(BUNDLE_DIGEST_PREFIX)
}

/// Whether `prefix`, the first bytes of a file, opens a bundle in the version this build writes
/// and reads. A shorter prefix than the header line and its newline is not.
pub fn is_current_bundle_prefix(prefix: &[u8]) -> bool {
    prefix.strip_prefix(BUNDLE_HEADER.as_bytes())
        .is_some_and(|rest| rest.first() == Some(&b'\n'))
}

/// Build a bundle of the whole warehouse: every parcel reachable from any pallet head,
/// each parcel's signature sidecar, and the full tree/blob closure — each object once.
/// The bundle is written atomically to the `latest` path.
///
/// # Returns
/// * `Ok(BuildStats)` - What was packed, and where.
/// * `Err(String)`    - If an object could not be read or the bundle written.
pub fn build_bundle() -> Result<BuildStats, String> {
    let bundle_path = get_latest_bundle_path();

    if let Some(parent) = bundle_path.parent() {
        file_utils::create_folder_if_not_exists(parent)?;
        remove_stale_staging(parent);
    }

    let staging = TemporaryBundleDirectory::new(bundle_path.parent().unwrap_or(Path::new(".")))?;
    let mut pack_builder = pack_utils::TransportPackBuilder::new(&staging.path)?;
    let signature_path = staging.path.join("signatures");
    let signature_file = std::fs::File::create(&signature_path)
        .map_err(|e| format!("Error while creating bundle signature staging: {}", e))?;
    let mut signature_writer = std::io::BufWriter::new(signature_file);

    let mut stats = BuildStats { objects: 0, deltas: 0, signatures: 0, path: bundle_path.clone() };

    // Every parcel reachable from any pallet head — user *and* meta, so a bundle carries
    // the office (a franchise imports trust from it), then each parcel's closure.
    let mut heads: Vec<String> = Vec::new();

    for (_, head) in pallet_utils::all_pallet_refs()? {
        heads.push(head);
    }

    let parcels = audit_utils::collect_reachable(&heads)?;

    // Oldest first, so a file's earlier version is emitted before a later version that
    // deltas against it (the importer must have the base stored to reconstruct the delta).
    let order = topo_order_oldest_first(&parcels)?;

    let plan = plan_emission(&order, PLAN_PARCEL_TREES)?;
    let mut depths: Vec<u32> = vec![0; plan.len()];
    let mut next = 0usize;

    // Reading, verifying and compressing each record is independent of every other record, so it
    // runs on worker threads; appending stays in plan order on this thread, where the delta-chain
    // depth (which depends on earlier records' outcomes) is decided.
    fanout_utils::prefetch_in_order(
        &plan,
        EMIT_WINDOW,
        EMIT_BUDGET_BYTES,
        |item| load_emission(&plan, item),
        |loaded| loaded.as_ref().map_or(0, Loaded::size),
        |_, loaded| {
            let index = next;
            next += 1;
            write_emission(&mut pack_builder, &mut signature_writer, &plan, index, loaded?,
                           &mut depths, &mut stats)
        },
    )?;

    let artifacts = pack_builder.finish()?;
    signature_writer.flush()
        .map_err(|e| format!("Error while flushing bundle signature staging: {}", e))?;
    drop(signature_writer);
    write_native_bundle(&bundle_path, &artifacts, &signature_path, stats.signatures)?;

    Ok(stats)
}

/// Write the version-5 transport envelope around already-finalized native packs. The small length
/// table precedes the pack bytes so an importer can validate every declaration before streaming
/// any section to quarantine, and the digest of everything written ends the file. Pack data is
/// already internally compressed; wrapping it in another zstd stream would only prevent direct
/// installation and add CPU for no gain.
fn write_native_bundle(bundle_path: &Path,
                       artifacts: &[pack_utils::TransportPackArtifact],
                       signature_path: &Path,
                       signature_count: usize) -> Result<(), String> {
    if artifacts.len() > pack_utils::MAX_TRANSPORT_PACKS {
        return Err(format!(
            "Cannot bundle {} native packs: the format limit is {}.",
            artifacts.len(), pack_utils::MAX_TRANSPORT_PACKS
        ));
    }

    let temporary_path = unique_sibling_temp(bundle_path, "latest.bundle");
    let result = write_envelope_and_publish(
        &temporary_path, bundle_path, artifacts, signature_path, signature_count
    );
    if result.is_err() {
        // A failed write or rename must not strand a store-sized temp envelope until the stale
        // sweep reclaims it; on success the rename already consumed the temp name.
        let _ = std::fs::remove_file(&temporary_path);
    }
    result
}

/// Write the envelope to `temporary_path`, then atomically publish it at `bundle_path`.
fn write_envelope_and_publish(temporary_path: &Path,
                              bundle_path: &Path,
                              artifacts: &[pack_utils::TransportPackArtifact],
                              signature_path: &Path,
                              signature_count: usize) -> Result<(), String> {
    let file = std::fs::File::create(temporary_path)
        .map_err(|e| format!("Error while creating the bundle file: {}", e))?;
    let mut writer = DigestingWriter {
        inner: std::io::BufWriter::new(file),
        hasher: blake3::Hasher::new(),
    };

    writer.write_all(BUNDLE_HEADER.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .and_then(|_| writer.write_all(&(artifacts.len() as u32).to_be_bytes()))
        .map_err(|e| format!("Error while writing the bundle header: {}", e))?;

    for artifact in artifacts {
        let data_len = artifact.data_path.metadata()
            .map_err(|e| format!("Error while sizing native pack data: {}", e))?.len();
        let index_len = artifact.index_path.metadata()
            .map_err(|e| format!("Error while sizing native pack index: {}", e))?.len();
        writer.write_all(&data_len.to_be_bytes())
            .and_then(|_| writer.write_all(&index_len.to_be_bytes()))
            .map_err(|e| format!("Error while writing the bundle pack table: {}", e))?;
    }

    for artifact in artifacts {
        copy_file_into(&artifact.data_path, &mut writer, "native pack data")?;
        copy_file_into(&artifact.index_path, &mut writer, "native pack index")?;
    }

    writer.write_all(&(signature_count as u64).to_be_bytes())
        .map_err(|e| format!("Error while writing the bundle signature count: {}", e))?;
    copy_file_into(signature_path, &mut writer, "signature section")?;

    let DigestingWriter { inner: mut writer, hasher } = writer;
    writer.write_all(hasher.finalize().as_bytes())
        .map_err(|e| format!("Error while writing the bundle digest: {}", e))?;
    writer.flush().map_err(|e| format!("Error while flushing the bundle file: {}", e))?;
    if file_utils::fsync_enabled() {
        writer.get_ref().sync_all()
            .map_err(|e| format!("Error while syncing the bundle file: {}", e))?;
    }
    drop(writer);

    file_utils::rename(temporary_path, bundle_path)
        .map_err(|e| format!("Error while moving the bundle into place: {}", e))?;
    if let Some(parent) = bundle_path.parent() {
        file_utils::sync_dir(parent)?;
    }

    Ok(())
}

/// Append one signature entry to the bounded-memory staging stream that follows the native packs
/// in the finished envelope. Signatures are small, but their count is not intrinsically bounded;
/// spooling preserves the builder's memory bound instead of retaining every sidecar in a `Vec`.
fn write_signature_record(writer: &mut impl Write,
                          hash: &str,
                          sidecar: &[u8]) -> Result<(), String> {
    if !crate::util::object_utils::is_object_hash(hash) {
        // The hash names a sidecar in this warehouse's own store, so it is shown (escaped) to say
        // which file to look at.
        return Err(format!(
            "Cannot bundle signature for {:?}: not an object hash (64 lowercase hex characters).", hash
        ));
    }
    writer.write_all(hash.as_bytes())
        .and_then(|_| writer.write_all(&(sidecar.len() as u64).to_be_bytes()))
        .and_then(|_| writer.write_all(sidecar))
        .map_err(|e| format!("Error while staging a bundle signature: {}", e))
}

/// A writer that hashes every byte it passes on, for the digest that ends the envelope.
struct DigestingWriter<W: Write> {
    inner: W,
    hasher: blake3::Hasher,
}

impl<W: Write> Write for DigestingWriter<W> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

fn copy_file_into(path: &Path,
                  writer: &mut impl Write,
                  what: &str) -> Result<(), String> {
    let mut file = std::fs::File::open(path)
        .map_err(|e| format!("Error while opening {}: {}", what, e))?;
    std::io::copy(&mut file, writer)
        .map(|_| ())
        .map_err(|e| format!("Error while copying {} into the bundle: {}", what, e))
}

/// A unique staging directory whose native pack artifacts are always removed after the enclosing
/// bundle build — on success, refusal, I/O error, or panic unwind.
struct TemporaryBundleDirectory {
    path: PathBuf,
}

impl TemporaryBundleDirectory {
    fn new(parent: &Path) -> Result<TemporaryBundleDirectory, String> {
        // A leftover directory from a killed builder with a recycled PID costs a retry, never the
        // build.
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let candidates = file_utils::pid_numbered(&COUNTER, |pid, n| {
            parent.join(format!(".bundle-packs-{}-{}", pid, n))
        });
        file_utils::create_first_new_folder(candidates)
            .map(|path| TemporaryBundleDirectory { path })
            .map_err(|e| format!("Error while creating bundle pack staging: {}", e))
    }
}

impl Drop for TemporaryBundleDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn unique_child(parent: &Path, stem: &str) -> PathBuf {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    parent.join(format!("{}-{}-{}", stem, std::process::id(), id))
}

fn unique_sibling_temp(path: &Path, stem: &str) -> PathBuf {
    unique_child(path.parent().unwrap_or(Path::new(".")), stem)
}

/// How old leftover build staging must be before a later build reclaims it. Normal builds remove
/// their staging on every exit path (`TemporaryBundleDirectory`); only a hard kill leaves debris.
/// The generous age keeps a plausibly-live concurrent build's staging out of reach.
const STALE_STAGING_SECONDS: u64 = 24 * 60 * 60;

/// Best-effort removal of staging debris a killed builder left next to the bundle: `.bundle-packs`
/// directories (`TemporaryBundleDirectory`) and `latest.bundle` temp envelopes (`unique_child`
/// names, holding whole pack copies) would otherwise accumulate forever. Errors are ignored:
/// reclaiming debris must never fail the build that triggered it.
fn remove_stale_staging(parent: &Path) {
    let Ok(entries) = std::fs::read_dir(parent) else { return };

    for entry in entries.flatten() {
        let name = entry.file_name();
        let is_staging = name.to_str().is_some_and(|name| {
            name.starts_with(".bundle-packs-")
                || name.starts_with("latest.bundle-")
                || name.starts_with("latest.tmp") // the pre-native-envelope writer's staging name
        });
        let is_stale = entry.metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|modified| modified.elapsed().ok())
            .is_some_and(|age| age.as_secs() >= STALE_STAGING_SECONDS);

        if is_staging && is_stale {
            let path = entry.path();
            let _ = if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                std::fs::remove_dir_all(&path)
            } else {
                std::fs::remove_file(&path)
            };
        }
    }
}

/// Build an in-memory partial bundle of the given objects (`POST /v1/objects/batch` —
/// the incremental counterpart of the full bundle). Objects that do not exist here are
/// skipped silently: the client notices what did not arrive and falls back to loose
/// fetches, so a partially-stocked remote degrades instead of failing. An object that
/// *does* exist but is above the whole-object ceiling (a grandfathered giant) is not
/// silently skipped like an absent one — it fails the whole call loudly, the same
/// writer-side refusal `build_bundle` gives, rather than silently omitting content the
/// caller asked for by name.
///
/// A hash repeated in `hashes` is written once, at its first occurrence (see [`BundleWriter`]).
///
/// # Arguments
/// * `hashes` - The objects to pack.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The bundle bytes (header line + one zstd stream of records).
/// * `Err(String)` - If a present object could not be read, or one is above the ceiling.
pub fn build_partial_bundle(hashes: &[String]) -> Result<Vec<u8>, String> {
    let mut bundle = BundleWriter::new()?;

    for hash in hashes {
        if bundle.holds(hash) || !file_utils::does_object_exist(hash)? {
            continue;
        }

        bundle.push(hash, &file_utils::retrieve_object_by_hash(hash)?)?;
    }

    bundle.finish()
}

/// A partial bundle written one object at a time, for a caller that has each object's bytes in
/// hand already and so need not read them back from the store: a serverless head mirroring a
/// batch from its object store pushes each object as it arrives, and stops when its answer is
/// big enough.
pub struct BundleWriter {
    encoder: zstd::stream::Encoder<'static, Vec<u8>>,
    written: HashSet<String>,
    object_bytes: u64,
}

impl BundleWriter {
    /// An empty bundle: the header line and the start of the record stream.
    pub fn new() -> Result<BundleWriter, String> {
        // Partial responses are the compact record stream: they are incremental additions to an
        // existing store, not the whole-clone artifact whose native packs can be installed as a unit.
        let mut bytes: Vec<u8> = Vec::new();
        bytes.extend_from_slice(BUNDLE_HEADER_V3.as_bytes());
        bytes.push(b'\n');

        let encoder = zstd::stream::Encoder::new(bytes, 0)
            .map_err(|e| format!("Error while starting the bundle stream: {}", e))?;

        Ok(BundleWriter { encoder, written: HashSet::new(), object_bytes: 0 })
    }

    /// Whether `hash` is in the bundle already.
    pub fn holds(&self, hash: &str) -> bool {
        self.written.contains(hash)
    }

    /// Whether no object has been pushed yet.
    pub fn is_empty(&self) -> bool {
        self.written.is_empty()
    }

    /// The object bytes pushed so far, before compression.
    pub fn object_bytes(&self) -> u64 {
        self.object_bytes
    }

    /// Append the object `hash` whose bytes are `object_bytes`, which the caller has verified
    /// against the hash. A hash already in the bundle is not written again: the importer admits
    /// each requested hash at most once (see [`StreamAdmission`]), so a repeat would fail the
    /// whole import. An object above the whole-object ceiling is refused.
    pub fn push(&mut self, hash: &str, object_bytes: &[u8]) -> Result<(), String> {
        if self.written.contains(hash) {
            return Ok(());
        }

        refuse_if_transporting_over_ceiling(&format!("object {}", hash), object_bytes.len())?;
        write_record(&mut self.encoder, KIND_OBJECT, hash, object_bytes)?;

        self.written.insert(hash.to_string());
        self.object_bytes += object_bytes.len() as u64;

        Ok(())
    }

    /// The finished bundle bytes.
    pub fn finish(self) -> Result<Vec<u8>, String> {
        self.encoder.finish()
            .map_err(|e| format!("Error while finishing the bundle stream: {}", e))
    }
}

/// An object hash in binary form: the plan holds one per record, so a whole-history plan costs
/// no allocation per record.
type PlanHash = [u8; pack_utils::HASH_LEN];

/// One record of a bundle build, in the order the bundle holds them.
enum Emission {
    /// A parcel, then its signature sidecar when it has one.
    Parcel { hash: PlanHash },

    /// A tree, stored in full. `directory` names it in a refusal.
    Tree { hash: PlanHash, directory: Arc<str> },

    /// A blob's first occurrence at `path`. `base` is the plan index of the version emitted
    /// before it at that path: the blob is stored as a delta against it when the chain allows
    /// that and it saves space.
    Blob { hash: PlanHash, path: Arc<str>, base: Option<usize> },
}

impl Emission {
    fn hash(&self) -> &PlanHash {
        match self {
            Emission::Parcel { hash } | Emission::Tree { hash, .. } | Emission::Blob { hash, .. } => hash,
        }
    }
}

fn plan_hash(hash: &str) -> Result<PlanHash, String> {
    pack_utils::hash_to_bytes(hash)
        .ok_or_else(|| format!("Cannot pack \"{}\": not a 64-character object hash.", hash))
}

/// The serial state of the walk: what is already planned, and the newest version at each path.
#[derive(Default)]
struct PlanWalk {
    plan: Vec<Emission>,
    seen_trees: HashSet<PlanHash>,
    /// Each planned blob's plan index.
    planned_blobs: HashMap<PlanHash, usize>,
    /// The plan index of the most recently emitted blob at each file path — the base a later
    /// version deltas against. Its keys are the paths the plan's blobs share.
    latest_blob_at_path: HashMap<Arc<str>, usize>,
    /// The directory names the plan's trees share.
    directories: HashSet<Arc<str>>,
}

/// Walk every parcel's closure (trees and blobs, each once) into the records the bundle holds,
/// in order: each parcel, then its tree, the tree's files, and its subtrees depth-first. Trees and
/// parcels are always stored in full; only blobs (where the version redundancy lives) are
/// considered for deltas, against the version most recently emitted at the same path.
///
/// The walk itself is serial (which trees and blobs are new depends on every parcel before), but
/// the trees it reads are loaded ahead on worker threads: for each parcel, those that differ from
/// its first parent's at the same path — a superset of the ones the walk will find unseen, since
/// the parent is walked first. A tree the workers did not load (or failed to) is read by the walk.
///
/// Workers load at most `parcel_trees` trees for any one parcel ([`PLAN_PARCEL_TREES`] in a build).
///
/// A chunked file anywhere in the closure, or a tree that cannot be read, refuses the build here,
/// before any record is written.
fn plan_emission(order: &[String], parcel_trees: usize) -> Result<Vec<Emission>, String> {
    let mut tree_of: HashMap<&str, String> = HashMap::new();
    let mut walks: Vec<(&str, String, Option<String>)> = Vec::new();

    for parcel_hash in order {
        let parcel = object_utils::load_parcel(parcel_hash)?;
        let parent_tree = parcel.parents.first().and_then(|parent| tree_of.get(parent.as_str())).cloned();
        tree_of.insert(parcel_hash, parcel.tree_hash.clone());
        walks.push((parcel_hash, parcel.tree_hash, parent_tree));
    }

    let mut walk = PlanWalk::default();

    fanout_utils::prefetch_in_order(
        &walks,
        PLAN_WINDOW,
        PLAN_BUDGET_ENTRIES,
        |(_, tree_hash, parent_tree)| {
            let mut changed: HashMap<String, TreeItem> = HashMap::new();
            load_changed_trees(tree_hash, parent_tree.as_deref(), parcel_trees, &mut changed);
            changed
        },
        |changed| changed.values().map(|tree| tree.get_files().len() + tree.get_subtrees().len()).sum(),
        |(parcel_hash, tree_hash, _), mut prefetched| {
            walk.plan.push(Emission::Parcel { hash: plan_hash(parcel_hash)? });

            plan_tree_closure(&mut walk, tree_hash, "", &mut prefetched)
        },
    )?;

    Ok(walk.plan)
}

/// Load, into `changed`, the trees under `tree_hash` that differ from `parent_hash` — the tree at
/// the same path in the parcel's first parent (`None`: there is none), stopping at
/// `limit` trees. Best-effort: a tree that cannot be read is left out, for the walk to
/// read and refuse in order. Trees are parsed without the shared parsed-tree cache, as other
/// one-shot walks do.
fn load_changed_trees(tree_hash: &str,
                      parent_hash: Option<&str>,
                      limit: usize,
                      changed: &mut HashMap<String, TreeItem>) {
    if parent_hash == Some(tree_hash)
        || changed.contains_key(tree_hash)
        || changed.len() >= limit {
        return;
    }

    let Ok(tree) = object_utils::load_tree(tree_hash) else { return };
    let parent = parent_hash.and_then(|parent| object_utils::load_tree(parent).ok());

    for (name, subtree) in tree.get_subtrees() {
        let parent_subtree = parent.as_ref()
            .and_then(|parent| parent.get_subtree(name))
            .map(|parent_subtree| parent_subtree.hash.as_str());
        load_changed_trees(&subtree.hash, parent_subtree, limit, changed);
    }

    changed.insert(tree_hash.to_string(), tree);
}

fn plan_tree_closure(walk: &mut PlanWalk,
                     tree_hash: &str,
                     path_prefix: &str,
                     prefetched: &mut HashMap<String, TreeItem>) -> Result<(), String> {
    if !walk.seen_trees.insert(plan_hash(tree_hash)?) {
        return Ok(());
    }

    let directory = if path_prefix.is_empty() { "/" } else { path_prefix };
    let directory = match walk.directories.get(directory) {
        Some(directory) => Arc::clone(directory),
        None => {
            let directory: Arc<str> = Arc::from(directory);
            walk.directories.insert(Arc::clone(&directory));
            directory
        }
    };
    walk.plan.push(Emission::Tree { hash: plan_hash(tree_hash)?, directory });

    let tree = match prefetched.remove(tree_hash) {
        Some(tree) => tree,
        None => {
            #[cfg(test)]
            PLAN_WALK_READS.with(|reads| reads.set(reads.get() + 1));
            object_utils::load_tree(tree_hash)?
        }
    };

    for (name, file) in tree.get_files() {
        let path = join_path(path_prefix, name);

        // Chunk transport is **per-object** (a chunked file's chunks ride the loose GET/PUT byte
        // plane, negotiated one hash at a time), and by design **no bundle ever carries a chunk**:
        // trees reference only recipes, so this closure walk never descends into a recipe, and a
        // chunk is therefore structurally absent from every bundle. A bundle could still carry a
        // chunked file's *recipe* as bytes (a blob record moves it undecoded), but the result would
        // be a bundle over a file whose chunks it cannot contain — unmaterializable wherever it
        // lands. So this guard stays even now that chunk transport has shipped: a chunked file
        // reaches a peer over the wire per object (franchise/lower/expand fetch the recipe from the
        // bundle, then the chunks loose), never inside a bundle. Refuse loudly rather than ship one
        // silently incomplete.
        if file.item_type.is_chunked() {
            // This walker stays `Result<_, String>` (the migration frontier); reframe the typed
            // refusal via the bridge shim so its code survives to the command boundary.
            return Err(scope_utils::chunked_transport_refusal(&path).into());
        }

        let path: Arc<str> = match walk.latest_blob_at_path.get_key_value(path.as_str()) {
            Some((path, _)) => Arc::clone(path),
            None => Arc::from(path),
        };
        let hash = plan_hash(&file.hash)?;

        // Already in the bundle: don't re-emit, but let a later version at this path delta
        // against it (it is the newest content seen here).
        let index = match walk.planned_blobs.get(&hash) {
            Some(index) => *index,
            None => {
                let index = walk.plan.len();
                let base = walk.latest_blob_at_path.get(&path).copied();
                walk.plan.push(Emission::Blob { hash, path: Arc::clone(&path), base });
                walk.planned_blobs.insert(hash, index);
                index
            }
        };

        walk.latest_blob_at_path.insert(path, index);
    }

    for (name, subtree) in tree.get_subtrees() {
        let child = join_path(path_prefix, name);
        plan_tree_closure(walk, &subtree.hash, &child, prefetched)?;
    }

    Ok(())
}

/// One record, read, verified and compressed — everything a record needs that does not depend on
/// the records before it.
enum Loaded {
    Parcel { compressed: Vec<u8>, sidecar: Option<Vec<u8>> },
    Tree { compressed: Vec<u8> },
    Blob {
        target_len: usize,

        /// The delta against the blob's base, when it has one and is small enough to delta. An
        /// error here (an unreadable base) fails the build only if the delta is the one taken.
        delta: Option<Result<Vec<u8>, String>>,

        /// The full record, unless the delta is the smaller of the two. `None` with a delta
        /// present means the delta is smaller; the record is compressed on the spot if the
        /// chain turns out too deep for it.
        full: Option<Vec<u8>>,
    },
}

impl Loaded {
    fn size(&self) -> usize {
        match self {
            Loaded::Parcel { compressed, sidecar } => {
                compressed.len() + sidecar.as_ref().map_or(0, Vec::len)
            }
            Loaded::Tree { compressed } => compressed.len(),
            Loaded::Blob { delta, full, .. } => {
                delta.as_ref().and_then(|delta| delta.as_ref().ok()).map_or(0, Vec::len)
                    + full.as_ref().map_or(0, Vec::len)
            }
        }
    }
}

/// Load one record on a worker. Each object's size is checked against the whole-object ceiling
/// right after its bytes are read, before anything of it is compressed.
fn load_emission(plan: &[Emission], item: &Emission) -> Result<Loaded, String> {
    let hash = &sign_utils::to_hex(item.hash());

    match item {
        Emission::Parcel { .. } => {
            // Parcels are stored full and read once: keep them out of the read cache the blobs'
            // delta bases come from.
            let bytes = file_utils::retrieve_object_by_hash_uncached(hash)?;
            refuse_if_transporting_over_ceiling(&format!("parcel {}", hash), bytes.len())?;
            let compressed = pack_utils::TransportPackBuilder::compress_full(hash, &bytes)?;

            let sidecar = sign_utils::load_raw_parcel_signature(hash)?;
            if let Some(sidecar) = &sidecar {
                refuse_if_transporting_over_ceiling(
                    &format!("signature for parcel {}", hash), sidecar.len()
                )?;
            }

            Ok(Loaded::Parcel { compressed, sidecar })
        }
        Emission::Tree { directory, .. } => {
            let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
            refuse_if_transporting_over_ceiling(
                &format!("the directory \"{}\" (object {})", directory, hash), bytes.len()
            )?;

            Ok(Loaded::Tree { compressed: pack_utils::TransportPackBuilder::compress_full(hash, &bytes)? })
        }
        Emission::Blob { path, base, .. } => {
            let target = file_utils::retrieve_object_by_hash_shared(hash)?;
            refuse_if_transporting_over_ceiling(&format!("\"{}\" (object {})", path, hash), target.len())?;

            // An over-large object is stored full, never delta'd — the same rule `pack_utils`
            // applies, and the one that lets `decompress_delta` enforce a real bomb ceiling on
            // the read side (`delta_utils::MAX_DELTA_TARGET_BYTES`).
            let deltable = target.len() <= delta_utils::MAX_DELTA_TARGET_BYTES;
            let delta = base.filter(|_| deltable).map(|base| {
                file_utils::retrieve_object_by_hash_shared(&sign_utils::to_hex(plan[base].hash()))
                    .and_then(|base| delta_utils::compress_delta(&base, &target))
            });

            // Only worth a delta record when it is actually smaller than the object it
            // replaces (a wholly-rewritten file is cheaper stored in full).
            let delta_is_smaller = matches!(&delta, Some(Ok(delta)) if delta.len() < target.len());
            let full = if delta_is_smaller {
                None
            } else {
                Some(pack_utils::TransportPackBuilder::compress_full(hash, &target)?)
            };

            Ok(Loaded::Blob { target_len: target.len(), delta, full })
        }
    }
}

/// Append the loaded record `plan[index]`, in plan order. A blob becomes a delta only while its
/// base's chain is shorter than [`MAX_DELTA_CHAIN`]; its own depth is recorded in `depths` (0 =
/// stored in full) so a later version at its path chains from it correctly.
fn write_emission(pack: &mut pack_utils::TransportPackBuilder,
                  signature_writer: &mut impl Write,
                  plan: &[Emission],
                  index: usize,
                  loaded: Loaded,
                  depths: &mut [u32],
                  stats: &mut BuildStats) -> Result<(), String> {
    let hash = &sign_utils::to_hex(plan[index].hash());

    match (&plan[index], loaded) {
        (Emission::Parcel { .. }, Loaded::Parcel { compressed, sidecar }) => {
            pack.append_compressed_full(hash, &compressed)?;
            stats.objects += 1;

            if let Some(sidecar) = sidecar {
                write_signature_record(signature_writer, hash, &sidecar)?;
                stats.signatures += 1;
            }
        }
        (Emission::Tree { .. }, Loaded::Tree { compressed }) => {
            pack.append_compressed_full(hash, &compressed)?;
            stats.objects += 1;
        }
        (Emission::Blob { base, .. }, Loaded::Blob { target_len, delta, full }) => {
            match (*base, delta) {
                (Some(base), Some(delta)) if depths[base] < MAX_DELTA_CHAIN => {
                    let delta = delta?;

                    if delta.len() < target_len {
                        let base_hash = sign_utils::to_hex(plan[base].hash());
                        pack.append_delta(hash, &base_hash, target_len as u64, &delta)?;
                        stats.deltas += 1;
                        depths[index] = depths[base] + 1;
                    } else {
                        append_full_blob(pack, hash, full)?;
                        stats.objects += 1;
                    }
                }
                _ => {
                    append_full_blob(pack, hash, full)?;
                    stats.objects += 1;
                }
            }
        }
        _ => unreachable!("a loaded record is always the kind of its plan item"),
    }

    Ok(())
}

/// Append a blob in full: its loaded full record, or — when the worker kept only the smaller
/// delta and the chain was too deep for it — one compressed now.
fn append_full_blob(pack: &mut pack_utils::TransportPackBuilder,
                    hash: &str,
                    full: Option<Vec<u8>>) -> Result<(), String> {
    let compressed = match full {
        Some(compressed) => compressed,
        None => {
            let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;
            pack_utils::TransportPackBuilder::compress_full(hash, &bytes)?
        }
    };

    pack.append_compressed_full(hash, &compressed)
}

/// Refuse to write a record above the whole-object ceiling into a bundle — the writer-side half
/// of the maintainer's chosen posture for a grandfathered giant (an object authored before
/// `MAX_OBJECT_BYTES` existed): it stays readable locally forever, but a bundle must never carry
/// it, because no reader accepts it (the record-stream reader refuses its declared length
/// pre-read, and the native reader refuses it on verification). Checked here, right after an
/// object's bytes are loaded and before a single byte is written into the bundle stream — so a
/// warehouse holding one anywhere in the closure fails loudly at the source, before writing
/// anything a consumer could partially import.
fn refuse_if_transporting_over_ceiling(what: &str, len: usize) -> Result<(), CoreError> {
    scope_utils::refuse_if_over_object_ceiling(what, len)
}

/// Join a path prefix and an entry name (`""` prefix yields the bare name).
fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// Order a set of reachable parcels oldest first — every parcel after all of its parents
/// that are in the set — so a delta's base is always emitted before the delta. A cycle
/// (only a corrupt history could produce one) falls back to arbitrary order; deltas simply
/// may not form, which is safe.
pub(crate) fn topo_order_oldest_first(reachable: &HashSet<String>) -> Result<Vec<String>, String> {
    use std::collections::BTreeSet;

    let mut indegree: HashMap<String, usize> = reachable.iter().map(|h| (h.clone(), 0)).collect();
    let mut children: HashMap<String, Vec<String>> = HashMap::new();

    for hash in reachable {
        for parent in object_utils::load_parcel(hash)?.parents {
            if reachable.contains(&parent) {
                *indegree.get_mut(hash).unwrap() += 1;
                children.entry(parent).or_default().push(hash.clone());
            }
        }
    }

    // A sorted ready set makes the order deterministic → reproducible bundles.
    let mut ready: BTreeSet<String> = indegree.iter()
        .filter(|(_, &degree)| degree == 0)
        .map(|(hash, _)| hash.clone())
        .collect();

    let mut order: Vec<String> = Vec::with_capacity(reachable.len());

    while let Some(hash) = ready.iter().next().cloned() {
        ready.remove(&hash);
        order.push(hash.clone());

        if let Some(kids) = children.get(&hash) {
            for kid in kids {
                let degree = indegree.get_mut(kid).unwrap();
                *degree -= 1;

                if *degree == 0 {
                    ready.insert(kid.clone());
                }
            }
        }
    }

    if order.len() != reachable.len() {
        return Ok(reachable.iter().cloned().collect());
    }

    Ok(order)
}

/// Write one record: kind byte, 64 hex hash bytes, big-endian u64 length, payload.
fn write_record<W: Write>(encoder: &mut zstd::stream::Encoder<'_, W>,
                          kind: u8,
                          hash: &str,
                          payload: &[u8]) -> Result<(), String> {
    if hash.len() != 64 {
        return Err(format!("Cannot bundle \"{}\": not a 64-character hash.", hash));
    }

    encoder.write_all(&[kind])
        .and_then(|_| encoder.write_all(hash.as_bytes()))
        .and_then(|_| encoder.write_all(&(payload.len() as u64).to_be_bytes()))
        .and_then(|_| encoder.write_all(payload))
        .map_err(|e| format!("Error while writing a bundle record: {}", e))
}

/// Import a whole-warehouse bundle file into the object store. Only the native format is read: it
/// is the only one any whole-bundle writer produces, and it is copied into the store byte for byte,
/// so an import never stores more bytes than the file holds. Any other header — including the
/// record-stream formats a whole bundle once used — is refused with the error
/// [`is_unsupported_bundle_error`] recognizes, on which a clone falls back to its verified walk.
/// A file that does not match its digest is refused with the error [`is_bundle_digest_error`]
/// recognizes, before anything is stored. Objects already present are skipped; every stored object
/// is hash-verified; a mismatching record fails the import (the bundle is corrupt — nothing
/// unverified may land).
///
/// # Arguments
/// * `path` - The bundle file.
///
/// # Returns
/// * `Ok(ImportStats)` - What was stored.
/// * `Err(String)`     - On an unknown header, a digest mismatch, a corrupt record, or an I/O
///   error.
pub fn import_bundle(path: &Path) -> Result<ImportStats, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Error while opening the bundle file: {}", e))?;

    import_bundle_reader(std::io::BufReader::new(file))
}

/// What a streamed partial bundle may store. Every record is checked against this before a byte
/// of its payload is read, so a remote can make the client write only objects it asked for, each
/// at most once, however long the stream runs.
pub enum StreamAdmission {
    /// A batch response: each record must name one of these hashes, and none twice.
    Requested(HashSet<String>),

    /// A subtree response: the object closure of this tree. The first admissible record is the
    /// root itself, and each admitted tree makes its files and subtrees admissible — the order
    /// the server's breadth-first closure walk emits, where every hash follows the tree naming it.
    /// At most [`MAX_MISSING_BATCH`] records are admitted, the server's own cap on one response.
    Closure { root: String },
}

/// The running state of a [`StreamAdmission`] over one stream.
struct Admitter {
    /// Every hash a record may still name, mapped to whether it is a tree whose entries become
    /// admissible once it is admitted — only ever true in a closure.
    expected: HashMap<String, bool>,
    admitted: HashSet<String>,

    /// A closure's root and record cap. A batch has neither: its requested set is its own bound.
    closure: Option<(String, usize)>,
}

impl Admitter {
    fn new(admission: StreamAdmission) -> Self {
        match admission {
            StreamAdmission::Requested(hashes) => Admitter {
                expected: hashes.into_iter().map(|hash| (hash, false)).collect(),
                admitted: HashSet::new(),
                closure: None,
            },
            StreamAdmission::Closure { root } => Admitter {
                expected: HashMap::from([(root.clone(), true)]),
                admitted: HashSet::new(),
                closure: Some((root, MAX_MISSING_BATCH)),
            },
        }
    }

    /// Admit a record naming `hash`, or refuse it. `Ok(true)` when it is a tree whose entries
    /// [`Self::expand`] must make admissible once it is in the store.
    fn admit(&mut self, hash: &str) -> Result<bool, String> {
        if self.admitted.contains(hash) {
            return Err(format!("The remote sent object {} twice in one response; refusing it.", hash));
        }

        let Some(&is_tree) = self.expected.get(hash) else {
            if let (Some((root, _)), true) = (&self.closure, self.admitted.is_empty()) {
                return Err(format!(
                    "The remote's subtree response starts at {}, not at the requested tree {}; \
                    refusing it.", hash, root
                ));
            }

            return Err(format!(
                "The remote sent object {}, which this request did not ask for; refusing it.", hash
            ));
        };

        if let Some((_, cap)) = &self.closure {
            if self.admitted.len() >= *cap {
                return Err(format!(
                    "The remote sent more than {} objects in one response; refusing it.", cap
                ));
            }
        }

        self.admitted.insert(hash.to_string());

        Ok(is_tree)
    }

    /// Make an admitted tree's files and subtrees admissible. The tree is read from the store,
    /// which holds it whether this record stored it or it was already present (and the record
    /// skipped): the server sends every closure object it has, including ones the client holds.
    ///
    /// A chunked file's recipe is admitted as the file entry, but never its chunks: chunk
    /// transport has not shipped, and the server's closure walk does not descend into recipes.
    /// When it does, this must learn to expand a recipe too, or every such response is refused.
    fn expand(&mut self, tree_hash: &str) -> Result<(), String> {
        let tree = object_utils::load_tree(tree_hash)?;

        for (_, file) in tree.get_files() {
            self.expected.entry(file.hash.clone()).or_insert(false);
        }

        for (_, subtree) in tree.get_subtrees() {
            self.expected.entry(subtree.hash.clone()).or_insert(true);
        }

        Ok(())
    }
}

/// Import a bundle streamed from a remote (a batch or subtree response) as it arrives, holding at
/// most one record in memory, and storing only what `admission` allows.
///
/// Only a version-3 bundle is accepted: that is what every batch and subtree producer writes, and
/// it is the version whose records stream. A whole-warehouse bundle imports through
/// temporary pack files and has no business arriving here, so it is refused rather than imported
/// by a path this one was never meant to take.
///
/// # Arguments
/// * `reader`    - The response body, as it arrives.
/// * `admission` - What the request asked for.
///
/// # Returns
/// * `Ok(ImportStats)` - What was stored.
/// * `Err(String)`     - On any header but version 3, a record `admission` refuses, a corrupt or
///   truncated record, or an I/O error (including the transfer ending before the bundle did).
pub fn import_streamed_bundle<R: std::io::BufRead>(mut reader: R,
                                                   admission: StreamAdmission) -> Result<ImportStats, String> {
    let header = read_bundle_header(&mut reader)?;

    if header != BUNDLE_HEADER_V3.as_bytes() {
        return Err(format!(
            "The remote sent a \"{}\" bundle where only a \"{}\" one belongs; refusing to import it.",
            String::from_utf8_lossy(&header), BUNDLE_HEADER_V3
        ));
    }

    import_record_stream(reader, Admitter::new(admission))
}

/// Read a bundle's uncompressed header line, without its newline.
fn read_bundle_header<R: std::io::BufRead>(reader: &mut R) -> Result<Vec<u8>, String> {
    // The header line is uncompressed; everything after it is one zstd stream. `length` is not
    // the only attacker-controlled number here: a hostile bundle need not carry a newline at all
    // (or may run arbitrarily many non-newline bytes before one), and an unbounded
    // `read_until(b'\n', ...)` would grow `header` without limit chasing it — the same
    // unbounded-growth shape the record-length checks in `import_record_stream` exist to close.
    // Every header this build recognizes is under 30 bytes, so bound the search to a small,
    // fixed cap: past it, this cannot be a bundle this build understands, however long the
    // stream actually runs.
    let mut header: Vec<u8> = Vec::new();
    let mut limited = reader.by_ref().take(MAX_HEADER_BYTES as u64);

    std::io::BufRead::read_until(&mut limited, b'\n', &mut header)
        .map_err(|e| format!("{}: {}", BUNDLE_HEADER_READ_PREFIX, e))?;

    if header.last() != Some(&b'\n') {
        return Err(format!(
            "{}: no newline within the first {} bytes.",
            NOT_A_BUNDLE_PREFIX, MAX_HEADER_BYTES
        ));
    }

    header.pop();

    Ok(header)
}

/// Import a bundle from a reader (see `import_bundle`). The header is read first, so a file in
/// another version is reported unsupported whatever its last bytes are; then the whole file is
/// checked against its digest before anything is written, so a file that is not the bytes its
/// builder wrote leaves nothing behind.
fn import_bundle_reader<R>(mut reader: R) -> Result<ImportStats, String>
where
    R: std::io::BufRead + std::io::Seek,
{
    let header = read_bundle_header(&mut reader)?;

    if header != BUNDLE_HEADER.as_bytes() {
        return Err(format!(
            "{}\"{}\" (this build reads \"{}\").",
            UNKNOWN_BUNDLE_HEADER_PREFIX, String::from_utf8_lossy(&header), BUNDLE_HEADER
        ));
    }

    let body_length = verify_bundle_digest(&mut reader)?;

    // The body ends where the digest starts, so the end-of-file check below sees the digest as
    // the end and not as trailing bytes.
    import_native_bundle_reader(&mut reader.take(body_length))
}

/// Check the whole bundle `reader` holds against the digest that ends it, and leave `reader` back
/// where it was, just past the header. Returns the length of the body between the header and the
/// digest.
fn verify_bundle_digest<R: Read + std::io::Seek>(reader: &mut R) -> Result<u64, String> {
    use std::io::SeekFrom;

    let seek_error = |e: std::io::Error| {
        format!("Error while reading the bundle for its digest: {}", e)
    };
    let body_start = reader.stream_position().map_err(seek_error)?;
    let total = reader.seek(SeekFrom::End(0)).map_err(seek_error)?;
    let digested = total.checked_sub(BUNDLE_DIGEST_BYTES as u64)
        .filter(|digested| *digested >= body_start)
        .ok_or_else(|| {
            format!("{}: it ends before its digest ({} bytes).", BUNDLE_DIGEST_PREFIX, total)
        })?;

    reader.seek(SeekFrom::Start(0)).map_err(seek_error)?;
    let mut hasher = blake3::Hasher::new();
    let mut digested_part = reader.by_ref().take(digested);
    hasher.update_reader(&mut digested_part).map_err(seek_error)?;
    let shrank = || format!("{}: it shrank while it was being read.", BUNDLE_DIGEST_PREFIX);
    if digested_part.limit() != 0 {
        return Err(shrank());
    }
    let mut recorded = [0u8; BUNDLE_DIGEST_BYTES];
    match reader.read_exact(&mut recorded) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Err(shrank()),
        Err(e) => return Err(seek_error(e)),
    }

    if hasher.finalize().as_bytes() != &recorded {
        return Err(format!(
            "{}: the file is not the one its builder wrote, so nothing of it is imported.",
            BUNDLE_DIGEST_PREFIX
        ));
    }

    reader.seek(SeekFrom::Start(body_start)).map_err(seek_error)?;
    Ok(digested - body_start)
}

/// Import the zstd record stream that follows a version-3 header, storing only the records
/// `admitter` admits.
fn import_record_stream<R: std::io::BufRead>(reader: R, mut admitter: Admitter) -> Result<ImportStats, String> {
    let mut decoder = object_utils::bounded_zstd_decoder(reader)
        .map_err(|e| format!("Error while opening the bundle stream: {}", e))?;

    let mut stats = ImportStats::default();

    loop {
        let mut kind = [0u8; 1];

        // Only a read that produces nothing ends the bundle: the stream finished on a frame
        // boundary between records. A truncated frame is an error from the decoder, never a clean
        // end — so a bundle cut short in transit, even exactly between two records, fails the
        // import instead of passing for a smaller one.
        let read = loop {
            match decoder.read(&mut kind) {
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other,
            }
        };

        match read {
            Ok(0) => break,
            Ok(_) => {}
            Err(e) => return Err(format!("Error while reading the bundle: {}", e)),
        }

        let mut hash_bytes = [0u8; 64];
        let mut length_bytes = [0u8; 8];

        decoder.read_exact(&mut hash_bytes)
            .and_then(|_| decoder.read_exact(&mut length_bytes))
            .map_err(|e| format!("The bundle is truncated: {}", e))?;

        let hash = String::from_utf8(hash_bytes.to_vec())
            .map_err(|_| "A bundle record's hash is not valid ASCII.".to_string())?;

        let length = u64::from_be_bytes(length_bytes);

        // Every check runs before a byte of the payload is read: what is refused here is never
        // decompressed, buffered or written.
        if kind[0] != KIND_OBJECT {
            return Err(format!("Unknown bundle record kind 0x{:02x}.", kind[0]));
        }

        let is_tree = admitter.admit(&hash)?;

        // The ceiling as *policy*: no writer emits an over-ceiling record. It is not the memory
        // defense — that is the streaming path below, which bounds memory whatever the length.
        if length > object_utils::MAX_OBJECT_BYTES as u64 {
            return Err(oversized_record_refusal("object", length));
        }

        // A small object is buffered whole and stored in one shot; a large one streams through an
        // incremental Blake3 to a temp file, bounding memory regardless of the declared length.
        // `store_object_stream` reports truncation, verifies the hash, and enforces the per-chunk
        // ceiling itself.
        let stored = if length <= object_utils::STREAM_STORE_THRESHOLD_BYTES as u64 {
            let payload = read_exact_payload(&mut decoder, length)?;
            object_utils::store_object_bytes(&hash, &payload)?
        } else {
            object_utils::store_object_stream(&hash, decoder.by_ref(), length)?
        };

        if stored { stats.stored_objects += 1; } else { stats.skipped_records += 1; }

        if is_tree {
            admitter.expand(&hash)?;
        }
    }

    Ok(stats)
}

/// Import the body of a version-5 whole-warehouse bundle, between its header and its digest:
/// quarantine and verify its native pack pairs, publish those aggregate files, then store the
/// signature sidecars that follow, each admitted only for a parcel those packs carry.
fn import_native_bundle_reader(reader: &mut impl Read) -> Result<ImportStats, String> {
    let pack_count = read_u32_be(reader, "native pack count")? as usize;
    if pack_count > pack_utils::MAX_TRANSPORT_PACKS {
        return Err(format!(
            "The bundle declares {} native packs, above the {}-pack limit.",
            pack_count, pack_utils::MAX_TRANSPORT_PACKS
        ));
    }

    let mut sections = Vec::with_capacity(pack_count);
    for _ in 0..pack_count {
        let data_len = read_u64_be(reader, "native pack length")?;
        let index_len = read_u64_be(reader, "native pack index length")?;
        if data_len > pack_utils::MAX_TRANSPORT_PACK_BYTES {
            return Err(format!(
                "A bundle pack section declares {} bytes, above the {}-byte limit.",
                data_len, pack_utils::MAX_TRANSPORT_PACK_BYTES
            ));
        }
        if index_len > pack_utils::MAX_TRANSPORT_INDEX_BYTES {
            return Err(format!(
                "A bundle pack index declares {} bytes, above the {}-byte limit.",
                index_len, pack_utils::MAX_TRANSPORT_INDEX_BYTES
            ));
        }
        sections.push((data_len, index_len));
    }

    let mut imported = pack_utils::import_transport_packs(reader, &sections)?;
    let total_objects = imported.stored_objects + imported.skipped_objects;
    let signature_count = read_u64_be(reader, "signature count")?;
    if signature_count > total_objects as u64 {
        return Err(format!(
            "The bundle declares {} signatures but its native packs contain only {} objects.",
            signature_count, total_objects
        ));
    }

    let mut stats = ImportStats {
        stored_objects: imported.stored_objects,
        stored_signatures: 0,
        skipped_records: imported.skipped_objects,
        // `import_transport_packs` only publishes (and marks the store densify-pending) when it
        // stored at least one object; mirror that gate here rather than re-deriving it.
        installed_packs: imported.stored_objects > 0,
    };

    // Each entry becomes its own sidecar file, so an entry is admitted only for a parcel the packs
    // carry, once: a sidecar naming anything else would cost a file for bytes the bundle never
    // carried. Each admitted parcel moves from the carried set to the signed one.
    let mut signed: HashSet<String> = HashSet::new();

    for _ in 0..signature_count {
        let mut hash_bytes = [0u8; 64];
        reader.read_exact(&mut hash_bytes)
            .map_err(|e| format!("The bundle is truncated while reading a signature hash: {}", e))?;
        let hash = std::str::from_utf8(&hash_bytes).ok()
            .filter(|text| object_utils::is_object_hash(text))
            .ok_or_else(|| "A bundle signature hash is not lowercase hexadecimal ASCII.".to_string())?
            .to_string();
        let length = read_u64_be(reader, "signature length")?;
        if length > object_utils::MAX_OBJECT_BYTES as u64 {
            return Err(oversized_record_refusal("signature", length));
        }
        if !imported.parcels.remove(&hash) {
            return Err(if signed.contains(&hash) {
                format!("The bundle carries two signatures for {}; refusing it.", hash)
            } else {
                format!(
                    "The bundle carries a signature for {}, which is not a parcel it carries; \
                    refusing it.", hash
                )
            });
        }
        signed.insert(hash.clone());
        let payload = read_exact_payload(reader, length)?;
        // An already-present sidecar wins and the bundle's copy is skipped — even a conflicting
        // one, whose refusal would otherwise fail the import after its packs were already
        // published.
        if sign_utils::load_raw_parcel_signature(&hash)?.is_some() {
            stats.skipped_records += 1;
        } else {
            sign_utils::store_raw_parcel_signature(&hash, &payload)?;
            stats.stored_signatures += 1;
        }
    }

    let mut trailing = [0u8; 1];
    match reader.read(&mut trailing) {
        Ok(0) => Ok(stats),
        Ok(_) => Err("The bundle has bytes between its signature section and its digest.".into()),
        Err(e) => Err(format!("Error while checking the end of the bundle: {}", e)),
    }
}

fn read_u32_be(reader: &mut impl Read, what: &str) -> Result<u32, String> {
    let mut bytes = [0u8; 4];
    reader.read_exact(&mut bytes)
        .map_err(|e| format!("The bundle is truncated while reading its {}: {}", what, e))?;
    Ok(u32::from_be_bytes(bytes))
}

fn read_u64_be(reader: &mut impl Read, what: &str) -> Result<u64, String> {
    let mut bytes = [0u8; 8];
    reader.read_exact(&mut bytes)
        .map_err(|e| format!("The bundle is truncated while reading its {}: {}", what, e))?;
    Ok(u64::from_be_bytes(bytes))
}

/// Read exactly `length` payload bytes from the bundle stream into memory, reporting a short
/// stream as truncation. The buffer grows with the bytes actually present — never pre-allocated to
/// a declared length, so a `u64::MAX` lie cannot capacity-panic — and the caller has already
/// bounded `length` (to the object ceiling, or to the streaming threshold), so this is a bounded
/// in-memory read.
fn read_exact_payload<R: Read>(reader: &mut R, length: u64) -> Result<Vec<u8>, String> {
    let mut payload = Vec::new();
    let read = reader.by_ref().take(length).read_to_end(&mut payload)
        .map_err(|e| format!("The bundle is truncated: {}", e))?;

    if read as u64 != length {
        return Err(format!(
            "The bundle is truncated: a record declared {} bytes but only {} remained.",
            length, read
        ));
    }

    Ok(payload)
}

/// The refusal for a bundle record whose *declared* length is above the whole-object ceiling —
/// raised before a byte of the payload is read, so a declared-length lie can neither allocate nor
/// stream a single byte. Names the record kind and the limit.
fn oversized_record_refusal(kind_name: &str, length: u64) -> String {
    format!(
        "A bundle {} record declares {} bytes, above the {}-byte object ceiling; refusing the bundle.",
        kind_name, length, object_utils::MAX_OBJECT_BYTES
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;
    use crate::model::chunk::Chunk;
    use crate::model::parcel::Parcel;
    use crate::model::recipe::{Recipe, RecipeChunk};
    use crate::model::tree_item::TreeItem;
    use crate::util::byte_utils::number_to_vlq_bytes;

    /// Import an in-memory bundle, as `import_bundle` imports a file.
    fn import_bundle_bytes(bytes: &[u8]) -> Result<ImportStats, String> {
        import_bundle_reader(std::io::Cursor::new(bytes))
    }

    /// Import an in-memory batch response to a request for `hashes`, as `fetch_batch` imports one.
    fn import_batch_bytes(bytes: &[u8], hashes: &[&str]) -> Result<ImportStats, String> {
        import_streamed_bundle(std::io::Cursor::new(bytes), requested(hashes))
    }

    fn requested(hashes: &[&str]) -> StreamAdmission {
        StreamAdmission::Requested(hashes.iter().map(|hash| hash.to_string()).collect())
    }

    /// Import an in-memory subtree response for the tree `root`, as `fetch_subtree` imports one.
    fn import_closure_bytes(bytes: &[u8], root: &str) -> Result<ImportStats, String> {
        import_streamed_bundle(std::io::Cursor::new(bytes), StreamAdmission::Closure { root: root.to_string() })
    }

    /// A fresh warehouse root for one test, entered as the active storage-root scope for
    /// its lifetime. Each test gets its own directory (and its own thread, `cargo test`'s
    /// default), so parallel tests never see each other's objects.
    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-bundle-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// Build a minimal record-stream bundle (an uncompressed header, then one zstd stream) from raw
    /// records, exactly as `import_streamed_bundle` reads one — the manual low-level construction
    /// the fuzz suite also uses, but here to hit semantic (not just never-panic) branches.
    fn raw_bundle(header: &str, records: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(header.as_bytes());
        bytes.push(b'\n');
        bytes.extend(zstd::encode_all(records, 3).unwrap());
        bytes
    }

    /// One record's on-wire bytes: kind byte, 64-hex hash, big-endian u64 length, payload.
    fn record(kind: u8, hash: &str, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(kind);
        bytes.extend(hash.as_bytes());
        bytes.extend((payload.len() as u64).to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    /// A well-formed (but not cryptographically meaningful) signature sidecar: version 1,
    /// an arbitrary key id and signature bytes — `sign_utils` never verifies a signature
    /// structurally beyond this shape (verification happens later, at ref-update time).
    fn raw_signature_sidecar(key_id: &str, signature: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(number_to_vlq_bytes(1));
        bytes.extend(number_to_vlq_bytes(key_id.len() as u64));
        bytes.extend(key_id.as_bytes());
        bytes.extend(number_to_vlq_bytes(signature.len() as u64));
        bytes.extend(signature);
        bytes
    }

    #[test]
    fn import_stores_a_valid_object_record() {
        let _scratch = Scratch::new("import-object-ok");

        let content = b"hello, bundle";
        let hash = object_utils::hash_object_bytes(content);
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &hash, content));

        let stats = import_batch_bytes(&bytes, &[&hash]).unwrap();
        assert_eq!(stats.stored_objects, 1);
        assert_eq!(stats.skipped_records, 0);
        assert!(file_utils::does_object_exist(&hash).unwrap());
        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), content);
    }

    #[test]
    fn import_rejects_a_hash_mismatched_object_record() {
        let _scratch = Scratch::new("import-object-mismatch");

        let content = b"hello, bundle";
        let wrong_hash = object_utils::hash_object_bytes(b"not the same content");
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &wrong_hash, content));

        let error = import_batch_bytes(&bytes, &[&wrong_hash]).err().unwrap();
        assert!(error.contains("does not match its claimed hash"), "{}", error);
        assert!(!file_utils::does_object_exist(&wrong_hash).unwrap(), "nothing unverified may land");
    }

    #[test]
    fn import_skips_an_object_already_present() {
        let _scratch = Scratch::new("import-object-skip");

        let content = b"already here";
        let hash = object_utils::hash_object_bytes(content);
        object_utils::store_object_bytes(&hash, content).unwrap();

        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &hash, content));
        let stats = import_batch_bytes(&bytes, &[&hash]).unwrap();

        assert_eq!(stats.stored_objects, 0);
        assert_eq!(stats.skipped_records, 1);
    }

    /// The record stream carries objects only. A signature or delta record — kinds the
    /// whole-warehouse stream once carried — is refused before its payload is read, and nothing
    /// lands.
    #[test]
    fn a_streamed_signature_or_delta_record_is_refused() {
        for kind in [b'S', b'D'] {
            let _scratch = Scratch::new("streamed-retired-kind");

            let parcel_content = b"a stand-in parcel object";
            let parcel_hash = object_utils::hash_object_bytes(parcel_content);
            object_utils::store_object_bytes(&parcel_hash, parcel_content).unwrap();

            let sidecar = raw_signature_sidecar("key-1", &[7u8; 64]);
            let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(kind, &parcel_hash, &sidecar));

            let error = import_batch_bytes(&bytes, &[&parcel_hash]).err().unwrap();
            assert!(error.contains("Unknown bundle record kind"), "'{}': {}", kind as char, error);
            assert_eq!(sign_utils::load_raw_parcel_signature(&parcel_hash).unwrap(), None, "no sidecar may land");
        }
    }

    #[test]
    fn import_rejects_an_unknown_record_kind() {
        let _scratch = Scratch::new("import-unknown-kind");

        let hash = "a".repeat(64);
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'Z', &hash, b"whatever"));
        let error = import_batch_bytes(&bytes, &[&hash]).err().unwrap();
        assert!(error.contains("Unknown bundle record kind"), "{}", error);
    }

    #[test]
    fn import_rejects_an_unknown_header() {
        let _scratch = Scratch::new("import-unknown-header");

        let bytes = raw_bundle("forklift-bundle 1999-01-01", &[]);
        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("Unknown bundle header"), "{}", error);
        assert!(is_unsupported_bundle_error(&error));
    }

    /// A hostile "bundle" that never carries a newline is refused within the small header cap,
    /// not by scanning arbitrarily far into the stream looking for one — an unbounded
    /// `read_until` on the header line would otherwise grow the header buffer without limit,
    /// undercutting this stage's own memory-bound thesis one field earlier than the length-prefixed
    /// records it protects. The input here is far larger than the cap and has no newline byte at
    /// all, so a bounded read must refuse quickly rather than buffer all of it.
    #[test]
    fn import_refuses_a_hostile_header_with_no_newline_within_the_bound() {
        let _scratch = Scratch::new("import-header-no-newline");

        let hostile: Vec<u8> = vec![b'x'; 10 * 1024 * 1024];

        let error = import_bundle_bytes(&hostile).err().unwrap();
        assert!(error.contains("no newline"), "{}", error);
        assert!(error.contains(&MAX_HEADER_BYTES.to_string()), "names the bound: {}", error);
    }

    /// A whole-warehouse bundle file is read only in the native format. Every record-stream
    /// header — version 3 included, which partial responses still use — is refused as
    /// unsupported before a record is read, so a clone handed such a file falls back to its
    /// verified walk.
    #[test]
    fn import_bundle_refuses_every_record_stream_header() {
        let _scratch = Scratch::new("import-record-stream-headers");

        let content = b"record-stream content";
        let hash = object_utils::hash_object_bytes(content);

        for header in [BUNDLE_HEADER_V3, "forklift-bundle 2026-07-06", "forklift-bundle 2026-07-03"] {
            let bytes = raw_bundle(header, &record(b'O', &hash, content));
            let error = import_bundle_bytes(&bytes).err()
                .unwrap_or_else(|| panic!("a \"{}\" bundle file must be refused", header));
            assert!(is_unsupported_bundle_error(&error), "{}", error);
            assert!(!file_utils::does_object_exist(&hash).unwrap(), "nothing past the header is read");
        }
    }

    /// A batch response may store only what the request named. An unrequested record is refused
    /// on its hash alone — even one declaring `u64::MAX` bytes, which would otherwise be refused
    /// on its length — and an honestly sized one is never stored.
    #[test]
    fn a_batch_refuses_an_object_it_did_not_ask_for() {
        let _scratch = Scratch::new("batch-unrequested");

        let asked = object_utils::hash_object_bytes(b"asked for");
        let content = b"never asked for";
        let other = object_utils::hash_object_bytes(content);

        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &other, content));
        let error = import_batch_bytes(&bytes, &[&asked]).err().unwrap();
        assert!(error.contains("did not ask for"), "{}", error);
        assert!(!file_utils::does_object_exist(&other).unwrap(), "an unrequested object never lands");

        let huge = record_with_declared_len(b'O', &other, u64::MAX, b"a few bytes");
        let error = import_batch_bytes(&raw_bundle(BUNDLE_HEADER_V3, &huge), &[&asked]).err().unwrap();
        assert!(error.contains("did not ask for"), "refused on its hash, before its length: {}", error);
    }

    /// A requested object sent twice is refused on the repeat, so a remote cannot make the client
    /// read and hash one object's bytes over and over.
    #[test]
    fn a_batch_refuses_an_object_sent_twice() {
        let _scratch = Scratch::new("batch-repeat");

        let content = b"sent twice";
        let hash = object_utils::hash_object_bytes(content);
        let mut records = record(b'O', &hash, content);
        records.extend(record(b'O', &hash, content));

        let error = import_batch_bytes(&raw_bundle(BUNDLE_HEADER_V3, &records), &[&hash]).err().unwrap();
        assert!(error.contains("twice"), "{}", error);
    }

    /// The producer writes a repeated hash once, so the honest response to a request carrying
    /// duplicates (the Lambda head passes a request through as given) imports.
    #[test]
    fn build_partial_bundle_writes_a_repeated_hash_once() {
        let content = b"asked for twice";
        let hash = object_utils::hash_object_bytes(content);

        let bytes = {
            let _source = Scratch::new("partial-bundle-dedupe-src");
            object_utils::store_object_bytes(&hash, content).unwrap();
            build_partial_bundle(&[hash.clone(), hash.clone()]).unwrap()
        };

        let _destination = Scratch::new("partial-bundle-dedupe-dst");
        let stats = import_batch_bytes(&bytes, &[&hash]).unwrap();
        assert_eq!(stats.stored_objects, 1);
        assert_eq!(stats.skipped_records, 0);
    }

    /// A subtree response's fixture: a root tree holding a file and a subtree holding another file,
    /// as the server's breadth-first closure walk emits them — root, its file, its subtree, then
    /// the subtree's file.
    struct ClosureFixture {
        records: Vec<u8>,
        root: String,
        subtree: (String, Vec<u8>),
        hashes: Vec<String>,
    }

    fn closure_fixture() -> ClosureFixture {
        let top = LooseObjectBuilder::build_blob(&Blob { content: b"top file".to_vec() });
        let nested = LooseObjectBuilder::build_blob(&Blob { content: b"nested file".to_vec() });

        let mut subtree_item = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        subtree_item.add_child(TreeItem::new("nested.txt".to_string(), nested.hash.clone(), DirEntryType::Normal));
        let subtree = LooseObjectBuilder::build_tree(&subtree_item);

        let mut root_item = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_item.add_child(TreeItem::new("top.txt".to_string(), top.hash.clone(), DirEntryType::Normal));
        root_item.add_child(TreeItem::new("sub".to_string(), subtree.hash.clone(), DirEntryType::Tree));
        let root = LooseObjectBuilder::build_tree(&root_item);

        let mut records = Vec::new();
        let mut hashes = Vec::new();
        for object in [&root, &top, &subtree, &nested] {
            records.extend(record(b'O', &object.hash, &object.content));
            hashes.push(object.hash.clone());
        }

        ClosureFixture {
            records,
            root: root.hash.clone(),
            subtree: (subtree.hash.clone(), subtree.content.clone()),
            hashes,
        }
    }

    #[test]
    fn a_subtree_response_imports_its_closure_in_full() {
        let fixture = closure_fixture();
        let _scratch = Scratch::new("closure-ok");

        let stats = import_closure_bytes(&raw_bundle(BUNDLE_HEADER_V3, &fixture.records), &fixture.root).unwrap();
        assert_eq!(stats.stored_objects, 4);
        assert!(fixture.hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
    }

    /// The first admissible record is the requested root, so a response rooted anywhere else is
    /// refused at its first record.
    #[test]
    fn a_subtree_response_for_another_root_is_refused() {
        let fixture = closure_fixture();
        let _scratch = Scratch::new("closure-wrong-root");

        let error = import_closure_bytes(&raw_bundle(BUNDLE_HEADER_V3, &fixture.records), &fixture.subtree.0)
            .err().unwrap();
        assert!(error.contains("not at the requested tree"), "{}", error);
        assert!(!file_utils::does_object_exist(&fixture.root).unwrap(), "the foreign root never lands");
    }

    #[test]
    fn a_subtree_response_carrying_an_object_outside_the_closure_is_refused() {
        let fixture = closure_fixture();
        let _scratch = Scratch::new("closure-stray");

        let stray_content = b"outside the closure";
        let stray = object_utils::hash_object_bytes(stray_content);
        let mut records = fixture.records.clone();
        records.extend(record(b'O', &stray, stray_content));

        let error = import_closure_bytes(&raw_bundle(BUNDLE_HEADER_V3, &records), &fixture.root).err().unwrap();
        assert!(error.contains("did not ask for"), "{}", error);
        assert!(!file_utils::does_object_exist(&stray).unwrap(), "the stray object never lands");
    }

    /// The server sends every closure object it has, including a tree the client already holds.
    /// That record is skipped, but the tree is still expanded — from the store — so the objects
    /// below it stay admissible and the response imports in full.
    #[test]
    fn a_subtree_response_imports_in_full_when_the_client_already_holds_a_tree_inside_it() {
        let fixture = closure_fixture();
        let _scratch = Scratch::new("closure-held-tree");
        object_utils::store_object_bytes(&fixture.subtree.0, &fixture.subtree.1).unwrap();

        let stats = import_closure_bytes(&raw_bundle(BUNDLE_HEADER_V3, &fixture.records), &fixture.root).unwrap();
        assert_eq!(stats.stored_objects, 3);
        assert_eq!(stats.skipped_records, 1, "the held tree is skipped");
        assert!(fixture.hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
    }

    /// (FORK-118) The largest pack a writer produces — exactly `PACK_ROLLOVER_OBJECTS` records,
    /// so its index is exactly the transport cap — round-trips through a native bundle: the cap
    /// admits it, import validates every bucket and the tiling, and every object lands.
    ///
    /// Mutation: compute `MAX_TRANSPORT_INDEX_BYTES` for one record fewer → the full pack's index no
    /// longer equals the cap (red), so the import would refuse it as over the cap.
    #[test]
    fn a_full_rollover_pack_round_trips_through_a_native_bundle() {
        let count = pack_utils::PACK_ROLLOVER_OBJECTS;
        let hashes: Vec<String> = (0..count)
            .map(|index| object_utils::hash_object_bytes(format!("rollover {}", index).as_bytes()))
            .collect();

        let bundle = {
            let source = Scratch::new("bundle-rollover-src");
            let staging = source.root.join("staging");
            std::fs::create_dir_all(&staging).unwrap();
            let mut builder = pack_utils::TransportPackBuilder::new(&staging).unwrap();
            for (index, hash) in hashes.iter().enumerate() {
                builder.append_full(hash, format!("rollover {}", index).as_bytes()).unwrap();
            }
            let artifacts = builder.finish().unwrap();
            assert_eq!(artifacts.len(), 1, "exactly the rollover count fits one pack");
            assert_eq!(artifacts[0].index_path.metadata().unwrap().len(), pack_utils::MAX_TRANSPORT_INDEX_BYTES,
                "a full pack's index is exactly the transport cap");

            let signatures = staging.join("signatures");
            std::fs::write(&signatures, b"").unwrap();
            let bundle_path = source.root.join("rollover.bundle");
            write_native_bundle(&bundle_path, &artifacts, &signatures, 0).unwrap();
            std::fs::read(&bundle_path).unwrap()
        };

        let _destination = Scratch::new("bundle-rollover-dst");
        import_bundle_bytes(&bundle).unwrap();
        for hash in [&hashes[0], &hashes[count / 2], &hashes[count - 1]] {
            assert!(file_utils::does_object_exist(hash).unwrap(), "{} must land", hash);
        }
    }

    /// Build a one-parcel/one-blob warehouse's native bundle in its own scratch, returning the
    /// bundle bytes, the signed parcel's hash and the blob's hash.
    fn build_tiny_native_bundle(name: &str) -> (Vec<u8>, String, String) {
        let _source = Scratch::new(name);

        let blob = Blob { content: b"version 1".to_vec() };
        let mut blob_object = LooseObjectBuilder::build_blob(&blob);
        blob_object.store().unwrap();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "a.txt".to_string(), blob_object.hash.clone(), DirEntryType::Normal
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("first parcel".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        let signature = raw_signature_sidecar("test-key", b"test-signature");
        sign_utils::store_raw_parcel_signature(&parcel_object.hash, &signature).unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let stats = build_bundle().unwrap();
        assert_eq!(stats.objects, 3, "the parcel, its tree and its one blob");
        assert_eq!(stats.deltas, 0);
        assert_eq!(stats.signatures, 1);

        (std::fs::read(&stats.path).unwrap(), parcel_object.hash.clone(), blob_object.hash.clone())
    }

    /// `bundle` without the digest that ends it.
    fn unsealed(bundle: &[u8]) -> &[u8] {
        &bundle[..bundle.len() - BUNDLE_DIGEST_BYTES]
    }

    /// `bytes` ended with their own digest, as the builder ends a bundle: a test that changes a
    /// built bundle re-seals it, so the import reaches the check the test is about instead of
    /// refusing the digest.
    fn sealed(mut bytes: Vec<u8>) -> Vec<u8> {
        let digest = blake3::hash(&bytes);
        bytes.extend(digest.as_bytes());
        bytes
    }

    /// `bundle` (from [`build_tiny_native_bundle`]) with its signature section replaced by
    /// `entries`, re-sealed. That section ends the body: a count, then one hash, length and
    /// sidecar.
    fn with_signature_section(bundle: &[u8], entries: &[(&str, &[u8])]) -> Vec<u8> {
        let body = unsealed(bundle);
        let sidecar = raw_signature_sidecar("test-key", b"test-signature");
        let start = body.len() - 8 - (64 + 8 + sidecar.len());
        assert_eq!(u64::from_be_bytes(body[start..start + 8].try_into().unwrap()), 1,
            "the tiny bundle's signature section holds one entry");

        let mut bytes = body[..start].to_vec();
        bytes.extend((entries.len() as u64).to_be_bytes());
        for (hash, payload) in entries {
            bytes.extend(hash.as_bytes());
            bytes.extend((payload.len() as u64).to_be_bytes());
            bytes.extend(*payload);
        }
        sealed(bytes)
    }

    /// Each signature entry becomes its own sidecar file, so one is admitted only for a parcel the
    /// bundle's packs carry, and only once. An entry naming a blob, a hash nowhere in the bundle,
    /// or a parcel already signed by an earlier entry is refused, and no sidecar is written for it.
    #[test]
    fn a_native_signature_entry_must_name_a_parcel_the_bundle_carries_once() {
        let (bundle, parcel_hash, blob_hash) = build_tiny_native_bundle("bundle-sig-admission-src");
        let sidecar = raw_signature_sidecar("test-key", b"test-signature");
        let absent = "f".repeat(64);

        {
            // The section rewrite itself is faithful: the bundle's own entry imports.
            let _destination = Scratch::new("bundle-sig-admission-control");
            let stats = import_bundle_bytes(&with_signature_section(&bundle, &[(&parcel_hash, &sidecar)])).unwrap();
            assert_eq!(stats.stored_signatures, 1);
        }

        for (name, signed) in [("blob", &blob_hash), ("absent", &absent)] {
            let _destination = Scratch::new("bundle-sig-admission-foreign");
            let error = import_bundle_bytes(&with_signature_section(&bundle, &[(signed, &sidecar)])).err()
                .unwrap_or_else(|| panic!("a signature for the {} must be refused", name));
            assert!(error.contains("not a parcel it carries"), "{}: {}", name, error);
            assert_eq!(sign_utils::load_raw_parcel_signature(signed).unwrap(), None, "{}: no sidecar lands", name);
        }

        let _destination = Scratch::new("bundle-sig-admission-repeat");
        let repeated = with_signature_section(&bundle, &[(&parcel_hash, &sidecar), (&parcel_hash, &sidecar)]);
        let error = import_bundle_bytes(&repeated).err().unwrap();
        assert!(error.contains("two signatures"), "{}", error);
    }

    /// The envelope/native-pack write-then-read semantics, end to end: a real (tiny)
    /// warehouse's `build_bundle` output, imported into a second, empty warehouse. The store
    /// census is the regression guard for the performance property: import publishes one
    /// aggregate pack and does not explode its objects into individually-fsynced loose files.
    #[test]
    fn build_bundle_then_import_bundle_round_trips_directly_into_a_native_pack() {
        let (bundle_bytes, _, _) = build_tiny_native_bundle("bundle-roundtrip-src");
        assert!(bundle_bytes.starts_with(format!("{}\n", BUNDLE_HEADER).as_bytes()));

        let _destination = Scratch::new("bundle-roundtrip-dst");
        let import_stats = import_bundle_bytes(&bundle_bytes).unwrap();
        assert_eq!(import_stats.stored_objects, 3);
        assert_eq!(import_stats.stored_signatures, 1);
        assert_eq!(import_stats.skipped_records, 0);

        let status = pack_utils::store_status().unwrap();
        assert_eq!(status.loose_objects, 0);
        assert_eq!(status.packed_objects, 3);
        assert_eq!(status.packs.len(), 1);

        // Re-importing the same bundle is idempotent: everything is now already present.
        let reimport_stats = import_bundle_bytes(&bundle_bytes).unwrap();
        assert_eq!(reimport_stats.stored_objects, 0);
        assert_eq!(reimport_stats.stored_signatures, 0);
        assert_eq!(reimport_stats.skipped_records, 4);
        assert_eq!(pack_utils::store_status().unwrap().packs.len(), 1);
    }

    /// Native packs are quarantined until every indexed object has reconstructed and matched its
    /// claimed hash. Changing the first record kind makes the payload invalid; no `.idx` commit
    /// point may become visible in the destination.
    #[test]
    fn corrupt_native_bundle_is_rejected_before_any_pack_is_published() {
        let (mut bundle_bytes, _, _) = build_tiny_native_bundle("bundle-corrupt-src");
        let table = BUNDLE_HEADER.len() + 1;
        let pack_count = u32::from_be_bytes(bundle_bytes[table..table + 4].try_into().unwrap());
        assert_eq!(pack_count, 1);

        let data_len = u64::from_be_bytes(
            bundle_bytes[table + 4..table + 12].try_into().unwrap()
        ) as usize;
        let data_start = table + 4 + pack_count as usize * 16;
        assert!(data_len > 12, "the native pack must contain at least one record");
        bundle_bytes[data_start + 12] = 0xff; // first record's kind byte
        let bundle_bytes = sealed(unsealed(&bundle_bytes).to_vec());

        let _destination = Scratch::new("bundle-corrupt-dst");
        let error = import_bundle_bytes(&bundle_bytes).err()
            .expect("a changed native record must fail verification");
        assert!(error.contains("unknown record kind"), "{}", error);

        let status = pack_utils::store_status().unwrap();
        assert_eq!(status.loose_objects, 0);
        assert_eq!(status.packed_objects, 0);
        assert!(status.packs.is_empty());
    }

    /// A bundle that is not the bytes its builder wrote is refused with the digest error before
    /// anything is stored. The first case changes one byte of the signature payload, the bytes a
    /// splice of two downloads could get wrong while every structural check still passes: without
    /// the digest, its packs would publish and the wrong sidecar would be stored. The others
    /// change the digest itself, and cut the file before its digest is complete.
    ///
    /// Revert: skip `verify_bundle_digest` → the first case imports (packs published, sidecar
    /// stored) and the second refuses the digest as trailing bytes → red.
    #[test]
    fn a_bundle_that_does_not_match_its_digest_is_refused_before_anything_is_stored() {
        let (bundle, parcel_hash, _) = build_tiny_native_bundle("bundle-digest-src");

        {
            let _destination = Scratch::new("bundle-digest-control");
            let stats = import_bundle_bytes(&bundle).unwrap();
            assert_eq!((stats.stored_objects, stats.stored_signatures), (3, 1), "the untouched bundle imports");
        }

        let mut spliced_signature = bundle.clone();
        let last_body_byte = bundle.len() - BUNDLE_DIGEST_BYTES - 1;
        spliced_signature[last_body_byte] ^= 0x01;
        let mut wrong_digest = bundle.clone();
        *wrong_digest.last_mut().unwrap() ^= 0x01;
        let cut_short = bundle[..BUNDLE_HEADER.len() + 1 + BUNDLE_DIGEST_BYTES - 1].to_vec();

        for (name, bytes) in [("spliced signature", spliced_signature), ("wrong digest", wrong_digest),
                              ("cut before its digest", cut_short)] {
            let _destination = Scratch::new("bundle-digest-refused");
            let error = import_bundle_bytes(&bytes).err()
                .unwrap_or_else(|| panic!("{}: a bundle that does not match its digest must be refused", name));
            assert!(is_bundle_digest_error(&error), "{}: {}", name, error);
            assert!(!is_unsupported_bundle_error(&error), "{}: {}", name, error);

            // The fresh destination has no objects folder, and import creates one for its first
            // quarantine temp: its absence shows nothing at all was written.
            assert!(!Path::new(&file_utils::get_path_objects_root()).exists(),
                "{}: nothing is written, not even a quarantine temp", name);
            assert_eq!(sign_utils::load_raw_parcel_signature(&parcel_hash).unwrap(), None,
                "{}: no sidecar is stored", name);
        }
    }

    /// The header decides the version before the digest is looked at: a bundle in the previous
    /// native version, which carries no digest, is reported unsupported (so a clone falls back to
    /// its walk) and not as a digest failure (which would fail it).
    ///
    /// Revert: run `verify_bundle_digest` before the header check → the old file's last 32 bytes
    /// are not its digest, so it is refused as a digest error → red.
    #[test]
    fn a_previous_version_bundle_is_unsupported_and_not_a_digest_failure() {
        let (bundle, _, _) = build_tiny_native_bundle("bundle-previous-version-src");
        let body = &unsealed(&bundle)[BUNDLE_HEADER.len()..];
        let mut previous = b"forklift-bundle 2026-07-13".to_vec();
        previous.extend(body);

        let _destination = Scratch::new("bundle-previous-version-dst");
        let error = import_bundle_bytes(&previous).err().expect("a previous-version bundle must be refused");
        assert!(is_unsupported_bundle_error(&error), "{}", error);
        assert!(!is_bundle_digest_error(&error), "{}", error);
        assert!(!Path::new(&file_utils::get_path_objects_root()).exists(), "nothing is written");
    }

    #[test]
    fn only_the_current_header_line_is_a_current_bundle_prefix() {
        let (bundle, _, _) = build_tiny_native_bundle("bundle-current-prefix");
        assert!(is_current_bundle_prefix(&bundle));
        assert!(is_current_bundle_prefix(format!("{}\n", BUNDLE_HEADER).as_bytes()));

        for prefix in [BUNDLE_HEADER.to_string(), format!("{}0\n", BUNDLE_HEADER),
                       "forklift-bundle 2026-07-13\n".to_string(), String::new()] {
            assert!(!is_current_bundle_prefix(prefix.as_bytes()), "{:?}", prefix);
        }
    }

    /// A destination that already carries a *different* sidecar for a bundled parcel keeps its
    /// own: the bundle's copy is skipped (first sidecar wins), never a failed import after the
    /// packs were already published.
    #[test]
    fn import_skips_a_conflicting_local_signature_instead_of_failing() {
        let (bundle_bytes, parcel_hash, _) = build_tiny_native_bundle("bundle-sig-conflict-src");

        let _destination = Scratch::new("bundle-sig-conflict-dst");
        let local = raw_signature_sidecar("another-key", b"a different signature");
        sign_utils::store_raw_parcel_signature(&parcel_hash, &local).unwrap();

        let stats = import_bundle_bytes(&bundle_bytes).unwrap();
        assert_eq!(stats.stored_objects, 3);
        assert_eq!(stats.stored_signatures, 0);
        assert_eq!(stats.skipped_records, 1, "the bundle's sidecar is skipped, not stored");

        let kept = sign_utils::load_raw_parcel_signature(&parcel_hash).unwrap().unwrap();
        assert_eq!(kept, local, "the local sidecar survives the import untouched");
    }

    /// Store a tree of `files` (name, blob hash) and `subtrees` (name, tree hash).
    fn store_tree(files: &[(&str, &str)], subtrees: &[(&str, &str)]) -> String {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash) in files {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), DirEntryType::Normal));
        }
        for (name, hash) in subtrees {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), DirEntryType::Tree));
        }
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();
        tree_object.hash
    }

    fn store_blob(content: &[u8]) -> String {
        let mut blob_object = LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() });
        blob_object.store().unwrap();
        blob_object.hash
    }

    /// Store a parcel of `tree_hash` on `parents` and make it `main`'s head.
    fn store_head_parcel(tree_hash: &str, parents: &[&str]) -> String {
        let parcel = Parcel {
            tree_hash: tree_hash.to_string(),
            parents: parents.iter().map(|parent| parent.to_string()).collect(),
            actions: Vec::new(),
            description: None,
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        parcel_object.hash
    }

    /// Each version of a file is stored as a delta against the version emitted before it at the
    /// same path, until the chain reaches `MAX_DELTA_CHAIN`; the next version starts over in full.
    /// A version seen again (a revert, in a tree that is itself new) is not stored twice, but
    /// becomes the base the following version deltas against — here one at the end of a chain, so
    /// that version is stored full.
    /// The bundle imports, every version reading back.
    #[test]
    fn a_bundle_deltas_each_version_against_the_last_at_its_path_and_caps_the_chain() {
        let source = Scratch::new("bundle-delta-chain");

        let shared: Vec<u8> = (0..4096u32).map(|i| b'a' + (i.wrapping_mul(2654435761) >> 27) as u8).collect();
        let version = |k: usize| [shared.as_slice(), format!("\nversion {}\n", k).as_bytes()].concat();

        let unchanged = store_blob(b"never changes");
        let dir = store_tree(&[("g.txt", &unchanged)], &[]);

        let mut versions: Vec<usize> = (0..MAX_DELTA_CHAIN as usize + 3).collect();
        // The revert to the chain's last delta (depth `MAX_DELTA_CHAIN`), then one new version.
        versions.push(MAX_DELTA_CHAIN as usize);
        versions.push(MAX_DELTA_CHAIN as usize + 3);

        let revert = versions.len() - 2;
        let note = store_blob(b"the revert parcel's own file");

        let mut parent: Option<String> = None;
        for (index, &k) in versions.iter().enumerate() {
            let f = store_blob(&version(k));
            // The revert's root differs from version 50's, so the walk visits it.
            let files: Vec<(&str, &str)> = if index == revert {
                vec![("f.txt", &f), ("note.txt", &note)]
            } else {
                vec![("f.txt", &f)]
            };
            let root = store_tree(&files, &[("dir", &dir)]);
            let parents: Vec<&str> = parent.iter().map(String::as_str).collect();
            parent = Some(store_head_parcel(&root, &parents));
        }

        PLAN_WALK_READS.with(|reads| reads.set(0));
        let stats = build_bundle().unwrap();
        // On a linear history the workers load every tree the walk reads.
        assert_eq!(PLAN_WALK_READS.with(|reads| reads.get()), 0, "trees the walk read itself");

        // Versions 0..=52 and 53: 0 full, 1..=50 deltas (depths 1..=50), 51 full (its base is 50
        // deep), 52 a delta, the revert to 50 no record, and 53 full (its base, 50, is 50 deep).
        assert_eq!(stats.deltas, MAX_DELTA_CHAIN as usize + 1, "deltas");
        // 55 parcels, 55 roots and `dir`, and the full blobs: versions 0, 51 and 53, `g.txt` and
        // `note.txt`.
        assert_eq!(stats.objects, 55 + 56 + 5, "full records");

        let bundle = std::fs::read(&stats.path).unwrap();
        // The order records are loaded in never reaches the bytes.
        build_bundle().unwrap();
        assert!(std::fs::read(&stats.path).unwrap() == bundle, "a rebuild writes the same bundle");
        drop(source);

        let _destination = Scratch::new("bundle-delta-chain-dst");
        let imported = import_bundle_bytes(&bundle).unwrap();
        assert_eq!(imported.stored_objects, stats.objects + stats.deltas);
        for k in 0..MAX_DELTA_CHAIN as usize + 4 {
            let content = version(k);
            let hash = LooseObjectBuilder::build_blob(&Blob { content: content.clone() }).hash;
            assert_eq!(object_utils::load_blob(&hash).unwrap().content, content, "version {}", k);
        }
    }

    /// A parcel whose changed trees exceed what the build loads ahead for one parcel still has
    /// every tree in its plan: the walk reads the rest itself. (Run at a small cap, so the fixture
    /// stays small: a build's own cap would need over a thousand stored trees.)
    #[test]
    fn a_bundle_carries_every_tree_of_a_parcel_past_the_prefetch_cap() {
        let _source = Scratch::new("bundle-prefetch-cap");

        const CAP: usize = 4;
        let directories: Vec<(String, String, String)> = (0..3 * CAP)
            .map(|i| {
                let blob = store_blob(format!("file {}", i).as_bytes());
                (format!("d{}", i), store_tree(&[("f.txt", &blob)], &[]), blob)
            })
            .collect();
        let subtrees: Vec<(&str, &str)> = directories.iter()
            .map(|(name, hash, _)| (name.as_str(), hash.as_str()))
            .collect();
        let root = store_tree(&[], &subtrees);
        let parcel = store_head_parcel(&root, &[]);

        PLAN_WALK_READS.with(|reads| reads.set(0));
        let plan = plan_emission(&[parcel.clone()], CAP).unwrap();
        assert!(PLAN_WALK_READS.with(|reads| reads.get()) > 0, "the walk read the trees past the cap");

        let planned = |kind: fn(&Emission) -> bool| -> HashSet<PlanHash> {
            plan.iter().filter(|item| kind(item)).map(|item| *item.hash()).collect()
        };
        let hashes = |hashes: &mut dyn Iterator<Item = &String>| -> HashSet<PlanHash> {
            hashes.map(|hash| plan_hash(hash).unwrap()).collect()
        };
        assert_eq!(plan.len(), 2 + 2 * directories.len(), "the parcel, its root, each tree and file");
        assert_eq!(planned(|item| matches!(item, Emission::Parcel { .. })), hashes(&mut std::iter::once(&parcel)));
        assert_eq!(planned(|item| matches!(item, Emission::Tree { .. })),
                   hashes(&mut std::iter::once(&root).chain(directories.iter().map(|(_, tree, _)| tree))));
        assert_eq!(planned(|item| matches!(item, Emission::Blob { .. })),
                   hashes(&mut directories.iter().map(|(_, _, blob)| blob)));

        let stats = build_bundle().unwrap();
        assert_eq!(stats.objects, plan.len());
        assert_eq!(stats.deltas, 0);
    }

    /// A warehouse with a chunked file anywhere in reachable history refuses to bundle: chunk
    /// transport has not shipped, so a bundle carrying only the recipe (never its chunks,
    /// structurally) would be silently incomplete. The refusal carries the stable code and
    /// names the file's path. This check lifts the moment chunk transport ships.
    #[test]
    fn build_bundle_refuses_a_warehouse_with_a_chunked_file() {
        let _scratch = Scratch::new("bundle-chunked-refuses");

        let chunk = Chunk { content: b"a chunk of a large file".to_vec() };
        let mut chunk_object = LooseObjectBuilder::build_chunk(&chunk);
        chunk_object.store().unwrap();

        let recipe = Recipe {
            content_hash: object_utils::hash_object_bytes(&chunk.content),
            total_size: chunk.content.len() as u64,
            chunks: vec![RecipeChunk { hash: chunk_object.hash.clone(), size: chunk.content.len() as u64 }],
        };
        let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);
        recipe_object.store().unwrap();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "big.bin".to_string(), recipe_object.hash.clone(), DirEntryType::NormalChunked
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("a chunked file".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let error = build_bundle().err().expect("a chunked file must refuse the bundle");
        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains("big.bin"), "the refusal names the path: {}", message);

        // Nothing was left behind: the bundle file is never renamed into place on failure.
        assert!(!get_latest_bundle_path().exists(), "a refused bundle must not be written");
    }

    /// Store an object over the whole-object ceiling, as a store authored before the ceiling holds
    /// one. `LooseObject::store` and `store_object_bytes` both refuse a fresh over-ceiling write,
    /// and no import admits one, so the fixture goes through `store_object_stream`, the one store
    /// path that does not enforce the ceiling.
    fn store_grandfathered_giant() -> crate::model::object::loose_object::LooseObject {
        let giant_object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0u8; object_utils::MAX_OBJECT_BYTES + 1],
        });
        let bytes = &giant_object.content;
        object_utils::store_object_stream(&giant_object.hash, &mut std::io::Cursor::new(bytes), bytes.len() as u64)
            .expect("the giant stores through the unchecked stream path");
        giant_object
    }

    /// A warehouse with a grandfathered giant blob (an object above the whole-object ceiling,
    /// authored before `MAX_OBJECT_BYTES` existed)
    /// refuses to bundle: no reader accepts a record that large (a version-3 reader refuses its
    /// declared length pre-read), so writing one would only produce a bundle nobody could finish
    /// importing. The refusal carries the stable code, names the path and the object, and nothing
    /// is written — the giant stays fully readable and checkout-able locally; only transport (and
    /// only transport) refuses, honestly, at the source.
    #[test]
    fn build_bundle_refuses_a_warehouse_with_a_grandfathered_giant_blob() {
        let _scratch = Scratch::new("bundle-giant-refuses");

        let giant_object = store_grandfathered_giant();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "big.bin".to_string(), giant_object.hash.clone(), DirEntryType::Normal
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("a grandfathered giant".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();

        pallet_utils::update_pallet_head(
            "main",
            pallet_utils::get_pallet_head("main").unwrap().as_deref(),
            &parcel_object.hash,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();

        let error = build_bundle().err().expect("a grandfathered giant must refuse the bundle");
        let (code, message, next_step) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains("big.bin"), "the refusal names the path: {}", message);
        assert!(message.contains(&giant_object.hash), "the refusal names the object: {}", message);
        assert!(next_step.contains("signed identity"), "states no migration exists: {}", next_step);

        // Nothing was left behind: the bundle file is never renamed into place on failure.
        assert!(!get_latest_bundle_path().exists(), "a refused bundle must not be written");
    }

    /// The same refusal on `build_partial_bundle` (`POST /v1/objects/batch`'s builder): a
    /// requested hash that resolves to a grandfathered giant is not silently omitted like an
    /// absent object — it fails the whole call loudly, so a consumer never receives (and chokes
    /// on) a partial bundle carrying a record no reader could finish importing anyway.
    #[test]
    fn build_partial_bundle_refuses_a_grandfathered_giant_blob() {
        let _scratch = Scratch::new("partial-bundle-giant-refuses");

        let giant_object = store_grandfathered_giant();

        let error = build_partial_bundle(&[giant_object.hash.clone()]).err()
            .expect("a grandfathered giant must refuse the partial bundle");
        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains(&giant_object.hash), "the refusal names the object: {}", message);
    }

    /// One record with an explicitly declared (possibly *lying*) length — for the bomb-defense
    /// tests, where the point is a length that disagrees with the bytes that follow.
    fn record_with_declared_len(kind: u8, hash: &str, declared_len: u64, payload: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.push(kind);
        bytes.extend(hash.as_bytes());
        bytes.extend(declared_len.to_be_bytes());
        bytes.extend(payload);
        bytes
    }

    /// A large object rides the streaming import path (above the buffered threshold) and lands
    /// byte-identical — the D/7 memory defense in the ordinary, honest case.
    #[test]
    fn import_streams_a_large_valid_object() {
        let _scratch = Scratch::new("import-stream-large");

        let object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0x7Eu8; object_utils::STREAM_STORE_THRESHOLD_BYTES + 300_000],
        });
        let raw = object.content.clone();
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &object.hash, &raw));

        let stats = import_batch_bytes(&bytes, &[&object.hash]).unwrap();
        assert_eq!(stats.stored_objects, 1);
        assert_eq!(file_utils::retrieve_object_by_hash(&object.hash).unwrap(), raw);
    }

    /// A streamed bundle refuses an `'O'` record whose *declared* length is above the object
    /// ceiling **before reading a byte** of it: the record declares a giant length but
    /// carries only a few real bytes, so a read would report truncation — the ceiling error
    /// (not truncation) proves the refusal happened pre-read.
    #[test]
    fn a_streamed_object_over_the_ceiling_is_refused_pre_read() {
        let _scratch = Scratch::new("import-ceiling-object");

        let hash = "a".repeat(64);
        let declared = object_utils::MAX_OBJECT_BYTES as u64 + 1;
        let record = record_with_declared_len(b'O', &hash, declared, b"only a few bytes");
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record);

        let error = import_batch_bytes(&bytes, &[&hash]).err().unwrap();
        assert!(error.contains("object ceiling"), "the ceiling refusal (not truncation): {}", error);
        assert!(!error.contains("truncated"), "must be refused before reading: {}", error);
    }

    /// An under-ceiling declared-length lie (the length is honest about the bytes, but the bytes
    /// do not hash to the claimed hash) is caught by the streaming hash check — nothing lands, and
    /// no temp file is left. Memory never exceeds the streaming bound because the object is never
    /// buffered whole (it is above the buffered threshold).
    #[test]
    fn a_large_object_that_lies_about_its_hash_is_refused_by_streaming() {
        let _scratch = Scratch::new("import-stream-lie");

        let object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0x4Du8; object_utils::STREAM_STORE_THRESHOLD_BYTES + 200_000],
        });
        let raw = object.content.clone();
        let wrong_hash = object_utils::hash_object_bytes(b"not what these bytes are");

        // Honest declared length (= the real payload), so this exercises the streaming *hash*
        // check, not the length check — the under-ceiling lie the ceiling alone cannot catch.
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &record(b'O', &wrong_hash, &raw));

        let error = import_batch_bytes(&bytes, &[&wrong_hash]).err().unwrap();
        assert!(error.contains("does not match its claimed hash"), "{}", error);
        assert!(!file_utils::does_object_exist(&wrong_hash).unwrap(), "nothing unverified may land");
    }

    /// [`raw_bundle`], but with the record stream's frame declaring a `window_log` window — a
    /// stream encoder with no pledged size writes the window into the frame header whatever the
    /// content, so a small bundle can declare a window as large as any bomb.
    fn bundle_with_window(header: &str, records: &[u8], window_log: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend(header.as_bytes());
        bytes.push(b'\n');

        let mut encoder = zstd::stream::Encoder::new(&mut bytes, 3).unwrap();
        encoder.window_log(window_log).unwrap();
        std::io::Write::write_all(&mut encoder, records).unwrap();
        encoder.finish().unwrap();
        bytes
    }

    /// Three object records and their hashes.
    fn three_objects() -> (Vec<u8>, Vec<String>) {
        let mut records = Vec::new();
        let mut hashes = Vec::new();

        for i in 0..3 {
            let content = format!("streamed object {} {}", i, "x".repeat(200 * i));
            let hash = object_utils::hash_object_bytes(content.as_bytes());
            records.extend(record(b'O', &hash, content.as_bytes()));
            hashes.push(hash);
        }

        (records, hashes)
    }

    #[test]
    fn a_streamed_bundle_cut_short_anywhere_fails_instead_of_importing_as_a_smaller_one() {
        // A transfer that ends early without an error of its own (a close-delimited body cut by
        // the peer) hands the importer a clean end of input mid-stream. Wherever that lands —
        // inside a record, or exactly between two — the import must fail: a bundle that loses
        // its tail is corrupt, never a smaller valid one.
        let (records, hashes) = three_objects();
        let asked: Vec<&str> = hashes.iter().map(String::as_str).collect();
        let bytes = raw_bundle(BUNDLE_HEADER_V3, &records);
        let header_len = BUNDLE_HEADER_V3.len() + 1;

        for cut in header_len..bytes.len() {
            let _scratch = Scratch::new("streamed-cut");
            let outcome = import_streamed_bundle(std::io::Cursor::new(&bytes[..cut]), requested(&asked));
            assert!(outcome.is_err(), "a bundle cut at byte {} of {} must not import", cut, bytes.len());
        }

        let _scratch = Scratch::new("streamed-whole");
        let stats = import_streamed_bundle(std::io::Cursor::new(&bytes), requested(&asked)).unwrap();
        assert_eq!(stats.stored_objects, 3);
        assert!(hashes.iter().all(|hash| file_utils::does_object_exist(hash).unwrap()));
    }

    #[test]
    fn a_streamed_bundle_is_accepted_only_at_version_3() {
        let _scratch = Scratch::new("streamed-versions");
        let (records, hashes) = three_objects();
        let asked: Vec<&str> = hashes.iter().map(String::as_str).collect();

        for header in [BUNDLE_HEADER, "forklift-bundle 2026-07-06", "forklift-bundle 2026-07-03"] {
            let error = import_streamed_bundle(std::io::Cursor::new(raw_bundle(header, &records)), requested(&asked))
                .err().unwrap_or_else(|| panic!("a \"{}\" bundle must be refused on the streamed path", header));
            assert!(error.contains("refusing to import it"), "{}", error);
        }

        assert!(import_streamed_bundle(std::io::Cursor::new(raw_bundle(BUNDLE_HEADER_V3, &records)), requested(&asked)).is_ok());
    }

    /// The streamed path is the one a remote's batch and subtree answers reach, so the hostile
    /// declared length the fuzz suite pins is pinned here too: a requested record declaring
    /// `u64::MAX` bytes is an error, never an allocation of them.
    #[test]
    fn a_streamed_record_declaring_a_huge_length_is_refused_without_allocating_it() {
        let _scratch = Scratch::new("streamed-huge-length");
        let hash = "a".repeat(64);
        let hostile = record_with_declared_len(b'O', &hash, u64::MAX, b"only a few real bytes");

        let outcome = import_batch_bytes(&raw_bundle(BUNDLE_HEADER_V3, &hostile), &[&hash]);
        let error = outcome.err().expect("a record of declared length u64::MAX must be refused");
        assert!(error.contains("declares 18446744073709551615 bytes"),
            "the record is refused on its declared length before any of it is read: {}", error);
    }

    #[test]
    fn a_bundle_stream_declaring_a_window_past_the_bound_is_refused_and_one_at_it_imports() {
        let (records, hashes) = three_objects();
        let asked: Vec<&str> = hashes.iter().map(String::as_str).collect();

        let over = bundle_with_window(BUNDLE_HEADER_V3, &records, object_utils::MAX_DECODE_WINDOW_LOG + 1);
        {
            let _scratch = Scratch::new("window-over");
            assert!(import_streamed_bundle(std::io::Cursor::new(&over), requested(&asked)).is_err(),
                "a streamed bundle over the window bound is refused");
            assert!(!hashes.iter().any(|hash| file_utils::does_object_exist(hash).unwrap()),
                "the refusal comes before the first record");
        }

        let at = bundle_with_window(BUNDLE_HEADER_V3, &records, object_utils::MAX_DECODE_WINDOW_LOG);
        let _scratch = Scratch::new("window-at");
        assert_eq!(import_streamed_bundle(std::io::Cursor::new(&at), requested(&asked)).unwrap().stored_objects, 3);
    }
}
