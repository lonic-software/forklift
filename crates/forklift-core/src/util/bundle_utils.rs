//! Bundles — many objects in one zstd stream (`docs/format/BUNDLE_FORMAT.md`).
//! A bundle is a clone-time optimization, never a source of truth: every record is
//! hash-verified on import, and anything a bundle lacks is fetched loose.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use crate::globals::forklift_root;
use crate::util::{audit_utils, delta_utils, file_utils, object_utils, pallet_utils, sign_utils};

/// The uncompressed ASCII header line (without the newline) that opens every bundle this
/// build *writes*. Version 2 (2026-07-06, §9.1 #1) added delta records (`KIND_DELTA`).
pub const BUNDLE_HEADER: &str = "forklift-bundle 2026-07-06";

/// The version-1 header (2026-07-03), before delta records. Still *read*, so an older
/// server's bundle imports fine: a v1 bundle has only 'O'/'S' records, which this build
/// understands. Only the writer moved to v2. An unknown header still means "refuse the
/// bundle, fall back to loose objects" — so an old client refuses a v2 bundle gracefully.
const BUNDLE_HEADER_V1: &str = "forklift-bundle 2026-07-03";

/// The record kind of a raw (full) object.
const KIND_OBJECT: u8 = b'O';

/// The record kind of a parcel signature sidecar.
const KIND_SIGNATURE: u8 = b'S';

/// The record kind of a delta object (§9.1 #1). Its payload is
/// `base-hash (64 ASCII hex) || decompressed-length (8, big-endian u64) || zstd frame`,
/// the frame compressed with the base object as a dictionary (`delta_utils`). The importer
/// loads the base, reconstructs the object, and hash-verifies it like any other — so a bad
/// delta only ever fails import, never corrupts the store.
const KIND_DELTA: u8 = b'D';

/// The longest delta chain the builder forms at one path: after this many successive
/// deltas, the next version is stored in full. This caps both the reconstruction cost of a
/// blob and the blast radius of a missing base (git's pack chain limit is the same order).
const MAX_DELTA_CHAIN: u32 = 50;

/// The folder (inside the forklift root) where bundles live.
const FOLDER_NAME_BUNDLES: &str = "bundles";

/// The file name of the most recent bundle.
const FILE_NAME_LATEST: &str = "latest";

/// What a bundle build packed.
pub struct BuildStats {
    /// Objects stored in full (`'O'` records).
    pub objects: usize,

    /// Blob versions stored as a delta against an earlier version (`'D'` records).
    pub deltas: usize,

    pub signatures: usize,
    pub path: PathBuf,
}

/// What a bundle import actually stored (records already present are skipped).
#[derive(Default)]
pub struct ImportStats {
    pub stored_objects: usize,
    pub stored_signatures: usize,
    pub skipped_records: usize,
}

/// The path of the latest bundle of the current warehouse.
pub fn get_latest_bundle_path() -> PathBuf {
    forklift_root()
        .join(FOLDER_NAME_BUNDLES)
        .join(FILE_NAME_LATEST)
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
    }

    let temporary_path = bundle_path.with_file_name(format!("latest.tmp{}", std::process::id()));

    let file = std::fs::File::create(&temporary_path)
        .map_err(|e| format!("Error while creating the bundle file: {}", e))?;

    let mut writer = std::io::BufWriter::new(file);

    writer.write_all(BUNDLE_HEADER.as_bytes())
        .and_then(|_| writer.write_all(b"\n"))
        .map_err(|e| format!("Error while writing the bundle header: {}", e))?;

    let mut encoder = zstd::stream::Encoder::new(writer, 0)
        .map_err(|e| format!("Error while starting the bundle stream: {}", e))?;

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

    let mut seen_trees: HashSet<String> = HashSet::new();
    // Every blob already emitted, mapped to its delta-chain depth (0 = stored in full).
    let mut emitted_depth: HashMap<String, u32> = HashMap::new();
    // The most recently emitted blob at each file path — the base a later version deltas against.
    let mut latest_blob_at_path: HashMap<String, String> = HashMap::new();

    for parcel_hash in &order {
        write_record(&mut encoder, KIND_OBJECT, parcel_hash,
                     &file_utils::retrieve_object_by_hash(parcel_hash)?)?;
        stats.objects += 1;

        if let Some(sidecar) = sign_utils::load_raw_parcel_signature(parcel_hash)? {
            write_record(&mut encoder, KIND_SIGNATURE, parcel_hash, &sidecar)?;
            stats.signatures += 1;
        }

        let tree_hash = object_utils::load_parcel(parcel_hash)?.tree_hash;

        write_tree_closure(&mut encoder, &tree_hash, "", &mut seen_trees,
                           &mut emitted_depth, &mut latest_blob_at_path, &mut stats)?;
    }

    let writer = encoder.finish()
        .map_err(|e| format!("Error while finishing the bundle stream: {}", e))?;

    writer.into_inner()
        .map_err(|e| format!("Error while flushing the bundle file: {}", e))?
        .sync_all()
        .map_err(|e| format!("Error while syncing the bundle file: {}", e))?;

    std::fs::rename(&temporary_path, &bundle_path)
        .map_err(|e| format!("Error while moving the bundle into place: {}", e))?;

    Ok(stats)
}

