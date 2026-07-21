use std::io::{Read, Write};
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

/// The largest *whole* object any store or bundle will accept on the way in — the write/import
/// ceiling that closes the max-object-size policy (DESIGN.html §9.4b, §5.0 row D item 7). After
/// chunking, the only object that can legitimately approach it is a big **tree** (a very large
/// single directory) or a big **recipe** (a very large chunked file's index): a blob is always
/// below the chunk threshold and a chunk is always at or below `MAX_CHUNK_BYTES`, so neither can
/// reach it.
///
/// It gates the way *in* only — [`store_object_bytes`] (import) and `LooseObject::store` (local
/// authorship) refuse an over-ceiling object, but no read path calls either, so a pre-existing
/// over-ceiling object authored before this policy stays fully readable and an old store never
/// bricks. An old-version bundle may still carry such a grandfathered giant; it is imported through
/// [`store_object_stream`], which deliberately does not enforce this ceiling.
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;

/// The largest object [`import_bundle_reader`](crate::util::bundle_utils) buffers whole before it
/// switches to the streaming store path. At or below it a bundle record is read into memory and
/// stored via [`store_object_bytes`] (memory bounded by this size); above it,
/// [`store_object_stream`] hashes and compresses it to a temp file without ever holding it whole.
/// It is `MAX_CHUNK_BYTES` so a legitimate `Chunk` object (never larger than that payload) is
/// small enough to take the buffered path in the common case, while the streaming path still
/// enforces the same per-chunk ceiling on anything that reaches it.
pub const STREAM_STORE_THRESHOLD_BYTES: usize = chunk_utils::MAX_CHUNK_BYTES;

// The ceiling must exceed every object a healthy store authors below it — a blob (< threshold), a
// chunk (<= max chunk), and the chunk threshold itself — so the ordering is a compile-time freeze.
const _: () = assert!(MAX_OBJECT_BYTES > chunk_utils::CHUNK_THRESHOLD_BYTES);
const _: () = assert!(MAX_OBJECT_BYTES > chunk_utils::MAX_CHUNK_BYTES);

/// Why a bounded zstd decode of untrusted bytes failed.
#[derive(Debug)]
pub(crate) enum BoundedDecodeError {
    /// Not a decodable zstd stream (corrupt, truncated, or not zstd).
    Malformed(String),
    /// Decoding produced a byte past `MAX_OBJECT_BYTES` — a bomb, or a grandfathered
    /// over-ceiling object; indistinguishable without finishing the decode, which is
    /// exactly what this refusal exists to avoid.
    OverCeiling,
}

/// Decode a whole zstd frame from untrusted bytes, refusing to *produce* more than
/// `MAX_OBJECT_BYTES` bytes. Store-wide policy for the sites that decode bytes the code has no
/// reason to trust yet (unlike a loose file's own decode, which is safe-as-written because it
/// only ever reads back what this store itself wrote).
///
/// Takes `impl Read` rather than `&[u8]`: a slice still implements `Read` at no cost to callers,
/// and it is what makes the streaming property testable — a throttled, counting `Read` wrapper
/// can prove the decoder stopped pulling input before the frame ended, instead of decoding it
/// whole and checking the output length after the fact.
///
/// Read one byte past the declared ceiling: producing it is what proves the frame is over. A
/// bare `take(MAX_OBJECT_BYTES)` cannot tell "decoded to exactly the ceiling" (legal) apart from
/// "silently truncated at the ceiling" (an over-ceiling frame returned as plausible bytes).
pub(crate) fn decode_object_bounded(compressed: impl Read) -> Result<Vec<u8>, BoundedDecodeError> {
    let mut decoder = zstd::stream::read::Decoder::new(compressed)
        .map_err(|e| BoundedDecodeError::Malformed(e.to_string()))?;
    let mut bytes = Vec::new();
    decoder.by_ref()
        .take(MAX_OBJECT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| BoundedDecodeError::Malformed(e.to_string()))?;

    if bytes.len() > MAX_OBJECT_BYTES {
        return Err(BoundedDecodeError::OverCeiling);
    }

    Ok(bytes)
}

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

/// A bounded content-addressed cache of *parsed* trees, one layer above `file_utils`'s raw-bytes
/// read cache. A tree's hash is its content and `parser::object::loose_object_parser::parse` is a
/// pure function of the bytes (no environment, no randomness), so a hash-keyed parse is valid
/// forever — the only question is how much of it to keep around, not when to invalidate it.
///
/// Without this, a reconstruction-heavy walk that revisits the same tree from many descents
/// (`resolve_tree_file` walking root→leaf once per changed file, each time re-entering the same
/// shared ancestors) re-parses that tree's bytes — reallocating every entry's name and hash
/// `String` — on every visit, even though the byte cache below already served the decompressed
/// bytes for free. Caching the *parse* turns every visit after the first into a pointer clone.
///
/// Mirrors `file_utils`'s read cache exactly — both share [`super::two_gen_cache::TwoGenCache`]'s
/// eviction/bounding shape, so they can't silently drift apart. Same warehouse-qualified key
/// (`read_cache_key`, so a server hosting several warehouses never serves one's parsed tree for
/// another's request). It is weighted by each tree's raw object size (the number already in hand
/// from the byte-cache read that always precedes a parse here) as an inexpensive proxy for the
/// parsed form's live memory — budgeted well below the byte cache's own 128 MiB, since a parsed
/// tree's `String`s cost more live memory per input byte than the bytes they were parsed from,
/// and this cache is additive on top of (not a replacement for) the byte cache it sits above.
const TREE_PARSE_CACHE_BUDGET: usize = 32 * 1024 * 1024;

/// Trees whose raw bytes exceed this are not cached (one huge tree must not evict the whole
/// working set of small trees a walk actually revisits) — the same one-eighth-of-budget ratio
/// `file_utils::READ_CACHE_MAX_ENTRY` uses.
const TREE_PARSE_CACHE_MAX_ENTRY: usize = TREE_PARSE_CACHE_BUDGET / 8;

