//! graph_utils — the commit-graph: a sharded, content-addressed, self-healing cache of the
//! parcel DAG (design note §B).
//!
//! # Why it exists
//!
//! Ancestry queries — `merge_utils::is_ancestor` (every `consolidate`, every `haul`
//! divergence check) and `merge_utils::find_merge_base` — walk the parcel graph back toward
//! the roots, decoding a parcel object at every step. On a large warehouse that is an
//! O(history) walk each time. This module denormalizes, per parcel, the two things a walk
//! needs — its **parents** and a **generation number** — so ancestry can short-circuit:
//! generation(child) > generation(parent) always, so a walk can stop descending a branch the
//! moment it drops below the generation of the parcel it is looking for.
//!
//! # Why it is safe to be a cache
//!
//! A record is keyed by the parcel's (immutable) hash, so it can never be *stale* — only
//! *missing*. Reads populate it: ask for a parcel's node, and if the record is absent the
//! parcel object is decoded, its generation computed, and the record written into its shard.
//! There are no write hooks scattered across `stack`/`consolidate`/`import`; the graph is
//! derived and repairs itself. A corrupt or unreadable shard is treated as empty and rebuilt
//! — the graph is an accelerator, never a source of truth, so it can always fall back to the
//! parcel objects. Generation numbers are derived purely from the DAG shape, so a rebuilt
//! record is byte-identical to the one it replaces.
//!
//! # Why it is sharded (not one file)
//!
//! Forklift's invariant is bounded RAM: the per-directory inventory shards so a `stocktake`
//! loads one subtree's worth of entries, never the whole tree's. A single graph file slurped
//! into a `Vec` would break that promise the same way a single inventory index would. So the
//! graph is sharded by parcel-hash prefix, exactly like the object store's fan-out
//! (`file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS`): each shard file holds the records for
//! parcels whose hash starts with that prefix, and a walk holds only a bounded, LRU-capped set
//! of shards resident. (Even git's single-file commit-graph stays bounded only because it is
//! mmap'd; sharding gets the same bounded-RAM property while matching Forklift's storage model.)
//!
//! # Changed-path filters
//!
//! Each record carries an optional changed-path filter (§B, Graph 2) — a Bloom filter of the
//! paths that changed at that parcel — used by `blame`/`log -- <path>` to skip parcels that
//! did not touch the queried path. The substrate stores the field (absent by default); the
//! filter machinery lives in [`changed_paths`]. A false "maybe" only costs a redundant real
//! check, never a wrong answer.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Mutex, OnceLock};

use crate::util::{file_utils, object_utils, sign_utils};

/// Magic bytes at the head of every shard file.
const GRAPH_MAGIC: &[u8; 8] = b"FORKGRPH";

/// The shard format version. Bumped only on an incompatible on-disk change; an unrecognised
/// version makes the shard read as empty (and is rebuilt), never an error.
const GRAPH_FORMAT_VERSION: u32 = 1;

/// Number of leading hex characters of a parcel hash that name its shard — the same 2-char
/// fan-out the object store uses, so the graph shards exactly as the objects do.
const SHARD_PREFIX_LEN: usize = file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS;

/// Raw (decoded) length of a parcel hash in bytes.
const HASH_LEN: usize = 32;

/// `filter_len` sentinel meaning "no changed-path filter has been computed for this parcel"
/// — a query must fall back to the real tree check. Distinct from a filter of length zero,
/// which is a *computed* filter for a parcel that changed nothing.
const FILTER_ABSENT: u32 = u32::MAX;

/// `filter_len` sentinel meaning "the filter was computed but the parcel changed too many
/// paths to be worth one" — like [`FILTER_ABSENT`], a query treats it as "maybe changed".
const FILTER_TOO_LARGE: u32 = u32::MAX - 1;

/// Above this many changed paths, no changed-path filter is stored for a parcel (a root commit
/// or a sweeping refactor); the parcel reads as "maybe changed" for every path. Matches git's
/// commit-graph default, and keeps both the filter and the diff that builds it bounded.
const CHANGED_PATH_MAX: usize = 512;

/// Bits allocated per changed path in a changed-path filter, and the number of hash probes —
/// git's commit-graph defaults, tuned for a ~1-2% false-positive rate.
const CHANGED_PATH_BITS_PER_ELEMENT: usize = 10;
const CHANGED_PATH_PROBES: u64 = 7;

/// Upper bound on how many shards are held resident at once. With a 2-hex fan-out there are at
/// most 256 shards, so holding the whole fan-out means a full-history walk (a `blame`, which
/// visits every parcel and so touches every shard) loads each shard once instead of thrashing a
/// smaller cache — the regression that a 128-shard cap caused (each of ~81k parcels re-parsing a
/// shard). Per-shard size grows with history, so at very large scale the lever is a deeper
/// fan-out (more, smaller shards), not a smaller resident count.
const MAX_RESIDENT_SHARDS: usize = 256;

/// One parcel's denormalized graph node: everything an ancestry walk needs without decoding
/// the parcel object.
#[derive(Clone)]
pub struct Node {
    /// The parcel's generation number: 1 for a root (no parents), else `1 + max(parent
    /// generations)`. Monotonic along every parent edge, so it bounds ancestry walks.
    pub generation: u32,

    /// The parcel's parent hashes (hex), in the parcel's own order (first parent first).
    pub parents: Vec<String>,
}

/// A parcel's changed-path filter (Graph 2): a Bloom filter of the paths that changed at the
/// parcel relative to its first parent, used to skip parcels that did not touch a queried path.
#[derive(Clone, PartialEq, Debug)]
enum PathFilter {
    /// Not computed yet — a query must fall back to the real tree check.
    Unknown,
    /// Computed, but the parcel changed too many paths to store a filter — same fallback.
    TooLarge,
    /// A Bloom filter over the changed paths (its bit count is `bytes.len() * 8`).
    Bloom(Vec<u8>),
}