/// Build an in-memory partial bundle of the given objects (`POST /v1/objects/batch` —
/// the incremental counterpart of the full bundle). Objects that do not exist here are
/// skipped silently: the client notices what did not arrive and falls back to loose
/// fetches, so a partially-stocked remote degrades instead of failing.
///
/// # Arguments
/// * `hashes` - The objects to pack.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The bundle bytes (header line + one zstd stream of records).
/// * `Err(String)` - If a present object could not be read.
pub fn build_partial_bundle(hashes: &[String]) -> Result<Vec<u8>, String> {
    let mut bytes: Vec<u8> = Vec::new();

    bytes.extend_from_slice(BUNDLE_HEADER.as_bytes());
    bytes.push(b'\n');

    let mut encoder = zstd::stream::Encoder::new(&mut bytes, 0)
        .map_err(|e| format!("Error while starting the bundle stream: {}", e))?;

    for hash in hashes {
        if !file_utils::does_object_exist(hash)? {
            continue;
        }

        write_record(&mut encoder, KIND_OBJECT, hash,
                     &file_utils::retrieve_object_by_hash(hash)?)?;
    }

    encoder.finish()
        .map_err(|e| format!("Error while finishing the bundle stream: {}", e))?;

    Ok(bytes)
}

/// Write a tree's closure (trees and blobs, deduplicated) into the bundle, tracking the
/// path to each blob so successive versions of a file can be delta-compressed. Trees and
/// parcels are always stored in full; only blobs (where the version redundancy lives) are
/// considered for deltas.
fn write_tree_closure<W: Write>(encoder: &mut zstd::stream::Encoder<'_, W>,
                                tree_hash: &str,
                                path_prefix: &str,
                                seen_trees: &mut HashSet<String>,
                                emitted_depth: &mut HashMap<String, u32>,
                                latest_blob_at_path: &mut HashMap<String, String>,
                                stats: &mut BuildStats) -> Result<(), String> {
    if !seen_trees.insert(tree_hash.to_string()) {
        return Ok(());
    }

    write_record(encoder, KIND_OBJECT, tree_hash,
                 &file_utils::retrieve_object_by_hash(tree_hash)?)?;
    stats.objects += 1;

    let tree = object_utils::load_tree(tree_hash)?;

    for (name, file) in tree.get_files() {
        let path = join_path(path_prefix, name);
        emit_blob(encoder, &file.hash, &path, emitted_depth, latest_blob_at_path, stats)?;
    }

    for (name, subtree) in tree.get_subtrees() {
        let child = join_path(path_prefix, name);
        write_tree_closure(encoder, &subtree.hash, &child,
                           seen_trees, emitted_depth, latest_blob_at_path, stats)?;
    }

    Ok(())
}