fn tree_parse_cache() -> &'static crate::util::two_gen_cache::TwoGenCache<TreeItem> {
    static CACHE: std::sync::OnceLock<crate::util::two_gen_cache::TwoGenCache<TreeItem>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(||
        crate::util::two_gen_cache::TwoGenCache::new(TREE_PARSE_CACHE_BUDGET, TREE_PARSE_CACHE_MAX_ENTRY)
    )
}

fn tree_parse_cache_get(hash: &str) -> Option<std::sync::Arc<TreeItem>> {
    tree_parse_cache().get(&file_utils::read_cache_key(hash))
}

fn tree_parse_cache_put(hash: &str, tree: std::sync::Arc<TreeItem>, weight: usize) {
    tree_parse_cache().put(&file_utils::read_cache_key(hash), tree, weight);
}

/// Read and parse a tree object's bytes straight from the byte cache, touching the parsed-tree
/// cache neither for a hit nor a put. The shared core of both `load_tree`'s and
/// [`load_tree_shared`]'s miss path — the parse itself, and the byte length [`load_tree_shared`]
/// needs to weigh the cache entry it inserts.
///
/// # Returns
/// * `Ok((TreeItem, usize))` - The parsed tree and its raw (pre-parse) object byte length.
/// * `Err(String)`           - If the object does not exist, could not be parsed, or is not a tree.
fn parse_tree_uncached(hash: &str) -> Result<(TreeItem, usize), String> {
    // Only the parse borrows the bytes, so take the shared `Arc` — a byte-cache hit is a pointer
    // clone under its lock, not a copy of the whole tree object.
    let bytes = file_utils::retrieve_object_by_hash_shared(hash)?;

    let tree = match parser::object::loose_object_parser::parse(&bytes)? {
        ParsedObject::Tree(tree) => tree,
        other => return Err(format!("Object {} is a {}, not a tree.", hash, other.get_type())),
    };

    Ok((tree, bytes.len()))
}

/// Load and parse the tree object with the given hash from the object store, sharing the parsed
/// result through the process-wide parsed-tree cache (see the module-level
/// [`super::two_gen_cache::TwoGenCache`] this is built on). Note that only one level is loaded: the
/// returned tree's subtree children carry their hashes, but their own children must be loaded
/// separately.
///
/// Callers that only need to borrow the tree (walk its entries, look one up by name) should
/// prefer this over [`load_tree`]: a cache hit is a pointer clone, not a copy of the tree's
/// entries. Prefer it specifically for a tree a caller expects to *revisit* — a resolve or walk
/// that re-enters the same shared ancestors from many descents. A one-shot caller that reads each
/// tree at most once (a full-history walk, an export) should still use [`load_tree`]: caching a
/// tree it will never revisit only pays a cache-maintenance tax and evicts trees other callers
/// (in the same process) would have reused.
///
/// # Arguments
/// * `hash` - The hash of the tree object.
///
/// # Returns
/// * `Ok(Arc<TreeItem>)` - The parsed tree, shared with the cache.
/// * `Err(String)`       - If the object does not exist, could not be parsed, or is not a tree.
pub fn load_tree_shared(hash: &str) -> Result<std::sync::Arc<TreeItem>, String> {
    if let Some(tree) = tree_parse_cache_get(hash) {
        return Ok(tree);
    }

    let (tree, weight) = parse_tree_uncached(hash)?;

    let tree = std::sync::Arc::new(tree);
    tree_parse_cache_put(hash, std::sync::Arc::clone(&tree), weight);
    Ok(tree)
}

