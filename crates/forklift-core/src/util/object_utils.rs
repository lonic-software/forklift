use std::io::Read;
use std::path::Path;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::object::parsed_object::ParsedObject;
use crate::enums::object_type::ObjectType;
use crate::globals;
use crate::model::blob::Blob;
use crate::model::chunk::Chunk;
use crate::model::parcel::Parcel;
use crate::model::recipe::{Recipe, RecipeChunk};
use crate::model::tree_item::TreeItem;
use crate::parser;
use crate::util::{byte_utils, chunk_utils, fanout_utils, file_utils};

/// Push a new line character to the content.
///
/// # Arguments
/// * `content` - The content to push the new line character to.
pub fn push_new_line(content: &mut Vec<u8>) {
    content.push(globals::BYTE_NEW_LINE);
}

/// Push an end of text byte to the content.
///
/// # Arguments
/// * `content` - The content to push the end of text byte to.
pub fn push_end_of_text(content: &mut Vec<u8>) {
    content.push(globals::BYTE_END_OF_TEXT);
}

/// Push a space character to the content.
///
/// # Arguments
/// * `content` - The content to push the space character to.
pub fn push_space(content: &mut Vec<u8>) {
    content.push(globals::BYTE_SPACE);
}

/// Push a null (zero) byte to the content.
///
/// # Arguments
/// * `content` - The content to push the null byte to.
pub fn push_null(content: &mut Vec<u8>) {
    content.push(globals::BYTE_NULL);
}

thread_local! {
    /// A short-lived, thread-local cache of decoded parcel *bytes*, active only while a
    /// [`ParcelReadMemo`] guard is held on this thread. See that type for the why.
    static PARCEL_BYTES_MEMO: std::cell::RefCell<Option<std::collections::HashMap<String, std::rc::Rc<Vec<u8>>>>> =
        const { std::cell::RefCell::new(None) };
}

/// An RAII guard that memoizes parcel **decodes** on the current thread for its lifetime.
///
/// Parcels deliberately bypass the shared read cache (`retrieve_object_by_hash_uncached`),
/// because a whole-history walk reads each parcel about once, so caching them is pure churn.
/// `compact --all`'s reachability phase is the one exception: it reads the *same* parcel set
/// several times over — the live-set walk (`gc_utils::collect_live_set` →
/// `collect_reachable_present`, then again per parcel for its `tree_hash`) and, when new deltas
/// must be built, the path-base walk (`compute_path_bases` → `collect_reachable`,
/// `topo_order_oldest_first`, then again per parcel for its `tree_hash`). Measured on a
/// 401-parcel synthetic warehouse: 2005 logical parcel reads when both walks run (garbage or
/// loose objects present) — exactly 5.0 per parcel, matching the roadmap's "~5" estimate — and
/// 802 (2.0 per parcel) in the steady-state case where only the live-set walk runs. Each read is
/// a pack lookup + zstd decode + Blake3 verify. This guard scopes a decode cache to exactly that
/// phase: a parcel is decoded once and every later read in the phase is an `Rc`-shared clone —
/// collapsing both cases to exactly 1 decode per parcel.
///
/// It stores the decoded *bytes* (not the parsed [`Parcel`]) so it needs no `Clone` on the model
/// and stays cheap on memory; the parse is the small residual. It is thread-local and single-
/// scoped (`compact` runs its reachability serially under the store lock), and is dropped before
/// the parallel pack-write batch, so a worker thread never sees or shares it.
pub struct ParcelReadMemo(());

impl ParcelReadMemo {
    /// Begin memoizing parcel decodes on this thread until the returned guard drops.
    pub fn activate() -> ParcelReadMemo {
        PARCEL_BYTES_MEMO.with(|memo| *memo.borrow_mut() = Some(std::collections::HashMap::new()));
        ParcelReadMemo(())
    }
}

impl Drop for ParcelReadMemo {
    fn drop(&mut self) {
        PARCEL_BYTES_MEMO.with(|memo| *memo.borrow_mut() = None);
    }
}

/// A parcel read's decoded bytes: either a plain owned `Vec` (the common, non-memo path) or an
/// `Rc`-shared clone served from an active [`ParcelReadMemo`]. Both variants deref to `[u8]`, so
/// callers that only need to borrow the bytes (the parse) don't care which one they got.
enum ParcelBytes {
    Owned(Vec<u8>),
    Shared(std::rc::Rc<Vec<u8>>),
}

impl std::ops::Deref for ParcelBytes {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        match self {
            ParcelBytes::Owned(bytes) => bytes,
            ParcelBytes::Shared(bytes) => bytes,
        }
    }
}