/// The record stored per parcel: its node plus its changed-path filter.
#[derive(Clone)]
struct Record {
    generation: u32,
    parents: Vec<String>,
    filter: PathFilter,
}

/// A resident shard: the records for one hash prefix, and whether it has unpersisted changes.
struct Shard {
    /// The graph root this shard belongs to, so it is flushed back to the right warehouse even
    /// when a single process (a server) serves several.
    root: String,
    /// The 2-hex prefix this shard holds.
    prefix: String,
    /// hash (hex) -> record.
    records: HashMap<String, Record>,
    /// Whether `records` has changes not yet written to disk.
    dirty: bool,
    /// The clock value at this shard's most recent access — the recency key eviction uses. An
    /// O(1) stamp, so a hot walk that hits a shard tens of thousands of times pays nothing per
    /// hit (a `blame` or a `history` frontier lookup would otherwise scan a recency list).
    last_access: u64,
    /// Bumped on every mutation to `records`. A flush serializes a snapshot under the lock, then
    /// writes it with the lock released; on re-lock it clears `dirty` only if `version` is
    /// unchanged, so a record another thread inserted meanwhile (raising `version`) is never
    /// falsely marked flushed — it stays dirty until its own writer persists it.
    version: u64,
}

/// The process-global resident shard cache, keyed by `root\0prefix` so warehouses never mix.
struct Cache {
    shards: HashMap<String, Shard>,
    /// A monotonic access clock; each shard access stamps its `last_access` from it, so eviction
    /// can pick the least-recently-used shard without maintaining an ordered list per access.
    access_clock: u64,
}

impl Cache {
    /// The next access-clock tick.
    fn tick(&mut self) -> u64 {
        self.access_clock = self.access_clock.wrapping_add(1);
        self.access_clock
    }
}

static GRAPH_CACHE: OnceLock<Mutex<Cache>> = OnceLock::new();

fn cache() -> &'static Mutex<Cache> {
    GRAPH_CACHE.get_or_init(|| Mutex::new(Cache { shards: HashMap::new(), access_clock: 0 }))
}

/// The shard prefix for a parcel hash (its first [`SHARD_PREFIX_LEN`] hex characters).
fn shard_prefix(hash: &str) -> String {
    hash[..SHARD_PREFIX_LEN.min(hash.len())].to_string()
}

/// The on-disk path of the shard file for a given prefix, under the current graph root.
fn shard_path(root: &str, prefix: &str) -> std::path::PathBuf {
    std::path::Path::new(root).join(prefix)
}

/// The composite cache key isolating a `(root, prefix)` shard from every other warehouse's.
fn cache_key(root: &str, prefix: &str) -> String {
    format!("{}\u{0}{}", root, prefix)
}

// ---- shard (de)serialization -------------------------------------------------------------

/// Serialize a shard's records into its on-disk byte form (see the module docs for the layout).
fn serialize_shard(records: &HashMap<String, Record>) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(GRAPH_MAGIC);
    out.extend_from_slice(&GRAPH_FORMAT_VERSION.to_le_bytes());

    for (hash, record) in records {
        // A malformed hash cannot be represented on disk; skip it rather than poison the shard
        // (it will simply be recomputed and stored correctly on the next access).
        let hash_bytes = match sign_utils::from_hex(hash) {
            Ok(bytes) if bytes.len() == HASH_LEN => bytes,
            _ => continue,
        };
        let parent_bytes: Vec<Vec<u8>> = record.parents.iter()
            .filter_map(|p| sign_utils::from_hex(p).ok().filter(|b| b.len() == HASH_LEN))
            .collect();
        if parent_bytes.len() != record.parents.len() || parent_bytes.len() > u8::MAX as usize {
            continue;
        }

        out.extend_from_slice(&hash_bytes);
        out.extend_from_slice(&record.generation.to_le_bytes());
        out.push(parent_bytes.len() as u8);
        for parent in &parent_bytes {
            out.extend_from_slice(parent);
        }
        match &record.filter {
            PathFilter::Unknown => out.extend_from_slice(&FILTER_ABSENT.to_le_bytes()),
            PathFilter::TooLarge => out.extend_from_slice(&FILTER_TOO_LARGE.to_le_bytes()),
            PathFilter::Bloom(bytes) => {
                // A Bloom filter can never be as long as the sentinels (they are near u32::MAX);
                // guard anyway so a pathological length can never be misread as a sentinel.
                let len = (bytes.len() as u32).min(FILTER_TOO_LARGE - 1);
                out.extend_from_slice(&len.to_le_bytes());
                out.extend_from_slice(&bytes[..len as usize]);
            }
        }
    }

    out
}

