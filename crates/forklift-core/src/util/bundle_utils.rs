//! Bundles — many objects in one zstd stream (`docs/format/BUNDLE_FORMAT.md`).
//! A bundle is a clone-time optimization, never a source of truth: every record is
//! hash-verified on import, and anything a bundle lacks is fetched loose.

use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use crate::globals::forklift_root;
use crate::util::{audit_utils, delta_utils, file_utils, object_utils, pallet_utils, scope_utils, sign_utils};

/// The uncompressed ASCII header line (without the newline) that opens every bundle this
/// build *writes*. Version 3 (2026-07-11) carries no new record kind — it marks the writer as one
/// that never emits a record above the whole-object ceiling (`object_utils::MAX_OBJECT_BYTES`), so
/// a reader of a version-3 bundle may refuse an over-ceiling `'O'`/`'D'` record *before* reading a
/// byte of its payload (the ceiling as policy; see `import_bundle_reader`).
pub const BUNDLE_HEADER: &str = "forklift-bundle 2026-07-11";

/// The version-2 header (2026-07-06, §9.1 #1), which added delta records (`KIND_DELTA`). Still
/// *read*: a version-2 bundle predates the ceiling, so it may carry a grandfathered giant `'O'`
/// record and is therefore **not** hard-refused on declared size — its oversized objects stream
/// bounded (see `import_bundle_reader`).
const BUNDLE_HEADER_V2: &str = "forklift-bundle 2026-07-06";

/// The version-1 header (2026-07-03), before delta records. Still *read*, so an older
/// server's bundle imports fine: a v1 bundle has only 'O'/'S' records, which this build
/// understands. Only the writer moved forward. An unknown header still means "refuse the
/// bundle, fall back to loose objects" — so an old client refuses a newer bundle gracefully.
const BUNDLE_HEADER_V1: &str = "forklift-bundle 2026-07-03";