/// Read a parcel's decoded bytes, serving and populating the [`ParcelReadMemo`] cache when one is
/// active on this thread. When no guard is held (the common case — audit, history, any whole-walk
/// caller), this takes a plain direct read with no `Rc` allocation at all: zero-cost outside
/// `compact`'s reachability phase, not just zero *reuse*.
fn read_parcel_bytes(hash: &str) -> Result<ParcelBytes, String> {
    let memo_active = PARCEL_BYTES_MEMO.with(|memo| memo.borrow().is_some());

    if !memo_active {
        return Ok(ParcelBytes::Owned(file_utils::retrieve_object_by_hash_uncached(hash)?));
    }

    if let Some(hit) = PARCEL_BYTES_MEMO.with(|memo| memo.borrow().as_ref().and_then(|c| c.get(hash).cloned())) {
        return Ok(ParcelBytes::Shared(hit));
    }

    let bytes = std::rc::Rc::new(file_utils::retrieve_object_by_hash_uncached(hash)?);

    PARCEL_BYTES_MEMO.with(|memo| {
        if let Some(cache) = memo.borrow_mut().as_mut() {
            cache.insert(hash.to_string(), std::rc::Rc::clone(&bytes));
        }
    });

    Ok(ParcelBytes::Shared(bytes))
}

/// Load and parse the parcel object with the given hash from the object store.
///
/// # Arguments
/// * `hash` - The hash of the parcel object.
///
/// # Returns
/// * `Ok(Parcel)`  - The parsed parcel.
/// * `Err(String)` - If the object does not exist, could not be parsed, or is not a parcel.
pub fn load_parcel(hash: &str) -> Result<Parcel, String> {
    // Parcels are stored full (never delta'd) and a walk reads each one about once, so the read
    // cache only taxes them (a per-read key allocation and churn) without ever paying off — read
    // them straight, and leave the cache to the trees and blobs that actually reuse it. The one
    // exception is `compact --all`, which re-reads the same parcel set several times; a
    // `ParcelReadMemo` guard (held only for that phase) collapses those re-reads to one decode.
    let bytes = read_parcel_bytes(hash)?;

    match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Parcel(parcel) => Ok(parcel),
        other => Err(format!("Object {} is a {}, not a parcel.", hash, other.get_type())),
    }
}

/// Load and parse the tree object with the given hash from the object store.
/// Note that only one level is loaded: the returned tree's subtree children carry their
/// hashes, but their own children must be loaded separately.
///
/// # Arguments
/// * `hash` - The hash of the tree object.
///
/// # Returns
/// * `Ok(TreeItem)` - The parsed tree.
/// * `Err(String)`  - If the object does not exist, could not be parsed, or is not a tree.
pub fn load_tree(hash: &str) -> Result<TreeItem, String> {
    // Only the parse borrows the bytes, so take the shared `Arc` — a cache hit is a pointer
    // clone under the lock, not a copy of the whole tree object.
    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;

    match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Tree(tree) => Ok(tree),
        other => Err(format!("Object {} is a {}, not a tree.", hash, other.get_type())),
    }
}

/// Load and parse the blob object with the given hash from the object store.
///
/// # Arguments
/// * `hash` - The hash of the blob object.
///
/// # Returns
/// * `Ok(Blob)`    - The parsed blob.
/// * `Err(String)` - If the object does not exist, could not be parsed, or is not a blob.
pub fn load_blob(hash: &str) -> Result<Blob, String> {
    // Borrow-only (the parse), so share the cached `Arc` — the win the read cache exists for on a
    // reconstruction-heavy walk (`blame`/`diff`/`export`) that reloads the same blobs and bases.
    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;

    match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Blob(blob) => Ok(blob),
        other => Err(format!("Object {} is a {}, not a blob.", hash, other.get_type())),
    }
}

/// Load and parse the recipe object with the given hash from the object store. The recipe's
/// structural invariants are enforced by the parser at this point (`sum(chunk_sizes)` equals
/// the declared total, every chunk hash is valid ASCII hex, no chunk exceeds the per-chunk
/// ceiling) — a lying `content_hash` is *not* caught here (only a real assembly re-derives it).
///
/// # Arguments
/// * `hash` - The hash of the recipe object.
///
/// # Returns
/// * `Ok(Recipe)` - The parsed, structurally valid recipe.
/// * `Err(String)` - If the object does not exist, is not a recipe, or is structurally invalid.
pub fn load_recipe(hash: &str) -> Result<Recipe, String> {
    // Borrow-only (the parse), so share the cached `Arc` — a recipe is re-read on every
    // materialization, diff, and gc/audit descent of the same chunked file.
    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;

    match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Recipe(recipe) => Ok(recipe),
        other => Err(format!("Object {} is a {}, not a recipe.", hash, other.get_type())),
    }
}

