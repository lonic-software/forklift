//! The verification bridge: reuse `forklift_core`'s filesystem-based audit against an
//! [`ObjectStore`](crate::store::ObjectStore) by mirroring the objects the audit must
//! *read* into a throwaway on-disk `.forklift`.
//!
//! Verification never reads a working-directory blob — it only checks that one exists
//! (the single `does_object_exist` call in `verify_parcel_closure`). Everything it *reads*
//! (parcels, trees, office-record blobs, signature sidecars) is small. So the AWS head
//! mirrors exactly those small objects into a scratch warehouse, enters its storage-root
//! scope, and runs the exact same `audit_utils` code the CLI and server head run; the
//! working blobs stay in object storage and their presence is checked with an S3 `HEAD`
//! via `verify_parcel_closure_with` (DESIGN.html §4.6).
//!
//! The scratch is a real filesystem directory precisely so the reuse is total: the same
//! bytes, the same parser, the same signature and privilege checks — no second
//! implementation of the security-critical path to drift out of sync.
//!
//! # What the mirror must contain, and what it may skip
//!
//! A ref update audits `new_head` knowing that `old_head` was audited when it was
//! committed, so the mirror is bounded at `old_head` — but only in the one dimension the
//! audit actually stops walking. Concretely, below the bound the audit still needs:
//!
//! * **parcel bodies** — `verify_parcel_closure_with` builds its prune set with
//!   `collect_reachable(old_head)`, which loads every parcel in `old_head`'s ancestry.
//!
//! What it may skip below the bound is the bulk: **trees, their blobs, and signature
//! sidecars**. Trees are read only for parcels outside the closure check's prune set;
//! sidecars only for parcels `verify_pallet_history` discovers, and that walk never
//! traverses *through* `old_head` — it skips the bound before enqueueing its parents. Both
//! sets are exactly the parcels this mirror still reaches with its `full` flag set, so a
//! merge lift whose new segment forks below `old_head` re-expands that older branch (as it
//! must) while a linear lift touches only its new parcels.
//!
//! That is where the mirror's cost lives — a tree per directory per parcel, a sidecar per
//! parcel — so bounding it is the win.
//!
//! The **office** chain is never bounded: `verify_office_chain` walks it from the head to
//! the genesis on every trusted ref update, reading each record blob.
//!
//! # Warm containers
//!
//! [`Scratch::shared`] keys a process-global scratch by warehouse, so a warm Lambda
//! container mirrors a parcel once and every later invocation finds it on disk. The safety
//! rests on that key: an object present in the scratch was read from *this* warehouse's
//! object store, so `does_object_exist` inside the scope answers a question about this
//! warehouse. Sharing one scratch across warehouses would let a closure check pass on an
//! object the tenant's own store never had.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use forklift_core::globals::StorageRootScope;
use forklift_core::util::{file_utils, object_utils, sign_utils, warehouse_utils};

use crate::store::ObjectStore;

/// Monotonic suffix so concurrent scratches on one host never collide.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The process-global scratches, one per warehouse (see [`Scratch::shared`]).
static POOL: OnceLock<Mutex<HashMap<String, Arc<Scratch>>>> = OnceLock::new();

/// A throwaway on-disk `.forklift` warehouse used to run `forklift_core`'s audit against
/// objects that live in an [`ObjectStore`]. An ephemeral one is removed on drop; a
/// [`shared`](Scratch::shared) one lives as long as the process.
pub struct Scratch {
    dir: PathBuf,
    ephemeral: bool,
}

