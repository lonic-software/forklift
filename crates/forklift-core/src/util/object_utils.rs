use std::path::Path;
use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::object::parsed_object::ParsedObject;
use crate::globals;
use crate::model::blob::Blob;
use crate::model::parcel::Parcel;
use crate::model::tree_item::TreeItem;
use crate::parser;
use crate::util::file_utils;

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

/// Read a parcel's decoded bytes, serving and populating the [`ParcelReadMemo`] cache when one is
/// active on this thread (otherwise a plain uncached read — the memo is never populated when no
/// guard is held, so this is zero-cost outside `compact`'s reachability phase).
fn read_parcel_bytes(hash: &str) -> Result<std::rc::Rc<Vec<u8>>, String> {
    if let Some(hit) = PARCEL_BYTES_MEMO.with(|memo| memo.borrow().as_ref().and_then(|c| c.get(hash).cloned())) {
        return Ok(hit);
    }

    let bytes = std::rc::Rc::new(file_utils::retrieve_object_by_hash_uncached(hash)?);

    PARCEL_BYTES_MEMO.with(|memo| {
        if let Some(cache) = memo.borrow_mut().as_mut() {
            cache.insert(hash.to_string(), std::rc::Rc::clone(&bytes));
        }
    });

    Ok(bytes)
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

    if file_utils::does_object_exist(claimed_hash)? {
        return Ok(false);
    }

    let compressed = zstd::encode_all(bytes, 0)
        .map_err(|e| format!("Error while compressing object {}: {}", claimed_hash, e))?;

    let (path, file_name) = file_utils::get_path_for_object(claimed_hash)?;

    file_utils::write_object_to_file(std::path::Path::new(&path), &file_name, compressed)?;

    Ok(true)
}