/// Load and parse the chunk object with the given hash from the object store. The per-chunk
/// ceiling is enforced on read by the parser.
///
/// # Arguments
/// * `hash` - The hash of the chunk object.
///
/// # Returns
/// * `Ok(Chunk)`   - The parsed chunk.
/// * `Err(String)` - If the object does not exist, is not a chunk, or exceeds the chunk ceiling.
pub fn load_chunk(hash: &str) -> Result<Chunk, String> {
    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;

    match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Chunk(chunk) => Ok(chunk),
        other => Err(format!("Object {} is a {}, not a chunk.", hash, other.get_type())),
    }
}

/// Resolve a file inside a tree by its warehouse path, loading subtree objects along the
/// way.
///
/// # Arguments
/// * `root_tree_hash` - The hash of the root tree.
/// * `path`           - The warehouse path of the file.
///
/// # Returns
/// * `Ok(Some((String, DirEntryType)))` - The file's blob hash and entry type.
/// * `Ok(None)`                         - If the path does not exist (or is a directory).
/// * `Err(String)`                      - If a tree object could not be loaded.
pub fn resolve_tree_file(root_tree_hash: &str,
                         path: &str) -> Result<Option<(String, DirEntryType)>, String> {
    let mut current = load_tree(root_tree_hash)?;
    let components: Vec<&str> = path.split('/').collect();

    for (index, component) in components.iter().enumerate() {
        let is_last = index == components.len() - 1;

        if is_last {
            let file = current.get_files()
                .find(|(name, _)| name == component)
                .map(|(_, item)| (item.hash.clone(), item.item_type));

            return Ok(file);
        }

        let subtree = current.get_subtrees()
            .find(|(name, _)| name == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(subtree_hash) => current = load_tree(&subtree_hash)?,
            None => return Ok(None),
        }
    }

    Ok(None)
}

/// Create a blob for a file. To store this in the object store, use the `LooseObjectBuilder`.
///
/// # Arguments
/// * `file_name`  - The name of the file.
/// * `entry_path` - The path to the file.
/// * `item_type`  - The type of the directory entry.
///
/// # Returns
/// * `Ok(Blob)`    - The blob for the file.
/// * `Err(String)` - The error message if the blob could not be created.
pub fn get_blob_for_file(file_name: &str,
                         entry_path: &Path,
                         item_type: &DirEntryType) -> Result<Blob, String> {
    let file_content = if *item_type == DirEntryType::SymbolicLink {
        let target = std::fs::read_link(entry_path).map_err(|e|
            format!("Failed to read the target of symlink \"{}\": {}", file_name, e)
        )?;

        target.to_str().ok_or(
            "Error while parsing the name of a symlink as UTF-8.".to_string()
        ).map(|s| s.as_bytes().to_vec())
    } else {
        std::fs::read(entry_path)
            .map_err(|e| format!("Error while reading file \"{}\": {}", file_name, e))
    }?;

    Ok (
        Blob {
            content: file_content
        }
    )
}

/// The Blake3 hex hash of raw (uncompressed) object bytes — the object's identity.
///
/// # Arguments
/// * `bytes` - The full object bytes.
///
/// # Returns
/// * `String` - The hash (lowercase hex).
pub fn hash_object_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

/// Verify that raw `bytes` are the object addressed by `hash` — the content-addressing invariant,
/// enforced on the way *out* of the store as [`store_object_bytes`] enforces it on the way in.
///
/// An object's hash *is* the Blake3 of its raw bytes, so any read — a loose file, a full pack
/// record, or a delta reconstructed against its base — must reproduce that hash. A mismatch means
/// the stored bytes are corrupt (a damaged pack or loose file, or a delta rebuilt against the
/// wrong base): the read fails loudly instead of silently returning wrong bytes under a store
/// whose whole pitch is tamper-evident history. Blake3 is fast (and parallel) enough that this
/// costs a small fraction of the surrounding decompression.
///
/// # Arguments
/// * `hash`  - The hex hash the bytes are addressed by.
/// * `bytes` - The raw (uncompressed) object bytes read back from the store.
///
/// # Returns
/// * `Ok(())`      - If the bytes hash to `hash`.
/// * `Err(String)` - If they do not (the object store is corrupt).
pub fn verify_object_bytes(hash: &str, bytes: &[u8]) -> Result<(), String> {
    let actual = hash_object_bytes(bytes);

    if actual != hash {
        return Err(format!(
            "Object {} is corrupt: its stored bytes hash to {} instead (content-addressing violated).",
            hash, actual
        ));
    }

    Ok(())
}