/// Parse a shard's on-disk bytes back into its records. A truncated, foreign, or
/// wrong-version file yields an empty map — the graph is derived, so an unreadable shard is
/// rebuilt, never fatal.
fn parse_shard(bytes: &[u8]) -> HashMap<String, Record> {
    let mut records = HashMap::new();
    if bytes.len() < 12 || &bytes[..8] != GRAPH_MAGIC {
        return records;
    }
    let version = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
    if version != GRAPH_FORMAT_VERSION {
        return records;
    }

    let mut offset = 12;
    while offset < bytes.len() {
        // hash(32) + generation(4) + parent_count(1)
        if offset + HASH_LEN + 4 + 1 > bytes.len() {
            break;
        }
        let hash = sign_utils::to_hex(&bytes[offset..offset + HASH_LEN]);
        offset += HASH_LEN;
        let generation = u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]);
        offset += 4;
        let parent_count = bytes[offset] as usize;
        offset += 1;

        if offset + parent_count * HASH_LEN + 4 > bytes.len() {
            break;
        }
        let mut parents = Vec::with_capacity(parent_count);
        for _ in 0..parent_count {
            parents.push(sign_utils::to_hex(&bytes[offset..offset + HASH_LEN]));
            offset += HASH_LEN;
        }
        let filter_len = u32::from_le_bytes([bytes[offset], bytes[offset + 1], bytes[offset + 2], bytes[offset + 3]]);
        offset += 4;
        let filter = if filter_len == FILTER_ABSENT {
            PathFilter::Unknown
        } else if filter_len == FILTER_TOO_LARGE {
            PathFilter::TooLarge
        } else {
            let len = filter_len as usize;
            if offset + len > bytes.len() {
                break;
            }
            let bytes = bytes[offset..offset + len].to_vec();
            offset += len;
            PathFilter::Bloom(bytes)
        };

        records.insert(hash, Record { generation, parents, filter });
    }

    records
}

// ---- resident shard access (disk I/O happens outside the cache lock) ---------------------