/// Load and parse the tree object with the given hash from the object store.
/// Note that only one level is loaded: the returned tree's subtree children carry their
/// hashes, but their own children must be loaded separately.
///
/// This owns-the-tree form is for callers that keep or mutate the result. Callers that only
/// *borrow* the tree (a walk that just reads entries) should use [`load_tree_shared`], which hands
/// back the cached `Arc` so a hit is a pointer clone rather than a clone of every entry.
///
/// A cache hit is served the same way `load_tree_shared` serves one (a pointer clone, then one
/// `TreeItem` clone off the lock — cheap, since only one tree level is ever loaded, see the
/// type's doc comment). A cache *miss*, though, is parsed and returned directly without ever
/// entering the parsed-tree cache: this owned form is also the one every one-shot walk uses (gc
/// reachability, `export-git`, `merge`, `audit`), and those never revisit a tree they load, so
/// caching it here would only evict the shared-ancestor working set `load_tree_shared`'s
/// revisit-heavy callers actually rely on, for a cache entry that would never be hit again.
///
/// # Arguments
/// * `hash` - The hash of the tree object.
///
/// # Returns
/// * `Ok(TreeItem)` - The parsed tree.
/// * `Err(String)`  - If the object does not exist, could not be parsed, or is not a tree.
pub fn load_tree(hash: &str) -> Result<TreeItem, String> {
    if let Some(tree) = tree_parse_cache_get(hash) {
        // The single clone an owned caller needs happens here, *outside* the cache lock.
        return Ok((*tree).clone());
    }

    Ok(parse_tree_uncached(hash)?.0)
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

/// The ordered chunk hashes of a recipe named by `hash`, loaded from the local object store.
/// The convenience the closure-audit and download descents both want: they never need the sizes
/// or the content hash, only which chunk objects a recipe references.
///
/// # Arguments
/// * `hash` - The hash of the recipe object (a chunked file's tree-entry hash).
///
/// # Returns
/// * `Ok(Vec<String>)` - The recipe's chunk hashes, in order.
/// * `Err(String)`     - If the recipe is absent, not a recipe, or structurally invalid.
pub fn recipe_chunk_hashes(hash: &str) -> Result<Vec<String>, String> {
    Ok(load_recipe(hash)?.chunks.into_iter().map(|chunk| chunk.hash).collect())
}

/// Parse a recipe from raw object bytes the caller already holds (rather than reading it from the
/// local object store), enforcing the whole-object ceiling first. This is the store-backed
/// recipe-load path the AWS head needs for the commit-gate chunk descent (§9.4b W4): a working
/// pallet's recipe is a file-entry object, so it is *not* mirrored into the audit scratch — the
/// head fetches its bytes from object storage and parses them here, without ever persisting a
/// (potentially large) recipe in the scratch. The ceiling check bounds a hand-crafted over-size
/// recipe the same way [`store_object_bytes`] would on any other import path.
///
/// # Arguments
/// * `hash`  - The hash the bytes are addressed by (for the error message; not re-verified here —
///             the caller reads it from a content-addressed store that already verified it).
/// * `bytes` - The full (uncompressed) recipe object bytes.
///
/// # Returns
/// * `Ok(Recipe)`  - The parsed, structurally valid recipe.
/// * `Err(String)` - If the bytes exceed the ceiling, are not a recipe, or are structurally invalid.
pub fn parse_recipe_bytes(hash: &str, bytes: &[u8]) -> Result<Recipe, String> {
    enforce_object_ceiling(bytes)?;

    match parser::object::loose_object_parser::parse(bytes)? {
        ParsedObject::Recipe(recipe) => Ok(recipe),
        other => Err(format!("Object {} is a {}, not a recipe.", hash, other.get_type())),
    }
}

/// Parse a tree from raw object bytes the caller already holds (rather than reading it from the
/// local object store), enforcing the whole-object ceiling first. The store-backed tree-load path
/// the AWS head needs for the commit-gate closure prune (§9.4b W1): pruning a new parcel's tree
/// against the prior head's (already-complete) tree requires reading the prior head's subtrees,
/// which are *not* mirrored into the audit scratch (they are already-audited history) — so the head
/// fetches their bytes from object storage and parses them here. The ceiling check bounds a
/// hand-crafted over-size tree the same way [`store_object_bytes`] would on any other import path.
///
/// # Arguments
/// * `hash`  - The hash the bytes are addressed by (for the error message; not re-verified here —
///             the caller reads it from a content-addressed store that already verified it).
/// * `bytes` - The full (uncompressed) tree object bytes.
///
/// # Returns
/// * `Ok(TreeItem)` - The parsed tree.
/// * `Err(String)`  - If the bytes exceed the ceiling, or are not a tree.
pub fn parse_tree_bytes(hash: &str, bytes: &[u8]) -> Result<TreeItem, String> {
    enforce_object_ceiling(bytes)?;

    match parser::object::loose_object_parser::parse(bytes)? {
        ParsedObject::Tree(tree) => Ok(tree),
        other => Err(format!("Object {} is a {}, not a tree.", hash, other.get_type())),
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
    // Every ancestor along the walk is only ever borrowed (name/hash lookups), so the shared
    // form pays off here in particular: a caller resolving many paths under the same root (a
    // `diff --staged` render loop, one call per changed file) re-enters the same shared ancestor
    // trees over and over, and each re-entry after the first is a pointer clone out of the cache
    // instead of a re-parse.
    let mut current = load_tree_shared(root_tree_hash)?;
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
            Some(subtree_hash) => current = load_tree_shared(&subtree_hash)?,
            None => return Ok(None),
        }
    }

    Ok(None)
}