/// Store raw object bytes received from elsewhere (a remote or a bundle). The claimed
/// hash is verified against the bytes first — nothing unverified may enter the object
/// store — and the object is compressed at rest like locally built ones.
///
/// # Arguments
/// * `claimed_hash` - The hash the bytes are supposed to have.
/// * `bytes`        - The raw (uncompressed) object bytes.
///
/// # Returns
/// * `Ok(true)`    - If the object was stored.
/// * `Ok(false)`   - If the object was already present (nothing written).
/// * `Err(String)` - If the hash does not match the bytes, or the write failed.
pub fn store_object_bytes(claimed_hash: &str, bytes: &[u8]) -> Result<bool, String> {
    let actual = hash_object_bytes(bytes);

    if actual != claimed_hash {
        return Err(format!(
            "Object content does not match its claimed hash {} (actual: {}); refusing to store it.",
            claimed_hash, actual
        ));
    }

    // Per-type ceiling (review W2): a `Chunk`-typed object above `MAX_CHUNK_BYTES` is refused on
    // store as well as on read, even though a larger object would otherwise be a legal object.
    // Without this a malicious recipe could reference an over-size chunk and the streaming-
    // assembly memory bound would be far looser than the per-chunk ceiling explicit types buy.
    enforce_chunk_ceiling(claimed_hash, bytes)?;

    if file_utils::does_object_exist(claimed_hash)? {
        return Ok(false);
    }

    let compressed = zstd::encode_all(bytes, 0)
        .map_err(|e| format!("Error while compressing object {}: {}", claimed_hash, e))?;

    let (path, file_name) = file_utils::get_path_for_object(claimed_hash)?;

    file_utils::write_object_to_file(std::path::Path::new(&path), &file_name, compressed)?;

    Ok(true)
}

/// Peek a loose object's header without a full parse: its type and the length of the header
/// (version VLQ, type VLQ, content-length VLQ, terminating null). The payload is everything after
/// the header, so `bytes.len() - header_len` is the true payload length.
///
/// # Arguments
/// * `bytes` - The full (uncompressed) object bytes.
///
/// # Returns
/// * `Ok((ObjectType, usize))` - The object type and the header length in bytes.
/// * `Err(String)`             - If the header is malformed.
pub fn peek_object_header(bytes: &[u8]) -> Result<(ObjectType, usize), String> {
    let (_version, after_version) = byte_utils::number_from_vlq_bytes(0, bytes)
        .map_err(|e| format!("Failed to peek object version: {}", e))?;

    let (type_code, after_type) = byte_utils::number_from_vlq_bytes(after_version, bytes)
        .map_err(|e| format!("Failed to peek object type: {}", e))?;

    let object_type = ObjectType::from_code(type_code)?;

    let (_length, after_length) = byte_utils::number_from_vlq_bytes(after_type, bytes)
        .map_err(|e| format!("Failed to peek object content length: {}", e))?;

    // The header ends at the terminating null byte (written by `LooseObjectBuilder::write_header`).
    let (_, null_read) = byte_utils::read_until_byte_value(after_length, bytes, globals::BYTE_NULL)
        .ok_or_else(|| "Object header has no terminating null byte.".to_string())?;

    Ok((object_type, after_length + null_read))
}

/// Enforce the per-type chunk ceiling on the way *into* the store: a `Chunk`-typed object whose
/// payload exceeds `chunk_utils::MAX_CHUNK_BYTES` is refused. Non-chunk objects pass untouched
/// (their own ceilings live elsewhere). The check is on the true payload length (after the
/// header), not a declared length, so a lying header cannot slip an over-size chunk through.
///
/// # Arguments
/// * `claimed_hash` - The object's hash (for the error message).
/// * `bytes`        - The full (uncompressed) object bytes.
///
/// # Returns
/// * `Ok(())`      - If the object is not an over-size chunk.
/// * `Err(String)` - If it is a `Chunk` object above the ceiling.
fn enforce_chunk_ceiling(claimed_hash: &str, bytes: &[u8]) -> Result<(), String> {
    // Only a *confirmed* over-size chunk is refused. An object whose header cannot be peeked is
    // not a recognizable chunk (a real chunk always has a valid header); pass it through — this
    // path is content-addressing, not a general validator, and the read-side parser (`load_chunk`)
    // enforces the same ceiling on anything that is actually read back as a chunk.
    let Ok((object_type, header_len)) = peek_object_header(bytes) else {
        return Ok(());
    };

    if object_type == ObjectType::Chunk {
        let payload_len = bytes.len().saturating_sub(header_len);
        if payload_len > chunk_utils::MAX_CHUNK_BYTES {
            return Err(format!(
                "Chunk object {} has a {}-byte payload, above the {}-byte chunk ceiling; refusing to store it.",
                claimed_hash, payload_len, chunk_utils::MAX_CHUNK_BYTES
            ));
        }
    }

    Ok(())
}