/// Run `f` with the shard for `prefix` resident, holding the cache lock **only** for the
/// in-memory work. `f` must not perform parcel or shard I/O.
///
/// A cold shard is read and parsed from disk with the lock **released** — the disk read (and a
/// dirty eviction victim's write) never block another thread's graph access. The sequence is:
/// take the lock and serve the shard if it is resident (the common case, one lock, no I/O);
/// otherwise release the lock, read+parse the shard file, then re-take the lock to insert it.
/// Two threads racing the same cold shard is benign — the records are derived from immutable
/// parcels, so both parses are equal; the first insert wins and the loser's parse is dropped.
/// The resident copy never loses a record while it stays resident; if it is later evicted and
/// reloaded from a stale on-disk copy, the reload can transiently omit a racing writer's record —
/// the same self-heal (object-walk recompute) that covers any other missing record covers this.
fn with_shard<R>(root: &str, prefix: &str, f: impl FnOnce(&mut Shard) -> R) -> Result<R, String> {
    let key = cache_key(root, prefix);

    // Fast path: the shard is already resident. One lock acquisition, no I/O.
    {
        let mut cache = cache().lock().map_err(poisoned)?;
        if cache.shards.contains_key(&key) {
            let stamp = cache.tick();
            let shard = cache.shards.get_mut(&key).expect("just checked present");
            shard.last_access = stamp;
            return Ok(f(shard));
        }
    }

    // Miss: read and parse the shard file with the lock released.
    let records = match std::fs::read(shard_path(root, prefix)) {
        Ok(bytes) => parse_shard(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
        Err(e) => return Err(format!("Error while reading commit-graph shard \"{}\": {}", prefix, e)),
    };

    // Re-take the lock to install the shard and run `f`. Any dirty eviction victims are removed
    // here but flushed to disk *after* the lock is released, below.
    let (result, victim_writes) = {
        let mut cache = cache().lock().map_err(poisoned)?;

        // Another thread may have loaded the same shard while we read it; if so, keep the
        // resident copy (it may already carry newer records) and drop our parse.
        let victim_writes = if cache.shards.contains_key(&key) {
            Vec::new()
        } else {
            let victim_writes = evict_collect(&mut cache, &key);
            cache.shards.insert(key.clone(), Shard {
                root: root.to_string(),
                prefix: prefix.to_string(),
                records,
                dirty: false,
                last_access: 0,
                version: 0,
            });
            victim_writes
        };

        let stamp = cache.tick();
        let shard = cache.shards.get_mut(&key).expect("resident or just inserted");
        shard.last_access = stamp;
        (f(shard), victim_writes)
    };

    // Flush the evicted dirty victims with the lock released. They are already out of the cache,
    // so their bytes are final; a write error is surfaced (a saved write is a saved rebuild).
    for (root, prefix, bytes) in victim_writes {
        write_shard_bytes(&root, &prefix, &bytes)?;
    }

    Ok(result)
}

/// The poisoned-lock error message (a panic while holding the graph lock is unexpected, but the
/// cache is a derived accelerator, so surfacing it as an error beats propagating a panic).
fn poisoned<T>(_: T) -> String {
    "The commit-graph cache lock was poisoned.".to_string()
}

/// Evict least-recently-used shards until there is room for one more, returning the serialized
/// bytes of any **dirty** victim so the caller can write it to disk *after releasing the lock*
/// (flushing under the lock would block every other graph access on an fsync). A victim's
/// records would only be recomputed if dropped, but a write saved is a walk saved. With a 2-hex
/// fan-out all ≤256 shards of one warehouse fit under the cap, so this only evicts when a single
/// process (a server) holds shards for many warehouses at once.
fn evict_collect(cache: &mut Cache, incoming: &str) -> Vec<(String, String, Vec<u8>)> {
    let mut pending_writes = Vec::new();
    while cache.shards.len() >= MAX_RESIDENT_SHARDS {
        // The least-recently-accessed shard that is not the one we are about to insert.
        let victim = cache.shards.iter()
            .filter(|(key, _)| key.as_str() != incoming)
            .min_by_key(|(_, shard)| shard.last_access)
            .map(|(key, _)| key.clone());
        let victim = match victim {
            Some(v) => v,
            None => break,
        };
        if let Some(shard) = cache.shards.remove(&victim) {
            if shard.dirty {
                // Serialize under the lock (in-memory, cheap); the fsyncing write is done by the
                // caller once the lock is released.
                pending_writes.push((
                    shard.root.clone(),
                    shard.prefix.clone(),
                    serialize_shard(&shard.records),
                ));
            }
        }
    }
    pending_writes
}

/// Write pre-serialized shard bytes to the shard file atomically. Callers serialize under the
/// cache lock and call this with the lock released, so the fsync never blocks the cache.
fn write_shard_bytes(root: &str, prefix: &str, bytes: &[u8]) -> Result<(), String> {
    file_utils::create_folder_if_not_exists(std::path::Path::new(root))?;
    file_utils::write_file_atomically(&shard_path(root, prefix), bytes)
}

/// Fetch a parcel's record if it is already stored (in a resident shard or on disk). Does not
/// compute anything — a miss returns `None`.
fn stored_record(root: &str, hash: &str) -> Result<Option<Record>, String> {
    let prefix = shard_prefix(hash);
    with_shard(root, &prefix, |shard| shard.records.get(hash).cloned())
}

/// Persist a batch of freshly computed records, grouped so each affected shard is rewritten
/// exactly once (never once per record). Updates the resident copy of any loaded shard too.
fn persist_records(root: &str, new: &HashMap<String, Record>) -> Result<(), String> {
    if new.is_empty() {
        return Ok(());
    }
    let mut by_prefix: HashMap<String, Vec<(&String, &Record)>> = HashMap::new();
    for (hash, record) in new {
        by_prefix.entry(shard_prefix(hash)).or_default().push((hash, record));
    }
    for (prefix, records) in by_prefix {
        // Insert the records and snapshot the shard's bytes + version under the lock; the
        // fsyncing write then happens with the lock released.
        let (bytes, version) = with_shard(root, &prefix, |shard| {
            for (hash, record) in records {
                shard.records.insert(hash.clone(), record.clone());
            }
            shard.dirty = true;
            shard.version = shard.version.wrapping_add(1);
            (serialize_shard(&shard.records), shard.version)
        })?;

        // Flush eagerly so the memoization survives a crash (otherwise only a rebuild, but
        // avoidable) — off the lock, so a concurrent graph access never waits on this fsync.
        write_shard_bytes(root, &prefix, &bytes)?;

        // Mark clean only if nothing changed the shard since the snapshot: a record another
        // thread inserted meanwhile (a higher `version`) is newer than what we just wrote and
        // stays dirty until that writer flushes it. (If the shard was evicted and reloaded in the
        // gap, its `version` reset to 0, so this simply no-ops — its records are already on disk.)
        with_shard(root, &prefix, |shard| {
            if shard.version == version {
                shard.dirty = false;
            }
        })?;
    }
    Ok(())
}

// ---- generation computation (iterative — survives 85k-deep chains) ------------------------

/// Ensure `hash` and every ancestor lacking a record get one, computing generation numbers by
/// an **iterative** post-order DAG walk (no recursion — the parent chain can be tens of
/// thousands deep). Returns the freshly computed records, already persisted.
fn ensure(root: &str, hash: &str) -> Result<HashMap<String, Record>, String> {
    let mut computed: HashMap<String, Record> = HashMap::new();
    // DFS state per node: `gray` = on the stack (being expanded); a node is "known" once it is
    // in `computed` or already stored. A gray parent encountered while expanding is a cycle.
    let mut gray: HashSet<String> = HashSet::new();
    let mut stack: Vec<String> = vec![hash.to_string()];

    // The generation of a parent, if known (computed this run or already stored). `None` means
    // it still needs to be computed (and pushed).
    let known_generation = |computed: &HashMap<String, Record>, h: &str| -> Result<Option<u32>, String> {
        if let Some(record) = computed.get(h) {
            return Ok(Some(record.generation));
        }
        Ok(stored_record(root, h)?.map(|record| record.generation))
    };

    while let Some(current) = stack.last().cloned() {
        if computed.contains_key(&current) || stored_record(root, &current)?.is_some() {
            stack.pop();
            gray.remove(&current);
            continue;
        }

        let parcel = object_utils::load_parcel(&current)?;

        if !gray.contains(&current) {
            // First visit: expand. Push any parent whose generation is not yet known.
            gray.insert(current.clone());
            for parent in &parcel.parents {
                if known_generation(&computed, parent)?.is_none() {
                    if gray.contains(parent) {
                        return Err(format!(
                            "The parcel graph has a cycle (parcel \"{}\" is its own ancestor); the warehouse is corrupt.",
                            parent
                        ));
                    }
                    stack.push(parent.clone());
                }
            }
            // Leave `current` on the stack; it is revisited after its parents resolve.
        } else {
            // Second visit: every parent now has a known generation.
            let mut max_parent = 0;
            for parent in &parcel.parents {
                let generation = known_generation(&computed, parent)?.ok_or_else(|| format!(
                    "Internal error computing the commit-graph: parent \"{}\" of \"{}\" was not resolved.",
                    parent, current
                ))?;
                max_parent = max_parent.max(generation);
            }
            computed.insert(current.clone(), Record {
                generation: max_parent + 1,
                parents: parcel.parents,
                // The changed-path filter is filled lazily (by a `blame`) or in bulk (by
                // `build_from_heads`); a generation-only self-heal leaves it to be computed later.
                filter: PathFilter::Unknown,
            });
            gray.remove(&current);
            stack.pop();
        }
    }

    persist_records(root, &computed)?;
    Ok(computed)
}

// ---- public API ---------------------------------------------------------------------------

/// The graph node for a parcel — its parents and generation number — computing and caching it
/// (and any missing ancestors) on a miss. This is the primitive ancestry walks read instead
/// of decoding parcel objects.
///
/// # Arguments
/// * `hash` - The parcel hash.
///
/// # Returns
/// * `Ok(Node)`    - The parcel's parents and generation number.
/// * `Err(String)` - If the parcel object could not be read, or the graph is corrupt (cycle).
pub fn node(hash: &str) -> Result<Node, String> {
    let root = file_utils::get_path_graph_root();
    if let Some(record) = stored_record(&root, hash)? {
        return Ok(Node { generation: record.generation, parents: record.parents });
    }
    let computed = ensure(&root, hash)?;
    let record = computed.get(hash)
        .cloned()
        .map(Ok)
        .unwrap_or_else(|| stored_record(&root, hash)?.ok_or_else(|| format!(
            "Internal error: the commit-graph record for \"{}\" was not produced.", hash
        )))?;
    Ok(Node { generation: record.generation, parents: record.parents })
}

/// A parcel's generation number (see [`node`]).
pub fn generation(hash: &str) -> Result<u32, String> {
    Ok(node(hash)?.generation)
}

/// A parcel's parent hashes, read from the graph when its record is present, else from the
/// parcel object. Unlike [`node`], this does **not** compute (or self-heal) a generation
/// number — it needs only the parent edges — so following a first-parent chain for `blame`
/// costs a resident-index lookup per parcel instead of decoding every ancestor. Falls back to
/// decoding a parcel whose record is not yet in the graph, so it is always correct, warm or cold.
///
/// # Arguments
/// * `hash` - The parcel hash.
///
/// # Returns
/// * `Ok(Vec<String>)` - The parcel's parent hashes (first parent first).
/// * `Err(String)`     - If the parcel is in neither the graph nor the object store.
pub fn parents(hash: &str) -> Result<Vec<String>, String> {
    let root = file_utils::get_path_graph_root();
    if let Some(record) = stored_record(&root, hash)? {
        return Ok(record.parents);
    }
    Ok(object_utils::load_parcel(hash)?.parents)
}

/// Populate the graph for everything reachable from `heads`, computing only the records that
/// are still missing (an already-recorded parcel prunes the walk, since its ancestors were
/// recorded with it). Cheap to call repeatedly; meant to run inside `compact`, which already
/// holds the warehouse lock and has warm object caches, so a warehouse is graph-ready right
/// after an import or a repack. Returns the number of new records written.
///
/// # Arguments
/// * `heads` - Parcel hashes to populate the ancestry of (all pallet heads, typically).
pub fn build_from_heads(heads: &[String]) -> Result<usize, String> {
    let root = file_utils::get_path_graph_root();

    // Collect the parcels still missing a record, and their parents, in one BFS. Pruning at an
    // already-recorded parcel assumes its ancestors are recorded too; if a partial write ever
    // broke that, `node`'s self-heal fills the gap on demand — this is only an optimization.
    let mut parents_of: HashMap<String, Vec<String>> = HashMap::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = heads.iter().cloned().collect();

    while let Some(hash) = queue.pop_front() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        if stored_record(&root, &hash)?.is_some() {
            continue;
        }
        let parcel = object_utils::load_parcel(&hash)?;
        for parent in &parcel.parents {
            queue.push_back(parent.clone());
        }
        parents_of.insert(hash, parcel.parents);
    }

    if parents_of.is_empty() {
        return Ok(0);
    }

    // Topologically order the missing parcels (parents before children) so a single forward
    // pass can assign generations. A parent's generation is either already stored or computed
    // earlier in this pass.
    let order = topo_order(&parents_of)?;

    let mut computed: HashMap<String, Record> = HashMap::new();
    for hash in &order {
        let parents = &parents_of[hash];
        let mut max_parent = 0;
        for parent in parents {
            let generation = match computed.get(parent) {
                Some(record) => record.generation,
                None => stored_record(&root, parent)?.map(|r| r.generation).unwrap_or(0),
            };
            max_parent = max_parent.max(generation);
        }
        // Compute the changed-path filter now, while the object caches are warm (a tree diff
        // against the first parent). Best-effort: if a tree is not present the filter stays
        // Unknown and is filled on the first query — never a reason to fail the build.
        let filter = compute_filter(hash, parents.first().map(String::as_str))
            .unwrap_or(PathFilter::Unknown);
        computed.insert(hash.clone(), Record {
            generation: max_parent + 1,
            parents: parents.clone(),
            filter,
        });
    }

    let count = computed.len();
    persist_records(&root, &computed)?;
    Ok(count)
}