impl Scratch {
    /// Create and prepare a fresh scratch warehouse, removed on drop.
    pub fn new() -> Result<Scratch, String> {
        let unique = format!(
            "forklift-head-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );

        Scratch::at(std::env::temp_dir().join(unique), true)
    }

    /// The process-global scratch for `warehouse_id`, created on first use and reused by
    /// every later invocation in the same warm container — the mirror is paid once.
    ///
    /// Keyed by warehouse *on purpose*: presence in the scratch is taken as presence in the
    /// object store during a closure check, and that inference is only sound for the
    /// warehouse the objects were read from.
    pub fn shared(warehouse_id: &str) -> Result<Arc<Scratch>, String> {
        let pool = POOL.get_or_init(|| Mutex::new(HashMap::new()));
        let mut pool = pool.lock().map_err(|e| format!("The scratch pool is poisoned: {}", e))?;

        if let Some(scratch) = pool.get(warehouse_id) {
            return Ok(Arc::clone(scratch));
        }

        // Hash the id: a warehouse name is not a safe path component, and the digest keeps
        // the directory stable across invocations.
        let dir = std::env::temp_dir().join(format!(
            "forklift-head-shared-{}",
            object_utils::hash_object_bytes(warehouse_id.as_bytes())
        ));

        let scratch = Arc::new(Scratch::at(dir, false)?);
        pool.insert(warehouse_id.to_string(), Arc::clone(&scratch));

        Ok(scratch)
    }

    /// Lay down `.forklift/…` at `dir` so the storage layer finds a real warehouse.
    /// `prepare_warehouse` is idempotent, so reopening a warm shared scratch is a no-op.
    fn at(dir: PathBuf, ephemeral: bool) -> Result<Scratch, String> {
        std::fs::create_dir_all(&dir)
            .map_err(|e| format!("Error while creating the scratch warehouse: {}", e))?;

        let scratch = Scratch { dir, ephemeral };
        scratch.scoped(|| warehouse_utils::prepare_warehouse().map(|_| ()))?;

        Ok(scratch)
    }

    /// Run `work` under this scratch's storage-root scope, so every `forklift_core`
    /// storage path resolves inside it. Strictly synchronous (the scope is thread-local
    /// and must never cross an `.await`). Generic over the error type so a caller can run
    /// the audit — which distinguishes a conflict from a verification failure — inside the
    /// same scope as the mirror.
    pub fn scoped<T, E>(&self, work: impl FnOnce() -> Result<T, E>) -> Result<T, E> {
        let _scope = StorageRootScope::enter(&self.dir);

        work()
    }

    /// The scratch warehouse root.
    pub fn root(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.ephemeral {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// What one mirroring pass has already written into the scratch. Shared across the several
/// [`materialize`] calls of a single ref update (the office chain and the target pallet
/// overlap), and deliberately order-independent: a parcel first seen below the bound can be
/// upgraded to a full mirror later, so it does not matter which pallet is walked first.
#[derive(Default)]
pub struct Mirror {
    /// Object hashes now present in the scratch — parcels, trees and blobs alike. A parcel
    /// here has had its signature sidecar mirrored too, when the store had one.
    objects: HashSet<String>,

    /// Parcels whose tree closure has been mirrored.
    expanded: HashSet<String>,

    /// Parcels mirrored body-only, below the `known_complete` bound.
    shallow: HashSet<String>,
}

/// Mirror into the current scratch (the caller must already be inside [`Scratch::scoped`])
/// every object the audit reads while walking the closure of `head`.
///
/// `include_file_blobs` is `true` for a meta pallet (the office), whose "blobs" are the
/// tracked-metadata records that `verify_office_chain` reads; it is `false` for a working
/// pallet, whose blobs are file content the audit only existence-checks — those stay in
/// the object store and are answered by [`ObjectStore::exists`].
///
/// `known_complete` is the pallet's current head: everything reachable from it was audited
/// when it was committed. Below it the walk mirrors parcel *bodies* only — the closure
/// check still reads them (see the module docs) — and skips the trees, blobs and sidecars
/// that make the mirror expensive. `None` mirrors everything, for a pallet creation and for
/// the office chain.
///
/// Objects the store lacks are skipped rather than erroring: the subsequent
/// `verify_parcel_closure` produces the authoritative "history incomplete" diagnostic.
pub fn materialize(
    store: &dyn ObjectStore,
    head: &str,
    include_file_blobs: bool,
    known_complete: Option<&str>,
    mirror: &mut Mirror,
) -> Result<(), String> {
    let mut queue: VecDeque<(String, bool)> = VecDeque::new();
    queue.push_back((head.to_string(), true));

    while let Some((hash, inherited)) = queue.pop_front() {
        // The bound itself is already-audited history: its body is still read (the closure
        // check walks its ancestry to build the prune set), its tree is not.
        let full = inherited && Some(hash.as_str()) != known_complete;

        if full {
            // A parcel already expanded needs nothing more; its parents are enqueued.
            if !mirror.expanded.insert(hash.clone()) {
                continue;
            }
        } else if mirror.expanded.contains(&hash) || !mirror.shallow.insert(hash.clone()) {
            continue;
        }

        if !mirror_object(store, &hash, mirror)? {
            // Absent: leave the gap for `verify_parcel_closure` to report precisely.
            continue;
        }

        // The head and every parent are parcels; a parse failure (never expected) simply
        // stops this branch and lets verification report the corruption.
        let Ok(parcel) = object_utils::load_parcel(&hash) else {
            continue;
        };

        if full {
            // Exactly the parcels `verify_pallet_history` will check. It never traverses
            // *through* the bound (it skips it before enqueueing its parents), so the set it
            // discovers is the set reached here with `full` still set — which is why a
            // sidecar below the bound is never read, and never fetched.
            mirror_signature(store, &hash)?;

            materialize_tree(store, &parcel.tree_hash, include_file_blobs, mirror)?;
        }

        for parent in parcel.parents {
            queue.push_back((parent, full));
        }
    }

    Ok(())
}

/// Ensure `hash`'s bytes are in the scratch, fetching them from the store only when they
/// are not already there. Returns whether the object exists at all.
///
/// A warm shared scratch answers most of these from disk: the object was mirrored by an
/// earlier invocation, *from this warehouse's store*, which is why the pool is keyed by
/// warehouse.
fn mirror_object(store: &dyn ObjectStore, hash: &str, mirror: &mut Mirror) -> Result<bool, String> {
    if mirror.objects.contains(hash) {
        return Ok(true);
    }

    if file_utils::does_object_exist(hash)? {
        mirror.objects.insert(hash.to_string());

        return Ok(true);
    }

    let Some(bytes) = store.get(hash)? else {
        return Ok(false);
    };

    object_utils::store_object_bytes(hash, &bytes)?;
    mirror.objects.insert(hash.to_string());

    Ok(true)
}

/// Mirror a parcel's signature sidecar when the store has one and the scratch does not.
/// Checked separately from the body: a parcel can be uploaded before it is signed, so a
/// warm scratch holding the body says nothing about the sidecar.
fn mirror_signature(store: &dyn ObjectStore, parcel_hash: &str) -> Result<(), String> {
    if sign_utils::load_raw_parcel_signature(parcel_hash)?.is_some() {
        return Ok(());
    }

    if let Some(sidecar) = store.get_signature(parcel_hash)? {
        sign_utils::store_raw_parcel_signature(parcel_hash, &sidecar)?;
    }

    Ok(())
}

/// Mirror a tree and everything below it that the audit reads: the tree objects always,
/// the file blobs only when `include_file_blobs` (see [`materialize`]).
fn materialize_tree(
    store: &dyn ObjectStore,
    tree_hash: &str,
    include_file_blobs: bool,
    mirror: &mut Mirror,
) -> Result<(), String> {
    if !mirror.objects.insert(tree_hash.to_string()) {
        return Ok(());
    }

    // `mirror_object` would re-check the set we just inserted into, so inline the fetch.
    if !file_utils::does_object_exist(tree_hash)? {
        let Some(bytes) = store.get(tree_hash)? else {
            mirror.objects.remove(tree_hash);

            return Ok(());
        };

        object_utils::store_object_bytes(tree_hash, &bytes)?;
    }

    let tree = object_utils::load_tree(tree_hash)?;

    if include_file_blobs {
        for (_, file) in tree.get_files() {
            mirror_object(store, &file.hash, mirror)?;
        }
    }

    for (_, subtree) in tree.get_subtrees() {
        materialize_tree(store, &subtree.hash, include_file_blobs, mirror)?;
    }

    Ok(())
}