/// The hash of an empty (root) tree: the canonical base for comparing or merging against
/// "nothing" — a root parcel with no parent (cherry-pick), or the reserved empty-tree diff
/// token (`diff :empty <revision>`, the head's CLI surface). Building and storing it is cheap
/// and content-addressed (identical every time), so every caller gets the same hash back and
/// the object always exists once any caller has run.
///
/// # Returns
/// * `Ok(String)`  - The hash of the (now-stored) empty tree.
/// * `Err(String)` - If the object could not be stored.
pub fn empty_tree_hash() -> Result<String, String> {
    let empty = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut object = LooseObjectBuilder::build_tree(&empty);
    object.store()?;

    Ok(object.hash)
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

/// Validate an object's content address and every import-side size policy without storing it.
/// Native transport packs use this after reconstructing a quarantined record: publication must
/// enforce the same boundary as [`store_object_bytes`], even though the object's final home is an
/// aggregate pack rather than a loose file.
pub(crate) fn validate_imported_object(claimed_hash: &str, bytes: &[u8]) -> Result<(), String> {
    verify_object_bytes(claimed_hash, bytes)?;
    enforce_object_ceiling(bytes)?;
    enforce_chunk_ceiling(claimed_hash, bytes)
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
    store_object_bytes_impl(claimed_hash, bytes, true)
}

/// The gate-free sibling of [`store_object_bytes`]: identical hash verification, ceilings,
/// compression and atomic write, but never consults [`file_utils::does_object_exist`] first — so
/// it overwrites whatever bytes are already at the object's path rather than skipping the write.
/// A sibling entry point, not a flag on [`store_object_bytes`] itself: every existing caller of
/// that function keeps its current dedup-on-presence behavior unchanged.
///
/// This exists for exactly one caller: the D4 corrupt-candidate refetch (DESIGN.html §3.1.1,
/// [`remote_utils::fetch_corrupt_replacements`](crate::util::remote_utils::fetch_corrupt_replacements)).
/// A corrupt loose dentry is never deleted before its replacement is fetched — deleting it would
/// create a window with no copy at all if the fetch then failed. Instead, once a verified good
/// copy is in hand, this function's call into [`file_utils::write_object_to_file`] (→
/// `write_file_atomically`) supersedes the corrupt bytes with one atomic rename: at every instant
/// the path holds either the old (corrupt) bytes or the new (verified) ones, never neither.
///
/// # Returns
/// * `Ok(())`      - The object was verified and stored, replacing whatever was there before.
/// * `Err(String)` - If the hash does not match the bytes, an object/chunk ceiling was exceeded,
///                   or the write failed.
pub(crate) fn force_store_object_bytes(claimed_hash: &str, bytes: &[u8]) -> Result<(), String> {
    store_object_bytes_impl(claimed_hash, bytes, false).map(|_| ())
}

/// Shared body for [`store_object_bytes`] and [`force_store_object_bytes`]: the only difference
/// between the two is whether the presence gate (`file_utils::does_object_exist`) is consulted
/// before writing — everything else (hash check, ceilings, compression, the write itself) is one
/// code path so the two can never silently drift apart.
fn store_object_bytes_impl(claimed_hash: &str, bytes: &[u8], check_presence: bool) -> Result<bool, String> {
    let actual = hash_object_bytes(bytes);

    if actual != claimed_hash {
        return Err(format!(
            "Object content does not match its claimed hash {} (actual: {}); refusing to store it.",
            claimed_hash, actual
        ));
    }

    // Whole-object ceiling: an over-`MAX_OBJECT_BYTES` object is refused on the way in (import and
    // local write share this policy). Reads never reach here, so a grandfathered giant stays
    // readable; only new authorship or a fresh import is gated.
    enforce_object_ceiling(bytes)?;

    // Per-type ceiling (review W2): a `Chunk`-typed object above `MAX_CHUNK_BYTES` is refused on
    // store as well as on read, even though a larger object would otherwise be a legal object.
    // Without this a malicious recipe could reference an over-size chunk and the streaming-
    // assembly memory bound would be far looser than the per-chunk ceiling explicit types buy.
    enforce_chunk_ceiling(claimed_hash, bytes)?;

    if check_presence && file_utils::does_object_exist(claimed_hash)? {
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

/// The honest refusal for an object that exceeds the whole-object ceiling: it names the limit and,
/// for the one object kind that can legitimately approach it, the practical bound the ceiling
/// implies (a very large single directory, or an un-representably large chunked file) — so a
/// maintainer who hits it learns *why*, not just *that*, they hit it.
fn object_ceiling_error(object_type: Option<&ObjectType>, len: usize) -> String {
    let implication = match object_type {
        // ~88 bytes/entry ⇒ ~762,000 entries in one directory at the ceiling.
        Some(ObjectType::Tree) => ": a single directory of roughly 762,000 entries. Split it \
            across subdirectories",
        // ~68 bytes/entry ⇒ ~987,000 chunks ⇒ ~964 GiB at the average chunk size.
        Some(ObjectType::Recipe) => ": a chunked file of roughly 964 GiB at the average chunk \
            size. A single file beyond that is not representable by one recipe",
        _ => "",
    };

    format!(
        "Object is {} bytes, above the {}-byte whole-object ceiling{}; refusing to store it.",
        len, MAX_OBJECT_BYTES, implication
    )
}

/// Refuse an object whose byte length exceeds the whole-object ceiling — the write-side check, for
/// a locally authored object whose type is already known (a tree or recipe `stack` is about to
/// author). The import-side counterpart is the peek-and-check inside [`store_object_bytes`].
///
/// # Arguments
/// * `object_type` - The object's type (only tailors the message).
/// * `len`         - The object's full (uncompressed) byte length.
///
/// # Returns
/// * `Ok(())`      - If the object is within the ceiling.
/// * `Err(String)` - If it exceeds `MAX_OBJECT_BYTES`.
pub fn check_object_ceiling(object_type: &ObjectType, len: usize) -> Result<(), String> {
    if len <= MAX_OBJECT_BYTES {
        return Ok(());
    }

    Err(object_ceiling_error(Some(object_type), len))
}

/// Enforce the whole-object ceiling on the way *into* the store from an untrusted source (a bundle
/// or a remote). The check is on the true byte length, so a lying header cannot slip an over-ceiling
/// object through; the peeked type only tailors the message (and a header that cannot be peeked
/// still fails on length alone).
fn enforce_object_ceiling(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() <= MAX_OBJECT_BYTES {
        return Ok(());
    }

    let object_type = peek_object_header(bytes).ok().map(|(object_type, _)| object_type);
    Err(object_ceiling_error(object_type.as_ref(), bytes.len()))
}

/// What a streamed store attempt observed about the bytes it consumed.
struct StreamOutcome {
    /// The total raw bytes actually read (for the exact-length/truncation check).
    read_total: u64,

    /// The Blake3 hex hash of those bytes (to verify against the claimed hash).
    hash: String,
}

/// Store an object streamed from `reader`, bounding memory to a small constant regardless of the
/// object's size or a lying declared length — the unconditional defense behind the bundle-import
/// bomb ceiling (DESIGN.html §9.4b, §5.0 row D item 7). Exactly `expected_len` bytes are read (a
/// shorter stream is reported as truncation), hashed through an incremental Blake3, and
/// zstd-encoded incrementally to a temp file in the object's own shard folder; the temp file is
/// promoted with an atomic rename **only** if the finished hash matches `claimed_hash`. A mismatch
/// (a bomb, a corrupt or a truncated record) discards the temp file — so nothing unverified ever
/// lands and a failed import cleans up after itself.
///
/// The whole-object ceiling (`MAX_OBJECT_BYTES`) is deliberately *not* enforced here: this path
/// must still import a grandfathered over-ceiling object from an old-version bundle. The per-type
/// chunk ceiling *is* enforced — a `Chunk`-typed object whose payload exceeds `MAX_CHUNK_BYTES` is
/// refused mid-stream, exactly as [`store_object_bytes`] refuses it whole.
///
/// # Arguments
/// * `claimed_hash` - The hash the streamed bytes must have.
/// * `reader`       - The source of the raw (uncompressed) object bytes.
/// * `expected_len` - The exact number of bytes the record declares (for the truncation check).
///
/// # Returns
/// * `Ok(true)`    - The object was verified, stored, and — once
///                   [`taint_utils::activate`](crate::util::taint_utils::activate) has been
///                   called in this process — no durability taint was found standing for this
///                   root on the re-check that runs right before this returns (see
///                   [`file_utils::sync_dir_or_taint`]). An unactivated process skips that
///                   re-check entirely.
/// * `Ok(false)`   - The object was already present (temp discarded, nothing written).
/// * `Err(String)` - On a short/truncated stream, a hash mismatch, an over-ceiling chunk, or I/O.
///                   A directory-sync failure specifically leaves the object visible at
///                   `final_path` with its directory entry not proven durable — once activated,
///                   that is recorded as a taint over exactly that path (see
///                   [`file_utils::sync_dir_or_taint`]). A later
///                   `heal_utils::heal_if_tainted` call restages exactly that path and clears the
///                   taint once it is durable again.
pub fn store_object_stream(claimed_hash: &str,
                           reader: &mut impl Read,
                           expected_len: u64) -> Result<bool, String> {
    let (folder, file_name) = file_utils::get_path_for_object(claimed_hash)?;
    let folder_path = std::path::Path::new(&folder);
    file_utils::create_folder_if_not_exists(folder_path)?;

    // A temp file in the object's own shard folder, unique per write. `write_file_atomically`
    // (`file_utils.rs`) names its own temp files unique-per-write the same way (a process-wide
    // atomic counter plus the pid), off its own independent `TEMP_FILE_COUNTER` — two
    // independent counters can coincidentally reach the same numeric value, so if this path used
    // the identical `"{name}.tmp{pid}-{id}"` infix, a write here and a concurrent
    // `write_file_atomically` write for the *same* hash (e.g. one thread streaming a large
    // duplicate-hash bundle record while another fetches the same hash loose) could collide on
    // one temp path — precisely the hazard `write_file_atomically`'s own per-write-uniqueness
    // comment exists to prevent. A distinct infix (`.stream.tmp` here, `.tmp` there) rules the
    // collision out structurally: the two paths can never match regardless of either counter's
    // value, which is stronger than relying on the counters staying apart.
    static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let write_id = TEMP_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp_path = folder_path.join(format!(
        "{}.stream.tmp{}-{}", file_name, std::process::id(), write_id
    ));

    // Stream to the temp file, hashing and (chunk-)ceiling-checking as we go. Wrapped so any error
    // path removes the temp file below — a failed import must not leave a partial file behind.
    let outcome = match stream_to_temp(claimed_hash, reader, expected_len, &temp_path) {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }
    };

    // Exactly `expected_len`, not fewer: a short stream is a truncated record, reported the same
    // way the buffered import path reports it.
    if outcome.read_total != expected_len {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "The bundle is truncated: a record declared {} bytes but only {} remained.",
            expected_len, outcome.read_total
        ));
    }

    // Content-addressing: nothing unverified may land.
    if outcome.hash != claimed_hash {
        let _ = std::fs::remove_file(&temp_path);
        return Err(format!(
            "Object content does not match its claimed hash {} (actual: {}); refusing to store it.",
            claimed_hash, outcome.hash
        ));
    }

    // Already present (an idempotent re-import): drop the temp and report the skip.
    if file_utils::does_object_exist(claimed_hash)? {
        let _ = std::fs::remove_file(&temp_path);
        return Ok(false);
    }

    // Promote atomically. The temp file's bytes were already fsynced in `stream_to_temp`, so the
    // rename can never publish a name whose contents never reached disk; fsync the directory so the
    // rename itself survives power loss (mirrors `file_utils::write_file_atomically`). A sync
    // failure taints `final_path` (the same fire-site class `write_file_atomically`'s own
    // immediate branch belongs to — this path exists only because it cannot reuse that function's
    // temp-naming scheme, not because its durability contract differs); a sync success is
    // re-checked against any taint already standing for this root before this function may report
    // `Ok(true)` — see `file_utils::sync_dir_or_taint`.
    let final_path = folder_path.join(&file_name);
    std::fs::rename(&temp_path, &final_path)
        .map_err(|e| format!("Error while moving a streamed object into place: {}", e))?;
    file_utils::sync_dir_or_taint(folder_path, &[final_path.as_path()])?;

    Ok(true)
}