/// Kahn's-algorithm topological order (oldest first) over a set of parcels given by their
/// parent lists. Parents outside the set (already recorded) are treated as roots for ordering
/// — their edges do not gate anything in the set. Falls back to the input order on a cycle,
/// which the caller's generation pass then still resolves defensively.
fn topo_order(parents_of: &HashMap<String, Vec<String>>) -> Result<Vec<String>, String> {
    // Children waiting on each in-set parent, and each node's count of in-set parents.
    let mut children: HashMap<&str, Vec<&str>> = HashMap::new();
    let mut indegree: HashMap<&str, usize> = parents_of.keys().map(|h| (h.as_str(), 0)).collect();

    for (hash, parents) in parents_of {
        for parent in parents {
            if parents_of.contains_key(parent) {
                *indegree.get_mut(hash.as_str()).unwrap() += 1;
                children.entry(parent.as_str()).or_default().push(hash.as_str());
            }
        }
    }

    let mut ready: VecDeque<&str> = indegree.iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(hash, _)| *hash)
        .collect();
    let mut order: Vec<String> = Vec::with_capacity(parents_of.len());

    while let Some(hash) = ready.pop_front() {
        order.push(hash.to_string());
        if let Some(kids) = children.get(hash) {
            for kid in kids {
                let degree = indegree.get_mut(kid).unwrap();
                *degree -= 1;
                if *degree == 0 {
                    ready.push_back(kid);
                }
            }
        }
    }

    if order.len() != parents_of.len() {
        // A cycle (corrupt warehouse): fall back to an arbitrary order. The generation pass
        // reads only already-stored parents, so it still terminates and produces usable numbers.
        return Ok(parents_of.keys().cloned().collect());
    }

    Ok(order)
}