/// Emit one blob — as a delta against the previous version at the same path when that saves
/// space, otherwise in full. Records the blob's chain depth so the chain stays bounded and
/// a later version chains from the right base.
fn emit_blob<W: Write>(encoder: &mut zstd::stream::Encoder<'_, W>,
                       blob_hash: &str,
                       path: &str,
                       emitted_depth: &mut HashMap<String, u32>,
                       latest_blob_at_path: &mut HashMap<String, String>,
                       stats: &mut BuildStats) -> Result<(), String> {
    // Already in the bundle: don't re-emit, but let a later version at this path delta
    // against it (it is the newest content seen here).
    if emitted_depth.contains_key(blob_hash) {
        latest_blob_at_path.insert(path.to_string(), blob_hash.to_string());
        return Ok(());
    }

    let target_bytes = file_utils::retrieve_object_by_hash(blob_hash)?;

    let mut depth = 0u32;
    let mut emitted_as_delta = false;

    if let Some(base_hash) = latest_blob_at_path.get(path).cloned() {
        let base_depth = *emitted_depth.get(&base_hash).unwrap_or(&0);

        // An over-large object is stored full, never delta'd — the same rule `pack_utils`
        // applies, and the one that lets `decompress_delta` enforce a real bomb ceiling on
        // the read side (`delta_utils::MAX_DELTA_TARGET_BYTES`).
        let deltable = target_bytes.len() <= delta_utils::MAX_DELTA_TARGET_BYTES;

        if deltable && base_depth < MAX_DELTA_CHAIN && base_hash != blob_hash {
            let base_bytes = file_utils::retrieve_object_by_hash(&base_hash)?;
            let delta = delta_utils::compress_delta(&base_bytes, &target_bytes)?;

            // Only worth a delta record when it is actually smaller than the object it
            // replaces (a wholly-rewritten file is cheaper stored in full).
            if delta.len() < target_bytes.len() {
                write_delta_record(encoder, blob_hash, &base_hash, target_bytes.len() as u64, &delta)?;
                stats.deltas += 1;
                emitted_as_delta = true;
                depth = base_depth + 1;
            }
        }
    }

    if !emitted_as_delta {
        write_record(encoder, KIND_OBJECT, blob_hash, &target_bytes)?;
        stats.objects += 1;
    }

    emitted_depth.insert(blob_hash.to_string(), depth);
    latest_blob_at_path.insert(path.to_string(), blob_hash.to_string());

    Ok(())
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

/// Write one delta record ('D'): the outer framing is identical to a normal record (kind,
/// target hash, length, payload), so the importer reads it the same way — only the payload
/// is structured `base-hash (64) || decompressed-length (8, big-endian) || zstd frame`.
fn write_delta_record<W: Write>(encoder: &mut zstd::stream::Encoder<'_, W>,
                                target_hash: &str,
                                base_hash: &str,
                                decompressed_len: u64,
                                delta: &[u8]) -> Result<(), String> {
    if base_hash.len() != 64 {
        return Err(format!("Cannot delta against \"{}\": not a 64-character hash.", base_hash));
    }

    let mut payload: Vec<u8> = Vec::with_capacity(64 + 8 + delta.len());
    payload.extend_from_slice(base_hash.as_bytes());
    payload.extend_from_slice(&decompressed_len.to_be_bytes());
    payload.extend_from_slice(delta);

    write_record(encoder, KIND_DELTA, target_hash, &payload)
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

/// Import a bundle file into the object store. Objects already present are skipped;
/// every stored object is hash-verified; a mismatching record fails the import (the
/// bundle is corrupt — nothing unverified may land).
///
/// # Arguments
/// * `path` - The bundle file.
///
/// # Returns
/// * `Ok(ImportStats)` - What was stored.
/// * `Err(String)`     - On an unknown header, a corrupt record, or an I/O error.
pub fn import_bundle(path: &Path) -> Result<ImportStats, String> {
    let file = std::fs::File::open(path)
        .map_err(|e| format!("Error while opening the bundle file: {}", e))?;

    import_bundle_reader(std::io::BufReader::new(file))
}

/// Import an in-memory bundle (a `POST /v1/objects/batch` response) into the object
/// store — the same verification as `import_bundle`.
pub fn import_bundle_bytes(bytes: &[u8]) -> Result<ImportStats, String> {
    import_bundle_reader(std::io::Cursor::new(bytes))
}

/// Import a bundle from a reader (see `import_bundle`).
fn import_bundle_reader<R: std::io::BufRead>(mut reader: R) -> Result<ImportStats, String> {
    // The header line is uncompressed; everything after it is one zstd stream.
    let mut header: Vec<u8> = Vec::new();

    std::io::BufRead::read_until(&mut reader, b'\n', &mut header)
        .map_err(|e| format!("Error while reading the bundle header: {}", e))?;

    if header.last() == Some(&b'\n') {
        header.pop();
    }

    if header != BUNDLE_HEADER.as_bytes() && header != BUNDLE_HEADER_V1.as_bytes() {
        return Err(format!(
            "Unknown bundle header \"{}\" (this build reads \"{}\" and \"{}\").",
            String::from_utf8_lossy(&header),
            BUNDLE_HEADER, BUNDLE_HEADER_V1
        ));
    }

    let mut decoder = zstd::stream::Decoder::new(reader)
        .map_err(|e| format!("Error while opening the bundle stream: {}", e))?;

    let mut stats = ImportStats::default();

    loop {
        let mut kind = [0u8; 1];

        match decoder.read_exact(&mut kind) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(format!("Error while reading the bundle: {}", e)),
        }

        let mut hash_bytes = [0u8; 64];
        let mut length_bytes = [0u8; 8];

        decoder.read_exact(&mut hash_bytes)
            .and_then(|_| decoder.read_exact(&mut length_bytes))
            .map_err(|e| format!("The bundle is truncated: {}", e))?;

        let hash = String::from_utf8(hash_bytes.to_vec())
            .map_err(|_| "A bundle record's hash is not valid ASCII.".to_string())?;

        let length = u64::from_be_bytes(length_bytes) as usize;

        // `length` is attacker-controlled (a bundle can arrive from an untrusted remote over
        // `franchise`), so never pre-allocate it: a lie like `u64::MAX` would be a one-record
        // denial of service (a capacity-overflow panic, or an allocator abort for a large-but-
        // representable value). Read as a bounded stream instead — the buffer grows with the
        // bytes actually present, and a short stream is reported as truncation, exactly as the
        // former `read_exact` did.
        let mut payload = Vec::new();
        let read = decoder.by_ref().take(length as u64).read_to_end(&mut payload)
            .map_err(|e| format!("The bundle is truncated: {}", e))?;

        if read != length {
            return Err(format!(
                "The bundle is truncated: a record declared {} bytes but only {} remained.",
                length, read
            ));
        }

        match kind[0] {
            KIND_OBJECT => {
                if object_utils::store_object_bytes(&hash, &payload)? {
                    stats.stored_objects += 1;
                } else {
                    stats.skipped_records += 1;
                }
            }
            KIND_DELTA => {
                // Reconstruct against the base (already stored), then store — which
                // hash-verifies, so a wrong reconstruction is rejected here.
                let object = reconstruct_delta(&hash, &payload)?;

                if object_utils::store_object_bytes(&hash, &object)? {
                    stats.stored_objects += 1;
                } else {
                    stats.skipped_records += 1;
                }
            }
            KIND_SIGNATURE => {
                if sign_utils::load_raw_parcel_signature(&hash)?.is_none() {
                    sign_utils::store_raw_parcel_signature(&hash, &payload)?;
                    stats.stored_signatures += 1;
                } else {
                    stats.skipped_records += 1;
                }
            }
            other => {
                return Err(format!("Unknown bundle record kind 0x{:02x}.", other));
            }
        }
    }

    Ok(stats)
}

/// Reconstruct a delta record's object from its payload
/// (`base-hash (64) || decompressed-length (8, big-endian) || zstd frame`): load the base
/// from the store and decompress the frame against it. The caller hash-verifies the result
/// before storing, so this never has to trust the reconstruction.
///
/// # Arguments
/// * `target_hash` - The hash the reconstructed object must have (for error messages).
/// * `payload`     - The delta record's payload.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The reconstructed object bytes.
/// * `Err(String)` - If the payload is truncated, the base is absent, or decoding fails.
fn reconstruct_delta(target_hash: &str, payload: &[u8]) -> Result<Vec<u8>, String> {
    // 64-byte base hash + 8-byte length prefix.
    if payload.len() < 72 {
        return Err(format!("The bundle delta record for {} is truncated.", target_hash));
    }

    let base_hash = std::str::from_utf8(&payload[0..64])
        .map_err(|_| format!("The bundle delta for {} has a non-ASCII base hash.", target_hash))?;

    let mut length_bytes = [0u8; 8];
    length_bytes.copy_from_slice(&payload[64..72]);
    let decompressed_len = u64::from_be_bytes(length_bytes) as usize;

    let frame = &payload[72..];

    // The base must already be in the store — the builder emits it before the delta, and an
    // incremental import may already hold it. An absent base is a corrupt/misordered bundle.
    let base_bytes = file_utils::retrieve_object_by_hash(base_hash).map_err(|_| format!(
        "The bundle delta for {} references base {}, which is not present; the bundle is corrupt.",
        target_hash, base_hash
    ))?;

    delta_utils::decompress_delta(&base_bytes, frame, decompressed_len)
}