/// The longest a bundle's opening header line may run before a newline, before the import is
/// refused outright rather than searching further. Every header this build recognizes
/// (`BUNDLE_HEADER`/`BUNDLE_HEADER_V2`/`BUNDLE_HEADER_V1`) is under 30 bytes; 128 leaves headroom
/// for a longer future header while staying a small, fixed bound — a hostile bundle with no
/// newline (or a very long run before one) cannot grow the header buffer past it, regardless of
/// how long the underlying stream actually is.
const MAX_HEADER_BYTES: usize = 128;

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
        let parcel_bytes = file_utils::retrieve_object_by_hash(parcel_hash)?;
        refuse_if_transporting_over_ceiling(&format!("parcel {}", parcel_hash), parcel_bytes.len())?;

        write_record(&mut encoder, KIND_OBJECT, parcel_hash, &parcel_bytes)?;
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
/// fetches, so a partially-stocked remote degrades instead of failing. An object that
/// *does* exist but is above the whole-object ceiling (a grandfathered giant) is not
/// silently skipped like an absent one — it fails the whole call loudly, the same
/// writer-side refusal `build_bundle` gives, rather than silently omitting content the
/// caller asked for by name.
///
/// # Arguments
/// * `hashes` - The objects to pack.
///
/// # Returns
/// * `Ok(Vec<u8>)` - The bundle bytes (header line + one zstd stream of records).
/// * `Err(String)` - If a present object could not be read, or one is above the ceiling.
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

        let object_bytes = file_utils::retrieve_object_by_hash(hash)?;
        refuse_if_transporting_over_ceiling(&format!("object {}", hash), object_bytes.len())?;

        write_record(&mut encoder, KIND_OBJECT, hash, &object_bytes)?;
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

    let tree_bytes = file_utils::retrieve_object_by_hash(tree_hash)?;
    let directory = if path_prefix.is_empty() { "/" } else { path_prefix };
    refuse_if_transporting_over_ceiling(
        &format!("the directory \"{}\" (object {})", directory, tree_hash), tree_bytes.len()
    )?;

    write_record(encoder, KIND_OBJECT, tree_hash, &tree_bytes)?;
    stats.objects += 1;

    let tree = object_utils::load_tree(tree_hash)?;

    for (name, file) in tree.get_files() {
        let path = join_path(path_prefix, name);

        // Chunk transport (uploading/fetching the chunk objects a recipe references) is not
        // wired up yet: a bundle can carry a chunked file's *recipe* just like any other small
        // object (`emit_blob` below would happily do it — a recipe is not decoded here, just
        // moved as bytes), but its chunks are excluded from bundles structurally (this walk
        // never descends into a recipe), so the result would be a bundle over a file that can
        // never be materialized wherever it lands. Refuse loudly rather than ship it silently
        // incomplete. Remove this check once chunk transport ships.
        if file.item_type.is_chunked() {
            return Err(scope_utils::chunked_transport_refusal(&path));
        }

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
    refuse_if_transporting_over_ceiling(
        &format!("\"{}\" (object {})", path, blob_hash), target_bytes.len()
    )?;

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

/// Refuse to write a record above the whole-object ceiling into a bundle — the writer-side half
/// of the maintainer's chosen posture for a grandfathered giant (an object authored, or imported
/// via an old-version bundle, before `MAX_OBJECT_BYTES` existed): it stays readable locally
/// forever, but a bundle must never carry it, because no reader accepts it (a version-3 reader
/// refuses its declared length pre-read; an older reader would only rediscover the problem on the
/// far end after streaming it). Checked here, right after an object's bytes are loaded and before
/// a single byte is written into the bundle stream — so a warehouse holding one anywhere in the
/// closure fails loudly at the source, before writing anything a consumer could partially import.
fn refuse_if_transporting_over_ceiling(what: &str, len: usize) -> Result<(), String> {
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
    // The header line is uncompressed; everything after it is one zstd stream. `length` is not
    // the only attacker-controlled number here: a hostile bundle need not carry a newline at all
    // (or may run arbitrarily many non-newline bytes before one), and an unbounded
    // `read_until(b'\n', ...)` would grow `header` without limit chasing it — the same
    // unbounded-growth shape the record-length fix elsewhere in this function exists to close.
    // Every header this build recognizes is under 30 bytes, so bound the search to a small,
    // fixed cap: past it, this cannot be a bundle this build understands, however long the
    // stream actually runs.
    let mut header: Vec<u8> = Vec::new();
    let mut limited = reader.by_ref().take(MAX_HEADER_BYTES as u64);

    std::io::BufRead::read_until(&mut limited, b'\n', &mut header)
        .map_err(|e| format!("Error while reading the bundle header: {}", e))?;

    if header.last() != Some(&b'\n') {
        return Err(format!(
            "This does not look like a forklift bundle: no newline within the first {} bytes.",
            MAX_HEADER_BYTES
        ));
    }

    header.pop();

    if header != BUNDLE_HEADER.as_bytes()
        && header != BUNDLE_HEADER_V2.as_bytes()
        && header != BUNDLE_HEADER_V1.as_bytes() {
        return Err(format!(
            "Unknown bundle header \"{}\" (this build reads \"{}\", \"{}\" and \"{}\").",
            String::from_utf8_lossy(&header),
            BUNDLE_HEADER, BUNDLE_HEADER_V2, BUNDLE_HEADER_V1
        ));
    }

    // A version-3 bundle is written by a build that never emits an over-ceiling record, so its
    // `'O'`/`'D'` records may be refused on declared size *before* a byte is read (the ceiling as
    // policy). Older bundles predate the ceiling and may carry a grandfathered giant, so they are
    // not hard-refused — their oversized `'O'` records still stream bounded (below), which is the
    // unconditional memory defense regardless of version.
    let is_new_version = header == BUNDLE_HEADER.as_bytes();

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

        let length = u64::from_be_bytes(length_bytes);

        // The ceiling as *policy*, only on a version-3 bundle (whose writer never emits an
        // over-ceiling record): refuse before a byte of the payload is read. This is cheap and
        // sound, but it is not the memory defense — that is the streaming `'O'` path below, which
        // bounds memory even on an older bundle the ceiling does not gate.
        if is_new_version && length > object_utils::MAX_OBJECT_BYTES as u64 {
            return Err(oversized_record_refusal(kind[0], length));
        }

        match kind[0] {
            KIND_OBJECT => {
                // A small object is buffered whole and stored in one shot; a large one streams
                // through an incremental Blake3 to a temp file, bounding memory regardless of the
                // declared length or the bundle version. `store_object_stream` reports truncation,
                // verifies the hash, and enforces the per-chunk ceiling itself.
                let stored = if length <= object_utils::STREAM_STORE_THRESHOLD_BYTES as u64 {
                    let payload = read_exact_payload(&mut decoder, length)?;
                    object_utils::store_object_bytes(&hash, &payload)?
                } else {
                    object_utils::store_object_stream(&hash, decoder.by_ref(), length)?
                };

                if stored { stats.stored_objects += 1; } else { stats.skipped_records += 1; }
            }
            KIND_DELTA => {
                // A delta's payload is 72 bytes of framing plus its zstd frame; no writer — of any
                // version — ever emits a delta near the object ceiling (a delta targets at most
                // `MAX_DELTA_TARGET_BYTES`, 16 MiB). Cap the *declared* length unconditionally: that
                // is what keeps a hostile frame from being read whole into memory as a bomb, on any
                // bundle version, before `reconstruct_delta`'s own bounded decompression even runs.
                if length > object_utils::MAX_OBJECT_BYTES as u64 {
                    return Err(oversized_record_refusal(kind[0], length));
                }

                let payload = read_exact_payload(&mut decoder, length)?;

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
                // A signature sidecar is small; cap its declared length too so a lie cannot read
                // unbounded bytes into memory (this record kind is never streamed).
                if length > object_utils::MAX_OBJECT_BYTES as u64 {
                    return Err(oversized_record_refusal(kind[0], length));
                }

                let payload = read_exact_payload(&mut decoder, length)?;

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
fn oversized_record_refusal(kind: u8, length: u64) -> String {
    let kind_name = match kind {
        KIND_OBJECT => "object",
        KIND_DELTA => "delta",
        KIND_SIGNATURE => "signature",
        _ => "record",
    };

    format!(
        "A bundle {} record declares {} bytes, above the {}-byte object ceiling; refusing the bundle.",
        kind_name, length, object_utils::MAX_OBJECT_BYTES
    )
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

    /// Build a minimal (uncompressed-header + one zstd stream) bundle byte string from
    /// raw records, exactly as `import_bundle_reader` expects to read one — the manual
    /// low-level construction the fuzz suite also uses, but here to hit semantic
    /// (not just never-panic) branches.
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
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &hash, content));

        let stats = import_bundle_bytes(&bytes).unwrap();
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
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &wrong_hash, content));

        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("does not match its claimed hash"), "{}", error);
        assert!(!file_utils::does_object_exist(&wrong_hash).unwrap(), "nothing unverified may land");
    }

    #[test]
    fn import_skips_an_object_already_present() {
        let _scratch = Scratch::new("import-object-skip");

        let content = b"already here";
        let hash = object_utils::hash_object_bytes(content);
        object_utils::store_object_bytes(&hash, content).unwrap();

        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &hash, content));
        let stats = import_bundle_bytes(&bytes).unwrap();

        assert_eq!(stats.stored_objects, 0);
        assert_eq!(stats.skipped_records, 1);
    }

    #[test]
    fn import_stores_a_signature_record_and_skips_a_duplicate() {
        let _scratch = Scratch::new("import-signature");

        // The signature path shares the object's hash-sharded folder, so (as in a real
        // bundle, where the parcel record always precedes its signature) the parcel
        // object must already be stored.
        let parcel_content = b"a stand-in parcel object";
        let parcel_hash = object_utils::hash_object_bytes(parcel_content);
        object_utils::store_object_bytes(&parcel_hash, parcel_content).unwrap();

        let sidecar = raw_signature_sidecar("key-1", &[7u8; 64]);
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'S', &parcel_hash, &sidecar));

        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_signatures, 1);
        assert_eq!(sign_utils::load_raw_parcel_signature(&parcel_hash).unwrap(), Some(sidecar.clone()));

        // The same sidecar again is a duplicate, not an error.
        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_signatures, 0);
        assert_eq!(stats.skipped_records, 1);
    }

    #[test]
    fn import_reconstructs_a_valid_delta_record() {
        let _scratch = Scratch::new("import-delta-ok");

        let base = b"the quick brown fox\n".repeat(8);
        let mut target = base.clone();
        target.extend_from_slice(b"one more line\n");
        let target_hash = object_utils::hash_object_bytes(&target);
        let base_hash = object_utils::hash_object_bytes(&base);

        object_utils::store_object_bytes(&base_hash, &base).unwrap();

        let frame = delta_utils::compress_delta(&base, &target).unwrap();
        let mut payload = Vec::new();
        payload.extend(base_hash.as_bytes());
        payload.extend((target.len() as u64).to_be_bytes());
        payload.extend(&frame);

        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'D', &target_hash, &payload));
        let stats = import_bundle_bytes(&bytes).unwrap();

        assert_eq!(stats.stored_objects, 1);
        assert_eq!(file_utils::retrieve_object_by_hash(&target_hash).unwrap(), target);
    }

    #[test]
    fn import_rejects_a_delta_record_whose_base_is_missing() {
        let _scratch = Scratch::new("import-delta-missing-base");

        let base = b"the quick brown fox\n".repeat(8);
        let mut target = base.clone();
        target.extend_from_slice(b"one more line\n");
        let target_hash = object_utils::hash_object_bytes(&target);
        let base_hash = object_utils::hash_object_bytes(&base);
        // Deliberately never stored: the base is absent from this warehouse.

        let frame = delta_utils::compress_delta(&base, &target).unwrap();
        let mut payload = Vec::new();
        payload.extend(base_hash.as_bytes());
        payload.extend((target.len() as u64).to_be_bytes());
        payload.extend(&frame);

        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'D', &target_hash, &payload));
        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("is not present; the bundle is corrupt"), "{}", error);
    }

    #[test]
    fn import_rejects_an_unknown_record_kind() {
        let _scratch = Scratch::new("import-unknown-kind");

        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'Z', &"a".repeat(64), b"whatever"));
        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("Unknown bundle record kind"), "{}", error);
    }

    #[test]
    fn import_rejects_an_unknown_header() {
        let _scratch = Scratch::new("import-unknown-header");

        let bytes = raw_bundle("forklift-bundle 1999-01-01", &[]);
        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("Unknown bundle header"), "{}", error);
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

    #[test]
    fn import_still_accepts_the_legacy_v1_header() {
        let _scratch = Scratch::new("import-v1-header");

        let content = b"legacy content";
        let hash = object_utils::hash_object_bytes(content);
        let bytes = raw_bundle(BUNDLE_HEADER_V1, &record(b'O', &hash, content));

        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_objects, 1);
    }

    /// The header/record write-then-read semantics, end to end: a real (tiny) warehouse's
    /// `build_bundle` output, imported into a second, empty warehouse — not just the
    /// never-panic fuzz suite's malformed-input focus.
    #[test]
    fn build_bundle_then_import_bundle_round_trips_a_real_warehouse() {
        let bundle_bytes = {
            let _source = Scratch::new("bundle-roundtrip-src");

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

            pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

            let stats = build_bundle().unwrap();
            assert_eq!(stats.objects, 3, "the parcel, its tree and its one blob");
            assert_eq!(stats.deltas, 0);

            std::fs::read(&stats.path).unwrap()
        };

        let _destination = Scratch::new("bundle-roundtrip-dst");
        let import_stats = import_bundle_bytes(&bundle_bytes).unwrap();
        assert_eq!(import_stats.stored_objects, 3);
        assert_eq!(import_stats.skipped_records, 0);

        // Re-importing the same bundle is idempotent: everything is now already present.
        let reimport_stats = import_bundle_bytes(&bundle_bytes).unwrap();
        assert_eq!(reimport_stats.stored_objects, 0);
        assert_eq!(reimport_stats.skipped_records, 3);
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

        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        let error = build_bundle().err().expect("a chunked file must refuse the bundle");
        let (code, message, _) = scope_utils::decode_refusal(&error)
            .expect("the refusal must decode via the shared sentinel framing");

        assert_eq!(code, scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains("big.bin"), "the refusal names the path: {}", message);

        // Nothing was left behind: the bundle file is never renamed into place on failure.
        assert!(!get_latest_bundle_path().exists(), "a refused bundle must not be written");
    }

    /// A warehouse with a grandfathered giant blob (an object above the whole-object ceiling,
    /// authored — here, imported via an old-version bundle — before `MAX_OBJECT_BYTES` existed)
    /// refuses to bundle: no reader accepts a record that large (a version-3 reader refuses its
    /// declared length pre-read), so writing one would only produce a bundle nobody could finish
    /// importing. The refusal carries the stable code, names the path and the object, and nothing
    /// is written — the giant stays fully readable and checkout-able locally; only transport (and
    /// only transport) refuses, honestly, at the source.
    #[test]
    fn build_bundle_refuses_a_warehouse_with_a_grandfathered_giant_blob() {
        let _scratch = Scratch::new("bundle-giant-refuses");

        // The only way such a blob can exist locally: it predates the ceiling. `LooseObject::
        // store`/`store_object_bytes` both refuse a fresh over-ceiling write, so importing it via
        // an old-version bundle (which does not hard-enforce the ceiling) is the honest way to
        // manufacture the fixture — mirrors `an_old_version_bundle_imports_a_grandfathered_giant`.
        let giant_object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0u8; object_utils::MAX_OBJECT_BYTES + 1],
        });
        let giant_bytes = giant_object.content.clone();
        let v2_bundle = raw_bundle(BUNDLE_HEADER_V2, &record(b'O', &giant_object.hash, &giant_bytes));
        import_bundle_bytes(&v2_bundle).expect("the grandfathered giant imports via an old-version bundle");

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

        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

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

        let giant_object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0u8; object_utils::MAX_OBJECT_BYTES + 1],
        });
        let giant_bytes = giant_object.content.clone();
        let v2_bundle = raw_bundle(BUNDLE_HEADER_V2, &record(b'O', &giant_object.hash, &giant_bytes));
        import_bundle_bytes(&v2_bundle).expect("the grandfathered giant imports via an old-version bundle");

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

    /// A version-2 bundle (the delta-record version) is still accepted on import — an older
    /// server's bundle reads fine.
    #[test]
    fn import_still_accepts_the_v2_header() {
        let _scratch = Scratch::new("import-v2-header");

        let content = b"a version-2 object";
        let hash = object_utils::hash_object_bytes(content);
        let bytes = raw_bundle(BUNDLE_HEADER_V2, &record(b'O', &hash, content));

        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_objects, 1);
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
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &object.hash, &raw));

        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_objects, 1);
        assert_eq!(file_utils::retrieve_object_by_hash(&object.hash).unwrap(), raw);
    }

    /// A new-version (v3) bundle refuses an `'O'` record whose *declared* length is above the
    /// object ceiling **before reading a byte** of it: the record declares a giant length but
    /// carries only a few real bytes, so a read would report truncation — the ceiling error
    /// (not truncation) proves the refusal happened pre-read.
    #[test]
    fn new_version_bundle_refuses_an_over_ceiling_object_pre_read() {
        let _scratch = Scratch::new("import-ceiling-object");

        let declared = object_utils::MAX_OBJECT_BYTES as u64 + 1;
        let record = record_with_declared_len(b'O', &"a".repeat(64), declared, b"only a few bytes");
        let bytes = raw_bundle(BUNDLE_HEADER, &record);

        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("object ceiling"), "the ceiling refusal (not truncation): {}", error);
        assert!(!error.contains("truncated"), "must be refused before reading: {}", error);
    }

    /// The same pre-read ceiling refusal for a `'D'` record — and, crucially, it applies on an
    /// **old-version** bundle too: no writer of any version ever emitted a delta near the ceiling,
    /// so an over-ceiling declared delta length is a bomb regardless of the header, and is capped
    /// before the frame is read into memory.
    #[test]
    fn a_delta_record_over_the_ceiling_is_refused_on_any_version() {
        let _scratch = Scratch::new("import-ceiling-delta");

        let declared = object_utils::MAX_OBJECT_BYTES as u64 + 1;
        let record = record_with_declared_len(b'D', &"b".repeat(64), declared, b"tiny frame");
        // An *old*-version bundle, to prove the delta cap is unconditional, not gated on v3.
        let bytes = raw_bundle(BUNDLE_HEADER_V2, &record);

        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("delta"), "names the delta record: {}", error);
        assert!(error.contains("object ceiling"), "the ceiling refusal: {}", error);
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
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &wrong_hash, &raw));

        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("does not match its claimed hash"), "{}", error);
        assert!(!file_utils::does_object_exist(&wrong_hash).unwrap(), "nothing unverified may land");
    }

    /// An old-version bundle carrying a **grandfathered giant** — a single `'O'` object above the
    /// object ceiling, from before the ceiling existed — still imports: the ceiling is not
    /// hard-enforced on an old-version bundle, and the object streams in with memory bounded. This
    /// is the "an existing store must not brick" guarantee, over the wire.
    #[test]
    fn an_old_version_bundle_imports_a_grandfathered_giant() {
        let _scratch = Scratch::new("import-grandfathered-giant");

        // A real object over the ceiling: the payload itself exceeds MAX_OBJECT_BYTES, so the
        // object is over-ceiling regardless of how large the prepended object header is (zeros:
        // cheap to hash and compress).
        let object = LooseObjectBuilder::build_blob(&Blob {
            content: vec![0u8; object_utils::MAX_OBJECT_BYTES + 1],
        });
        let raw = object.content.clone();
        assert!(raw.len() > object_utils::MAX_OBJECT_BYTES, "the object must exceed the ceiling");

        // A version-2 header: predates the ceiling, so it is not hard-refused on declared size.
        let bytes = raw_bundle(BUNDLE_HEADER_V2, &record(b'O', &object.hash, &raw));

        let stats = import_bundle_bytes(&bytes).unwrap();
        assert_eq!(stats.stored_objects, 1, "the grandfathered giant imports");
        assert_eq!(file_utils::retrieve_object_by_hash(&object.hash).unwrap().len(), raw.len());

        // But the same giant in a *new-version* bundle is refused before it is read.
        let bytes = raw_bundle(BUNDLE_HEADER, &record(b'O', &object.hash, &raw));
        let error = import_bundle_bytes(&bytes).err().unwrap();
        assert!(error.contains("object ceiling"), "a v3 bundle refuses the giant: {}", error);
    }
}