// ---- changed-path filters (Graph 2) -------------------------------------------------------

/// Whether the file at `path` may have changed at `parcel_hash` relative to its first parent.
///
/// This is the primitive `blame`/`log -- <path>` use to skip parcels that did not touch the
/// path. A changed-path Bloom filter has no false negatives, so a `false` here is definitive —
/// the path did not change at this parcel, so the caller carries the previous version's
/// attribution forward without reading the tree at all. A `true` is only "maybe": the caller
/// does the real tree check, which a false positive makes redundant, never wrong. If the filter
/// has not been computed yet it is computed now and stored, so the first query pays and every
/// later one (for any file) is free.
///
/// Computing the filter diffs the parcel's tree against its parent's, which in a sparse store can
/// descend toward a sealed out-of-scope subtree the store never fetched. An object that is absent
/// (or otherwise unreadable) during that computation is absorbed into the honest "maybe changed"
/// answer here — the function is safe by construction for every caller, not only for `blame`,
/// which is why callers do not need to wrap it defensively.
///
/// # Arguments
/// * `parcel_hash` - The parcel to test.
/// * `path`        - The warehouse path of the file.
///
/// # Returns
/// * `Ok(bool)`    - `true` if the path may have changed (or the filter is uncomputable), else `false`.
/// * `Err(String)` - If the parcel record could not be resolved or the computed filter not stored.
pub fn path_maybe_changed(parcel_hash: &str, path: &str) -> Result<bool, String> {
    let root = file_utils::get_path_graph_root();
    let prefix = shard_prefix(parcel_hash);

    // Hot path: if the filter is already computed, decide inside the shard lock without cloning
    // the record. This is the per-parcel cost of a `blame` over a long history, so it must be
    // just a map lookup plus a Bloom probe. `Some(answer)` = decided; `None` = the filter is not
    // computed (or there is no record yet), handled off the hot path below.
    let decided = with_shard(&root, &prefix, |shard| {
        shard.records.get(parcel_hash).and_then(|record| match &record.filter {
            PathFilter::Bloom(bytes) => Some(bloom_contains(bytes, path)),
            PathFilter::TooLarge => Some(true),
            PathFilter::Unknown => None,
        })
    })?;
    if let Some(answer) = decided {
        return Ok(answer);
    }

    // Cold path: no record, or its filter has not been computed. Ensure the record exists
    // (parents + generation), compute the filter now, store it so later queries are free.
    let record = match stored_record(&root, parcel_hash)? {
        Some(record) => record,
        None => {
            node(parcel_hash)?; // self-heal the generation record
            stored_record(&root, parcel_hash)?.ok_or_else(|| format!(
                "Internal error: the commit-graph record for \"{}\" was not produced.", parcel_hash))?
        }
    };

    // Absorb an absent/unreadable object during the diff into `Unknown` — exactly as
    // `build_from_heads` does for the bulk path (`.unwrap_or(PathFilter::Unknown)`) — so a sparse
    // store's sealed out-of-scope subtree degrades this cold path to the honest "maybe" answer
    // rather than propagating the read error out of a query the caller expects to be total.
    let filter = compute_filter(parcel_hash, record.parents.first().map(String::as_str))
        .unwrap_or(PathFilter::Unknown);
    let updated = Record {
        generation: record.generation,
        parents: record.parents,
        filter: filter.clone(),
    };
    persist_records(&root, &HashMap::from([(parcel_hash.to_string(), updated)]))?;

    Ok(match filter {
        PathFilter::Bloom(bytes) => bloom_contains(&bytes, path),
        _ => true, // TooLarge (a root or a sweeping change) or Unknown (uncomputable) → maybe
    })
}

/// Compute a parcel's changed-path filter: the file paths that differ between the parcel's tree
/// and its first parent's (every path, when it is a root). Past [`CHANGED_PATH_MAX`] changed
/// paths a filter is not worth keeping — the parcel then reads as "maybe" for every path.
fn compute_filter(parcel_hash: &str, first_parent: Option<&str>) -> Result<PathFilter, String> {
    let new_tree = object_utils::load_parcel(parcel_hash)?.tree_hash;
    let old_tree = match first_parent {
        Some(parent) => Some(object_utils::load_parcel(parent)?.tree_hash),
        None => None,
    };

    let mut changed: HashSet<String> = HashSet::new();
    match diff_trees(old_tree.as_deref(), Some(&new_tree), "", &mut changed) {
        Ok(()) => Ok(PathFilter::Bloom(build_bloom(&changed))),
        Err(DiffLimit::TooManyPaths) => Ok(PathFilter::TooLarge),
        Err(DiffLimit::Read(e)) => Err(e),
    }
}

/// Reasons a tree diff stops early.
enum DiffLimit {
    /// More than [`CHANGED_PATH_MAX`] paths changed — the filter is abandoned as not worth it.
    TooManyPaths,
    /// An object could not be read.
    Read(String),
}

impl From<String> for DiffLimit {
    fn from(error: String) -> Self {
        DiffLimit::Read(error)
    }
}