/// Whether an ingest should persist the objects it produces (`load`, `park`) or only compute
/// their hashes (`stocktake`, `diff`'s change classification — read-only paths that must not
/// mutate the object store).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum IngestMode {
    /// Store every chunk and the recipe (the `load`/`park` path).
    Store,

    /// Compute the recipe hash and chunk hashes without writing anything (the read-only
    /// stocktake/diff classification path). A changed giant is re-chunked to learn its new recipe
    /// hash, but no chunk or recipe object is written — read paths never mutate the store.
    ComputeOnly,
}

/// The result of ingesting an on-disk file: the identity the inventory/tree records for it, plus
/// (for a small file) the built-but-unstored blob the caller stores or drops.
pub struct IngestedFile {
    /// The recipe hash (chunked file) or blob hash (small file) — the inventory entry's hash.
    pub hash: String,

    /// The assembled file size (chunked) or the blob byte length (small file).
    pub file_size: u64,

    /// The entry type, upgraded to a `*Chunked` variant when the file was chunked.
    pub item_type: DirEntryType,

    /// For a small file, the built blob object the caller stores (deferred, as before) or drops
    /// (read-only stocktake). `None` for a chunked file — its chunks and recipe were handled per
    /// the ingest mode already (nothing is left for the caller to store).
    pub deferred: Option<crate::model::object::loose_object::LooseObject>,
}

/// How many bytes of chunk payload to buffer before fanning a batch out for hashing/storage. A
/// bound on the peak memory the chunk pipeline holds beyond the streaming window — independent of
/// the file size.
const CHUNK_FANOUT_BATCH_BYTES: usize = 32 * 1024 * 1024;

/// How much the streaming buffer may accumulate behind the read cursor before it is compacted
/// (front bytes dropped). Keeps the buffer bounded to roughly this plus one max chunk.
const CHUNK_BUFFER_COMPACT_BYTES: usize = chunk_utils::CHUNK_THRESHOLD_BYTES;