/// The streaming core of [`store_object_stream`]: read exactly up to `expected_len` bytes from
/// `reader`, hash them, zstd-encode them to `temp_path`, and enforce the per-chunk ceiling on a
/// `Chunk`-typed object mid-stream. Returns what it observed; the caller verifies length + hash and
/// promotes or discards the temp file. Peak memory is one read block plus zstd's own window —
/// never the whole object.
fn stream_to_temp(claimed_hash: &str,
                  reader: &mut impl Read,
                  expected_len: u64,
                  temp_path: &Path) -> Result<StreamOutcome, String> {
    let temp_file = std::fs::File::create(temp_path)
        .map_err(|e| format!("Error while creating a streamed object temp file: {}", e))?;
    let mut encoder = zstd::stream::Encoder::new(std::io::BufWriter::new(temp_file), 0)
        .map_err(|e| format!("Error while starting a streamed object: {}", e))?;

    let mut hasher = blake3::Hasher::new();
    let mut read_total: u64 = 0;
    // The object header (version, type, length, null) so a `Chunk`-typed object's payload can be
    // ceiling-checked mid-stream. `header_len` becomes `Some` once the header parses.
    let mut object_type: Option<ObjectType> = None;
    let mut header_len: usize = 0;
    let mut prefix: Vec<u8> = Vec::new();
    let mut block = vec![0u8; 64 * 1024];

    let mut bounded = reader.take(expected_len);

    loop {
        let read = bounded.read(&mut block)
            .map_err(|e| format!("Error while reading a streamed object: {}", e))?;
        if read == 0 {
            break;
        }

        let data = &block[..read];
        hasher.update(data);
        encoder.write_all(data)
            .map_err(|e| format!("Error while writing a streamed object: {}", e))?;
        read_total += read as u64;

        // Peek the header once (a real header terminates within a handful of bytes; if it has not
        // parsed within a small prefix the object is not a recognizable chunk, so stop trying and
        // let the read-side parser enforce the ceiling on anything read back as a chunk).
        if object_type.is_none() && header_len == 0 {
            prefix.extend_from_slice(data);
            match peek_object_header(&prefix) {
                Ok((peeked_type, peeked_header_len)) => {
                    object_type = Some(peeked_type);
                    header_len = peeked_header_len;
                }
                Err(_) if prefix.len() > 4096 => {
                    // Sentinel: give up peeking, but do not fail — an unrecognized header is not a
                    // chunk, and the hash check still guards what actually lands.
                    header_len = usize::MAX;
                }
                Err(_) => {}
            }
        }

        // A `Chunk`-typed object whose payload exceeds the per-chunk ceiling is refused mid-stream,
        // before the whole (bounded) payload is even written — the streaming twin of
        // `enforce_chunk_ceiling`.
        if object_type == Some(ObjectType::Chunk)
            && read_total.saturating_sub(header_len as u64) > chunk_utils::MAX_CHUNK_BYTES as u64 {
            return Err(format!(
                "Chunk object {} exceeds the {}-byte chunk ceiling; refusing to store it.",
                claimed_hash, chunk_utils::MAX_CHUNK_BYTES
            ));
        }
    }

    let buf_writer = encoder.finish()
        .map_err(|e| format!("Error while finishing a streamed object: {}", e))?;
    let temp_file = buf_writer.into_inner()
        .map_err(|e| format!("Error while flushing a streamed object: {}", e.into_error()))?;
    if file_utils::fsync_enabled() {
        temp_file.sync_all()
            .map_err(|e| format!("Error while syncing a streamed object: {}", e))?;
    }
    // Close the handle before the caller renames it (renaming an open file fails on Windows).
    drop(temp_file);

    Ok(StreamOutcome { read_total, hash: hasher.finalize().to_hex().to_string() })
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

    /// The whole-object ceiling refuses a locally authored over-size **tree** on the way in, with
    /// an honest message that names the limit and the practical bound (a huge single directory).
    /// A pre-existing giant is never re-authored, so this gates only new authorship.
    #[test]
    fn store_refuses_an_over_ceiling_tree_on_write() {
        use crate::model::object::loose_object::LooseObject;
        let _scratch = Scratch::new("ceiling-write-tree");

        // The ceiling check runs before the object is hashed or written, so the (dummy) hash is
        // never consulted — a cheap zeroed buffer one byte over the ceiling is enough.
        let mut giant = LooseObject {
            content: vec![0u8; MAX_OBJECT_BYTES + 1],
            object_type: ObjectType::Tree,
            hash: "0".repeat(64),
        };

        let err = giant.store().expect_err("an over-ceiling tree must be refused on write");
        assert!(err.contains("whole-object ceiling"), "names the limit: {}", err);
        assert!(err.contains("directory"), "names the practical bound for a tree: {}", err);
    }

    /// The same ceiling refuses a locally authored over-size **recipe**, with the recipe-specific
    /// bound (an un-representably large chunked file).
    #[test]
    fn store_refuses_an_over_ceiling_recipe_on_write() {
        use crate::model::object::loose_object::LooseObject;
        let _scratch = Scratch::new("ceiling-write-recipe");

        let mut giant = LooseObject {
            content: vec![0u8; MAX_OBJECT_BYTES + 1],
            object_type: ObjectType::Recipe,
            hash: "0".repeat(64),
        };

        let err = giant.store().expect_err("an over-ceiling recipe must be refused on write");
        assert!(err.contains("whole-object ceiling"), "names the limit: {}", err);
        assert!(err.contains("chunked file"), "names the practical bound for a recipe: {}", err);
    }

    /// `check_object_ceiling` accepts anything at or below the ceiling — the write-path fast path
    /// that never allocates a giant to learn it is fine.
    #[test]
    fn check_object_ceiling_accepts_up_to_the_limit() {
        assert!(check_object_ceiling(&ObjectType::Tree, MAX_OBJECT_BYTES).is_ok());
        assert!(check_object_ceiling(&ObjectType::Recipe, 0).is_ok());
        assert!(check_object_ceiling(&ObjectType::Tree, MAX_OBJECT_BYTES + 1).is_err());
    }

    /// A large (streamed) object round-trips: it is hashed and zstd-encoded to a temp file, then
    /// promoted, and reads back byte-identical — memory bounded, never the whole object at once.
    #[test]
    fn store_object_stream_round_trips_a_large_object() {
        let _scratch = Scratch::new("stream-roundtrip");

        // A real blob object above the streaming threshold (so the stream path is taken and its
        // header parses as a non-chunk type).
        let content = vec![0x5Au8; STREAM_STORE_THRESHOLD_BYTES + 500_000];
        let object = LooseObjectBuilder::build_blob(&Blob { content });
        let raw = object.content.clone();

        let mut reader = std::io::Cursor::new(raw.clone());
        let stored = store_object_stream(&object.hash, &mut reader, raw.len() as u64).unwrap();
        assert!(stored, "the object was newly stored");

        assert_eq!(file_utils::retrieve_object_by_hash(&object.hash).unwrap(), raw);

        // Storing it again is an idempotent skip — nothing new written.
        let mut reader = std::io::Cursor::new(raw.clone());
        let stored = store_object_stream(&object.hash, &mut reader, raw.len() as u64).unwrap();
        assert!(!stored, "an already-present object is skipped");
    }

    /// Fire-site d (durable-taint wiring): `store_object_stream`'s own promote-then-sync — the
    /// same fire-site class `write_file_atomically`'s immediate branch belongs to, wired through
    /// the same shared helper. Reverting `file_utils::sync_dir_or_taint` back to a bare
    /// `file_utils::sync_dir(folder_path)?` call kills this test: no taint file would exist to
    /// read back.
    #[test]
    #[cfg(unix)]
    fn store_object_stream_dir_sync_failure_taints_the_single_final_path_once_activated() {
        use crate::util::taint_utils;
        use crate::util::file_utils::SyncDirFaultGuard;

        let _serial = taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        taint_utils::activate();

        let scratch = Scratch::new("stream-taint");
        let forklift = crate::globals::forklift_root();

        let content = vec![0x77u8; STREAM_STORE_THRESHOLD_BYTES + 10_000];
        let object = LooseObjectBuilder::build_blob(&Blob { content });
        let raw = object.content.clone();

        // Needle on this object's own shard folder (`objects/<2-hex-prefix>`) — unique enough not
        // to collide with the scratch root's own name.
        let needle = format!("objects{}{}", std::path::MAIN_SEPARATOR, &object.hash[0..2]);
        let _guard = SyncDirFaultGuard::failing(&needle);

        let mut reader = std::io::Cursor::new(raw.clone());
        let error = store_object_stream(&object.hash, &mut reader, raw.len() as u64)
            .expect_err("a blocked directory sync must fail the promote");
        assert!(error.contains("injected directory-sync failure"), "got: {}", error);

        let (folder, file_name) = file_utils::get_path_for_object(&object.hash).unwrap();
        let final_path = std::path::Path::new(&folder).join(&file_name);
        assert!(final_path.exists(), "the rename must have landed before the directory sync failed");

        let state = taint_utils::read_taints(&forklift).unwrap();
        let expected: std::collections::BTreeSet<PathBuf> =
            [final_path.strip_prefix(&forklift).unwrap().to_path_buf()].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "the taint must record exactly the single final path, root-relative");

        drop(scratch);
    }

    /// A streamed object whose bytes do not match the claimed hash is refused, nothing lands, and
    /// the temp file is cleaned up — the streaming twin of `store_object_bytes`'s hash check, the
    /// defense that catches an under-ceiling declared-length lie without ever buffering the object.
    #[test]
    fn store_object_stream_refuses_a_hash_mismatch_and_leaves_nothing() {
        let _scratch = Scratch::new("stream-mismatch");

        let content = vec![0x11u8; STREAM_STORE_THRESHOLD_BYTES + 100_000];
        let object = LooseObjectBuilder::build_blob(&Blob { content });
        let raw = object.content.clone();
        let wrong_hash = hash_object_bytes(b"a different object entirely");

        let mut reader = std::io::Cursor::new(raw.clone());
        let err = store_object_stream(&wrong_hash, &mut reader, raw.len() as u64)
            .expect_err("a hash mismatch must be refused");
        assert!(err.contains("does not match its claimed hash"), "{}", err);

        assert!(!file_utils::does_object_exist(&wrong_hash).unwrap(), "nothing unverified may land");
        assert!(no_temp_files_left(&_scratch.root), "the temp file must be cleaned up");
    }

    /// A `Chunk`-typed object that reaches the streaming path with a payload above the per-chunk
    /// ceiling is refused mid-stream — the streaming enforcement of W2, so a hand-crafted bundle
    /// cannot slip an over-size chunk past the buffered check by making it large enough to stream.
    #[test]
    fn store_object_stream_refuses_an_over_ceiling_chunk() {
        let _scratch = Scratch::new("stream-chunk-ceiling");

        let over = vec![0x22u8; chunk_utils::MAX_CHUNK_BYTES + 100_000];
        let object = LooseObjectBuilder::build_chunk(&Chunk { content: over });
        let raw = object.content.clone();

        let mut reader = std::io::Cursor::new(raw.clone());
        let err = store_object_stream(&object.hash, &mut reader, raw.len() as u64)
            .expect_err("an over-ceiling chunk must be refused");
        assert!(err.contains("chunk ceiling"), "{}", err);
        assert!(!file_utils::does_object_exist(&object.hash).unwrap(), "nothing unverified may land");
        assert!(no_temp_files_left(&_scratch.root), "the temp file must be cleaned up");
    }

    /// A stream shorter than its declared length is reported as truncation, and nothing lands.
    #[test]
    fn store_object_stream_reports_truncation() {
        let _scratch = Scratch::new("stream-truncated");

        let content = vec![0x33u8; 500_000];
        let object = LooseObjectBuilder::build_blob(&Blob { content });
        let raw = object.content.clone();

        // Declare far more than the stream actually carries (above the streaming threshold).
        let mut reader = std::io::Cursor::new(raw.clone());
        let declared = (STREAM_STORE_THRESHOLD_BYTES + 1_000_000) as u64;
        let err = store_object_stream(&object.hash, &mut reader, declared)
            .expect_err("a short stream must be truncation");
        assert!(err.contains("truncated"), "{}", err);
        assert!(no_temp_files_left(&_scratch.root), "the temp file must be cleaned up");
    }

    /// `store_object_stream`'s temp file must never collide with `write_file_atomically`'s (the
    /// review's W2 finding): both name a temp file `"{name}.tmp{pid}-{id}"`-style off their own
    /// independent process-wide counter, so two concurrent writes of the *same* hash — one via
    /// each path — could reach the identical (pid, id) pair by coincidence if the infixes matched.
    /// Firing several of each concurrently at the same hash proves the fix: every writer converges
    /// on the same correct bytes with no corruption, regardless of interleaving.
    #[test]
    fn store_object_stream_never_collides_with_write_file_atomically_on_the_same_hash() {
        let scratch = Scratch::new("stream-vs-atomic-no-collision");

        let content = vec![0x99u8; STREAM_STORE_THRESHOLD_BYTES + 50_000];
        let object = LooseObjectBuilder::build_blob(&Blob { content });
        let raw = object.content.clone();
        let hash = object.hash.clone();
        let root = scratch.root.clone();

        let handles: Vec<_> = (0..8).map(|i| {
            let raw = raw.clone();
            let hash = hash.clone();
            let root = root.clone();

            std::thread::spawn(move || {
                // `StorageRootScope` is thread-local, so a spawned thread must re-enter it.
                let _scope = crate::globals::StorageRootScope::enter(&root);

                if i % 2 == 0 {
                    store_object_bytes(&hash, &raw)
                } else {
                    let mut reader = std::io::Cursor::new(raw.clone());
                    store_object_stream(&hash, &mut reader, raw.len() as u64)
                }
            })
        }).collect();

        for handle in handles {
            handle.join().expect("no writer thread panics").expect("no writer fails");
        }

        assert_eq!(file_utils::retrieve_object_by_hash(&hash).unwrap(), raw,
            "the object lands byte-identical regardless of which writer's temp path won the race");
    }

    /// Whether any `.tmp*` file is left under the object store (a failed streamed store must clean
    /// up after itself).
    fn no_temp_files_left(root: &Path) -> bool {
        let objects = root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join("objects");
        let Ok(folders) = std::fs::read_dir(&objects) else { return true; };
        for folder in folders.flatten() {
            if let Ok(files) = std::fs::read_dir(folder.path()) {
                for file in files.flatten() {
                    if file.file_name().to_string_lossy().contains(".tmp") { return false; }
                }
            }
        }
        true
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

    /// A `Read` wrapper that hands out at most one byte per call and counts how many it has
    /// handed out — the deterministic proof a decode stopped pulling input before the compressed
    /// frame ended, without any fault hook.
    ///
    /// One byte at a time defeats `BufReader`'s normal behaviour of filling its whole internal
    /// buffer (tens of KiB) from a single underlying read whenever that much is available: a
    /// bomb frame's *compressed* representation is tiny (a genuine bomb is small on disk), so a
    /// single eager fill would slurp the entire frame before decode output ever reaches the
    /// ceiling, making "bytes consumed" trivially equal "frame length" regardless of whether the
    /// decode is bounded. Throttling to one byte per call keeps input consumption in lockstep
    /// with decode progress, so under-consumption reflects the decoder's real behaviour.
    struct OneByteAtATime<'a> {
        remaining: &'a [u8],
        consumed: usize,
    }

    impl<'a> Read for OneByteAtATime<'a> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.remaining.is_empty() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.remaining[0];
            self.remaining = &self.remaining[1..];
            self.consumed += 1;
            Ok(1)
        }
    }

    /// A zstd frame that decodes to exactly `len` zero bytes. Zeros RLE-compress to a few dozen
    /// bytes regardless of `len`, so a fixture many times the ceiling stays cheap to build.
    fn zstd_frame_of_zeros(len: u64) -> Vec<u8> {
        zstd::stream::encode_all(std::io::repeat(0u8).take(len), 0)
            .expect("encoding a repeat-byte source never fails")
    }

    #[test]
    fn decode_object_bounded_stops_reading_a_bomb_before_the_frame_ends() {
        // INV-1 (streaming cap, the central spike): a small frame that *would* decode to 4x the
        // ceiling must be refused without the decoder ever consuming the whole compressed frame —
        // that under-consumption is the deterministic proof the decode is bounded, not just its
        // output.
        let frame = zstd_frame_of_zeros(4 * MAX_OBJECT_BYTES as u64);
        let mut throttled = OneByteAtATime { remaining: &frame, consumed: 0 };

        let result = decode_object_bounded(&mut throttled);

        assert!(matches!(result, Err(BoundedDecodeError::OverCeiling)),
            "a bomb frame must be refused as over-ceiling");
        assert!(throttled.consumed < frame.len(),
            "the decoder must stop pulling input before the frame is fully consumed \
            (consumed {} of {} bytes)", throttled.consumed, frame.len());
    }

    #[test]
    fn decode_object_bounded_accepts_exactly_the_ceiling() {
        // INV-2: the ceiling itself is legal — this is not an off-by-one refusal.
        let frame = zstd_frame_of_zeros(MAX_OBJECT_BYTES as u64);

        let bytes = decode_object_bounded(frame.as_slice()).expect("exactly-at-ceiling is Ok");

        assert_eq!(bytes.len(), MAX_OBJECT_BYTES);
    }

    #[test]
    fn decode_object_bounded_refuses_one_byte_over_the_ceiling() {
        // INV-3 (the `+1` clause): one byte over must be refused outright, never silently
        // truncated down to a plausible-looking Ok at the ceiling.
        let frame = zstd_frame_of_zeros(MAX_OBJECT_BYTES as u64 + 1);

        let result = decode_object_bounded(frame.as_slice());

        assert!(matches!(result, Err(BoundedDecodeError::OverCeiling)),
            "a frame one byte over the ceiling must be refused, not truncated to Ok");
    }

    #[test]
    fn zstd_decode_all_appears_in_production_code_only_at_the_two_exempted_trusted_sites() {
        // INV-6 (class guard, structural): every decode of bytes this store did not itself
        // write must go through the shared bounded helper (`decode_object_bounded`); a bare
        // `zstd::stream::decode_all` call is exempt at exactly two production sites, both
        // reading back bytes this store (or a verified transport ingress) already wrote —
        // `file_utils.rs`'s loose-object read and `pack_utils.rs`'s `reconstruct_record`. This
        // pins that no third site, anywhere in the crate, now or later, reintroduces an
        // unbounded decode of untrusted bytes.
        //
        // Scoped to production code with this crate's own uniform convention rather than a
        // real parser: every file puts its whole `#[cfg(test)] mod tests { ... }` block,
        // unindented, as the very last item in the file — checked per file below, so a file
        // that ever stopped following that convention fails this test loudly instead of
        // silently mis-scanning.
        let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

        let mut found: Vec<(String, usize)> = Vec::new();
        let mut stack = vec![src_root.clone()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|e| e.to_str()) != Some("rs") {
                    continue;
                }

                let source = std::fs::read_to_string(&path).unwrap();
                let lines: Vec<&str> = source.lines().collect();

                let mut test_boundary_line: Option<usize> = None;
                for i in 0..lines.len() {
                    if lines[i] == "#[cfg(test)]"
                        && lines.get(i + 1).map(|s| s.starts_with("mod tests")).unwrap_or(false) {
                        test_boundary_line = Some(i + 1); // 1-based line of the attribute itself
                        break;
                    }
                }

                if let Some(boundary) = test_boundary_line {
                    assert_eq!(*lines.last().unwrap(), "}",
                        "{}: the test module is assumed to be the last item in the file for \
                        this scan to be sound, but the file's last line is not a bare `}}` \
                        (test module starts at line {})", path.display(), boundary);
                }

                let relative = path.strip_prefix(&src_root).unwrap().to_string_lossy().into_owned();
                for (i, line) in lines.iter().enumerate() {
                    let line_no = i + 1;
                    if test_boundary_line.is_some_and(|b| line_no >= b) {
                        continue;
                    }
                    if line.contains("zstd::stream::decode_all(") {
                        found.push((relative.clone(), line_no));
                    }
                }
            }
        }

        found.sort();
        // Asserted by *file*, not by exact line number: pinning a line number would make this
        // test fail on an unrelated edit that merely shifts lines, not on the invariant it
        // actually guards (no third site, and no second untracked call in either exempted
        // file). `found` (with its line numbers) still prints in full on failure for
        // debugging.
        let found_files: Vec<String> = found.iter().map(|(file, _)| file.clone()).collect();
        let expected_files = vec!["util/file_utils.rs".to_string(), "util/pack_utils.rs".to_string()];
        assert_eq!(found_files, expected_files,
            "zstd::stream::decode_all( must appear in production code only at the two exempted \
            trusted sites (file_utils.rs's loose read, pack_utils.rs's reconstruct_record), \
            each exactly once; a new site must go through decode_object_bounded instead unless \
            it is reading back bytes this store itself wrote. Full findings: {:?}", found);
    }
}