/// Collect into `changed` the file paths that differ between two trees, descending only into
/// subtrees whose hashes differ — an identical subtree hash means nothing under it changed, the
/// heart of a cheap tree diff. Bails out with [`DiffLimit::TooManyPaths`] once past the cap.
fn diff_trees(old: Option<&str>, new: Option<&str>, prefix: &str, changed: &mut HashSet<String>)
    -> Result<(), DiffLimit>
{
    if old == new {
        return Ok(()); // identical subtree (or absent on both sides) — nothing changed here
    }
    if changed.len() > CHANGED_PATH_MAX {
        return Err(DiffLimit::TooManyPaths);
    }

    let old_tree = match old {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };
    let new_tree = match new {
        Some(hash) => Some(object_utils::load_tree(hash)?),
        None => None,
    };

    // Files: added, removed, or modified all count as a change at that path.
    let old_files: HashMap<&str, &str> = old_tree.iter()
        .flat_map(|tree| tree.get_files())
        .map(|(name, item)| (name.as_str(), item.hash.as_str())).collect();
    let new_files: HashMap<&str, &str> = new_tree.iter()
        .flat_map(|tree| tree.get_files())
        .map(|(name, item)| (name.as_str(), item.hash.as_str())).collect();

    for (name, hash) in &new_files {
        if old_files.get(name) != Some(hash) {
            insert_changed(changed, join_path(prefix, name))?;
        }
    }
    for name in old_files.keys() {
        if !new_files.contains_key(name) {
            insert_changed(changed, join_path(prefix, name))?;
        }
    }

    // Subtrees: recurse only where the hash differs. A subtree on one side only means every file
    // under it was added or removed — the recursion collects them all (bounded by the cap).
    let old_subtrees: HashMap<&str, &str> = old_tree.iter()
        .flat_map(|tree| tree.get_subtrees())
        .map(|(name, item)| (name.as_str(), item.hash.as_str())).collect();
    let new_subtrees: HashMap<&str, &str> = new_tree.iter()
        .flat_map(|tree| tree.get_subtrees())
        .map(|(name, item)| (name.as_str(), item.hash.as_str())).collect();

    let mut names: HashSet<&str> = HashSet::new();
    names.extend(old_subtrees.keys().copied());
    names.extend(new_subtrees.keys().copied());
    for name in names {
        diff_trees(
            old_subtrees.get(name).copied(),
            new_subtrees.get(name).copied(),
            &join_path(prefix, name),
            changed,
        )?;
    }

    Ok(())
}