/// Ingest an on-disk file into the object store the chunk-aware way: a file whose hashed content
/// is at or above `CHUNK_THRESHOLD_BYTES` becomes a recipe plus chunks; below it, an ordinary
/// blob. Classification is pinned to the bytes actually read — a bounded look-ahead over the file
/// itself, never a pre-read `stat` — so a file that grows across the threshold during the read is
/// classified by its true content, closing the TOCTOU a stat-then-read would open (review W5).
///
/// Memory is bounded regardless of file size: a small file is read whole (below the threshold, so
/// bounded by it); a large file streams through a rolling window plus a bounded fan-out batch, and
/// the whole file is never resident at once.
///
/// # Arguments
/// * `file_name`  - The name of the file (for error messages).
/// * `entry_path` - The path to the file.
/// * `item_type`  - The file's entry type (normal/executable/symlink). A symlink is never chunked.
/// * `mode`       - Whether to store the produced objects or only compute their hashes.
///
/// # Returns
/// * `Ok(IngestedFile)` - The file's identity (and, for a small file, its unstored blob).
/// * `Err(String)`      - If the file could not be read or an object could not be stored.
pub fn ingest_file(file_name: &str,
                   entry_path: &Path,
                   item_type: DirEntryType,
                   mode: IngestMode) -> Result<IngestedFile, String> {
    // A symlink's content is its (tiny) target path — never chunked, and never large.
    if item_type == DirEntryType::SymbolicLink {
        let blob = get_blob_for_file(file_name, entry_path, &item_type)?;
        let file_size = blob.content.len() as u64;
        let object = LooseObjectBuilder::build_blob(&blob);

        return Ok(IngestedFile {
            hash: object.hash.clone(),
            file_size,
            item_type,
            deferred: Some(object),
        });
    }

    let file = std::fs::File::open(entry_path)
        .map_err(|e| format!("Error while opening file \"{}\": {}", file_name, e))?;
    let mut reader = std::io::BufReader::new(file);

    // Look-ahead: buffer up to the threshold while reading. If EOF arrives first, the hashed
    // content is below the threshold → an ordinary blob (built from exactly the bytes read).
    let threshold = chunk_utils::CHUNK_THRESHOLD_BYTES;
    let mut buffer = read_up_to(&mut reader, threshold, file_name)?;

    if buffer.len() < threshold {
        // Below the threshold: an ordinary blob. This holds the whole (small) file in memory,
        // bounded by the threshold, exactly as before chunking existed.
        let blob = Blob { content: buffer };
        let file_size = blob.content.len() as u64;
        let object = LooseObjectBuilder::build_blob(&blob);

        return Ok(IngestedFile {
            hash: object.hash.clone(),
            file_size,
            item_type,
            deferred: Some(object),
        });
    }

    // At or above the threshold: chunk it. The look-ahead prefix already read becomes the first
    // bytes of the stream; boundary-finding continues from byte 0 (FastCDC restarts its
    // fingerprint at each chunk), so the boundaries are identical to a pure whole-file chunk.
    let mut content_hasher = blake3::Hasher::new();
    let mut recipe_chunks: Vec<RecipeChunk> = Vec::new();
    let mut batch: Vec<Vec<u8>> = Vec::new();
    let mut batch_bytes = 0usize;
    let mut pos = 0usize;
    let mut eof = false;

    loop {
        // Ensure a full look-ahead window (or EOF) so the next boundary is definitive.
        while buffer.len() - pos < chunk_utils::MAX_CHUNK_BYTES && !eof {
            let read = read_up_to(&mut reader, 256 * 1024, file_name)?;
            if read.is_empty() {
                eof = true;
            } else {
                buffer.extend_from_slice(&read);
            }
        }

        if pos >= buffer.len() {
            break;
        }

        let cut = chunk_utils::next_boundary(&buffer[pos..]);
        let chunk_bytes = buffer[pos..pos + cut].to_vec();
        content_hasher.update(&chunk_bytes);
        pos += cut;

        batch_bytes += chunk_bytes.len();
        batch.push(chunk_bytes);

        if batch_bytes >= CHUNK_FANOUT_BATCH_BYTES {
            flush_chunk_batch(&mut batch, &mut recipe_chunks, mode)?;
            batch_bytes = 0;
        }

        // Drop the already-chunked prefix so the buffer stays bounded.
        if pos >= CHUNK_BUFFER_COMPACT_BYTES {
            buffer.drain(..pos);
            pos = 0;
        }
    }

    flush_chunk_batch(&mut batch, &mut recipe_chunks, mode)?;

    let total_size: u64 = recipe_chunks.iter().map(|c| c.size).sum();
    let content_hash = content_hasher.finalize().to_hex().to_string();

    let recipe = Recipe { content_hash, total_size, chunks: recipe_chunks };
    let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);

    if mode == IngestMode::Store {
        recipe_object.store()?;
    }

    Ok(IngestedFile {
        hash: recipe_object.hash,
        file_size: total_size,
        item_type: item_type.to_chunked(),
        deferred: None,
    })
}

/// Hash (and, in `Store` mode, store) a batch of chunks in parallel, appending their
/// `(hash, size)` to `recipe_chunks` in the batch's order. The batch is drained.
fn flush_chunk_batch(batch: &mut Vec<Vec<u8>>,
                     recipe_chunks: &mut Vec<RecipeChunk>,
                     mode: IngestMode) -> Result<(), String> {
    if batch.is_empty() {
        return Ok(());
    }

    // The expensive per-chunk work (Blake3, zstd, IO) fans out over the shared idiom; results are
    // returned in the batch's order so the recipe's chunk list stays correctly ordered.
    let results = fanout_utils::fanout_map(batch, |chunk_bytes| -> Result<RecipeChunk, String> {
        let mut object = LooseObjectBuilder::build_chunk(&Chunk { content: chunk_bytes.clone() });
        let hash = object.hash.clone();
        let size = chunk_bytes.len() as u64;

        if mode == IngestMode::Store {
            object.store()?;
        }

        Ok(RecipeChunk { hash, size })
    });

    for result in results {
        recipe_chunks.push(result?);
    }

    batch.clear();
    Ok(())
}

/// Read up to `limit` bytes from `reader` into a fresh buffer, returning fewer only at EOF.
///
/// The buffer grows only to what is actually read (`Read::take` + `read_to_end`'s ordinary
/// amortized-growth allocation), never pre-allocated and zero-filled to `limit` up front. This
/// runs at least once per ingested file — the very first call is always `limit =
/// CHUNK_THRESHOLD_BYTES` (8 MiB) regardless of the file's real size — so a fixed-size
/// pre-allocation here would cost every file ingested (times however many run in parallel), not
/// just the ones that actually turn out large.
fn read_up_to(reader: &mut impl Read, limit: usize, file_name: &str) -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();

    reader.take(limit as u64).read_to_end(&mut buffer)
        .map_err(|e| format!("Error while reading file \"{}\": {}", file_name, e))?;

    Ok(buffer)
}