/// Add a changed path, enforcing the cap so a huge diff cannot balloon the set unbounded.
fn insert_changed(changed: &mut HashSet<String>, path: String) -> Result<(), DiffLimit> {
    changed.insert(path);
    if changed.len() > CHANGED_PATH_MAX {
        return Err(DiffLimit::TooManyPaths);
    }
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

/// Build a changed-path Bloom filter over a set of paths, its bit count sized to the count.
fn build_bloom(paths: &HashSet<String>) -> Vec<u8> {
    let bits = (paths.len().max(1) * CHANGED_PATH_BITS_PER_ELEMENT).max(8);
    let byte_len = bits.div_ceil(8);
    let nbits = (byte_len * 8) as u64;
    let mut filter = vec![0u8; byte_len];
    for path in paths {
        for probe in bloom_probes(path, nbits) {
            filter[(probe / 8) as usize] |= 1 << (probe % 8);
        }
    }
    filter
}

/// Whether a Bloom filter possibly contains `path` (every probe bit set). No false negatives:
/// a path that was inserted always reads back as present.
fn bloom_contains(filter: &[u8], path: &str) -> bool {
    let nbits = (filter.len() * 8) as u64;
    if nbits == 0 {
        return true; // an empty filter carries no information → "maybe"
    }
    bloom_probes(path, nbits).all(|probe| filter[(probe / 8) as usize] & (1 << (probe % 8)) != 0)
}

/// The bit positions a path probes, by double hashing (two FNV-1a variants). Deterministic and
/// self-contained, so a filter read back off disk probes exactly as it was built.
fn bloom_probes(path: &str, nbits: u64) -> impl Iterator<Item = u64> {
    let h1 = fnv1a(path.as_bytes(), 0xcbf2_9ce4_8422_2325);
    let h2 = fnv1a(path.as_bytes(), 0x8422_2325_cbf2_9ce4) | 1; // odd, so the stride covers all bits
    (0..CHANGED_PATH_PROBES).map(move |i| h1.wrapping_add(i.wrapping_mul(h2)) % nbits)
}

/// FNV-1a 64-bit hash with a seedable offset basis.
fn fnv1a(bytes: &[u8], seed: u64) -> u64 {
    let mut hash = seed;
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shard_round_trips_through_serialization() {
        let hash_a = "aa".repeat(32);
        let hash_b = "bb".repeat(32);
        let hash_c = "cc".repeat(32);
        let mut records = HashMap::new();
        records.insert(hash_a.clone(), Record { generation: 1, parents: vec![], filter: PathFilter::Unknown });
        records.insert(hash_b.clone(), Record {
            generation: 2,
            parents: vec![hash_a.clone()],
            filter: PathFilter::Bloom(vec![1, 2, 3, 4]),
        });
        records.insert(hash_c.clone(), Record {
            generation: 3,
            parents: vec![hash_b.clone()],
            filter: PathFilter::TooLarge,
        });

        let parsed = parse_shard(&serialize_shard(&records));

        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[&hash_a].generation, 1);
        assert!(parsed[&hash_a].parents.is_empty());
        assert_eq!(parsed[&hash_a].filter, PathFilter::Unknown);
        assert_eq!(parsed[&hash_b].generation, 2);
        assert_eq!(parsed[&hash_b].parents, vec![hash_a]);
        assert_eq!(parsed[&hash_b].filter, PathFilter::Bloom(vec![1, 2, 3, 4]));
        assert_eq!(parsed[&hash_c].filter, PathFilter::TooLarge);
    }

    #[test]
    fn a_foreign_or_short_shard_reads_as_empty_not_an_error() {
        assert!(parse_shard(b"").is_empty());
        assert!(parse_shard(b"NOTAGRPH____").is_empty());
        // Right magic, unknown version.
        let mut bytes = GRAPH_MAGIC.to_vec();
        bytes.extend_from_slice(&999u32.to_le_bytes());
        assert!(parse_shard(&bytes).is_empty());
    }

    #[test]
    fn shard_prefix_matches_the_object_fan_out_width() {
        let hash = "9c423b8".to_string() + &"0".repeat(57);
        assert_eq!(shard_prefix(&hash), "9c");
        assert_eq!(shard_prefix(&hash).len(), SHARD_PREFIX_LEN);
    }

    #[test]
    fn topo_order_places_every_parent_before_its_children() {
        // a <- b <- c, plus a <- d (a diamond-ish shape).
        let parents_of: HashMap<String, Vec<String>> = [
            ("a", vec![]),
            ("b", vec!["a"]),
            ("c", vec!["b"]),
            ("d", vec!["a"]),
        ].into_iter().map(|(h, ps)| (h.to_string(), ps.into_iter().map(String::from).collect())).collect();

        let order = topo_order(&parents_of).unwrap();
        let at = |h: &str| order.iter().position(|x| x == h).unwrap();

        assert_eq!(order.len(), 4);
        assert!(at("a") < at("b"), "a before b");
        assert!(at("b") < at("c"), "b before c");
        assert!(at("a") < at("d"), "a before d");
    }

    #[test]
    fn changed_path_bloom_has_no_false_negatives_and_a_low_false_positive_rate() {
        // A realistic spread of changed paths for one commit.
        let paths: HashSet<String> = (0..200)
            .map(|i| format!("src/module{}/file{}.rs", i % 11, i))
            .collect();
        let filter = build_bloom(&paths);

        // Every inserted path MUST read back as present — this is the property blame relies on.
        for path in &paths {
            assert!(bloom_contains(&filter, path), "false negative for {}", path);
        }

        // Paths never inserted should almost always be absent (10 bits/elem, 7 probes ≈ 1%).
        let trials = 3000;
        let false_positives = (0..trials)
            .filter(|i| bloom_contains(&filter, &format!("never/inserted/path-{}.txt", i)))
            .count();
        assert!(false_positives < trials / 20,
                "false-positive rate too high: {}/{}", false_positives, trials);
    }

    #[test]
    fn changed_path_bloom_distinguishes_a_present_from_an_absent_path() {
        let paths: HashSet<String> = ["a/b.txt".to_string(), "c/d/e.rs".to_string()].into_iter().collect();
        let filter = build_bloom(&paths);
        assert!(bloom_contains(&filter, "a/b.txt"));
        assert!(bloom_contains(&filter, "c/d/e.rs"));
        assert!(!bloom_contains(&filter, "a/b.rs"), "a near-miss path is (almost surely) absent");
    }

    #[test]
    fn topo_order_falls_back_without_looping_on_a_cycle() {
        // A corrupt warehouse: x and y are each other's parent. The order must still return
        // (the generation pass then reads only stored parents, so it stays terminating).
        let parents_of: HashMap<String, Vec<String>> = [
            ("x", vec!["y"]),
            ("y", vec!["x"]),
        ].into_iter().map(|(h, ps)| (h.to_string(), ps.into_iter().map(String::from).collect())).collect();

        let order = topo_order(&parents_of).unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn concurrent_persists_to_one_shard_never_lose_a_record() {
        // P2 stress: many threads persist distinct records that all hash into the *same* shard,
        // so they contend on the out-of-lock shard load, the re-lock insert, and the double-load
        // race. The invariant the restructure must keep is that the resident shard (what every
        // read consults) never drops a record *while resident*: the first insert of a shard wins,
        // and a thread that finds the shard already resident *adds* to it rather than replacing
        // it. (The on-disk copy is deliberately last-write-wins; if the shard is later evicted and
        // reloaded from a stale on-disk copy, the reloaded resident copy can transiently lack a
        // racing writer's record — covered by the same self-heal as any other missing record
        // (object-walk recompute), so this checks the authoritative in-memory copy via
        // `stored_record` within one residency, not across an evict/reload.)
        let root = std::env::temp_dir()
            .join(format!("forklift-graph-conc-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let _ = std::fs::remove_dir_all(&root);

        const THREADS: u64 = 8;
        const PER_THREAD: u64 = 64;

        std::thread::scope(|scope| {
            for t in 0..THREADS {
                let root = root.clone();
                scope.spawn(move || {
                    // Every hash shares the prefix "ab", so all records target the one shard —
                    // maximal contention on that shard's load/insert/flush path.
                    let batch: HashMap<String, Record> = (0..PER_THREAD)
                        .map(|i| {
                            let n = t * 100_000 + i;
                            (format!("ab{:062x}", n), Record {
                                generation: (n + 1) as u32,
                                parents: vec![],
                                filter: PathFilter::Unknown,
                            })
                        })
                        .collect();
                    persist_records(&root, &batch).expect("persist under contention");
                });
            }
        });

        // Every record from every thread must read back with exactly its stored generation.
        for t in 0..THREADS {
            for i in 0..PER_THREAD {
                let n = t * 100_000 + i;
                let record = stored_record(&root, &format!("ab{:062x}", n))
                    .expect("read back")
                    .unwrap_or_else(|| panic!("record {n} was lost under concurrent persists"));
                assert_eq!(record.generation, (n + 1) as u32, "record {n} corrupted");
            }
        }

        std::fs::remove_dir_all(&root).ok();
    }
}