/// Assemble a chunked file's bytes from its recipe, streaming each chunk to `writer` in order and
/// verifying `Blake3(assembled) == recipe.content_hash` as it goes. The `content_hash` is
/// untrusted until this check passes: a recipe whose declared content hash disagrees with the
/// bytes its own chunks assemble to fails here (a checkout DoS on that one file, never silent
/// corruption — every chunk still content-addresses).
///
/// Memory is bounded to one chunk at a time (`<= MAX_CHUNK_BYTES`), never the whole file.
///
/// # Arguments
/// * `recipe_hash` - The hash of the recipe to assemble.
/// * `writer`      - The sink for the assembled bytes.
///
/// # Returns
/// * `Ok(u64)`     - The number of bytes written (the assembled file size).
/// * `Err(String)` - If a chunk is missing/corrupt, or the assembled hash mismatches.
pub fn assemble_chunked_file(recipe_hash: &str, writer: &mut impl std::io::Write) -> Result<u64, String> {
    let recipe = load_recipe(recipe_hash)?;

    let mut hasher = blake3::Hasher::new();
    let mut written = 0u64;

    for chunk in &recipe.chunks {
        let bytes = load_chunk(&chunk.hash)?.content;

        hasher.update(&bytes);
        writer.write_all(&bytes)
            .map_err(|e| format!("Error while assembling chunked file {}: {}", recipe_hash, e))?;
        written += bytes.len() as u64;
    }

    let assembled = hasher.finalize().to_hex().to_string();
    if assembled != recipe.content_hash {
        return Err(format!(
            "Chunked file {} failed integrity: its chunks assemble to {}, not the recipe's declared content hash {}.",
            recipe_hash, assembled, recipe.content_hash
        ));
    }

    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;
    use std::path::PathBuf;

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime (mirrors the `gc_utils` test fixture).
    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "forklift-object-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn store_chunk(content: &[u8]) -> String {
        let mut object = LooseObjectBuilder::build_chunk(&Chunk { content: content.to_vec() });
        object.store().unwrap();
        object.hash
    }

    fn store_recipe(content_hash: &str, chunks: &[(String, u64)]) -> String {
        let total_size = chunks.iter().map(|(_, size)| *size).sum();
        let recipe = Recipe {
            content_hash: content_hash.to_string(),
            total_size,
            chunks: chunks.iter().map(|(h, s)| RecipeChunk { hash: h.clone(), size: *s }).collect(),
        };
        let mut object = LooseObjectBuilder::build_recipe(&recipe);
        object.store().unwrap();
        object.hash
    }

    #[test]
    fn assembly_streams_chunks_and_verifies_the_content_hash() {
        let _scratch = Scratch::new("assemble-ok");

        let a = store_chunk(b"hello ");
        let b = store_chunk(b"world");
        let content_hash = hash_object_bytes(b"hello world"); // Blake3 of the assembled bytes
        let recipe = store_recipe(&content_hash, &[(a, 6), (b, 5)]);

        let mut out = Vec::new();
        let written = assemble_chunked_file(&recipe, &mut out).expect("assembly succeeds");

        assert_eq!(out, b"hello world");
        assert_eq!(written, 11);
    }

    #[test]
    fn assembly_fails_loudly_on_a_wrong_content_hash() {
        // A recipe whose declared content_hash disagrees with what its own chunks assemble to
        // fails at assembly — a checkout DoS on that one file, never silent corruption (each
        // chunk still content-addresses). content_hash is untrusted until assembly re-derives it.
        let _scratch = Scratch::new("assemble-bad-hash");

        let a = store_chunk(b"hello ");
        let b = store_chunk(b"world");
        let recipe = store_recipe(&"f".repeat(64), &[(a, 6), (b, 5)]); // lying content_hash

        let mut out = Vec::new();
        let err = assemble_chunked_file(&recipe, &mut out).expect_err("a lying content_hash must fail");
        assert!(err.contains("integrity"), "unexpected error: {}", err);
    }

    #[test]
    fn a_small_file_ingests_as_a_blob_and_a_large_one_as_a_recipe() {
        use std::io::Write;
        let scratch = Scratch::new("ingest-classify");

        // Just below the threshold → a blob.
        let small_path = scratch.root.join("small.bin");
        let small = vec![0x41u8; chunk_utils::CHUNK_THRESHOLD_BYTES - 1];
        std::fs::File::create(&small_path).unwrap().write_all(&small).unwrap();
        let ingested = ingest_file("small.bin", &small_path, DirEntryType::Normal, IngestMode::Store).unwrap();
        assert_eq!(ingested.item_type, DirEntryType::Normal, "below the threshold stays a blob");
        assert!(ingested.deferred.is_some(), "a blob is returned unstored for the caller");

        // Exactly at the threshold → a recipe (chunk iff hashed_len >= threshold).
        let big_path = scratch.root.join("big.bin");
        // Incompressible-ish deterministic content so it actually splits into several chunks.
        let mut big = Vec::with_capacity(chunk_utils::CHUNK_THRESHOLD_BYTES);
        let mut state = 0x1234_5678u64;
        while big.len() < chunk_utils::CHUNK_THRESHOLD_BYTES {
            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
            big.extend_from_slice(&state.to_le_bytes());
        }
        big.truncate(chunk_utils::CHUNK_THRESHOLD_BYTES);
        std::fs::File::create(&big_path).unwrap().write_all(&big).unwrap();

        let ingested = ingest_file("big.bin", &big_path, DirEntryType::Normal, IngestMode::Store).unwrap();
        assert_eq!(ingested.item_type, DirEntryType::NormalChunked, "at the threshold becomes a recipe");
        assert!(ingested.deferred.is_none(), "a chunked file stores its own objects");
        assert_eq!(ingested.file_size, chunk_utils::CHUNK_THRESHOLD_BYTES as u64);

        // Round-trip: assembling the recipe reproduces the exact bytes.
        let mut assembled = Vec::new();
        assemble_chunked_file(&ingested.hash, &mut assembled).unwrap();
        assert_eq!(assembled, big, "assembled bytes are identical to the ingested file");
    }

    #[test]
    fn compute_only_ingest_writes_nothing_to_the_store() {
        use std::io::Write;
        let scratch = Scratch::new("ingest-compute-only");

        let big_path = scratch.root.join("big.bin");
        let mut big = Vec::with_capacity(chunk_utils::CHUNK_THRESHOLD_BYTES + 100);
        let mut state = 0x9E37_79B9u64;
        while big.len() < chunk_utils::CHUNK_THRESHOLD_BYTES + 100 {
            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
            big.extend_from_slice(&state.to_le_bytes());
        }
        std::fs::File::create(&big_path).unwrap().write_all(&big).unwrap();

        let ingested = ingest_file("big.bin", &big_path, DirEntryType::Normal, IngestMode::ComputeOnly).unwrap();
        assert_eq!(ingested.item_type, DirEntryType::NormalChunked);
        // Nothing was written — the recipe it computed is not in the store (read-only stocktake).
        assert!(!file_utils::does_object_exist(&ingested.hash).unwrap(),
                "ComputeOnly must not write the recipe");
    }

    #[test]
    fn identical_large_files_share_every_chunk_and_recipe() {
        use std::io::Write;
        let scratch = Scratch::new("ingest-dedup");

        let mut content = Vec::with_capacity(chunk_utils::CHUNK_THRESHOLD_BYTES + 5000);
        let mut state = 0xDEAD_BEEFu64;
        while content.len() < chunk_utils::CHUNK_THRESHOLD_BYTES + 5000 {
            state = state.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
            content.extend_from_slice(&state.to_le_bytes());
        }

        let a_path = scratch.root.join("a.bin");
        let b_path = scratch.root.join("b.bin");
        std::fs::File::create(&a_path).unwrap().write_all(&content).unwrap();
        std::fs::File::create(&b_path).unwrap().write_all(&content).unwrap();

        let first = ingest_file("a.bin", &a_path, DirEntryType::Normal, IngestMode::Store).unwrap();
        let count_after_first = loose_object_count(&scratch.root);

        let second = ingest_file("b.bin", &b_path, DirEntryType::Normal, IngestMode::Store).unwrap();
        let count_after_second = loose_object_count(&scratch.root);

        // Identical content → identical recipe hash → whole-file dedup, and no new chunk objects.
        assert_eq!(first.hash, second.hash, "identical content shares the recipe hash");
        assert_eq!(count_after_first, count_after_second, "no new objects for identical content");
    }

    /// Count loose objects under a warehouse root (chunks + recipes + anything else).
    fn loose_object_count(root: &Path) -> usize {
        let objects = root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join("objects");
        let mut count = 0;
        let Ok(folders) = std::fs::read_dir(&objects) else { return 0; };
        for folder in folders.flatten() {
            let name = folder.file_name().to_string_lossy().to_string();
            if name.len() != 2 || !folder.path().is_dir() { continue; }
            if let Ok(files) = std::fs::read_dir(folder.path()) {
                for file in files.flatten() {
                    if !file.file_name().to_string_lossy().ends_with(".sig") { count += 1; }
                }
            }
        }
        count
    }
}
