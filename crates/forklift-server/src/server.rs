//! The HTTP surface of the server head: one handler per protocol endpoint.
//!
//! Every handler does its filesystem work inside `spawn_blocking` (the storage code is
//! synchronous by design) under the resolved warehouse's storage-root scope. Nothing here
//! serializes requests: the mutations with ordering requirements — a ref update and the one-way
//! door of trust establishment — each commit through core's compare-and-set, which checks under
//! the warehouse's ref lock that what the decision read still stands, and refuses otherwise.
//!
//! Two serving modes share every handler: `--root` serves a single warehouse at
//! `/v1/…`, `--warehouses` serves each subdirectory of a base folder at
//! `/warehouses/{id}/v1/…` (the id travels inside `remote.url`, so clients need
//! nothing new). Warehouse creation is explicit: `PUT /warehouses/{id}` on a
//! token-protected multi-warehouse server (never a side effect of a lift).

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use axum::body::{Body, Bytes, HttpBody};
use axum::extract::{DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{on, MethodFilter, MethodRouter};
use axum::{Extension, Json, Router};
use forklift_core::globals::{StorageRootScope, FOLDER_NAME_FORKLIFT_ROOT};
use forklift_core::model::hooks::{
    AdmissionHookRequest, AdmissionHookResponse, AuthenticationHookRequest,
    AuthenticationHookResponse, HookEvent, ResolutionHookRequest, ResolutionHookResponse,
    EVENT_KEY_REVOKED, EVENT_PALLET_DELETED, EVENT_PALLET_UPDATED, EVENT_TRUST_ESTABLISHED, EVENT_TRUST_RESET,
    EVENT_WAREHOUSE_CREATED,
};
use forklift_core::model::remote::{
    ErrorResponse, HoldAnswer, HoldsRequest, HoldsResponse, MissingObjectsRequest,
    MissingObjectsResponse, RefDeleteRequest, RefUpdateRequest,
    ResolveRequest, ResolveResponse, ServedRedaction, ServedRedactionState, ServedWithholding, ServedWithholdingState, TrustAnchorDto, UploadTargetsRequest, UploadTargetsResponse,
    WarehouseInfo, MAX_MISSING_BATCH, MAX_UPLOAD_TARGETS_BATCH, PROTOCOL_VERSION,
    REFUSAL_NOT_AN_OBJECT_HASH, distinct_resolve_identifiers, resolve_answer_cap,
};
use forklift_core::util::authority_utils::{self, Denied, Principal};
use forklift_core::util::office_utils::OFFICE_PALLET_NAME;
use forklift_core::util::pallet_utils::{
    AnchorPrecondition, BoundaryScope, OfficePrecondition, PalletsPrecondition, RefPreconditions, RefUpdateError,
};
use forklift_core::util::lock_utils::{self, BaseServeLock, FormatLease, ServeLock, StoreAccessLock};
use forklift_core::util::{
    applied_redactions, audit_utils, body_utils, bay_utils, bundle_utils, file_utils, franchise_utils, hook_utils, merge_utils, meta_tree,
    object_utils, office_utils, origin_utils, pack_utils, pallet_utils, progress_utils, redaction_deleter, redaction_utils, scope_utils,
    sealed_utils, sign_utils, tree_utils, unavailable, warehouse_utils,
};
use forklift_core::util::applied_redactions::AppliedSet;
use forklift_core::util::withholding::{self, WithheldSet};
use subtle::ConstantTimeEq;

/// One served warehouse: its storage root, its bundle-rebuild state and the locks it is served
/// under.
struct WarehouseHandle {
    /// The warehouse root; every blocking storage closure enters it as a storage-root
    /// scope (the process never changes its working directory, which is what lets one
    /// process serve several roots).
    root: PathBuf,

    /// Accepted lifts since the bundle was last (re)built — the auto-rebuild trigger.
    lifts_since_bundle: std::sync::atomic::AtomicU32,

    /// Whether a bundle rebuild is running right now (never two at once).
    bundling: std::sync::atomic::AtomicBool,

    /// Whether the running rebuild must be followed by another: a redaction applied while it ran,
    /// so it publishes nothing.
    rebuild_again: std::sync::atomic::AtomicBool,

    /// Whether a redaction run's round must rebuild the bundle after its deletion: a ref write
    /// applied a redaction, withdrawing it. Read only by the round, never by a running rebuild, so
    /// the build waits for the drain rather than run beside it.
    rebuild_after_drain: std::sync::atomic::AtomicBool,

    /// The serve lock for this root, held for as long as the handle is served. `None` for
    /// handles that are not a live served warehouse (test fixtures). Held in the handle so the
    /// lock releases exactly when the served warehouse stops being served: when the server stops,
    /// or when a multi-warehouse server lets an idle warehouse go.
    _serve_lock: Option<ServeLock>,

    /// The shared store-access hold for this root, held beside the serve lock for the same
    /// lifetime and `None` exactly when it is. It orders everything the server writes against
    /// every operation that deletes objects on a liveness decision — a full repack or gc, run by
    /// another process on this root — so a lift can never dedup against an object one of them is
    /// about to delete. (Scope-prune, the other deleter, runs only on a sparse warehouse, which is
    /// never served.)
    ///
    /// Behind a mutex only so the redaction deleter can trade it for the exclusive side while the
    /// warehouse is drained ([`delete_drained`]), and take it back before anything is served
    /// again. Nothing else touches it.
    store_access: std::sync::Mutex<Option<StoreAccessLock>>,

    /// The format lease for this root, held for the handle's lifetime, so nothing upgrades a
    /// warehouse while it is served. `None` exactly when the two above are.
    _format_lease: Option<FormatLease>,

    /// Whether the warehouse is drained for the redaction deleter: while set, every request that
    /// resolves it is refused with a `503`, so no new reference to the handle is taken. Read and
    /// written only under the registry mutex, which every resolve takes.
    draining: std::sync::atomic::AtomicBool,

    /// Cancelled to cut every bundle download running from this handle, then replaced for the
    /// downloads that start after. A download takes the token in force before it opens the bundle.
    bundle_cut: std::sync::Mutex<tokio_util::sync::CancellationToken>,

    /// Whether a redaction run ([`run_redactions`]) is running or waiting to retry for this handle
    /// (never two at once).
    redacting: std::sync::atomic::AtomicBool,

    /// Whether the running redaction run must go round again: a ref write landed while it ran.
    redact_again: std::sync::atomic::AtomicBool,

    /// How many redaction rounds have been asked for ([`run_redactions`]).
    redactions_asked: std::sync::atomic::AtomicU64,

    /// Wakes a redaction run waiting to retry an owed deletion, when a ref write asks for a round.
    redaction_wake: Arc<tokio::sync::Notify>,

    /// Whether this handle's first redaction run is still to start. Set for a served handle, so
    /// a deletion owed when an earlier process stopped serving the warehouse is carried out once
    /// it is served again, ref write or not.
    first_run_due: std::sync::atomic::AtomicBool,

    /// The applies owed here, numbered in order, and the number up to which an apply has since
    /// discharged them ([`apply_discharging`]): while one is not discharged, a redaction the
    /// warehouse's scan would apply may be unapplied, and no content is served
    /// ([`await_applies`]). One is owed by each ref write here, from just before it may land
    /// ([`WarehouseHandle::owe_apply`]), and one by a served handle from the start, settled by its
    /// first run's apply: a ref write an earlier process took may have let a redaction apply.
    applies_owed: std::sync::atomic::AtomicU64,
    applies_discharged: std::sync::atomic::AtomicU64,

    /// The numbers of the applies owed by ref writes that have not returned yet: an apply
    /// discharges only those owed before the earliest of them, since its scan may miss what one
    /// still in flight writes. Each is numbered and added under this mutex.
    applies_in_flight: std::sync::Mutex<std::collections::BTreeSet<u64>>,

    /// Wakes the requests waiting for the owed applies ([`await_applies`]), when an apply
    /// discharges some and when the warehouse is drained.
    /// The damage refusal ([`damage_refusal`]) the last apply stopped on, while it stands: an apply
    /// owed then waits for the operator's repair, not for time, so content is refused with it at
    /// once rather than after [`APPLY_WAIT`]. Cleared by the next apply that discharges.
    apply_damage: std::sync::Mutex<Option<String>>,

    applies_wake: tokio::sync::Notify,

    /// Which chunks are served as absent while a deletion is owed ([`withheld_among`]).
    chunk_gate: std::sync::Mutex<ChunkGate>,

    /// Held while [`withheld_among`] walks the live set, so one request walks at a time and the
    /// rest wait for its answer.
    chunk_gate_walk: std::sync::Mutex<()>,

    /// The ref writes this server has started here, and those it has finished, landed or not
    /// ([`RefWriteCounted`]): a withheld set is trusted only when walked with none in flight and
    /// none started since, so it never misses a head that names one of its chunks.
    ref_writes_started: std::sync::atomic::AtomicU64,
    ref_writes_finished: std::sync::atomic::AtomicU64,
}

/// The chunks a warehouse serves as absent while a deletion is owed (see
/// [`redaction_deleter::withheld_chunks`]), as last worked out.
#[derive(Default)]
struct ChunkGate {
    /// The applied redactions and withheld hashes `candidates` was read for; `None` until it is
    /// read, and once a deletion has run.
    basis: Option<AppliedSet>,

    /// The chunks of the removed recipes the store held, and the chunks the journal owes.
    candidates: std::collections::HashSet<String>,

    /// The candidates no other object names, with the count of ref writes started before the walk
    /// and when the walk began: kept only for a walk no ref write here overlapped.
    withheld: Option<(u64, std::time::Instant, std::collections::HashSet<String>)>,
}

/// How long a walked withheld set is trusted when no ref write here has landed since. A command
/// run on the served root itself writes refs this server does not count, so a chunk it makes
/// another file's is served again within this long.
const CHUNK_GATE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

impl WarehouseHandle {
    /// A handle with no serve lock — for test fixtures, which are not a live served warehouse. A
    /// served warehouse is built with [`WarehouseHandle::serving`].
    fn new(root: PathBuf) -> WarehouseHandle {
        WarehouseHandle {
            root,
            lifts_since_bundle: std::sync::atomic::AtomicU32::new(0),
            bundling: std::sync::atomic::AtomicBool::new(false),
            rebuild_again: std::sync::atomic::AtomicBool::new(false),
            rebuild_after_drain: std::sync::atomic::AtomicBool::new(false),
            _serve_lock: None,
            store_access: std::sync::Mutex::new(None),
            _format_lease: None,
            draining: std::sync::atomic::AtomicBool::new(false),
            bundle_cut: std::sync::Mutex::new(tokio_util::sync::CancellationToken::new()),
            redacting: std::sync::atomic::AtomicBool::new(false),
            redact_again: std::sync::atomic::AtomicBool::new(false),
            redactions_asked: std::sync::atomic::AtomicU64::new(0),
            redaction_wake: Arc::new(tokio::sync::Notify::new()),
            first_run_due: std::sync::atomic::AtomicBool::new(false),
            applies_owed: std::sync::atomic::AtomicU64::new(0),
            applies_discharged: std::sync::atomic::AtomicU64::new(0),
            applies_in_flight: std::sync::Mutex::new(std::collections::BTreeSet::new()),
            applies_wake: tokio::sync::Notify::new(),
            apply_damage: std::sync::Mutex::new(None),
            chunk_gate: std::sync::Mutex::new(ChunkGate::default()),
            chunk_gate_walk: std::sync::Mutex::new(()),
            ref_writes_started: std::sync::atomic::AtomicU64::new(0),
            ref_writes_finished: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// A handle for a warehouse this process is about to serve: acquires the serve lock and the
    /// shared store-access hold at `root` and keeps both in the handle for its lifetime. Errors if
    /// the root holds an unfinished clone, is already served by another process, or a `gc` or
    /// full repack is running against it — in each case serving it would be unsafe. Every handle
    /// the server registers must come from here, in single and multi mode alike.
    ///
    /// The shared hold is tried, never waited for: this runs under the registry mutex, on a
    /// runtime thread.
    ///
    /// The root is marked as served (`tombstone_utils::mark_served`) before the handle is
    /// returned, durably, and a root whose mark cannot be written is refused. A first mark waits for
    /// a bay's ref lock only when `wait` (a single root, served once at startup, under no registry);
    /// otherwise the root is refused as busy, and a later request serves it: from then on every ref
    /// removal there leaves a tombstone, which a client that freed its copy of a head this server
    /// listed relies on.
    ///
    /// A root that may lack content (one cloned with `--only` or `--cache`) is refused for
    /// good. Clients treat a served head as proof that the server holds its whole closure:
    /// scope prune frees local copies of what the remote heads reach, so such a root would let a
    /// client free content the server cannot serve back. Checking once, here, is enough because a
    /// full root never turns into one: the warehouse marker records the mode when the warehouse is
    /// prepared and nothing rewrites it, and the fetch scope can be set only on a warehouse already
    /// recorded as sparse.
    ///
    /// So is a root whose sealed-object record holds anything, or cannot be read, whatever its
    /// marker says: each hash recorded there is an object it holds by hash alone, without its
    /// content. Only a cache records one in the normal course, but the record is read rather than
    /// inferred from the mode. And so is a root that records an origin, which only a partial
    /// warehouse is prepared with. Once is enough here too: a new claim is refused where no origin
    /// is recorded, and the push-downs that record without one start only from entries already
    /// there, or run only in a cache; with neither an entry nor an origin, nothing adds one. And so
    /// is a root that lacks content its unavailable record lists, which a fetch left out because
    /// its remote's host withholds it; an entry whose object it holds counts nothing.
    fn serving(root: PathBuf, wait: bool) -> Result<WarehouseHandle, ServingRefusal> {
        use forklift_core::error::RefusalCode;
        // Acquire inside the root's storage scope so the locks land at this warehouse's store
        // root (the scope is synchronous and never held across an `.await`; both guards keep an
        // absolute path or an open file, so they stay valid once the scope drops).
        let (serve_lock, store_access, format_lease) = {
            // An unfinished clone has not finished fetching, so even one marked full may lack
            // what its heads name: it is refused before anything it holds is trusted.
            franchise_utils::gate(&root).map_err(|e| ServingRefusal::Unfinished(String::from(e)))?;

            // A `--warehouses` route reaches here having only classified the root, which takes any
            // format. Before anything in the warehouse is read as this build's format.
            let format_lease = warehouse_utils::hold_warehouse_format(&root, warehouse_utils::Head::Server)
                .map_err(|e| match &e {
                    forklift_core::error::CoreError::Refusal { code: RefusalCode::WarehouseFormatOlder | RefusalCode::WarehouseFormatNewer
                         | RefusalCode::WarehouseUpgradeIncomplete | RefusalCode::WarehouseUpgradeRunning, .. } =>
                        ServingRefusal::OtherFormat(String::from(e)),
                    _ => ServingRefusal::Unreadable(String::from(e)),
                })?;

            let _scope = StorageRootScope::enter(&root);

            // Before the locks, so a sparse root under a running scope prune is told it is partial,
            // not busy.
            if scope_utils::content_may_be_absent().map_err(ServingRefusal::Unreadable)? {
                return Err(ServingRefusal::Partial);
            }
            let sealed = sealed_utils::read_sealed().map_err(ServingRefusal::Unreadable)?;
            if !sealed.is_empty() {
                return Err(ServingRefusal::Sealed(sealed.len()));
            }
            if origin_utils::read_origin().map_err(ServingRefusal::Unreadable)?.is_some() {
                return Err(ServingRefusal::Origin);
            }
            // An entry the store holds again is no gap: only the absent ones count.
            let unavailable = unavailable::absent_count().map_err(ServingRefusal::Unreadable)?;
            if unavailable > 0 {
                return Err(ServingRefusal::Unavailable(unavailable));
            }

            let serve_lock = ServeLock::acquire().map_err(ServingRefusal::Busy)?;
            let store_access = StoreAccessLock::try_shared().map_err(ServingRefusal::Busy)?;

            if !forklift_core::util::tombstone_utils::mark_served(wait).map_err(ServingRefusal::Unreadable)? {
                return Err(ServingRefusal::Busy(
                    "A command in this warehouse is changing its pallets. Try again once it finishes.".to_string()
                ));
            }

            (serve_lock, store_access, format_lease)
        };
        let mut handle = WarehouseHandle::new(root);
        handle._serve_lock = Some(serve_lock);
        handle.store_access = std::sync::Mutex::new(Some(store_access));
        handle.first_run_due = std::sync::atomic::AtomicBool::new(true);
        // A ref write an earlier process took may have let a redaction apply, and the process
        // stopped before applying it: the first run's apply is owed before any content is served.
        // With every redaction applied, that apply reads two records and settles it.
        handle.applies_owed = std::sync::atomic::AtomicU64::new(1);
        handle._format_lease = Some(format_lease);
        Ok(handle)
    }

    /// Owe an apply ([`WarehouseHandle::applies_owed`]): taken just before a ref write here may
    /// land, and dropped once the write returns, landed or not. Content waits from the moment the
    /// write could let a redaction apply until an apply that began after it returned discharges it
    /// ([`apply_discharging`]).
    fn owe_apply(self: &Arc<Self>) -> OwedApply {
        let mut in_flight = self.applies_in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let number = self.applies_owed.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
        in_flight.insert(number);

        OwedApply { warehouse: Arc::clone(self), number }
    }

    /// The number up to which an apply beginning now may discharge the applies owed: every one
    /// owed so far, less those of ref writes still in flight and any owed after them.
    fn dischargeable(&self) -> u64 {
        let in_flight = self.applies_in_flight.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

        match in_flight.first() {
            Some(earliest) => earliest - 1,
            None => self.applies_owed.load(std::sync::atomic::Ordering::SeqCst),
        }
    }

    /// Whether an apply is owed here that one beginning now would discharge: one no ref write
    /// still in flight owes, so nothing but an apply that is not under way will discharge it.
    fn apply_owed(&self) -> bool {
        self.applies_discharged.load(std::sync::atomic::Ordering::SeqCst) < self.dischargeable()
    }

    /// Whether an apply is owed here, so no content is served ([`await_applies`]).
    fn applies_closed(&self) -> bool {
        use std::sync::atomic::Ordering;

        self.applies_discharged.load(Ordering::SeqCst) < self.applies_owed.load(Ordering::SeqCst)
    }
}

/// An apply a ref write owes, while the write is in flight ([`WarehouseHandle::owe_apply`]).
struct OwedApply {
    warehouse: Arc<WarehouseHandle>,
    number: u64,
}

impl Drop for OwedApply {
    fn drop(&mut self) {
        self.warehouse.applies_in_flight.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.number);
    }
}

/// Why [`WarehouseHandle::serving`] would not serve a root.
#[derive(Debug)]
enum ServingRefusal {
    /// Another server holds it, or a `gc` or full repack is running against it. It can be served
    /// once that ends.
    Busy(String),

    /// It holds a clone that has not finished: it is resumed or deleted first.
    Unfinished(String),

    /// It is in a format this build does not read, or an upgrade of it has not finished or is
    /// running. One whose format could not be checked is [`ServingRefusal::Unreadable`]. The others this server holds are served all the same.
    OtherFormat(String),

    /// Its warehouse marker, fetch scope, sealed-object record, origin record or unavailable record
    /// could not be read, or its served mark could not be read or written.
    Unreadable(String),

    /// It is sparse or a cache, so it may lack content, and is never served.
    Partial,

    /// Its sealed-object record holds this many objects it has by hash alone, so it may lack
    /// content, and is never served.
    Sealed(usize),

    /// It records an origin, as only a partial warehouse does, so its files may come from one that
    /// lacks content, and it is never served. The message does not name the origin: a
    /// `--warehouses` server sends it to clients, and a URL can carry credentials.
    Origin,

    /// It lacks this many objects its unavailable record lists: content a fetch left out because
    /// its remote's host withholds it, so it may lack content its pallets reach, and it is not
    /// served until that content is back.
    Unavailable(usize),
}

impl ServingRefusal {
    /// The refusal as a message, naming the warehouse as `name`.
    fn message(self, name: &str) -> String {
        match self {
            ServingRefusal::Busy(reason)
            | ServingRefusal::Unfinished(reason)
            | ServingRefusal::OtherFormat(reason)
            | ServingRefusal::Unreadable(reason) => reason,
            ServingRefusal::Partial => format!(
                "{} is a partial warehouse: it was cloned with --only or --cache, so it may \
                lack content its pallets reach. A client trusts a server to hold the whole history \
                it serves, and could free local copies this one cannot send back. Serve a full \
                warehouse instead: clone the remote again without --only or --cache.",
                name
            ),
            ServingRefusal::Sealed(count) => format!(
                "{} records {} object{} it holds by hash alone, without the content, so it may lack \
                content its pallets reach. A client trusts a server to hold the whole history it \
                serves, and could free local copies this one cannot send back. A full warehouse records \
                nothing by hash, so this root may be a copy of a cache: clone \
                the warehouse it was copied from again, from its server, into a new root, without \
                --only or --cache, and serve that.",
                name,
                count,
                if count == 1 { "" } else { "s" }
            ),
            ServingRefusal::Origin => format!(
                "{} records an origin (.forklift/origin), which only a warehouse cloned with \
                --only or --cache has, so its files may come from one that lacks content its pallets \
                reach. A client trusts a server to hold the whole history it serves. Clone that \
                origin again into a new root, without --only or --cache, and serve that.",
                name
            ),
            ServingRefusal::Unavailable(count) => format!(
                "{} lacks {} file{} its record of unavailable content lists: {} remote's host withholds \
                the content, so a fetch left {} out (.forklift/unavailable). A client trusts a server \
                to hold the whole history it serves. Load the missing files' bytes into this \
                warehouse, or, if no pallet reaches them any more, run `forklift store compact --all` \
                there to drop their entries; then serve it again.",
                name,
                count,
                if count == 1 { "" } else { "s" },
                if count == 1 { "its" } else { "their" },
                if count == 1 { "it" } else { "them" },
            ),
        }
    }
}

/// What the process serves.
enum ServeMode {
    /// One warehouse at `/v1/…`.
    Single(Arc<WarehouseHandle>),

    /// Every prepared subdirectory of a base folder, at `/warehouses/{id}/v1/…`.
    Multi { base: PathBuf },
}

/// The most warehouses a multi-warehouse server keeps live at once, whatever the open-file limit
/// allows. Above it, the least recently used idle warehouse is let go before another is served.
const MAX_LIVE_WAREHOUSES: usize = 1024;

/// How many warehouses to keep live given the soft open-file limit (`None` where there is none to
/// read). Each live warehouse holds one open file, its store-access hold, and every request needs
/// files of its own, so half the limit goes to live warehouses, never more than
/// [`MAX_LIVE_WAREHOUSES`]. An unlimited soft limit (`RLIM_INFINITY`) is capped the same way.
fn live_warehouse_cap(open_file_limit: Option<u64>) -> usize {
    match open_file_limit {
        Some(limit) => usize::try_from(limit / 2).unwrap_or(usize::MAX).clamp(1, MAX_LIVE_WAREHOUSES),
        None => MAX_LIVE_WAREHOUSES,
    }
}

/// One live warehouse in the registry, and when it was last resolved.
struct RegistryEntry {
    handle: Arc<WarehouseHandle>,

    /// A tick of [`WarehouseRegistry::clock`], not a wall-clock time: only the order matters.
    last_used: u64,
}

/// The live handles of a multi-warehouse server, at most `cap` of them while any is idle.
///
/// A handle holds its warehouse's serve lock and shared store-access hold, so a live warehouse
/// costs an open file and keeps `gc` and a full repack off it. Serving a warehouse
/// that is not live lets the least recently used *idle* one go first (see
/// [`WarehouseRegistry::make_room`]). When none is idle the new one is admitted over the cap: a
/// request is never refused for the cap alone, and running out of files surfaces as
/// [`WarehouseHandle::serving`]'s refusal: a 503 when the clone gate or the locks cannot
/// be checked or taken, a 500 when the marker, fetch scope, sealed-object record or origin
/// record cannot be read.
struct WarehouseRegistry {
    entries: HashMap<String, RegistryEntry>,
    clock: u64,
    cap: usize,
}

impl WarehouseRegistry {
    fn new(cap: usize) -> WarehouseRegistry {
        WarehouseRegistry { entries: HashMap::new(), clock: 0, cap }
    }

    fn tick(&mut self) -> u64 {
        self.clock += 1;
        self.clock
    }

    /// The live handle for `id`, marked as just used.
    fn get(&mut self, id: &str) -> Option<Arc<WarehouseHandle>> {
        let now = self.tick();
        let entry = self.entries.get_mut(id)?;
        entry.last_used = now;

        Some(Arc::clone(&entry.handle))
    }

    fn insert(&mut self, id: String, handle: Arc<WarehouseHandle>) {
        let last_used = self.tick();
        self.entries.insert(id, RegistryEntry { handle, last_used });
    }

    /// Let idle warehouses go, least recently used first, until there is room for one more under
    /// the cap or none is idle.
    ///
    /// Idle means the registry holds the only reference (`strong_count == 1`). Every other
    /// reference was cloned from the registry's under the registry mutex, which the caller holds,
    /// so no request, blocking task, bundle rebuild or download holds an idle handle and none can
    /// take one while it is let go: at most one handle per id stays true, and the next handle for
    /// the id finds its serve lock free. Each pass is a scan of the registry; it runs only for a
    /// warehouse that is not live while the registry is full.
    fn make_room(&mut self) {
        while self.entries.len() >= self.cap {
            let idle = self.entries.iter()
                .filter(|(_, entry)| Arc::strong_count(&entry.handle) == 1)
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(id, _)| id.clone());

            let Some(id) = idle else {
                return;
            };

            if let Some(entry) = self.entries.remove(&id) {
                let_go(entry.handle);
            }
        }
    }
}

/// Stop serving a warehouse whose handle nothing else holds: forget everything this process
/// caches about its objects, then drop the handle with its serve lock and store-access hold.
///
/// Forgetting the caches is what keeps letting go safe, not just tidy. Once the holds are released
/// `gc` or a full repack may delete objects, and the pack set, the object cache and the parsed-tree
/// cache all answer a hit without looking at the disk: kept, they would answer "present" for a
/// deleted object to the next request for this warehouse, both to a client deciding what to upload
/// and to the closure check a ref update must pass. The caller holds the registry mutex, so this
/// finishes before any request can serve the warehouse again.
///
/// The serve lock is a file, released here explicitly: one left behind would refuse this server the
/// warehouse from then on, so a failed removal is logged as the error it is. Probing for the file
/// afterwards could not say whose it is — letting go is what lets `gc` or a repack take it.
fn let_go(handle: Arc<WarehouseHandle>) {
    {
        let _scope = StorageRootScope::enter(&handle.root);
        pack_utils::forget_warehouse();
    }

    // The registry held the only reference; anything else is let go by its last holder's drop.
    let Ok(mut handle) = Arc::try_unwrap(handle) else {
        return;
    };

    if let Some(Err(error)) = handle._serve_lock.take().map(ServeLock::release) {
        tracing::error!(%error, "a warehouse let go kept its serve lock; it is refused until the file is removed");
    }
}

/// The shared server state.
struct AppState {
    mode: ServeMode,

    /// The static bearer token (full access), when configured.
    token: Option<String>,

    /// Per-operator bearer tokens: token → office identifier. What the
    /// operator may do derives from their role in the target warehouse's office —
    /// the tracked, signed metadata — not from the token itself.
    operator_tokens: HashMap<String, String>,

    /// Rebuild a warehouse's bundle after this many accepted lifts (`None` = never;
    /// `forklift-server bundle` stays the manual path).
    rebuild_after_lifts: Option<u32>,

    /// The live handles of the warehouses served (multi mode). Exactly one handle may ever exist
    /// per warehouse id — a second handle would need a second serve lock, which the first refuses,
    /// and would keep bundle-rebuild state of its own.
    warehouses: Mutex<WarehouseRegistry>,

    /// The configured hooks (`docs/format/HOOK_PROTOCOL.md`) — the typed seam a
    /// hosting provider plugs into. Only the soft surface is hookable; signature and
    /// privilege verification never are.
    authentication_hook: Option<HookEndpoint>,
    admission_hook: Option<HookEndpoint>,
    events_hook: Option<HookEndpoint>,
    resolution_hook: Option<HookEndpoint>,

    /// The outbound HTTP client of the hooks.
    http: reqwest::Client,

    /// Successful authentication-hook answers, cached per token (hot path). A revoked
    /// credential outlives its revocation by at most the TTL.
    authentication_cache: Mutex<HashMap<String, (String, std::time::Instant)>>,
    authentication_cache_ttl: std::time::Duration,

    /// The explicit opt-out of authentication — see [`ServeOptions::open`]. `check_auth` reads
    /// this, and only this, to decide whether an otherwise-empty auth config means `Open`;
    /// `serve` refuses to start rather than ever construct an `AppState` with this `false` and
    /// no token/operator tokens/hook configured (see the check right after this struct's
    /// construction below), so in a server built through `serve` this field being `false`
    /// guarantees at least one of those three is `Some`/non-empty.
    open: bool,
}

/// A handler error: a status code and the message for the JSON error body.
type HandlerError = (StatusCode, String);

/// The path parameters of a request; which keys exist depends on the route and the
/// serving mode (multi mode adds `warehouse` to every route).
type PathParams = HashMap<String, String>;

/// One configured hook endpoint. The secret is mandatory: every hook request is
/// signed (Blake3 keyed MAC over timestamp + body), because a spoofable
/// authentication hook is game over (§8.13).
///
/// `Debug` is hand-written, not derived, so that redaction is structural rather than a claim
/// about which paths happen to print this type today: a derived `Debug` here would print
/// `secret` in full the moment anything — a test's `unwrap_err()`, a future
/// `tracing::debug!(?options)`, a panic message — formats a value that contains one. `main.rs`'s
/// `ConfigFile` hand-writes its own `Debug` for the identical reason (PR #124 round 2, F3): it
/// used to derive `Debug` solely for a test's `unwrap_err()` trait bound, which round 2 replaced
/// once that derive was found to print `ConfigFile::token` — the higher-value secret of the two
/// — in full for the same reason. See `tests::hook_endpoint_debug_never_prints_the_secret` and
/// `main.rs::tests::config_file_debug_never_prints_the_token`.
#[derive(Clone)]
pub struct HookEndpoint {
    pub url: String,
    pub secret: String,
}

impl std::fmt::Debug for HookEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HookEndpoint")
            .field("url", &self.url)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// What to serve and how (the merged flags/config of the `serve` subcommand).
pub struct ServeOptions {
    /// The single warehouse root to serve at `/v1` (mutually exclusive with
    /// `warehouses`).
    pub root: Option<String>,

    /// The base folder whose subdirectories are served at `/warehouses/{id}/v1`.
    pub warehouses: Option<String>,

    /// The address to bind (port 0 picks a free port).
    pub addr: String,

    /// The static bearer token (full access), if any. Already normalized by the time it
    /// reaches here when built through `main.rs::serve` (`main.rs::merge_static_token`
    /// filters an empty-or-whitespace-only string out of *each* of the flag and config-file
    /// sources before deciding precedence between them — filtering only the already-merged
    /// value, as `serve` below alone used to, cannot recover a real value that precedence already
    /// discarded in favor of a blank one from the other source). `serve` below still filters
    /// again — this struct is `pub`, so any other constructor (a test, a future embedder) gets
    /// the same "a blank string is not a credential" guarantee regardless of whether it
    /// normalized its own sources first. Blank means empty *or* whitespace-only (PR #124 round 3,
    /// F1) — the same rule `parse_operator_tokens` and `authenticate_via_hook` apply to their own
    /// credential/identity fields, so every blank check in this module agrees on what blank
    /// means.
    pub token: Option<String>,

    /// The path of the per-operator token file, if any.
    pub tokens: Option<String>,

    /// Whether either of `token`'s two raw sources (the `--token` flag, the config file's
    /// `token` key) was present but blank (empty or whitespace-only) — i.e. whether `token`
    /// above is `None` *because* a blank credential was supplied, as opposed to nothing being
    /// supplied at all. Computed by `main.rs::merge_static_token`, alongside the normalization
    /// above; used only by `serve`'s startup refusal, to name that case as "a blank
    /// --token/token" rather than the misdiagnosing "no --token/token" (PR #124 round 2, F2).
    /// Never consulted for authentication itself.
    pub blank_token_supplied: bool,

    /// Refuse request bodies over this size (MiB); `None` = the default cap
    /// (`object_utils::MAX_OBJECT_BYTES`, currently 64 MiB) applies — never unlimited. See
    /// `body_limit_bytes`.
    pub max_body_mb: Option<u64>,

    /// Rebuild a warehouse's bundle after this many accepted lifts; `None` = never.
    pub rebuild_after_lifts: Option<u32>,

    /// `authentication` hook: credential → operator identifier (hot, fail closed).
    pub authentication_hook: Option<HookEndpoint>,

    /// `admission` hook: soft policy gate on uploads/ref updates/creation (hot,
    /// fail closed).
    pub admission_hook: Option<HookEndpoint>,

    /// `event` hook: lift/trust/revocation webhooks (cold, retried).
    pub events_hook: Option<HookEndpoint>,

    /// `resolution` hook: operator id → display name, behind `POST /v1/resolve`
    /// (cold, degrade to pseudonyms).
    pub resolution_hook: Option<HookEndpoint>,

    /// How long a positive authentication-hook answer is cached (seconds; default 60).
    pub authentication_cache_secs: Option<u64>,

    /// The explicit opt-out of authentication (`--open` / `open = true`). This is the ONLY
    /// way [`check_auth`] ever returns [`Principal::Open`] — an empty auth config (no token, no
    /// operator tokens, no authentication hook) with this unset is a startup error, never a
    /// silent "serve openly". Mirrors `forklift-aws-lambda`'s `AuthConfig::Open`
    /// (`FORKLIFT_OPEN_ACCESS=1`): the explicit local/LocalStack opt-out, never inferred from
    /// an absent or malformed setting.
    pub open: bool,
}

/// The flat request timeout this head arms on the `reqwest::Client` it calls every hook
/// through ([`build_hook_client`] builds from this constant and takes no figure of its own).
/// It bounds *every* hook that client carries — authentication, admission, events and
/// resolution — individually, not collectively: a request running two of them may wait twice
/// this long.
///
/// Which hooks a given request actually runs varies, and callers sizing anything against this
/// must not assume a fixed count: a mutating request may run authentication alone (`put_trust`)
/// or authentication and admission (`put_signature`, `put_object`). The authentication and
/// admission hooks fail closed on a transport failure, so a hook endpoint that never answers
/// must not wedge the request it gates forever — but 10s is generous enough that a hook doing
/// real work (a directory lookup, a policy check) never trips it under normal load.
///
/// Module-private, not `pub` — unlike its previous home in `forklift-core::model::hooks`. This
/// head's own outbound hook timeout is this head's own operational business, never a quantity a
/// client budget may read: `forklift-core::util::remote_utils`'s single-write allowance used to
/// import this exact constant to size a client-side deadline, which priced this head's specific
/// hook behavior into an arithmetic that has to be honest against every head, including
/// `forklift-aws-lambda`, which runs no hooks at all. Moving the constant here — so a
/// `forklift-core` budget can no longer reach it without adding a dependency this workspace does
/// not have and should not gain — is what makes that coupling a compile error instead of a
/// silent design defect.
const HOOK_CLIENT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Build the outbound HTTP client `AppState.http` holds, armed at [`HOOK_CLIENT_TIMEOUT`] so a
/// hook endpoint that accepts a connection and then never answers cannot wedge the gated request
/// forever. [`serve`] and the test harness both construct `AppState.http` through it, and it takes
/// no argument, so `serve` has no figure to get wrong.
/// `tests::the_hook_client_is_armed_at_the_production_timeout` reads the armed figure off the
/// built client.
fn build_hook_client() -> Result<reqwest::Client, String> {
    hook_client_armed_at(HOOK_CLIENT_TIMEOUT)
}

/// The construction behind [`build_hook_client`]. The tests that watch a hook time out call it
/// with a short figure, so they build the same client production runs except for the timeout.
fn hook_client_armed_at(timeout: std::time::Duration) -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(|e| format!("Error while building the hook HTTP client: {}", e))
}

/// Convert `max_body_mb` (MiB) to the byte count `axum`'s `DefaultBodyLimit` wants, refusing
/// rather than silently wrapping when the conversion overflows. This is the magnitude half of
/// the "an unsigned config value overflows the arithmetic built on it" class (PR #124 round 1
/// closed the sign half — a negative `max_body_mb` — in `main.rs::optional_non_negative_integer`,
/// which cannot reach this hole: it only ever runs against the config-file `max_body_mb` key, not
/// the `--max-body-mb` flag, which clap already parses straight into `u64`, and it only ever
/// checks the sign, never the magnitude after conversion). `mb.checked_mul(1024 * 1024)` catches
/// the arithmetic overflow directly (`mb >= 2^44` overflows a `u64` byte count); the
/// `usize::try_from` after it additionally catches a 32-bit target, where a byte count that fits
/// in `u64` may still not fit in `usize`. Left unguarded, `(mb as usize) * 1024 * 1024` — the
/// code this replaces — panics at startup on a debug build (what `bin/serve` runs, since
/// overflow checks are on by default in the dev profile) and silently wraps to a *smaller* limit
/// on a release build (`(1u64 << 44) as usize * 1024 * 1024` wraps to exactly `0` on a 64-bit
/// target, rejecting every request body).
///
/// Zero is refused outright, for the identical reason (PR #124 round 4, F6): `max_body_mb`'s own
/// sign check (`optional_non_negative_integer`) only ever rejected negative values, so `0` — a
/// value this comment used to argue was safe because it "needs no equivalent ceiling", true only
/// of the overflow half of the class — sailed through and reached `DefaultBodyLimit::max(0)`. The
/// server then starts normally and 413s every request carrying a body, a 1-byte lift included:
/// non-negative but nonsensical, the same class this PR already closed for
/// `authentication_cache_secs` (a value can be in-range and still make the field meaningless).
/// There is no legitimate deployment that wants every write refused this way — that is what
/// leaving `authentication`/`admission`/`tokens` unconfigured (or simply not running the write
/// paths) is for — so this is a refusal, not a default substitution: `Some(0)` almost certainly
/// means a config typo (`400` meant to be MiB, mistyped as `0`; a template value never filled
/// in), and silently swapping in a default would hide exactly that.
fn body_limit_bytes(max_body_mb: Option<u64>) -> Result<usize, String> {
    match max_body_mb {
        Some(0) => Err(
            "\"max_body_mb\" is 0, which would reject every request carrying a body (even a \
            1-byte lift); pick a real limit, or leave it unset to use the default."
                .to_string()
        ),
        Some(mb) => mb
            .checked_mul(1024 * 1024)
            .and_then(|bytes| usize::try_from(bytes).ok())
            .ok_or_else(|| format!(
                "\"max_body_mb\" is {} MiB, which overflows converting to a byte count; pick a \
                smaller value.",
                mb
            )),
        None => Ok(object_utils::MAX_OBJECT_BYTES),
    }
}

/// The ceiling on `authentication_cache_secs`: how long a revoked credential can keep
/// authenticating after the hook stops vouching for it (`docs/format/HOOK_PROTOCOL.md`: "a
/// revoked credential outlives its revocation by at most the TTL") — it is a revocation-latency
/// budget, not a general-purpose cache knob, and its documented default is 60 seconds. One day
/// is three orders of magnitude past that default — room for any legitimate "reduce hook
/// chatter" setting — while matching the one other day-scale staleness window this same binary
/// already accepts (`Gc`'s `--grace-hours`, default 24) rather than inventing an unrelated
/// number.
///
/// Defined here, not in `main.rs`, because this is the single source of truth both call sites
/// share (PR #124 round 5, F4): `main.rs::bounded_authentication_cache_secs` re-exports this
/// same value into its own, config-file-specific error message rather than keeping a second
/// copy of the number.
pub const MAX_AUTHENTICATION_CACHE_SECS: u64 = 24 * 60 * 60;

/// [`body_limit_bytes`]'s sibling for `authentication_cache_secs`: enforced here, at the
/// `ServeOptions` boundary, not only in `main.rs::bounded_authentication_cache_secs` (the TOML
/// reader) (PR #124 round 5, F4). Every other bound this PR added is deliberately re-enforced
/// here — the blank-token filter above exists so "`ServeOptions`, which is `pub`, gives the
/// same guarantee to any other constructor of it", and `body_limit_bytes` just above is called
/// from `serve` for the identical reason — but this ceiling used to live only in the config-file
/// path, so SERVER.md's and HOOK_PROTOCOL.md's flat claim that the server refuses to start with
/// a value over 24 hours was true only of that one path: `ServeOptions { authentication_cache_secs:
/// Some(u64::MAX), .. }`, built directly (a future CLI flag, a test, another head embedding this
/// crate), sailed straight past `main.rs`'s check and reached `Duration::from_secs` uncontested,
/// making the cache TTL effectively infinite — the same "a revoked credential is never
/// re-checked again" hole `main.rs`'s sign and magnitude checks close for the config-file path.
fn authentication_cache_ttl(secs: Option<u64>) -> Result<std::time::Duration, String> {
    match secs {
        Some(v) if v > MAX_AUTHENTICATION_CACHE_SECS => Err(format!(
            "\"authentication_cache_secs\" is {} seconds, which is over the {}-second (24h) \
            ceiling: a revoked credential must not be able to outlive its revocation by more \
            than about a day.",
            v, MAX_AUTHENTICATION_CACHE_SECS
        )),
        _ => Ok(std::time::Duration::from_secs(secs.unwrap_or(60))),
    }
}

/// Serve one warehouse root, or every warehouse under a base folder.
///
/// # Returns
/// * `Err(String)` - If the root is not a warehouse or the address cannot be bound
///                   (serving itself runs until the process is stopped or receives
///                   SIGINT/SIGTERM, which drains gracefully).
pub async fn serve(options: ServeOptions) -> Result<(), String> {
    // Structured request logs; RUST_LOG overrides (default: info). Coloured only on a terminal:
    // a log file or a service manager's journal would otherwise keep the escape sequences as text.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"))
        )
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .try_init();

    // Every live warehouse keeps an open file (its store-access hold), so the limit also sets how
    // many warehouses a multi-warehouse server keeps live.
    let open_file_limit = lock_utils::raise_open_file_limit();

    // Kept past the `match` below (which consumes `options.tokens`) so the startup refusal can
    // name the empty-`[operators]`-table case distinctly from "no --tokens/tokens file at all":
    // an operator who passed `--tokens ops.toml` and got an empty map back (the file parsed, its
    // `[operators]` table just has no entries) should not be told to look for a flag they already
    // passed.
    let tokens_supplied = options.tokens.is_some();

    let operator_tokens = match options.tokens {
        Some(path) => parse_operator_tokens(&path)?,
        None => HashMap::new(),
    };

    // Defense-in-depth, not the primary enforcement point (see `ServeOptions::token`'s doc): a
    // blank (empty or whitespace-only) string is not a credential — without this,
    // `strip_bearer_prefix("Bearer ")` yields `Some("")`, `tokens_match("", "")` is `true`, and a
    // request carrying the literal header `Authorization: Bearer ` (no credential after the
    // scheme) would authenticate as `Principal::Static` with full privileges; a whitespace-only
    // token has the identical problem one level removed (PR #124 round 3, F1) — either no client
    // can ever present it at all, or, on a transport this crate makes no assumption about
    // trimming, a guessable all-whitespace value can. `main.rs::merge_static_token` already
    // normalizes both of the token's raw sources individually before this ever runs (PR #124
    // round 2, F1: filtering only here, after the flag and the config file were already merged by
    // precedence, could not recover a real token that precedence had already discarded in favor
    // of a blank one from the other source) — this line stays so `ServeOptions`, which is `pub`,
    // gives the same guarantee to any other constructor of it. Mirrors
    // `forklift-aws-lambda::entrypoint::TokenDigest::of_token`'s identical blank check.
    let token = options.token.filter(|value| !value.trim().is_empty());

    // PR #124 round 5, F4: re-enforced here, beside the blank-token filter just above, rather
    // than trusting `main.rs::bounded_authentication_cache_secs` (the TOML reader) to be the
    // only path into this value — see `authentication_cache_ttl`'s own doc for why.
    let authentication_cache_ttl = authentication_cache_ttl(options.authentication_cache_secs)?;

    // Refuse to start rather than ever construct an `AppState` that would fall through to
    // `Principal::Open` by accident: `check_auth` only returns `Open` when `options.open` is
    // set, so an operator who forgot to configure a token/tokens file/hook — or who typo'd its
    // key name past `parse_config`'s strict readers — gets a startup error naming the remedy,
    // never a server that quietly serves the world unauthenticated (the fail-open finding this
    // check exists to close: a hand-edited `token = 12345` used to parse as "no token" and,
    // with nothing else configured, serve every request openly with no warning at all).
    let auth_configured =
        token.is_some() || !operator_tokens.is_empty() || options.authentication_hook.is_some();

    if !auth_configured && !options.open {
        // F2 of PR #124 round 2: an operator who passed `--token ""` (or `token = ""`) must be
        // told that specifically — "no --token/token" sends them looking for a flag they
        // already passed, the same misdiagnosis this fixed for the empty-`[operators]`-table
        // case below. "Blank" covers both an empty string and a whitespace-only one (PR #124
        // round 4, F1): round 3 widened `blank_token_supplied` to catch `--token "   "` but left
        // this clause hardcoded to "an empty ... (an empty string is not a credential)", so a
        // whitespace-only token was told it was literally empty — a wrong diagnosis pinned by a
        // passing test until this round. Reachable here only when the merge in
        // `main.rs::merge_static_token`
        // discarded a blank value from *and never found a real token in* either source: a real
        // token from the other source would have made `auth_configured` true above, and this
        // whole branch would never run.
        let token_clause = if options.blank_token_supplied {
            "a blank --token/token (an empty or whitespace-only string is not a credential)"
        } else {
            "no --token/token"
        };

        let tokens_clause = if tokens_supplied {
            "a --tokens/tokens file whose [operators] table defines no entries"
        } else {
            "no --tokens/tokens file"
        };

        return Err(format!(
            "No authentication is configured: {}, {}, and no authentication \
            hook. Refusing to start serving requests unauthenticated by accident — set one \
            of those, or pass --open (or `open = true` in the config file) to explicitly run \
            this server with no authentication at all.",
            token_clause, tokens_clause
        ));
    }

    // Held until `serve` returns, i.e. until the server stops.
    let mut _base_lock: Option<BaseServeLock> = None;
    let mode = match (options.root, options.warehouses) {
        (Some(root), None) => {
            // Anything there but a warehouse is refused before any lock is taken.
            // Its lease is kept until the handle holds its own, so no upgrade starts in between.
            let opened = open_root(&root)?;
            let root = opened.root.clone();

            // Hold the serve lock for the served root for the process lifetime: a second
            // server on the same root is refused here rather than silently breaking the CAS.
            let handle = WarehouseHandle::serving(root.clone(), true)
                .map_err(|refusal| refusal.message(&format!("\"{}\"", root.to_string_lossy())))?;
            drop(opened);
            ServeMode::Single(Arc::new(handle))
        }
        (None, Some(base)) => {
            let base = std::fs::canonicalize(&base)
                .map_err(|e| format!("Error while resolving \"{}\": {}", base, e))?;

            if !base.is_dir() {
                return Err(format!("\"{}\" is not a folder.", base.to_string_lossy()));
            }

            // One server per base folder, for the process lifetime: creation and the sweep below
            // assume no other server works in this folder.
            _base_lock = Some(BaseServeLock::try_acquire(&base)?);
            sweep_interrupted_creations(&base);

            ServeMode::Multi { base }
        }
        _ => return Err("Pass exactly one of --root and --warehouses.".to_string()),
    };

    // Blank (empty or whitespace-only) is treated as absent for both fields, applying the same
    // rule this PR unified everywhere else a string is checked for "is this configured at all"
    // (PR #124 round 4, F4). This is a config-completeness gate, not a credential comparison —
    // the secret is outbound MAC key material (`hook_utils::hook_request_headers`), never
    // compared against attacker-supplied input, so a whitespace-only one is merely weak, not
    // silently-absent the way a bearer credential is; a whitespace-only *url*, though, was worse
    // than weak: it made `auth_configured` (above) true, so the server believed authentication
    // was configured and started, only to 503 every request at `authenticate_via_hook` because
    // `reqwest` cannot build a client request against a blank/whitespace URL. Failing closed, so
    // not a hole — but a startup refusal here, naming the hook, beats a server that starts and
    // then always fails every request with no diagnostic at start time. Covers all four hook
    // endpoints (authentication, admission, events, resolution) via this one shared loop.
    //
    // The message below names the check that actually ran — "a blank URL or secret" — rather
    // than `parse_config::hook_of`'s own, superficially similar "only one of {name}_url and
    // {name}_secret" message: that one fires when a config-file operator sets only one of the
    // two keys at all, a different rule from this one, which fires when both keys are present
    // but one is blank (PR #124 round 5, F1). An operator who left `authentication_secret = ""`
    // behind from a template has both keys set and was, before this fix, told the hook "needs
    // both a URL and a secret" — sending them looking for a key they already have.
    for (name, hook) in [
        ("authentication", &options.authentication_hook),
        ("admission", &options.admission_hook),
        ("events", &options.events_hook),
        ("resolution", &options.resolution_hook),
    ] {
        if let Some(hook) = hook {
            if hook.url.trim().is_empty() || hook.secret.trim().is_empty() {
                return Err(format!(
                    "The {} hook has a blank URL or secret (an empty or whitespace-only \
                    string is not a hook credential): hook requests are signed, and an \
                    unsigned hook would be spoofable.",
                    name
                ));
            }

            // PR #124 round 5, F2: a non-blank but non-absolute URL (scheme omitted, e.g.
            // "provider.example/hooks/auth" — far likelier in practice than an all-whitespace
            // one) passes the check above yet produces exactly the failure this whole loop
            // exists to catch at startup rather than at request time: `post_hook` dials the URL
            // via `state.http.post(&hook.url)`, whose `IntoUrl` impl parses it with
            // `Url::parse` and fails the request (surfaced as a 503 on every call) when that
            // parse fails. Re-running the identical parser here means a startup refusal and a
            // request-time failure can never disagree about which URLs are dialable.
            if let Err(e) = reqwest::Url::parse(&hook.url) {
                return Err(format!(
                    "The {} hook's URL \"{}\" is not a valid absolute URL ({}): hook requests \
                    are sent to it directly, and a URL that cannot be parsed cannot be dialed.",
                    name, hook.url, e
                ));
            }
        }
    }

    let http = build_hook_client()?;

    let state = Arc::new(AppState {
        mode,
        token,
        operator_tokens,
        rebuild_after_lifts: options.rebuild_after_lifts,
        warehouses: Mutex::new(WarehouseRegistry::new(live_warehouse_cap(open_file_limit))),
        authentication_hook: options.authentication_hook,
        admission_hook: options.admission_hook,
        events_hook: options.events_hook,
        resolution_hook: options.resolution_hook,
        http,
        authentication_cache: Mutex::new(HashMap::new()),
        authentication_cache_ttl,
        open: options.open,
    });

    // A deletion owed when the warehouse was last served is carried out now, not at the first
    // ref write.
    if let ServeMode::Single(handle) = &state.mode {
        if handle.first_run_due.swap(false, std::sync::atomic::Ordering::SeqCst) {
            start_serving(&state, handle);
        }
    }

    // `auth_configured` was computed above, before the startup refusal, and is still valid here
    // (a `bool` copy) — used only for the startup-bind warning once the address is actually
    // bound. Reaching this point at all means `auth_configured || options.open` held.

    // The hash check gates correctness; the body cap gates disk-fill abuse (§9.4b R9/D7). It now
    // defaults **on** at the whole-object ceiling: after chunking, the largest legitimate object a
    // client ever PUTs is a `MAX_OBJECT_BYTES` recipe or tree (a blob is below the chunk threshold,
    // a chunk is at most `MAX_CHUNK_BYTES`), so this is a principled default value rather than the
    // old "unlimited unless an operator remembers to cap it". An operator may still raise it for a
    // grandfathered-giant read path or lower it, via `max_body_mb`.
    let app = router(state, DefaultBodyLimit::max(body_limit_bytes(options.max_body_mb)?));

    let listener = tokio::net::TcpListener::bind(&options.addr)
        .await
        .map_err(|e| format!("Error while binding \"{}\": {}", options.addr, e))?;

    let bound = listener.local_addr()
        .map_err(|e| format!("Error while reading the bound address: {}", e))?;

    if let Some(message) = startup_bind_warning(&bound, auth_configured) {
        tracing::warn!(%bound, "{}", message);
    }

    // The single startup line is machine-readable on purpose: tools (and the tests)
    // parse the port out of it, which is what makes `--addr 127.0.0.1:0` usable.
    println!("forklift-server listening on http://{}", bound);

    use std::io::Write;
    std::io::stdout().flush().ok();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| format!("The server failed: {}", e))
}

/// The method one [`RouteEntry`] answers.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Verb {
    Get,
    Put,
    Post,
    Delete,
}

impl Verb {
    fn filter(self) -> MethodFilter {
        match self {
            Verb::Get => MethodFilter::GET,
            Verb::Put => MethodFilter::PUT,
            Verb::Post => MethodFilter::POST,
            Verb::Delete => MethodFilter::DELETE,
        }
    }
}

/// Where a [`RouteEntry`]'s path is mounted: at the server's root, or under the protocol prefix,
/// which is [`SINGLE_PROTOCOL_PREFIX`] in single mode and [`MULTI_PROTOCOL_PREFIX`] in multi mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Mount {
    Root,
    Protocol,
}

/// The serving modes a [`RouteEntry`] is registered in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Modes {
    Both,
    Multi,
}

impl Modes {
    fn includes(self, multi: bool) -> bool {
        match self {
            Modes::Both => true,
            Modes::Multi => multi,
        }
    }
}

/// Whether a [`RouteEntry`] sits behind the authentication layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Access {
    Authenticated,
    /// Open to everyone: no layer runs, and a handler without `Extension<Principal>` serves any
    /// caller. The public routes are pinned by name in the tests.
    Public,
}

/// One method on one path the server answers. [`ROUTES`] holds every one of them.
struct RouteEntry {
    verb: Verb,
    path: &'static str,
    mount: Mount,
    modes: Modes,
    access: Access,
    /// Builds the entry's method router from the entry's own method, so an entry cannot answer a
    /// method it does not name.
    handler: fn(MethodFilter) -> MethodRouter<Arc<AppState>>,
}

const SINGLE_PROTOCOL_PREFIX: &str = "/v1";
const MULTI_PROTOCOL_PREFIX: &str = "/warehouses/{warehouse}/v1";

/// Every route the server answers, and the only place one is registered from: [`router`]
/// registers each entry and nothing else. `docs/format/REMOTE_PROTOCOL.md` carries the same list
/// as its route table, and a test holds the two to each other, holds the crate's source to
/// registering only through this list, and asks the built router which methods each path
/// answers.
const ROUTES: &[RouteEntry] = &[
    RouteEntry { verb: Verb::Get, path: "/warehouse", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, get_warehouse) },
    RouteEntry { verb: Verb::Post, path: "/objects/missing", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_missing) },
    RouteEntry { verb: Verb::Post, path: "/objects/upload-targets", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_upload_targets) },
    RouteEntry { verb: Verb::Post, path: "/objects/batch", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_objects_batch) },
    RouteEntry { verb: Verb::Get, path: "/parcels/{parcel}/subtree/{*path}", mount: Mount::Protocol,
        modes: Modes::Both, access: Access::Authenticated, handler: |method| on(method, get_subtree) },
    RouteEntry { verb: Verb::Get, path: "/objects/{hash}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, get_object) },
    RouteEntry { verb: Verb::Put, path: "/objects/{hash}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, put_object) },
    RouteEntry { verb: Verb::Put, path: "/withheld/{hash}/recipe", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, put_withheld_recipe) },
    RouteEntry { verb: Verb::Get, path: "/signatures/{hash}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, get_signature) },
    RouteEntry { verb: Verb::Put, path: "/signatures/{hash}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, put_signature) },
    RouteEntry { verb: Verb::Put, path: "/trust", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, put_trust) },
    RouteEntry { verb: Verb::Post, path: "/pallets/{*name}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_ref_update) },
    RouteEntry { verb: Verb::Delete, path: "/pallets/{*name}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, delete_ref) },
    RouteEntry { verb: Verb::Post, path: "/holds/{*name}", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_holds) },
    RouteEntry { verb: Verb::Post, path: "/resolve", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, post_resolve) },
    RouteEntry { verb: Verb::Get, path: "/bundles/latest", mount: Mount::Protocol, modes: Modes::Both,
        access: Access::Authenticated, handler: |method| on(method, get_bundle) },
    RouteEntry { verb: Verb::Put, path: "/warehouses/{warehouse}", mount: Mount::Root, modes: Modes::Multi,
        access: Access::Authenticated, handler: |method| on(method, put_warehouse) },
    // Liveness only: unauthenticated and warehouse-free on purpose.
    RouteEntry { verb: Verb::Get, path: "/healthz", mount: Mount::Root, modes: Modes::Both,
        access: Access::Public, handler: |method| on(method, || async { "ok" }) },
];

/// Register every [`ROUTES`] entry for one mount and serving mode. An authenticated entry carries
/// the authentication layer on its own method router, so the layer runs before any extractor: a
/// refused caller's request body is never buffered. A handler that needs to know who is calling
/// takes the `Extension<Principal>` the layer leaves, and refuses without it. The tests pin this
/// shape: one `route(entry.path, answer)` inside a `for` loop over `ROUTES.iter().filter(…)`.
fn register(state: &Arc<AppState>, mount: Mount, multi: bool) -> Router<Arc<AppState>> {
    let mut registered = Router::new();

    for entry in ROUTES.iter().filter(|entry| entry.mount == mount && entry.modes.includes(multi)) {
        let mut answer = (entry.handler)(entry.verb.filter());

        if entry.access == Access::Authenticated {
            answer = answer.route_layer(middleware::from_fn_with_state(Arc::clone(state), authenticate));
        }

        // axum merges a second method router on a path it already holds into the first.
        registered = registered.route(entry.path, answer);
    }

    registered
}

/// The server's router: [`ROUTES`], registered by [`register`], with the protocol mounted at its
/// serving mode's prefix.
fn router(state: Arc<AppState>, body_limit: DefaultBodyLimit) -> Router {
    let multi = matches!(state.mode, ServeMode::Multi { .. });
    let protocol = register(&state, Mount::Protocol, multi);
    let root = register(&state, Mount::Root, multi);

    let app = if multi {
        root.nest(MULTI_PROTOCOL_PREFIX, protocol)
    } else {
        root.nest(SINGLE_PROTOCOL_PREFIX, protocol)
    };

    app.layer(body_limit)
        // Objects travel uncompressed on the wire by format (the hash covers the
        // uncompressed form, §4.4), so transport compression is nearly free wins;
        // bundle/batch responses are already zstd streams and mark themselves
        // `content-encoding: identity`, which the layer respects.
        .layer(tower_http::compression::CompressionLayer::new())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state)
}

/// The authentication layer: refuse the request from its headers alone, or pass it on with its
/// [`Principal`] in the request extensions. See [`router`].
async fn authenticate(State(state): State<Arc<AppState>>, mut request: Request, next: Next) -> Response {
    match check_auth(&state, request.headers()).await {
        Ok(principal) => {
            request.extensions_mut().insert(principal);
            next.run(request).await
        }
        Err(error) => {
            let body = request.into_body();

            if !body.is_end_stream() {
                tokio::spawn(discard_refused_body(
                    body, REFUSED_BODY_DISCARD_TIME, REFUSED_BODY_DISCARD_BYTES
                ));
            }

            error_response(error)
        }
    }
}

/// How long, and how many bytes, one refused request's body is read and thrown away.
const REFUSED_BODY_DISCARD_TIME: std::time::Duration = std::time::Duration::from_secs(5);
const REFUSED_BODY_DISCARD_BYTES: usize = 128 * 1024 * 1024;

/// Read a refused request's body and keep none of it. Closing the connection on a client still
/// writing would break its pipe before it reads the refusal, and it would report a network failure
/// instead of a bad token. Past either bound the connection closes and that client does see the
/// failure. The bounds are per request: a body finished inside them leaves the connection open for
/// the next request, as any refusal does, so limiting a peer's request rate stays the proxy's job.
async fn discard_refused_body(body: Body, time: std::time::Duration, bytes: usize) {
    use tokio_stream::StreamExt;

    let mut frames = body.into_data_stream();
    let mut discarded = 0;

    let _ = tokio::time::timeout(time, async {
        while let Some(Ok(chunk)) = frames.next().await {
            discarded += chunk.len();

            if discarded >= bytes {
                break;
            }
        }
    }).await;
}

/// The startup-log risk to warn about for a bound address, or `None` when there is nothing to
/// say. A loopback bind is never warned about regardless of auth (that is the ordinary
/// single-team self-host case this head is designed for); a non-loopback bind always is, on
/// either side of the auth question: with a token configured, requests — including the bearer
/// itself — still travel as plaintext HTTP unless the operator puts a TLS-terminating proxy in
/// front of this process; with none configured — which, since `serve`'s startup check above
/// refuses to bind at all otherwise, can only mean the operator passed `--open`/`open = true` —
/// every request is served as `Principal::Open` to whoever can reach the address at all.
fn startup_bind_warning(bound: &std::net::SocketAddr, auth_configured: bool) -> Option<&'static str> {
    if bound.ip().is_loopback() {
        return None;
    }

    if auth_configured {
        Some(
            "serving a non-loopback address: the bearer token still travels as plaintext HTTP \
            unless a TLS-terminating proxy sits in front of this process"
        )
    } else {
        Some(
            "serving a non-loopback address with no token, operator tokens, or authentication \
            hook configured, and `open` explicitly set: every request is served as \
            Principal::Open"
        )
    }
}

/// Resolve when the process should shut down (SIGINT/ctrl-c, or SIGTERM on Unix), so
/// in-flight transfers drain instead of being severed.
async fn shutdown_signal() {
    let interrupt = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => { signal.recv().await; }
            Err(_) => std::future::pending().await,
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => {}
        _ = terminate => {}
    }
}

/// Parse the operator-token file: a TOML `[operators]` table of `"<token>" =
/// "<office identifier>"` entries. Tokens are transport secrets and live only here,
/// server-side — never in the tracked office metadata.
fn parse_operator_tokens(path: &str) -> Result<HashMap<String, String>, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading the token file \"{}\": {}", path, e))?;

    let doc: toml_edit::DocumentMut = content.parse()
        .map_err(|e| format!("The token file \"{}\" is not valid TOML: {}", path, e))?;

    let Some(operators) = doc.get("operators").and_then(|item| item.as_table()) else {
        return Err(format!(
            "The token file \"{}\" has no [operators] table (\"<token>\" = \"<identifier>\").",
            path
        ));
    };

    let mut tokens = HashMap::new();

    for (token, identifier) in operators.iter() {
        let identifier = identifier.as_str().ok_or(format!(
            "The token file \"{}\" maps a token to a non-string value.", path
        ))?;

        // A blank (empty or whitespace-only) string is not a credential (the same hole as
        // `--token ""`/`--token "   "`, see `serve`): a request carrying the literal header
        // `Authorization: Bearer ` (no credential after the scheme) would otherwise match an
        // empty-key entry via `state.operator_tokens.get("")` and authenticate as `identifier`
        // with no credential presented at all — and a whitespace-only key has the identical
        // problem one level removed (PR #124 round 3, F1): a client on a transport that trims
        // trailing whitespace off header values could never present it (the entry is permanently
        // dead, silently locking "identifier" out), while on one that does not it is a
        // trivially-guessable credential. This is a presence check only, not a normalization: a
        // real, non-blank token key is still matched byte-for-byte, verbatim, via
        // `state.operator_tokens.get(token)` — nothing here trims it.
        if token.trim().is_empty() {
            return Err(format!(
                "The token file \"{}\" maps an empty or whitespace-only token to \"{}\": that is \
                not a credential — remove that entry.",
                path, identifier
            ));
        }

        // F4 of PR #124 round 2: an identity is not a credential — nothing compares it
        // byte-exactly against a configured value the way a token is compared, it is *recorded*
        // (into `Principal::Operator`, then admission/event payloads, `post_resolve`'s `caller`)
        // as a legitimate-looking actor. Blank is rejected on both sides now (PR #124 round 3, F1
        // unified the rule the token check above used to lack); what still differs between them
        // is normalization, not presence — a token's real, non-blank value is compared byte-exact
        // and never trimmed (above), and an identity's real, non-blank value is *also* never
        // trimmed: `authority_utils::office_user_of` matches it by exact equality against the
        // tracked office roster, so trimming a legitimately whitespace-padded identifier here
        // would be the reason it can never match again (see
        // `a_token_file_identifier_with_incidental_whitespace_is_stored_untrimmed` below).
        // Deliberately does not echo `token` into the message — unlike the blank-token case
        // above, `token` here is a real, live credential, and printing it would leak it into the
        // server's own startup-failure output and logs.
        if identifier.trim().is_empty() {
            return Err(format!(
                "The token file \"{}\" maps a token to a blank identifier: an identity must not \
                be empty or whitespace-only — remove that entry.",
                path
            ));
        }

        tokens.insert(token.to_string(), identifier.to_string());
    }

    Ok(tokens)
}

/// Build the JSON error response of a handler error. A `503` says when to try again: each one is
/// a refusal for now (a warehouse drained for the redaction deleter, under maintenance, or not
/// yet servable), never for good. A [`damage_refusal`] is the exception: no retry mends it, so it
/// says no time, and a client takes it as an answer.
fn error_response(error: HandlerError) -> Response {
    let status = error.0;
    let damaged = is_damage(&error.1);
    let mut response = (status, Json(error_body(error.1))).into_response();

    if status == StatusCode::SERVICE_UNAVAILABLE && !damaged {
        response.headers_mut().insert(axum::http::header::RETRY_AFTER, HeaderValue::from(RETRY_AFTER_SECONDS));
    }

    response
}

/// Build the JSON error body, threading a classified refusal's stable code (§7.4) onto the wire.
/// A `forklift-core` refusal reaches this head as a sentinel-framed message; decoding it here sets
/// the additive `code`/`next_step` fields machine-readably and puts the de-framed human text in
/// `error` — so a server-side refusal classifies on the client with the same code and exit code as
/// a local one, and the raw frame never leaks into the body. A plain message passes through with no
/// code (an older client simply ignores the absent fields).
///
/// Trust-boundary note: `deframe` decodes *any* handler message that happens to start with the
/// sentinel prefix, regardless of where that `String` originated. That is safe today only because
/// every framed message is refusal text minted by this codebase's own sanitized constructors
/// (`CoreError::refusal`/its bridge shim) and returned solely to the requester whose own action
/// triggered it — never third-party or attacker-supplied content. Keep it that way: a future error
/// path that let outside-influenced text reach byte zero of a handler's message could spoof a
/// `code`/`next_step` pair here.
fn error_body(message: String) -> ErrorResponse {
    match forklift_core::error::deframe(&message) {
        Some((code, human, next_step)) => ErrorResponse {
            error: human.to_string(),
            code: Some(code.to_string()),
            next_step: Some(next_step.to_string()),
        },
        None => ErrorResponse { error: message, code: None, next_step: None },
    }
}

/// The refusal for a ref update whose old head is not the pallet's head: classified, so the client
/// can branch to `lower`.
fn pallet_moved(current: Option<&str>, expected: Option<&str>) -> HandlerError {
    (StatusCode::CONFLICT, pallet_utils::pallet_moved_refusal(current, expected).into())
}

/// Whether user pallet `bare` is deleted at `head`: its tombstone holds that head. What a retried
/// delete whose first try landed finds.
fn already_deleted(bare: &str, head: &str) -> Result<bool, HandlerError> {
    let tombstones = forklift_core::util::tombstone_utils::read_tombstones().map_err(damage_refusal)?;

    Ok(tombstones.named.get(bare).map(String::as_str) == Some(head))
}

/// [`pallet_moved`], unless the pallet expected at a head is gone because it was deleted here: then
/// a refusal naming the deletion, since lowering cannot resolve it (a lift after a lower would be
/// a creation, which the tombstone binds).
fn pallet_moved_or_deleted(namespace: pallet_utils::PalletNamespace,
                           bare: &str,
                           current: Option<&str>,
                           expected: Option<&str>) -> HandlerError {
    if current.is_some() || expected.is_none() || namespace != pallet_utils::PalletNamespace::User {
        return pallet_moved(current, expected);
    }

    match forklift_core::util::tombstone_utils::read_tombstones() {
        Ok(tombstones) => match tombstones.named.get(bare) {
            Some(head) => (StatusCode::CONFLICT, pallet_utils::pallet_deleted_refusal(bare, head, true).into()),
            None => pallet_moved(current, expected),
        },
        Err(error) => damage_refusal(error),
    }
}

/// The refusal for a creation of a pallet deleted on this head whose tombstone still stands: a `409`
/// unless the writer acknowledged the tombstone and the new head descends from its head.
fn pallet_deleted(pallet: &str, head: &str, acknowledged: bool) -> HandlerError {
    (StatusCode::CONFLICT, pallet_utils::pallet_recreation_refusal(pallet, head, acknowledged, true).into())
}

/// The refusal for a ref update whose audit consumed an office head that moved before the write:
/// the pallet did not move, but the office state the audit trusted did. The same wording as the
/// Lambda head's.
fn office_moved(current: Option<&str>) -> HandlerError {
    (StatusCode::CONFLICT, pallet_utils::remote_ahead_refusal(format!(
        "The office pallet moved during the audit: its head is now {}. Lower and retry.",
        current.unwrap_or("unborn")
    )).into())
}

/// The refusal for a ref update whose audit consumed a trust anchor that changed before the
/// write (established, or replaced by a re-genesis). The same wording as the Lambda head's.
fn anchor_moved() -> HandlerError {
    (StatusCode::CONFLICT, "The trust anchor changed during the audit; re-read the warehouse and retry.".to_string())
}

/// Resolve a root the command line names to an absolute path, for entering it as a storage-root
/// scope. It judges nothing: [`open_root`] does, and prepare asks core's own check.
pub(crate) fn canonical_root(root: &str) -> Result<PathBuf, String> {
    std::fs::canonicalize(root).map_err(|e| format!("Error while resolving \"{}\": {}", root, e))
}

/// Open the warehouse at `root`, the one way `serve --root`, `bundle` and `gc` take their root:
/// resolved to an absolute path, then judged by core's gate before anything in it is read, written
/// or locked, so anything there but a warehouse is refused with a next step for this server. The
/// caller holds the result for as long as it works there.
pub(crate) fn open_root(root: &str) -> Result<warehouse_utils::OpenedWarehouse, String> {
    let resolved = canonical_root(root)?;
    Ok(warehouse_utils::open_warehouse_root(&resolved, warehouse_utils::Head::Server)?)
}

/// Whether `root` holds a warehouse. A bay's working directory holds a `.forklift` folder too, so
/// the shared classifier decides, never the folder's mere presence: a bay is never served,
/// adopted or reported as an existing warehouse. A `.forklift` that cannot be classified (a
/// broken redirect, an unreadable entry) is not a warehouse either: it is never served, and a
/// creation finds the name taken.
fn holds_warehouse(root: &std::path::Path) -> bool {
    matches!(bay_utils::classify_forklift(&root.join(FOLDER_NAME_FORKLIFT_ROOT)), Ok(bay_utils::ForkliftKind::Warehouse))
}

/// Whether nothing stands at `root`, or only an empty folder: what a creation would take.
///
/// # Returns
/// * `Ok(bool)`              - Whether a creation would take `root`.
/// * `Err(std::io::Error)`   - `root` could not be checked.
fn is_missing_or_empty_folder(root: &std::path::Path) -> std::io::Result<bool> {
    match std::fs::symlink_metadata(root) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
        Ok(meta) if meta.is_dir() => Ok(std::fs::read_dir(root)?.next().is_none()),
        Ok(_) => Ok(false),
    }
}

/// Map a trust install that did not land to the answer a ref update gives for the same condition:
/// a `409` when the office or the anchor it was judged against moved, or when a pallet head here
/// stands outside the anchor's boundary (it wrote nothing), and a 500 when a write failed.
fn trust_install_refusal(error: RefUpdateError) -> HandlerError {
    match error {
        RefUpdateError::OfficeMoved { found } => office_moved(found.as_deref()),
        RefUpdateError::AnchorMoved => anchor_moved(),
        // The boundary is the client's, fixed when it established trust: a retry sends the same
        // one, so this names what is outside it and promises no way out.
        RefUpdateError::PalletsMoved { pallet, found } => (StatusCode::CONFLICT, format!(
            "Pallet \"{}\" here is at {}, which the trust anchor's boundary does not hold: \
            installing the anchor would leave that history outside trust. Nothing was written.",
            pallet.to_wire(), found
        )),
        moved @ (RefUpdateError::Moved(_)
            | RefUpdateError::Incomplete { .. }
            | RefUpdateError::WitnessMoved { .. }
            | RefUpdateError::Deleted { .. }
            | RefUpdateError::Backward { .. }) => {
            (StatusCode::CONFLICT, moved.to_string())
        }
        // Refused however often it is sent, as a ref update naming redacted content is.
        refused @ RefUpdateError::Redacted { .. } => unprocessable(refused.into()),
        RefUpdateError::Failed(message) => damage_refusal(message),
    }
}

/// The `409` for a trust anchor that would replace a different one without being its re-genesis.
fn trust_one_way_door() -> HandlerError {
    (StatusCode::CONFLICT, "This warehouse already has a different trust anchor; trust is a one-way \
        door and cannot be replaced.".to_string())
}

/// Whether `PUT /v1/trust` wrote `requested`, from what its install returned. An install found
/// already in place, or refused because the office or the anchor moved while the anchor on disk is
/// now the requested one, was an identical request's (or a bay's) doing: `Ok(false)`, the `200`
/// with no event a request arriving after it gets. One refused while the anchor on disk is now one
/// `requested` is no re-genesis of gets the answer such a later request gets, the one-way door.
/// Any other refusal is the request's own answer, as is a refusal whose anchor cannot be read back.
fn trust_install_outcome(installed: Result<office_utils::TrustInstalled, RefUpdateError>,
                         requested: &TrustAnchorDto) -> Result<bool, HandlerError> {
    match installed {
        Ok(office_utils::TrustInstalled::Written) => Ok(true),
        Ok(office_utils::TrustInstalled::AlreadyInPlace) => Ok(false),
        Err(error @ (RefUpdateError::OfficeMoved { .. } | RefUpdateError::AnchorMoved)) => {
            match office_utils::read_trust_anchor().map(|held| held.as_ref().map(TrustAnchorDto::from)) {
                Ok(Some(held)) if held == *requested => Ok(false),
                Ok(Some(held)) if requested.prior_genesis.as_deref() != Some(held.genesis.as_str()) => {
                    Err(trust_one_way_door())
                }
                _ => Err(trust_install_refusal(error)),
            }
        }
        Err(error) => Err(trust_install_refusal(error)),
    }
}

/// Where each redaction on this warehouse's `@redactions` stands here, for the handshake. An applied
/// one is deleted once its object is gone from the store and the deletion owes none of its chunks
/// (a deletion cut short after its recipe went leaves them, for the next one to delete), and its
/// deletion is owed while a copy is still there: the deleter has yet to run, could not drain the
/// warehouse, or keeps a copy a bay here still holds. Nothing is walked: what holds a pending one
/// is the scan's business after each ref write, not every handshake's. A cell this build cannot
/// read is not reported.
fn served_redactions() -> Result<Vec<ServedRedaction>, HandlerError> {
    let applied = read_applied()?;
    // Deleted is claimed only on seeing the object and its owed chunks absent: a journal or a store
    // that cannot be read reports the deletion owed, as a handshake must not fail over one object.
    let owing_chunks = redaction_deleter::recipes_owing_chunks();

    redaction_utils::read_redactions().map_err(internal)?.redactions.iter().map(|attributed| {
        let hash = attributed.redaction.hash.clone();
        let state = match applied.contains(&hash) {
            false => ServedRedactionState::Pending,
            true => match (file_utils::does_object_exist(&hash), &owing_chunks) {
                (Ok(false), Ok(owing)) if !owing.contains(&hash) => ServedRedactionState::Deleted,
                _ => ServedRedactionState::DeletionOwed,
            },
        };

        Ok(ServedRedaction { hash, state })
    }).collect()
}

/// What this head withholds, for the handshake: each hash with the host's statement, its deletion
/// owed while the store still holds a copy, or while the deletion owes a chunk of it (as for an
/// applied redaction, see [`served_redactions`]). A store or a journal that cannot be read reports
/// the deletion owed, as a handshake must not fail over one object.
fn served_withheld() -> Result<Vec<ServedWithholding>, HandlerError> {
    let owing_chunks = redaction_deleter::recipes_owing_chunks();

    let withheld = read_withheld()?;

    Ok(withheld.entries().map(|(hash, statement)| ServedWithholding {
        hash: hash.clone(),
        statement: statement.clone(),
        state: match (file_utils::does_object_exist(hash), &owing_chunks) {
            (Ok(false), Ok(owing)) if !owing.contains(hash) => ServedWithholdingState::Deleted,
            _ => ServedWithholdingState::DeletionOwed,
        },
        chunks: withheld.digest(hash).and_then(|digest| digest.known()).map(str::to_string),
    }).collect())
}

/// The redactions this warehouse applied, read for a request that serves or takes objects by hash.
fn read_applied() -> Result<AppliedSet, HandlerError> {
    applied_redactions::read_applied().map_err(internal)
}

/// What this head withholds ([`withholding`]), read for a request that serves or takes objects by
/// hash.
fn read_withheld() -> Result<WithheldSet, HandlerError> {
    withholding::read_withheld().map_err(internal)
}

/// Refuse a request naming `hash` with a `410`, when a redaction this warehouse applied names it,
/// or with a `451`, when its host withholds it: its bytes are not served or taken, whether or not
/// they are deleted yet. A redaction wins, since it is for good.
fn refuse_removed(applied: &AppliedSet, withheld: &WithheldSet, hash: &str) -> Result<(), HandlerError> {
    if applied.contains(hash) {
        return Err((StatusCode::GONE, applied_redactions::served_refusal(hash)));
    }

    match withheld.statement(hash) {
        Some(statement) => Err((StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS, withholding::served_refusal(hash, statement))),
        None => Ok(()),
    }
}

/// Of `hashes`, the chunks `warehouse` serves as absent: those only removed recipes name (an
/// applied redaction's or a withheld one's), while their deletion is owed
/// ([`redaction_deleter::withheld_chunks`]). They are not refused (neither a ban nor a takedown
/// extends to a chunk), so they are answered as the deletion will leave them, absent. Run in a
/// request's blocking closure, beside its read of `removed`, the applied redactions and withheld
/// hashes ([`withholding::removed_from`]).
///
/// The candidates are read again whenever the removed set differs from theirs, which is cheap,
/// and not at all while nothing is removed: then no deletion ever ran, and no journal was written.
/// Only a request naming a candidate needs the live set, walked once per ref write here (a walk
/// a ref write overlapped is never kept) and at most every [`CHUNK_GATE_TTL`], one walk at a time.
fn withheld_among(warehouse: &WarehouseHandle,
                  removed: &AppliedSet,
                  hashes: &[String]) -> Result<std::collections::HashSet<String>, HandlerError> {
    use std::sync::atomic::Ordering;

    if removed.is_empty() {
        return Ok(std::collections::HashSet::new());
    }

    // The withheld chunks among `hashes` when the gate knows them now, or else the candidates
    // for this removed set, which a walk is to judge.
    type Answer = Result<std::collections::HashSet<String>, std::collections::HashSet<String>>;
    let answer = |gate: &mut ChunkGate| -> Result<Answer, HandlerError> {
        if gate.basis.as_ref() != Some(removed) {
            gate.candidates = redaction_deleter::withheld_candidates(removed).map_err(internal)?;
            gate.basis = Some(removed.clone());
            gate.withheld = None;
        }

        let asked: Vec<&String> = hashes.iter().filter(|hash| gate.candidates.contains(*hash)).collect();
        if asked.is_empty() {
            return Ok(Ok(std::collections::HashSet::new()));
        }

        match &gate.withheld {
            Some((started, at, withheld))
                if *started == warehouse.ref_writes_started.load(Ordering::SeqCst) && at.elapsed() < CHUNK_GATE_TTL => {
                Ok(Ok(asked.into_iter().filter(|hash| withheld.contains(*hash)).cloned().collect()))
            }
            _ => Ok(Err(gate.candidates.clone())),
        }
    };

    if let Ok(withheld) = answer(&mut lock_gate(warehouse))? {
        return Ok(withheld);
    }

    let _walk = warehouse.chunk_gate_walk.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    // Another request may have walked while this one waited. The candidates come from the same
    // look, under the same lock, that matched them to this removed set.
    let candidates = match answer(&mut lock_gate(warehouse))? {
        Ok(withheld) => return Ok(withheld),
        Err(candidates) => candidates,
    };

    // Started read before finished: equal, no write was in flight once both were read.
    let started = warehouse.ref_writes_started.load(Ordering::SeqCst);
    let settled = started == warehouse.ref_writes_finished.load(Ordering::SeqCst);
    let at = std::time::Instant::now();

    let withheld = redaction_deleter::withheld_among_candidates(&candidates, removed).map_err(internal)?;
    let among = hashes.iter().filter(|hash| withheld.contains(*hash)).cloned().collect();

    let mut gate = lock_gate(warehouse);
    if settled && gate.basis.as_ref() == Some(removed) {
        gate.withheld = Some((started, at, withheld));
    }

    Ok(among)
}

/// The chunk gate of `warehouse`, locked.
fn lock_gate(warehouse: &WarehouseHandle) -> std::sync::MutexGuard<'_, ChunkGate> {
    warehouse.chunk_gate.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Counts a ref write here for the chunk gate ([`withheld_among`]): started when it is made, and
/// finished when the blocking closure making it drops this, landed or not, even if the request is
/// gone. Every handler that writes a pallet head or the office takes one: a ref update, a pallet
/// deletion and a trust install.
struct RefWriteCounted(Arc<WarehouseHandle>);

impl RefWriteCounted {
    fn start(warehouse: &Arc<WarehouseHandle>) -> Self {
        warehouse.ref_writes_started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        RefWriteCounted(Arc::clone(warehouse))
    }
}

impl Drop for RefWriteCounted {
    fn drop(&mut self) {
        self.0.ref_writes_finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// Map an internal storage error to a 500.
fn internal(message: String) -> HandlerError {
    (StatusCode::INTERNAL_SERVER_ERROR, message)
}

/// The refusal codes of a host record that is lost or damaged, which only the server's operator
/// repairs.
const DAMAGE_CODES: &[forklift_core::error::RefusalCode] = &[forklift_core::error::RefusalCode::TombstonesDamaged];

/// Whether `message` is a refusal of one of the [`DAMAGE_CODES`].
fn is_damage(message: &str) -> bool {
    forklift_core::error::deframe(message).is_some_and(|(code, _, _)| DAMAGE_CODES.iter().any(|damage| damage.as_str() == code))
}

/// The operator's remedy for a damaged host record, as a client is told it: the server's own
/// directories are not named over the wire, so the warehouse is a placeholder.
fn operator_remedy(code: forklift_core::error::RefusalCode) -> &'static str {
    match code {
        forklift_core::error::RefusalCode::TombstonesDamaged => "forklift-server tombstone repair --root <warehouse>",
        _ => unreachable!("not a damage code"),
    }
}

/// Map an error from a read of a host record: a `503` carrying its code and the operator's remedy
/// ([`operator_remedy`]) when the record is damaged ([`is_damage`]), with no `Retry-After`
/// ([`error_response`]), since only the operator's repair mends it; any other error is
/// [`internal`]. The damage itself is logged, with the local next step.
fn damage_refusal(message: String) -> HandlerError {
    let damage = forklift_core::error::deframe(&message).and_then(|(code, human, _)| {
        DAMAGE_CODES.iter().find(|damage| damage.as_str() == code).map(|code| (*code, human.to_string()))
    });

    match damage {
        Some((code, human)) => {
            tracing::error!(%message, "a host record is damaged");
            (StatusCode::SERVICE_UNAVAILABLE, forklift_core::error::CoreError::refusal(code, human, operator_remedy(code)).into())
        }
        None => internal(message),
    }
}

/// Map a verification failure to a 422.
fn unprocessable(message: String) -> HandlerError {
    (StatusCode::UNPROCESSABLE_ENTITY, message)
}

/// Map a lifted office chain that did not verify: a 403 when a parcel the lift brings
/// oversteps its signer's privileges, a 422 otherwise (see
/// `audit_utils::classify_office_lift_refusal`).
fn office_lift_refusal(error: audit_utils::OfficeChainError, held_head: Option<&str>) -> HandlerError {
    match audit_utils::classify_office_lift_refusal(error, held_head) {
        audit_utils::OfficeLiftRefusal::Overstep(message) => (StatusCode::FORBIDDEN, message),
        audit_utils::OfficeLiftRefusal::Unprocessable(message) => unprocessable(message),
    }
}

/// Compare two bearer tokens without leaking which byte differs first: a naive `==` on `str`
/// short-circuits at the first mismatch, which is exactly the timing side channel a bearer
/// check must not have. The slice lengths are still compared up front (`subtle`'s own
/// short-circuit) — a token's length is not the secret, only its bytes are.
fn tokens_match(provided: &str, expected: &str) -> bool {
    bool::from(provided.as_bytes().ct_eq(expected.as_bytes()))
}

/// Strip the `Bearer` auth-scheme token case-insensitively (RFC 7235 §2.1: `auth-scheme` is a
/// `token`, and the scheme name itself is case-insensitive — `bearer`, `Bearer`, `BEARER` all
/// name the same scheme), leaving the credentials (the token after the scheme and its one
/// separating space) untouched: still compared case-sensitively, in constant time, by
/// [`tokens_match`]. Only the 7-byte scheme prefix is matched loosely; a non-`Bearer` scheme
/// (`Basic …`) still fails to match at all.
fn strip_bearer_prefix(value: &str) -> Option<&str> {
    const SCHEME_LEN: usize = "bearer ".len();

    let prefix = value.get(..SCHEME_LEN)?;

    if prefix.eq_ignore_ascii_case("bearer ") {
        value.get(SCHEME_LEN..)
    } else {
        None
    }
}

/// Authenticate a request: who is this? Reads are open to every authenticated
/// principal; writes are further authorized against the office (see
/// `require_uploader` and the ref-update handler). Tokens unknown locally are asked
/// of the authentication hook, when one is configured (fail closed: a hook failure
/// refuses the request, it never waves it through).
///
/// `Principal::Open` is returned in exactly one case: no token, operator tokens, or
/// authentication hook is configured, AND `state.open` is explicitly `true`. It is never
/// inferred from the absence of auth config alone — `serve`'s startup check refuses to run a
/// server that would reach that state with `open` unset, so in practice this function's
/// `Open` branch and its `unauthorized` fallback are reachable only via the deliberately open
/// path or a test harness that builds an `AppState` directly. This is the fix for the fail-open
/// finding where a present-but-wrong-typed config value (e.g. an unquoted `token = 12345`) used
/// to parse as "no token", collapse into "nothing is configured", and serve every request as
/// `Principal::Open` with no warning at all.
async fn check_auth(state: &AppState, headers: &HeaderMap) -> Result<Principal, HandlerError> {
    let unauthorized = || (
        StatusCode::UNAUTHORIZED,
        "A valid bearer token is required.".to_string()
    );

    if state.token.is_none()
        && state.operator_tokens.is_empty()
        && state.authentication_hook.is_none() {
        return if state.open { Ok(Principal::Open) } else { Err(unauthorized()) };
    }

    // A blank (empty or whitespace-only) credential is not a credential: `Authorization: Bearer `
    // (no bytes after the scheme, so `strip_bearer_prefix` yields `Some("")`) must never match
    // anything — not the static token, not an operator-tokens entry, not the hook — and neither
    // must an all-whitespace one (PR #124 round 3, F1): this checkpoint cannot assume any given
    // transport trims trailing whitespace off header values before this code ever sees it (axum
    // also serves h2c, where HPACK does not), so a client presenting `Bearer    ` must be refused
    // the same way as a client presenting `Bearer `. `serve`'s options filter
    // (`options.token.filter(|v| !v.trim().is_empty())`) and `parse_operator_tokens`'s blank-key
    // rejection already keep a blank string out of `state.token`/`state.operator_tokens` in the
    // first place; this is the belt-and-braces guard at the one checkpoint every
    // principal-granting comparison goes through, so a future change that reintroduces a blank
    // credential upstream (a new construction path for `AppState`, a relaxed parser) still cannot
    // authenticate a blank-credential request. This filters presence only, not the value: a real,
    // non-blank credential is still compared byte-for-byte, verbatim, by `tokens_match` — nothing
    // here trims it.
    let provided = headers.get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(strip_bearer_prefix)
        .filter(|token| !token.trim().is_empty());

    match provided {
        Some(token) if state.token.as_deref().is_some_and(|expected| tokens_match(token, expected)) =>
            Ok(Principal::Static),
        Some(token) => {
            if let Some(identifier) = state.operator_tokens.get(token) {
                return Ok(Principal::Operator(identifier.clone()));
            }

            match &state.authentication_hook {
                Some(hook) => authenticate_via_hook(state, hook, token).await,
                None => Err(unauthorized()),
            }
        }
        None => Err(unauthorized()),
    }
}

/// Ask the authentication hook who a token is; positive answers are cached for the
/// configured TTL. Failure policy: fail closed — a hook the server cannot reach
/// refuses the request (503), it never becomes an open door.
async fn authenticate_via_hook(state: &AppState,
                               hook: &HookEndpoint,
                               token: &str) -> Result<Principal, HandlerError> {
    if let Ok(cache) = state.authentication_cache.lock() {
        if let Some((identifier, at)) = cache.get(token) {
            if at.elapsed() < state.authentication_cache_ttl {
                return Ok(Principal::Operator(identifier.clone()));
            }
        }
    }

    let request = AuthenticationHookRequest { token: token.to_string() };

    let response = post_hook(state, hook, "authentication", &request).await
        .map_err(|reason| {
            tracing::warn!(reason, "the authentication hook is unreachable; failing closed");

            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The authentication service is unavailable; try again later.".to_string()
            )
        })?;

    if !response.status().is_success() {
        return Err((
            StatusCode::UNAUTHORIZED,
            "A valid bearer token is required.".to_string()
        ));
    }

    let answer: AuthenticationHookResponse = body_utils::read_json_capped(
        response, body_utils::SMALL_BODY_CAP
    ).await.map_err(|e| {
        tracing::warn!(error = ?e, "the authentication hook's answer is unreadable, oversized or malformed");

        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The authentication service is unavailable; try again later.".to_string()
        )
    })?;

    // F4 of PR #124 round 2: a well-formed but blank identifier is exactly as unusable as
    // malformed JSON — fail closed the same way, before the cache insert below, so a blank
    // answer is never cached and repeatedly accepted for the TTL. Unlike the token this hook is
    // answering *for*, nothing later compares an identifier byte-exactly against a configured
    // value; it is recorded (`Principal::Operator`, then admission/event payloads,
    // `post_resolve`'s `caller`) as a legitimate-looking actor, so whitespace-only (`"   "`) is
    // rejected here too, not just the empty string.
    if answer.identifier.trim().is_empty() {
        tracing::warn!("the authentication hook answered a blank identifier; failing closed");

        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The authentication service is unavailable; try again later.".to_string()
        ));
    }

    if let Ok(mut cache) = state.authentication_cache.lock() {
        // The cache is bounded by the set of live tokens; expired entries are
        // dropped opportunistically so failed brute-force tokens cannot pile up
        // (they are never inserted — only successes are cached).
        cache.retain(|_, (_, at)| at.elapsed() < state.authentication_cache_ttl);
        cache.insert(token.to_string(), (answer.identifier.clone(), std::time::Instant::now()));
    }

    Ok(Principal::Operator(answer.identifier))
}

/// POST one signed hook request (see `hook_utils`: Blake3 keyed MAC over
/// timestamp + body).
async fn post_hook<T: serde::Serialize>(state: &AppState,
                                        hook: &HookEndpoint,
                                        kind: &str,
                                        payload: &T) -> Result<reqwest::Response, String> {
    let body = serde_json::to_vec(payload)
        .map_err(|e| format!("Error while encoding the {} hook request: {}", kind, e))?;

    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let mut request = state.http.post(&hook.url)
        .header("content-type", "application/json")
        .body(body.clone());

    for (name, value) in hook_utils::hook_request_headers(kind, &hook.secret, timestamp, &body) {
        request = request.header(name, value);
    }

    request.send().await
        .map_err(|e| format!("The {} hook at \"{}\" failed: {}", kind, hook.url, e))
}

/// Ask the admission hook whether a request may proceed (soft policy: quotas, plan
/// limits, suspensions). Fail closed on hook failure; a denial carries the hook's
/// reason. No hook configured = everything admitted (the default server).
async fn check_admission(state: &AppState,
                         params: &PathParams,
                         principal: &Principal,
                         action: &str,
                         pallet: Option<&str>) -> Result<(), HandlerError> {
    let Some(hook) = &state.admission_hook else {
        return Ok(());
    };

    let request = AdmissionHookRequest {
        action: action.to_string(),
        warehouse: params.get("warehouse").cloned(),
        operator: match principal {
            Principal::Operator(identifier) => Some(identifier.clone()),
            _ => None,
        },
        pallet: pallet.map(|name| name.to_string()),
    };

    let response = post_hook(state, hook, "admission", &request).await
        .map_err(|reason| {
            tracing::warn!(reason, "the admission hook is unreachable; failing closed");

            (
                StatusCode::SERVICE_UNAVAILABLE,
                "The admission service is unavailable; try again later.".to_string()
            )
        })?;

    if !response.status().is_success() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "The admission service is unavailable; try again later.".to_string()
        ));
    }

    let verdict: AdmissionHookResponse = body_utils::read_json_capped(
        response, body_utils::SMALL_BODY_CAP
    ).await.map_err(|e| {
        tracing::warn!(error = ?e, "the admission hook's answer is unreadable, oversized or malformed");

        (
            StatusCode::SERVICE_UNAVAILABLE,
            "The admission service is unavailable; try again later.".to_string()
        )
    })?;

    if verdict.allow {
        Ok(())
    } else {
        Err((
            StatusCode::FORBIDDEN,
            verdict.reason.unwrap_or("The request was refused by the server's admission policy.".to_string())
        ))
    }
}

/// Deliver an event to the events hook, when one is configured: fire-and-forget
/// from the handler's perspective, at-least-once towards the hook — retried with
/// backoff, logged (never failed-on) when the hook stays down.
fn emit_event(state: &Arc<AppState>, event: HookEvent) {
    let Some(hook) = state.events_hook.clone() else {
        return;
    };

    let state = Arc::clone(state);

    tokio::spawn(async move {
        // ~1 + 5 + 25 + 125 seconds of patience before giving up.
        let mut delay = std::time::Duration::from_secs(1);

        for attempt in 1..=5 {
            match post_hook(&state, &hook, "event", &event).await {
                Ok(response) if response.status().is_success() => return,
                Ok(response) => tracing::warn!(
                    event = event.event,
                    status = %response.status(),
                    attempt,
                    "the events hook refused the event"
                ),
                Err(reason) => tracing::warn!(
                    event = event.event,
                    reason,
                    attempt,
                    "the events hook is unreachable"
                ),
            }

            if attempt < 5 {
                tokio::time::sleep(delay).await;
                delay *= 5;
            }
        }

        tracing::error!(
            event = event.event,
            "an event was dropped after 5 delivery attempts"
        );
    });
}

/// Test-only seam (FORK-95 falsifier): a rendezvous, not a fixed sleep like
/// `remote_utils::test_retrieval_delay` — what a test needs to pin here is not a
/// duration but an ordering (a revocation landing after the authorization answer is
/// fixed and before the write), and a duration long enough to be safe against scheduling jitter would also
/// be long enough to slow every other test that happens to reach the ref update's move
/// gate while it runs. Keyed by identifier, not a bare on/off flag: several other tests
/// in this file exercise that gate concurrently (different threads, same process), and
/// an unkeyed seam would occasionally rendezvous with one of *their* requests instead of
/// the armed test's own, hanging it. `Mutex`, not thread-local: the gate runs on
/// whichever `spawn_blocking` thread tokio schedules the request onto, which is never
/// the thread a test arms this from. The gate reaches it for every operator decision,
/// allowed or refused; an upload never does.
///
/// The armed seam: the identifier to rendezvous on, the channel the checkpoint
/// signals it has been reached, and the gate it then waits on.
#[cfg(test)]
type OfficeLookupSeam = Option<(String, std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>;

#[cfg(test)]
static OFFICE_LOOKUP_TEST_SEAM: Mutex<OfficeLookupSeam> = Mutex::new(None);

#[cfg(test)]
fn office_lookup_test_checkpoint(identifier: &str) {
    let armed = {
        let mut slot = OFFICE_LOOKUP_TEST_SEAM.lock().unwrap_or_else(|e| e.into_inner());

        match slot.as_ref() {
            Some((armed_for, _, _)) if armed_for == identifier => slot.take(),
            _ => None,
        }
    };

    if let Some((_, reached, gate)) = armed {
        let _ = reached.send(());
        let _ = gate.recv();
    }
}

#[cfg(not(test))]
fn office_lookup_test_checkpoint(_identifier: &str) {}

/// The key [`REF_WRITE_TEST_SEAMS`] arms `PUT /v1/trust` under: no pallet name holds a `/`.
const TRUST_WRITE_SEAM: &str = "/v1/trust";

/// How many times a ref update decides at most: a further attempt runs only when the write found
/// that the office or the trust anchor its decision consumed moved meanwhile.
const REF_UPDATE_ATTEMPTS: u32 = 3;

/// The armed write seams: by the warehouse's forklift root and the name each is armed for, the
/// closure every checkpoint for that pair runs.
#[cfg(test)]
type RefWriteSeams = BTreeMap<(PathBuf, String), Box<dyn FnMut() + Send>>;

/// Test seam: a ref update for the armed pallet name (or `PUT /v1/trust`, armed as
/// [`TRUST_WRITE_SEAM`]) in the armed warehouse runs the armed closure after each decision and
/// before its write, so a test can move what the decision read in exactly that window, on every
/// attempt. The closure is taken out of the slot and run with the slot unlocked, then put back, so
/// a closure that parks never blocks another request's checkpoint (which finds the slot empty and
/// runs nothing). Keyed and a `Mutex` for the same reasons as [`OFFICE_LOOKUP_TEST_SEAM`]; keyed by
/// warehouse too, so tests arming the same name in their own warehouses never meet.
#[cfg(test)]
static REF_WRITE_TEST_SEAMS: Mutex<RefWriteSeams> = Mutex::new(BTreeMap::new());

#[cfg(test)]
fn ref_write_test_checkpoint(name: &str) {
    let key = (forklift_core::globals::forklift_root(), name.to_string());
    let armed = REF_WRITE_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);

    if let Some(mut run) = armed {
        run();

        REF_WRITE_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert(run);
    }
}

#[cfg(not(test))]
fn ref_write_test_checkpoint(_name: &str) {}

/// Test counter: the decisions each ref update made, by warehouse forklift root and pallet name.
#[cfg(test)]
static REF_UPDATE_ATTEMPTS_TEST_COUNT: Mutex<BTreeMap<(PathBuf, String), u32>> = Mutex::new(BTreeMap::new());

#[cfg(test)]
fn ref_update_attempt_test_count(name: &str) {
    let key = (forklift_core::globals::forklift_root(), name.to_string());

    *REF_UPDATE_ATTEMPTS_TEST_COUNT.lock().unwrap_or_else(|e| e.into_inner()).entry(key).or_insert(0) += 1;
}

#[cfg(not(test))]
fn ref_update_attempt_test_count(_name: &str) {}

/// Derive the office state from a head **already read once by the caller** — the two
/// arms C9 requires, in one place so neither can be reinstated independently. `Some`
/// goes through the hash-taking accessor (content-addressed reads only, never a second
/// head lookup); `None` is the ordinary bootstrap window, trust established and the
/// office not yet lifted, which is not an error.
///
/// Derived at each use rather than eagerly: on a ref update by a non-operator principal
/// to a user pallet nothing consumes it, and the read costs one object per enrolled user
/// and per key, on every attempt the update decides.
fn office_state_at(office_head: &Option<String>) -> Result<office_utils::OfficeState, HandlerError> {
    match office_head {
        Some(head) => office_utils::read_office_state_of(head).map_err(internal),
        None => Ok(office_utils::OfficeState { users: Vec::new(), keys: Vec::new() }),
    }
}

/// An office authority rule's refusal, as the `403` it is.
fn forbidden(Denied(message): Denied) -> HandlerError {
    (StatusCode::FORBIDDEN, message)
}

/// Authorize an upload (objects, signatures) — runs under the warehouse's storage-root
/// scope. Unlike the ref update, this commits nothing and guards no audit, so it has no
/// consumed values to agree with — it reads the anchor and office state for itself,
/// deliberately, rather than threading them through from a caller that has none to give
/// it. Only an operator is judged against the office, so only an operator pays the reads.
fn require_uploader(principal: &Principal) -> Result<(), HandlerError> {
    if !matches!(principal, Principal::Operator(_)) {
        return Ok(());
    }

    let anchor = office_utils::read_trust_anchor().map_err(internal)?;
    let office_state = office_utils::read_office_state().map_err(internal)?;

    authority_utils::may_upload(principal, &anchor, &office_state).map_err(forbidden)
}

/// Get a path parameter of the matched route.
fn param(params: &PathParams, key: &str) -> Result<String, HandlerError> {
    params.get(key)
        .cloned()
        .ok_or_else(|| internal(format!("The \"{}\" path parameter is missing.", key)))
}

/// Get the `hash` path parameter, refused with a `422` unless it is an object hash
/// ([`require_object_hashes`]).
fn hash_param(params: &PathParams) -> Result<String, HandlerError> {
    let hash = param(params, "hash")?;
    require_object_hashes(std::slice::from_ref(&hash))?;
    Ok(hash)
}

/// Refuse a request naming anything but object hashes, with a `422`. The object store answers
/// for one spelling only, 64 lowercase hex characters, so a client-sent string is checked here
/// rather than reaching a presence check that fails on it as an internal error. The value is not
/// repeated back.
fn require_object_hashes(hashes: &[String]) -> Result<(), HandlerError> {
    if hashes.iter().all(|hash| object_utils::is_object_hash(hash)) {
        return Ok(());
    }

    Err(unprocessable(REFUSAL_NOT_AN_OBJECT_HASH.to_string()))
}

/// Validate a warehouse id (the one shared rule, [`warehouse_utils::validate_warehouse_id`]); a
/// refusal is a `422`.
fn validate_warehouse_id(id: &str) -> Result<(), HandlerError> {
    forklift_core::util::warehouse_utils::validate_warehouse_id(id).map_err(unprocessable)
}

/// Resolve the warehouse a request addresses ([`resolve_handle`]), and start serving a handle
/// served for it just now ([`start_serving`]).
fn resolve_warehouse(state: &Arc<AppState>,
                     params: &PathParams) -> Result<Arc<WarehouseHandle>, HandlerError> {
    let handle = resolve_handle(state, params)?;

    if handle.first_run_due.swap(false, std::sync::atomic::Ordering::SeqCst) {
        start_serving(state, &handle);
    }

    Ok(handle)
}

/// How long a client refused while a warehouse is drained, or otherwise unavailable for now, is
/// told to wait before trying again (the `Retry-After` of every `503`).
const RETRY_AFTER_SECONDS: u64 = 10;

/// The `503` of a request for a warehouse drained for the redaction deleter.
fn draining_refusal() -> HandlerError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "The warehouse is briefly unavailable while content redacted or withheld from it is deleted. \
        Try again shortly.".to_string(),
    )
}

/// Resolve the warehouse a request addresses: the served one in single mode, the one
/// named by the `warehouse` path parameter in multi mode. A warehouse that does not
/// exist on disk is a `404` — creation is explicit (`PUT /warehouses/{id}`), never a
/// side effect. One drained for the redaction deleter is a `503`.
///
/// Every reference to a served handle is taken here, under the registry mutex, which is what
/// lets the deleter's drain ([`drain`]) tell that nothing holds the handle but the registry and
/// itself.
fn resolve_handle(state: &AppState,
                  params: &PathParams) -> Result<Arc<WarehouseHandle>, HandlerError> {
    let mut registry = state.warehouses.lock()
        .map_err(|_| internal("The warehouse registry lock is poisoned.".to_string()))?;

    let base = match &state.mode {
        ServeMode::Single(handle) => return match handle.draining.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err(draining_refusal()),
            false => Ok(Arc::clone(handle)),
        },
        ServeMode::Multi { base } => base,
    };

    let id = param(params, "warehouse")?;
    validate_warehouse_id(&id)?;

    if let Some(handle) = registry.get(&id) {
        return match handle.draining.load(std::sync::atomic::Ordering::SeqCst) {
            true => Err(draining_refusal()),
            false => Ok(handle),
        };
    }

    let root = base.join(&id);

    // Only a name the create would take (nothing there, or an empty folder) is missing. Anything
    // else that is not a warehouse (a file, a folder with content but no marker, a bay's) would
    // refuse the create a 404 points at, so it is answered as unusable instead. One that cannot
    // be judged is a server fault, with no remedy offered. An unfinished clone, which may not
    // have marked its folder yet, goes on to `serving` and its 503.
    // The cause of a check that failed is logged, never sent: it names this server's paths.
    let unchecked = |error: String| {
        tracing::error!(warehouse = %id, %error, "whether the warehouse is one could not be checked");
        (StatusCode::INTERNAL_SERVER_ERROR, format!("Whether \"{}\" on this server is a warehouse could not be checked.", id))
    };
    let missing = match bay_utils::classify_forklift(&root.join(FOLDER_NAME_FORKLIFT_ROOT)) {
        Ok(bay_utils::ForkliftKind::Warehouse) => None,
        Ok(bay_utils::ForkliftKind::Unprepared) if franchise_utils::gate(&root).is_err() => None,
        Ok(bay_utils::ForkliftKind::Absent) => Some(is_missing_or_empty_folder(&root).map_err(|e| unchecked(e.to_string()))?),
        Err(error) => return Err(unchecked(error)),
        Ok(_) => Some(false),
    };
    match missing {
        None => (),
        Some(true) => return Err((
            StatusCode::NOT_FOUND,
            format!(
                "No warehouse \"{}\" exists on this server. Create it first: \
                PUT /warehouses/{}.",
                id, id
            )
        )),
        Some(false) => return Err((
            StatusCode::CONFLICT,
            format!(
                "\"{}\" exists on this server but is not a usable warehouse. The server's operator must \
                re-create it or remove it.",
                id
            )
        )),
    }

    // Acquire this warehouse's serve lock as it is served, and hold it in the registry's handle
    // until the handle is let go. Refuses (503) if a `gc` or full repack is running against this
    // warehouse, or another server already serves it — serving it then would be unsafe. A partial
    // or sealed warehouse, or one that records an origin, stays refused (409) until it is
    // replaced, one lacking content its unavailable record lists (409) until that is back, one whose records cannot be read gets a 500, and an unfinished clone (503) until
    // it is resumed or deleted.
    let handle = Arc::new(
        WarehouseHandle::serving(root, false).map_err(|refusal| match refusal {
            ServingRefusal::Busy(reason) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!("Warehouse \"{}\" is temporarily unavailable (under maintenance): {}", id, reason),
            ),
            // Not the gate's own refusal: its code and next step name this server's directories,
            // and a client would read them as a clone of its own to resume.
            ServingRefusal::Unfinished(_) => (
                StatusCode::SERVICE_UNAVAILABLE,
                format!(
                    "Warehouse \"{}\" is temporarily unavailable: it holds a clone that has not \
                    finished, or that could not be checked. It is served once the clone is resumed \
                    or deleted.",
                    id
                ),
            ),
            // Not the gate's own refusal: its next step names this server's directories and a
            // command only its operator can run. The cause goes to the log for the same reason.
            ServingRefusal::OtherFormat(reason) => {
                tracing::error!(warehouse = %id, %reason, "the warehouse is not served: its format");
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    format!(
                        "Warehouse \"{}\" is unavailable: it is not in the format this server reads, or \
                        it is being upgraded. The server's operator has to upgrade it or this server.",
                        id
                    ),
                )
            }
            ServingRefusal::Unreadable(reason) => damage_refusal(reason),
            refusal @ (ServingRefusal::Partial | ServingRefusal::Sealed(_) | ServingRefusal::Origin
                | ServingRefusal::Unavailable(_)) => (
                StatusCode::CONFLICT,
                refusal.message(&format!("Warehouse \"{}\"", id)),
            ),
        })?
    );

    // Only once it is served: a refused warehouse must not cost a live one its place.
    registry.make_room();
    registry.insert(id, Arc::clone(&handle));

    Ok(handle)
}

/// Run blocking storage work on the blocking pool, inside the warehouse's storage-root
/// scope. The scope is thread-local and the closure runs synchronously on one blocking
/// thread, so it can never leak into another request.
async fn blocking<T: Send + 'static>(
    warehouse: Arc<WarehouseHandle>,
    work: impl FnOnce() -> Result<T, HandlerError> + Send + 'static
) -> Result<T, HandlerError> {
    blocking_unscoped(move || {
        let _scope = StorageRootScope::enter(&warehouse.root);

        work()
    }).await
}

/// Run blocking work on the blocking pool outside any warehouse's scope — for work on the server's
/// folders themselves, such as creating a warehouse.
async fn blocking_unscoped<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, HandlerError> + Send + 'static
) -> Result<T, HandlerError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| internal(format!("A storage task failed: {}", e)))?
}

/// What a warehouse creation found.
#[derive(Debug, PartialEq, Eq)]
enum Creation {
    /// This call made the warehouse.
    Created,

    /// The warehouse was already there (a re-PUT, or a racing PUT got there first).
    Existed,
}

/// The prefix of the folder a warehouse is prepared in before it is moved into place. It starts
/// with a dot, which no warehouse id may, so a half-prepared warehouse is never served.
const CREATING_PREFIX: &str = ".creating-";

/// Numbers [`create_warehouse`]'s staging folders, which are unique per creation, not just per
/// process.
static NEXT_CREATION: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Make the warehouse `id` in `base`, or find it already made. The warehouse is prepared in a
/// folder of its own and renamed into place, so it appears complete or not at all, and the rename
/// decides which of several racing calls made it: exactly one sees [`Creation::Created`].
///
/// An empty folder at `base/id` is taken over (an operator's `mkdir`); anything else there that
/// is not a warehouse is refused with a `409`, never adopted.
///
/// Blocking: call it from blocking code.
fn create_warehouse(base: &std::path::Path, id: &str) -> Result<Creation, HandlerError> {
    create_warehouse_numbered(base, id, &NEXT_CREATION)
}

/// [`create_warehouse`] drawing its staging names from `counter`.
fn create_warehouse_numbered(base: &std::path::Path, id: &str,
                             counter: &std::sync::atomic::AtomicU64) -> Result<Creation, HandlerError> {
    let target = base.join(id);
    if holds_warehouse(&target) {
        return Ok(Creation::Existed);
    }

    // A name taken by a killed earlier process with this one's id, which the startup sweep could
    // not remove, is stepped past rather than failing every creation of `id`.
    let candidates = file_utils::pid_numbered(counter, |pid, n| base.join(format!(
        "{}{}-{}-{}", CREATING_PREFIX, id, pid, n
    )));
    let staging = file_utils::create_first_new_folder(candidates)
        .map_err(|e| internal(format!("Error while creating the warehouse folder: {}", e)))?;

    // Synced whole before the rename (prepare builds `.forklift` synced and renames it into the
    // synced staging folder): a warehouse visible after a power loss with part of its preparation
    // missing would never be repaired, since a later `PUT` finds it and answers 200.
    let prepared = {
        let _scope = StorageRootScope::enter(&staging);
        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).map(|_| ())
    };

    if let Err(error) = prepared {
        let _ = forklift_core::util::file_utils::remove_dir_all(&staging);
        return Err(internal(error));
    }

    let conflict = || (
        StatusCode::CONFLICT,
        format!(
            "\"{}\" already exists on this server and is not a warehouse; remove it or choose \
            another id.",
            id
        ),
    );

    // Unix renames over an empty folder and Windows over no folder at all; removing an empty one
    // first (and nothing else: `remove_dir` refuses a folder with anything in it) makes both agree.
    let _ = forklift_core::util::file_utils::remove_dir(&target);

    // Windows renames over a *file*, replacing it, so anything at the id that is not a folder is
    // refused here rather than left to the rename, which refuses it only on Unix.
    if std::fs::symlink_metadata(&target).is_ok_and(|metadata| !metadata.is_dir()) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&staging);
        return Err(conflict());
    }

    match file_utils::rename(&staging, &target) {
        Ok(()) => {
            // The warehouse is visible from here on, so this is `Created` whatever the sync says:
            // a retry would find it and answer 200, and its creation would never be announced.
            if let Err(error) = file_utils::sync_dir(base) {
                tracing::error!(warehouse = id, error, "the new warehouse may not survive a crash");
            }

            Ok(Creation::Created)
        }
        Err(rename_error) => {
            let _ = forklift_core::util::file_utils::remove_dir_all(&staging);

            if holds_warehouse(&target) {
                Ok(Creation::Existed)
            } else if std::fs::symlink_metadata(&target).is_ok() {
                Err(conflict())
            } else {
                Err(internal(format!(
                    "The warehouse could not be moved into place, so it was not created: {}. \
                    Try again.",
                    rename_error
                )))
            }
        }
    }
}

/// Remove what interrupted warehouse creations left in `base`: the folders [`create_warehouse`]
/// prepares in, which only a crash between their creation and their rename leaves behind. Runs at
/// startup under the base-folder lock, when no creation can be in progress. Best-effort: anything
/// it cannot remove is logged and stays, and is never served either way.
fn sweep_interrupted_creations(base: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(base) else {
        return;
    };

    for entry in entries.flatten() {
        if !entry.file_name().to_string_lossy().starts_with(CREATING_PREFIX) {
            continue;
        }

        let path = entry.path();
        let removed = entry.file_type()
            .or_else(|_| std::fs::symlink_metadata(&path).map(|metadata| metadata.file_type()))
            .and_then(|file_type| forklift_core::util::file_utils::remove_entry_unfollowed(&path, file_type));
        match removed {
            Ok(()) => tracing::info!(path = %path.display(), "removed an interrupted warehouse creation"),
            Err(error) => tracing::warn!(
                path = %path.display(), %error, "could not remove an interrupted warehouse creation"
            ),
        }
    }
}

/// `PUT /warehouses/{id}` — create a warehouse (multi mode only). Explicitly gated:
/// only token-protected servers accept creation (an open server would be a junk farm),
/// and a lift can never create a warehouse by accident. Idempotent: `201` when
/// created, `200` when the warehouse already exists, `409` when something that is not a
/// warehouse holds its name.
async fn put_warehouse(State(state): State<Arc<AppState>>,
                       Extension(principal): Extension<Principal>,
                       Path(params): Path<PathParams>) -> Response {
    let ServeMode::Multi { base } = &state.mode else {
        return error_response((
            StatusCode::NOT_FOUND,
            "This server serves a single warehouse; there is nothing to create.".to_string()
        ));
    };

    // Creation is a server-administration act: only the static token qualifies (an
    // open server would be a junk farm, and operator tokens are warehouse-scoped
    // identities, not server admins).
    if principal != Principal::Static {
        return error_response((
            StatusCode::FORBIDDEN,
            "Warehouse creation requires the server's static token (start the server \
            with --token).".to_string()
        ));
    }

    let id = match param(&params, "warehouse").and_then(|id| {
        validate_warehouse_id(&id).map(|_| id)
    }) {
        Ok(id) => id,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "warehouse_create", None).await {
        return error_response(error);
    }

    let (base, create_id) = (base.clone(), id.clone());
    let result = blocking_unscoped(move || create_warehouse(&base, &create_id)).await;

    match result {
        Ok(creation) => {
            // Serve it the way every request does: the registry's handle when this warehouse is
            // already served (a re-PUT stays an idempotent 200), else a locked one built under
            // the registry mutex, so exactly one handle ever exists per
            // warehouse. Never `serving` directly: the serve lock refuses even its own holder.
            let served = resolve_warehouse(&state, &params);

            // The warehouse exists from here on, served or not: a retry after a refused
            // registration finds it existing and answers 200, so a refused registration must
            // still announce it here or the announcement is lost.
            if creation == Creation::Created {
                let mut event = HookEvent::new(EVENT_WAREHOUSE_CREATED);
                event.warehouse = Some(id);
                emit_event(&state, event);
            }

            match served {
                Err(error) => error_response(error),
                Ok(_) if creation == Creation::Created => StatusCode::CREATED.into_response(),
                Ok(_) => StatusCode::OK.into_response(),
            }
        }
        Err(error) => error_response(error),
    }
}

/// `GET /v1/warehouse` — the handshake: protocol version, refs and trust.
async fn get_warehouse(State(state): State<Arc<AppState>>,
                       Path(params): Path<PathParams>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        let mut pallets = std::collections::BTreeMap::new();

        // Both namespaces travel in one map, keyed by the qualified reference form: user
        // pallets bare, meta pallets as `@office` — so clients route by namespace, never by a
        // hard-coded name.
        for (pallet_ref, head) in pallet_utils::all_pallet_refs().map_err(internal)? {
            pallets.insert(pallet_ref.to_wire(), head);
        }

        Ok(WarehouseInfo {
            protocol: PROTOCOL_VERSION.to_string(),
            default_pallet: pallet_utils::read_position().map_err(internal)?.pallet,
            pallets,
            trust: office_utils::read_trust_anchor()
                .map_err(internal)?
                .map(|anchor| TrustAnchorDto::from(&anchor)),
            // This head serves and stores chunked large files as ordinary content-addressed
            // objects, and its ref-update closure audit descends recipes to presence-check chunks
            // before a ref moves. A chunk-aware client reads this to know it may lift chunked
            // content here (the byte plane is the local object store, so a chunk GET/PUT is served
            // whole like any other loose object).
            chunking: true,
            // This head deletes pallets, and every tombstone it keeps is listed, as widely as the
            // live refs are. A kept head has no name, so it is not listed: a name would be a guess.
            // A damaged list refuses the handshake rather than list part of the set.
            tombstones: Some(forklift_core::util::tombstone_utils::read_tombstones().map_err(damage_refusal)?.named),
            redactions: Some(served_redactions()?),
            withheld: Some(served_withheld()?),
            // `PUT /v1/withheld/{hash}/recipe` records the digest of a withheld recipe this head
            // never held a copy of.
            withheld_intake: forklift_core::model::remote::WithheldIntake::Direct,
        })
    }).await;

    match result {
        Ok(info) => Json(info).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/missing` — which of these objects does the remote lack?
async fn post_missing(State(state): State<Arc<AppState>>,
                      Path(params): Path<PathParams>,
                      Json(request): Json<MissingObjectsRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_MISSING_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per request; batch larger sets.",
            MAX_MISSING_BATCH
        )));
    }

    if let Err(error) = require_object_hashes(&request.hashes) {
        return error_response(error);
    }

    let gate = Arc::clone(&warehouse);
    let result = blocking(warehouse, move || {
        // A chunk an owed deletion withholds is missing here, as `GET` answers it: a lift naming
        // it uploads it. Content the host withholds is never missing, held or not: this head
        // takes none of it, and its ref writes tolerate its absence.
        let host_withheld = read_withheld()?;
        let removed = withholding::removed_from(&read_applied()?, &host_withheld);
        let owed_chunks = withheld_among(&gate, &removed, &request.hashes)?;
        let mut missing: Vec<String> = Vec::new();

        for hash in request.hashes {
            if host_withheld.contains(&hash) {
                continue;
            }
            if owed_chunks.contains(&hash) || !file_utils::does_object_exist(&hash).map_err(internal)? {
                missing.push(hash);
            }
        }

        Ok(MissingObjectsResponse { missing })
    }).await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

/// The most holds walks this server runs at once (see [`post_holds`]).
const MAX_CONCURRENT_HOLDS_WALKS: usize = 4;

static HOLDS_WALKS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_HOLDS_WALKS);

/// `POST /v1/holds/{name}` — which of these parcels does the pallet still hold, its head
/// or one of the head's ancestors? A bounded walk ([`merge_utils::holds`]): a candidate it
/// could not settle is answered `unknown`, never `not_held`. Reading history is what fetching
/// already allows, so nothing past authentication is asked. A pallet deleted here is walked from
/// the head its tombstone holds, which stays stored and served like a live ref's.
async fn post_holds(State(state): State<Arc<AppState>>,
                    Path(params): Path<PathParams>,
                    Json(request): Json<HoldsRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let name = match param(&params, "name") {
        Ok(name) => name,
        Err(error) => return error_response(error),
    };

    if let Err(error) = merge_utils::check_holds_candidates(&request.candidates).map_err(unprocessable) {
        return error_response(error);
    }

    if let Err(error) = require_object_hashes(&request.candidates) {
        return error_response(error);
    }

    // Refused before the cap is consulted, so whether a name is a `422` never depends on load.
    let pallet_ref = match pallet_utils::PalletRef::parse(&name).map_err(unprocessable) {
        Ok(pallet_ref) => pallet_ref,
        Err(error) => return error_response(error),
    };

    // A walk holds a blocking thread for up to its time bound, and the pool serves every other
    // request too. Past the cap the answer is `unknown` at once: correct, and only keeps more. The
    // permit moves into the walk, so a client that hangs up does not free it while the walk runs.
    let Ok(permit) = HOLDS_WALKS.try_acquire() else {
        let holds = request.candidates.into_iter().map(|candidate| (candidate, HoldAnswer::Unknown)).collect();
        return Json(HoldsResponse { holds }).into_response();
    };

    let result = blocking(warehouse, move || {
        let _permit = permit;
        let head = match pallet_utils::get_pallet_head_in(pallet_ref.namespace, &pallet_ref.name).map_err(internal)? {
            Some(head) => Some(head),
            None if pallet_ref.namespace == pallet_utils::PalletNamespace::User => {
                forklift_core::util::tombstone_utils::read_tombstones().map_err(damage_refusal)?.named.remove(&pallet_ref.name)
            }
            None => None,
        };

        let holds = merge_utils::holds(head.as_deref(), &request.candidates, None, |round| {
            round.iter().map(|hash| object_utils::load_parcel(hash).map(|parcel| parcel.parents)).collect()
        }).map_err(unprocessable)?;

        Ok(HoldsResponse { holds })
    }).await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/upload-targets` — the body-less upload negotiation. The direct head
/// serves object bytes itself and verifies every `PUT` inline, so it has no staging prefix and
/// no presigned targets: it answers with what it already has (`present`) and every missing hash
/// in `direct` (upload those to `/v1/objects/{hash}` for inline verification), with `targets`
/// empty. That is exactly what lets one client code path serve both this head and a storage-
/// backed one (`REMOTE_PROTOCOL.md`), and it subsumes `missing` on the upload path.
async fn post_upload_targets(State(state): State<Arc<AppState>>,
                             Path(params): Path<PathParams>,
                             Json(request): Json<UploadTargetsRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_UPLOAD_TARGETS_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per upload-targets request; batch larger sets.",
            MAX_UPLOAD_TARGETS_BATCH
        )));
    }

    if let Err(error) = require_object_hashes(&request.hashes) {
        return error_response(error);
    }

    let gate = Arc::clone(&warehouse);
    let result = blocking(warehouse, move || {
        let (applied, withheld) = (read_applied()?, read_withheld()?);

        // A redacted hash refuses the lift: the client checks its history against its own
        // redactions first, so one asked about here is a client that did not.
        request.hashes.iter().try_for_each(|hash| refuse_removed(&applied, &WithheldSet::default(), hash))?;

        // A chunk an owed deletion withholds is answered `direct`, as `missing` lists it, so the
        // lift uploads it: should the deletion take the stored copy before the upload, the upload
        // stores it again. One the deletion takes between the upload and the ref write is gone,
        // and the ref write refuses the lift.
        let owed_chunks = withheld_among(&gate, &withholding::removed_from(&applied, &withheld), &request.hashes)?;

        let mut response = UploadTargetsResponse {
            present: Vec::new(),
            targets: std::collections::BTreeMap::new(),
            direct: Vec::new(),
            withheld: Vec::new(),
        };

        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

        for hash in request.hashes {
            if !seen.insert(hash.clone()) {
                continue;
            }

            // Content the host withholds is never taken, but a lift may name it: its ref write
            // tolerates the absence. So it is answered apart rather than refusing the lift.
            if withheld.contains(&hash) {
                response.withheld.push(hash);
            } else if !owed_chunks.contains(&hash) && file_utils::does_object_exist(&hash).map_err(unprocessable)? {
                response.present.push(hash);
            } else {
                // No staging prefix here — the client PUTs the body and the head verifies it
                // inline (invariant 1 is upheld by the inline hash check, not a staging copy).
                response.direct.push(hash);
            }
        }

        Ok(response)
    }).await;

    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/objects/batch` — many objects in one round trip, as a bundle-format
/// stream (the incremental counterpart of `GET /v1/bundles/latest`). Objects the
/// warehouse lacks are simply absent from the stream; the client notices and falls
/// back to loose fetches.
async fn post_objects_batch(State(state): State<Arc<AppState>>,
                            Path(params): Path<PathParams>,
                            Json(request): Json<MissingObjectsRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if request.hashes.len() > MAX_MISSING_BATCH {
        return error_response(unprocessable(format!(
            "At most {} hashes per request; batch larger sets.",
            MAX_MISSING_BATCH
        )));
    }

    if let Err(error) = require_object_hashes(&request.hashes) {
        return error_response(error);
    }

    if let Err(error) = await_applies(&warehouse).await {
        return error_response(error);
    }

    let gate = Arc::clone(&warehouse);
    let result = blocking(warehouse, move || {
        let removed = withholding::removed().map_err(internal)?;
        let withheld = withheld_among(&gate, &removed, &request.hashes)?;
        let hashes: Vec<String> = request.hashes.into_iter().filter(|hash| !withheld.contains(hash)).collect();

        bundle_utils::build_partial_bundle(&hashes).map_err(internal)
    }).await;

    match result {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                // The payload is already a zstd stream; the compression layer must
                // not wrap it again.
                (axum::http::header::CONTENT_ENCODING, "identity"),
            ],
            bytes,
        ).into_response(),
        Err(error) => error_response(error),
    }
}

/// `GET /v1/parcels/{parcel}/subtree/{path}` — the object closure of the subtree at `path` in
/// `parcel`, as a bundle-format stream.
///
/// This is the **path-addressed** fetch (the client's `RemoteClient::fetch_subtree`): the
/// remote resolves `path` to a subtree itself, which is what lets it authorize a fetch by path.
/// A hash-addressed `GET /v1/objects/{hash}` is path-blind — it cannot enforce a per-path grant
/// without a hash→path index — so this endpoint is the wire surface file-level path enforcement
/// (FORK-10) is designed to gate; see [`authorize_subtree`]. The endpoint is additive: a client
/// whose remote predates the route gets a `404` and falls back to the hash-addressed scoped
/// walk, so shipping it needs no protocol bump.
async fn get_subtree(State(state): State<Arc<AppState>>,
                     Extension(principal): Extension<Principal>,
                     Path(params): Path<PathParams>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let parcel = match param(&params, "parcel") {
        Ok(parcel) => parcel,
        Err(error) => return error_response(error),
    };

    let path = match param(&params, "path") {
        Ok(path) => path,
        Err(error) => return error_response(error),
    };

    if let Err(error) = authorize_subtree(&state, &params, &principal, &path).await {
        return error_response(error);
    }

    if let Err(error) = await_applies(&warehouse).await {
        return error_response(error);
    }

    let result = blocking(warehouse, move || {
        let root = object_utils::load_parcel(&parcel)
            .map_err(|_| (StatusCode::NOT_FOUND, format!("No parcel {} exists.", parcel)))?
            .tree_hash;

        let outcome = tree_utils::collect_subtree_closure(&root, &path, MAX_MISSING_BATCH)
            .map_err(internal)?
            .ok_or_else(|| (StatusCode::NOT_FOUND, format!("No subtree at \"{}\" in {}.", path, parcel)))?;

        // The same cap `objects/batch` enforces, for the same reason: an uncapped subtree would
        // buffer an arbitrarily large bundle in memory before it can even check the size. The
        // empty path (the whole tree) is exactly the request expected to blow this cap on a big
        // repo — that is the correct outcome: the client falls back to the hash-addressed walk,
        // the same fallback a 404 already triggers, so a 422 here reuses that exact path.
        //
        // The cap bounds the walk itself, not just this check: `collect_subtree_closure` aborts
        // as soon as the closure exceeds it, so an oversized subtree is refused without first
        // loading every tree and blob beneath it.
        let closure = match outcome {
            tree_utils::SubtreeClosure::Overflowed => {
                return Err(unprocessable(format!(
                    "The subtree at \"{}\" has more than {} objects; fetch it with the \
                    hash-addressed walk instead.",
                    path, MAX_MISSING_BATCH
                )));
            }
            tree_utils::SubtreeClosure::Complete(closure) => closure,
        };

        bundle_utils::build_partial_bundle(&closure).map_err(internal)
    }).await;

    match result {
        Ok(bytes) => (
            [
                (axum::http::header::CONTENT_TYPE, "application/octet-stream"),
                // Already a zstd stream; the compression layer must not wrap it again.
                (axum::http::header::CONTENT_ENCODING, "identity"),
            ],
            bytes,
        ).into_response(),
        Err(error) => error_response(error),
    }
}

/// The file-level path-authorization seam (FORK-10) for the path-addressed subtree fetch.
///
/// Today a read is open to every authenticated principal — exactly what a hash-addressed object
/// `GET` already allows — so this admits everything and adds no enforcement: the endpoint
/// exposes nothing a path-blind object fetch does not. It is kept as a distinct function because
/// the request names a *path*: once file-level grants land, the office resolves the principal's
/// granted paths here and refuses (`403`) a subtree outside them — the one enforcement point,
/// wired in one place rather than scattered across the read handlers.
async fn authorize_subtree(_state: &AppState,
                           _params: &PathParams,
                           _principal: &Principal,
                           _path: &str) -> Result<(), HandlerError> {
    Ok(())
}

/// `GET /v1/objects/{hash}` — the raw object bytes.
///
/// This buffers the whole object in memory. `put_object` below buffers the whole request body too,
/// and *that* side is bounded by the default-on body cap (`DefaultBodyLimit`, wired to
/// `MAX_OBJECT_BYTES` — 64 MiB — unless `max_body_mb` overrides it): a request larger than the cap
/// is rejected by the framework before this handler's body even runs. `get_object`'s bound is a
/// different, write-side guarantee: `store_object_bytes`/`store_object_stream` refuse to *write* an
/// over-ceiling object (W7), and post-chunking a blob is below the chunk threshold and a chunk is
/// `<=` 4 MiB — so any object *authored under this policy* that a healthy GET can reach is already
/// `<= MAX_OBJECT_BYTES` by construction, not because this handler enforces anything on the way out.
/// **The one exception, by design:** a grandfathered pre-ceiling blob (imported from an old-version
/// bundle before `MAX_OBJECT_BYTES` existed) stays readable forever — the ceiling gates writes and
/// imports, never reads — so `get_object` serves it whole and genuinely unbounded; that is the
/// documented cost of "old data stays readable," not an oversight here. True streaming of these two
/// handlers (to match the AWS head's presigned-URL data plane) is deliberately not done: a streamed
/// `GET` would have to bypass `retrieve_object_by_hash`, which content-verifies before returning and
/// reconstructs packed/delta objects in memory — i.e. rework the store read API, not just the
/// handler — and a streamed `PUT` would need a new ceiling-enforcing, EOF-terminated streaming store
/// variant plus an async→sync body bridge. Neither is a handler-local change, and neither closes the
/// grandfathered-blob gap above, so the boundedness is left as described rather than reworked here.
async fn get_object(State(state): State<Arc<AppState>>,
                    Path(params): Path<PathParams>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match hash_param(&params) {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    if let Err(error) = await_applies(&warehouse).await {
        return error_response(error);
    }

    let gate = Arc::clone(&warehouse);
    let result = blocking(warehouse, move || {
        let (applied, withheld) = (read_applied()?, read_withheld()?);
        refuse_removed(&applied, &withheld, &hash)?;

        if !file_utils::does_object_exist(&hash).map_err(unprocessable)?
            || !withheld_among(&gate, &withholding::removed_from(&applied, &withheld), std::slice::from_ref(&hash))?
                .is_empty() {
            return Err((StatusCode::NOT_FOUND, format!("No object {} exists.", hash)));
        }

        file_utils::retrieve_object_by_hash(&hash).map_err(internal)
    }).await;

    match result {
        Ok(bytes) => bytes.into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/objects/{hash}` — store an object; the hash is verified before anything
/// becomes fetchable (the non-negotiable invariant of the protocol).
async fn put_object(State(state): State<Arc<AppState>>,
                    Extension(principal): Extension<Principal>,
                    Path(params): Path<PathParams>,
                    body: Bytes) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "upload", None).await {
        return error_response(error);
    }

    let hash = match hash_param(&params) {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        require_uploader(&principal)?;

        // An upload that checked before a redaction applied, or before its host withheld it, and
        // stores after is never served: every read checks too.
        refuse_removed(&read_applied()?, &read_withheld()?, &hash)?;

        object_utils::store_object_bytes(&hash, &body).map_err(unprocessable)
    }).await;

    match result {
        Ok(true) => StatusCode::CREATED.into_response(),
        Ok(false) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/withheld/{hash}/recipe` — the digest intake: the bytes of the withheld recipe `hash`,
/// sent by a client that holds it, so this head can record the chunk-list digest a chunk-list
/// redaction compares when it never held a copy to compute it from. The bytes must hash to `hash`
/// and parse as a recipe; nothing of them is stored or served. A hash not withheld, or withheld
/// with a digest already, is answered the same, with nothing changed.
async fn put_withheld_recipe(State(state): State<Arc<AppState>>,
                             Extension(principal): Extension<Principal>,
                             Path(params): Path<PathParams>,
                             body: Bytes) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "upload", None).await {
        return error_response(error);
    }

    let hash = match hash_param(&params) {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        require_uploader(&principal)?;

        let digest = withholding::recipe_digest(&hash, &body).map_err(unprocessable)?;

        withholding::record_digest(&hash, &digest).map_err(internal)
    }).await;

    match result {
        Ok(_) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(error),
    }
}

/// `GET /v1/signatures/{hash}` — a parcel's signature sidecar.
async fn get_signature(State(state): State<Arc<AppState>>,
                       Path(params): Path<PathParams>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let hash = match hash_param(&params) {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        sign_utils::load_raw_parcel_signature(&hash).map_err(internal)
    }).await;

    match result {
        Ok(Some(bytes)) => bytes.into_response(),
        Ok(None) => error_response((StatusCode::NOT_FOUND, "The parcel carries no signature.".to_string())),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/signatures/{hash}` — store a signature sidecar. Structure is validated
/// here; whether the signature *verifies* is decided at ref update time. A conflicting
/// sidecar for an already-signed parcel is refused (signatures are immutable).
async fn put_signature(State(state): State<Arc<AppState>>,
                       Extension(principal): Extension<Principal>,
                       Path(params): Path<PathParams>,
                       body: Bytes) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "upload", None).await {
        return error_response(error);
    }

    let hash = match hash_param(&params) {
        Ok(hash) => hash,
        Err(error) => return error_response(error),
    };

    let result = blocking(warehouse, move || {
        require_uploader(&principal)?;

        let existed = sign_utils::load_raw_parcel_signature(&hash).map_err(internal)?;

        match sign_utils::store_raw_parcel_signature(&hash, &body) {
            Ok(()) => Ok(existed.is_none()),
            Err(message) if message.contains("immutable") => Err((StatusCode::CONFLICT, message)),
            Err(message) => Err(unprocessable(message)),
        }
    }).await;

    match result {
        Ok(true) => StatusCode::CREATED.into_response(),
        Ok(false) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `PUT /v1/trust` — establish the trust anchor: the same one-way door it is locally.
/// The one sanctioned way through the door is a re-genesis anchor (§8.7), and only
/// the server's own operator authority (the static token — the authority *outside*
/// the dead chain) may sanction it; per-operator tokens may not, since their roles
/// come from exactly the chain being replaced.
async fn put_trust(State(state): State<Arc<AppState>>,
                   Extension(principal): Extension<Principal>,
                   Path(params): Path<PathParams>,
                   Json(anchor): Json<TrustAnchorDto>) -> Response {
    // Clients refuse a malformed anchor at the handshake, and trust here is a one-way door: one
    // stored would lock every client out of this warehouse for good.
    if let Err(problem) = anchor.validate() {
        return error_response(unprocessable(problem));
    }

    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let warehouse_id = params.get("warehouse").cloned();

    // Trust establishment is a one-way door, held by the install's preconditions rather than by
    // ordering requests: two first contacts can both read "no anchor" below, and the second to
    // install finds the anchor no longer absent and is refused (or finds its own anchor in place).
    let counted = RefWriteCounted::start(&warehouse);
    let result = blocking(Arc::clone(&warehouse), move || {
        let _counted = counted;

        match office_utils::read_trust_anchor_with_bytes().map_err(internal)? {
            Some((existing, existing_bytes)) => {
                if TrustAnchorDto::from(&existing) == anchor {
                    return Ok(None);
                }

                // A re-genesis anchor: it must name the current genesis as its prior
                // and adopt the office head exactly as it stands — nothing of the old
                // chain may be silently dropped.
                let is_regenesis = anchor.prior_genesis.as_deref() == Some(existing.genesis.as_str());

                if !is_regenesis {
                    return Err(trust_one_way_door());
                }

                authority_utils::may_regenesis(&principal).map_err(forbidden)?;

                let office_head = pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)
                    .map_err(internal)?;

                if anchor.adopts.as_deref() != office_head.as_deref() {
                    return Err(unprocessable(format!(
                        "The re-genesis anchor adopts office head {}, but this \
                        warehouse's office head is {}. The reset would drop history; \
                        re-run the re-genesis from a warehouse in sync with this one.",
                        anchor.adopts.as_deref().unwrap_or("(none)"),
                        office_head.as_deref().unwrap_or("(unborn)")
                    )));
                }

                ref_write_test_checkpoint(TRUST_WRITE_SEAM);

                // The anchor and the office head the tests above read are the install's
                // preconditions, and the boundary it records holds every pallet head: a bay of
                // this warehouse moving either meanwhile refuses the install rather than planting
                // an anchor over history it never adopted.
                let installed = office_utils::install_trust(&office_utils::TrustInstall {
                    anchor: &anchor.to_anchor(),
                    replaces: Some(&existing_bytes),
                    office: office_head.as_deref(),
                    new_office: None,
                    served: None,
                    pallets: PalletsPrecondition::WithinBoundary {
                        boundary: anchor.boundary.clone(),
                        scope: BoundaryScope::AllButOffice,
                    },
                });

                if !trust_install_outcome(installed, &anchor)? {
                    return Ok(None);
                }

                tracing::warn!(
                    old_genesis = existing.genesis,
                    new_genesis = anchor.genesis,
                    adopts = anchor.adopts.as_deref().unwrap_or(""),
                    "TRUST RESET: the trust anchor was replaced by a re-genesis"
                );

                let mut event = HookEvent::new(EVENT_TRUST_RESET);
                event.detail = Some(anchor.genesis.clone());
                Ok(Some(event))
            }
            None => {
                // No anchor but an office: the anchor was lost, and whichever client lifts first
                // would otherwise plant its own over a chain it never verified. The install below
                // conditions on the office still being unborn, so a bay of this warehouse enrolling
                // meanwhile refuses it.
                if pallet_utils::get_meta_pallet_head(office_utils::OFFICE_PALLET_NAME)
                    .map_err(internal)?
                    .is_some() {
                    return Err((
                        StatusCode::CONFLICT,
                        "This warehouse holds an office but no trust anchor: its anchor was \
                        lost. The server operator must restore it; a client cannot \
                        re-establish it.".to_string()
                    ));
                }

                // A first anchor arrives over an unborn office, so nothing above ties its
                // `adopts` to held history — but every warehouse that adopts this anchor later
                // fetches that history from here. The client uploads it before the anchor.
                // Strictly, nothing tolerated: a pin is served whole to every adopter, so one naming
                // content this head withholds or redacted is a pin it can never serve.
                audit_utils::require_adopted_history(
                    anchor.adopts.as_deref(),
                    &|head| audit_utils::verify_parcel_closure(head, None),
                )
                .or_else(|refusal| Err((
                    StatusCode::UNPROCESSABLE_ENTITY,
                    audit_utils::withheld_beyond_files(refusal, &read_withheld()?),
                )))?;

                ref_write_test_checkpoint(TRUST_WRITE_SEAM);

                // Every pallet head but the office's must be inside the boundary: unsigned history
                // outside it — a meta pallet's included — would stand under the anchor unvouched.
                let installed = office_utils::install_trust(&office_utils::TrustInstall {
                    anchor: &anchor.to_anchor(),
                    replaces: None,
                    office: None,
                    new_office: None,
                    served: None,
                    pallets: PalletsPrecondition::WithinBoundary {
                        boundary: anchor.boundary.clone(),
                        scope: BoundaryScope::AllButOffice,
                    },
                });

                if !trust_install_outcome(installed, &anchor)? {
                    return Ok(None);
                }

                let mut event = HookEvent::new(EVENT_TRUST_ESTABLISHED);
                event.detail = Some(anchor.genesis.clone());
                Ok(Some(event))
            }
        }
    }).await;

    match result {
        Ok(Some(mut event)) => {
            event.warehouse = warehouse_id;
            emit_event(&state, event);

            StatusCode::CREATED.into_response()
        }
        Ok(None) => StatusCode::OK.into_response(),
        Err(error) => error_response(error),
    }
}

/// `POST /v1/pallets/{name}` — the CAS ref update: the commit point of a lift, and the
/// place where the server enforces everything (DESIGN.html §4.2 step 6): presence of
/// the full closure, fast-forward-ness, and — on trusted warehouses — the same audit
/// the CLI runs offline.
async fn post_ref_update(State(state): State<Arc<AppState>>,
                         Extension(principal): Extension<Principal>,
                         Path(params): Path<PathParams>,
                         Json(request): Json<RefUpdateRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let name = match param(&params, "name") {
        Ok(name) => name,
        Err(error) => return error_response(error),
    };

    // Parsed before the admission hook, so a hook only ever sees a valid reference.
    let pallet_ref = match pallet_utils::PalletRef::parse(&name).map_err(unprocessable) {
        Ok(pallet_ref) => pallet_ref,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "ref_update", Some(&name)).await {
        return error_response(error);
    }

    let warehouse_id = params.get("warehouse").cloned();

    let acting_operator = match &principal {
        Principal::Operator(identifier) => Some(identifier.clone()),
        _ => None,
    };

    let counted = RefWriteCounted::start(&warehouse);
    // Whether the write reached the point where it may land, and so owes an apply.
    let took = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let took_owe = Arc::clone(&took);
    let write = blocking(Arc::clone(&warehouse), move || {
        let counted = counted;
        // A meta pallet arrives as `@office`. The server routes by *namespace*, never by a
        // hard-coded name (DESIGN.html §3.3).
        let namespace = pallet_ref.namespace;
        let bare = pallet_ref.name.clone();
        let is_meta = namespace == pallet_utils::PalletNamespace::Meta;
        let is_office = is_meta && bare == OFFICE_PALLET_NAME;

        // A meta name this server does not know is refused outright, never stored unchecked.
        if is_meta {
            meta_tree::check_meta_pallet_name(&bare).map_err(unprocessable)?;
        }

        let pallet_rules = if is_meta { meta_tree::meta_rules(&bare) } else { None };

        // The closure check's proof, kept from the first attempt for the ones after it.
        let mut verified_closure: Option<pallet_utils::VerifiedClosure> = None;
        let mut attempt = 0;

        loop {
            attempt += 1;

            ref_update_attempt_test_count(&name);

            // The trust anchor and office state are read once per attempt, so transport
            // authorization below and the audit that follows consume exactly the same values.
            // Nothing holds another request or a local bay of the warehouse out meanwhile: the final
            // write conditions on the anchor's bytes and the office head being the ones read here,
            // so a writer moving either refuses the write rather than committing what a stale audit
            // settled, and the update then decides again from fresh reads.
            let (anchor, anchor_bytes) = match office_utils::read_trust_anchor_with_bytes().map_err(internal)? {
                Some((anchor, bytes)) => (Some(anchor), Some(bytes)),
                None => (None, None),
            };

            // The office head, read once, is an `Option`: `Some` derives the state
            // through the hash-taking accessor (content-addressed reads only, no second
            // head lookup); `None` is the ordinary bootstrap window — trust established
            // but the office not yet lifted — not an error.
            let office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).map_err(internal)?;
            let office_head_read = office_head.clone();

            // Transport authorization: may this principal move this ref? The role and
            // the pallet grants come from the office — signed, tracked metadata — read
            // against the anchor and office state the audit below consumes, not a second,
            // independent read of them. Decided before the CAS and object-presence checks
            // below, as it always was: an unauthorized request is refused on authorization
            // and never leaks whether some other precondition also happened to fail.
            if let Principal::Operator(identifier) = &principal {
                let office_state = office_state_at(&office_head)?;
                let decision = authority_utils::may_move(&principal, &anchor, &office_state, &pallet_ref);

                // No-op outside a test that has armed the seam for this identifier (see
                // `office_lookup_test_checkpoint`'s doc): a falsifier's rendezvous point, right
                // where this decision is fixed, whichever way it went, and before the write it
                // authorizes.
                office_lookup_test_checkpoint(identifier);
                decision.map_err(forbidden)?;
            }

            // An early refusal, before the audit below spends anything on a request that cannot
            // land. It is not the guard: the write at the end is the compare-and-set.
            let current = pallet_utils::get_pallet_head_in(namespace, &bare).map_err(internal)?;

            if current != request.old_head {
                return Err(pallet_moved_or_deleted(namespace, &bare, current.as_deref(), request.old_head.as_deref()));
            }

            // The head is stored as given; a spelling the ref reader refuses is the request's fault,
            // and on a case-insensitive filesystem the presence check below would not catch it.
            if !forklift_core::util::object_utils::is_object_hash(&request.new_head) {
                return Err(unprocessable(
                    "The new head is not an object hash (64 lowercase hex characters).".to_string()
                ));
            }

            if !file_utils::does_object_exist(&request.new_head).map_err(internal)? {
                // The negotiation never asks for withheld content, so say why this one is absent.
                let why = match read_withheld()?.contains(&request.new_head) {
                    true => " This server's host withholds it, so it never takes it.",
                    false => "",
                };
                return Err(unprocessable(format!(
                    "The new head {} has not been uploaded.{}",
                    request.new_head, why
                )));
            }

            // A ref must never point at missing history. The write below takes this verification
            // rather than walking the history a second time, and a further attempt reuses it: it
            // covers the request's two heads, whose content no other writer can change.
            let verified = match verified_closure.take() {
                Some(verified) => verified,
                // An incomplete history is classified, so a lift can tell it apart and upload again.
                None => audit_utils::verify_parcel_closure_for_write(&request.new_head, request.old_head.as_deref())
                    .map_err(|refusal| unprocessable(refusal.for_ref_update()))?,
            };

            // An early refusal too, after the closure check, so its walk of the new history reads
            // only what is present: the ref write applies the same rule under its lock.
            let mut recreated: Option<String> = None;

            if request.old_head.is_none() && !is_meta {
                if let Some(head) = forklift_core::util::tombstone_utils::read_tombstones().map_err(damage_refusal)?.named.remove(&bare) {
                    let acknowledged = request.tombstone.as_deref() == Some(head.as_str());

                    let descends = acknowledged
                        && merge_utils::is_ancestor(&head, &request.new_head).map_err(internal)?;

                    if !acknowledged || !descends {
                        return Err(pallet_deleted(&bare, &head, acknowledged));
                    }

                    recreated = Some(head);
                }
            }

            if let Some(old_head) = &request.old_head {
                // The one sanctioned non-fast-forward: the office lift right after a
                // re-genesis, where the (already replaced) anchor adopts exactly the head
                // being moved away from — the old chain is pinned, not dropped.
                let adopted_reset = is_office
                    && anchor.as_ref().and_then(|anchor| anchor.adopts.as_deref()) == Some(old_head.as_str());

                if !adopted_reset && !merge_utils::is_ancestor(old_head, &request.new_head).map_err(internal)? {
                    return Err((
                        StatusCode::CONFLICT,
                        "The update is not a fast-forward; the protocol has no force \
                        push. Lower, consolidate, and lift the merge.".to_string()
                    ));
                }
            }

            // Every head a user pallet takes can be checked out. Judged against the old head, or a
            // creation against the tombstone this warehouse itself stores, never the request's
            // tombstone field, which is free client data when none stands. A meta pallet's tree
            // carries `.forklift` by design, so the namespace decides, never the tree's shape.
            if !is_meta {
                let base = request.old_head.as_deref().or(recreated.as_deref());

                if let Some(path) = audit_utils::first_metadata_folder_path(
                    &request.new_head, base, &object_utils::load_parcel, &object_utils::load_tree, &object_utils::load_tree,
                ).map_err(internal)? {
                    return Err(unprocessable(audit_utils::head_not_materializable_message(&path)));
                }
            }

            // The events of this update (delivered after the commit): the ref move, plus
            // any revocations an office update carries — the old office state must be
            // read *before* the head moves.
            let mut events: Vec<HookEvent> = Vec::new();

            let mut ref_event = HookEvent::new(EVENT_PALLET_UPDATED);
            ref_event.warehouse = warehouse_id.clone();
            ref_event.operator = acting_operator.clone();
            ref_event.pallet = Some(name.clone());
            ref_event.old_head = request.old_head.clone();
            ref_event.new_head = Some(request.new_head.clone());
            events.push(ref_event);

            if is_office {
                // The office chain carries the keys; it must verify against the anchor.
                let anchor = anchor.ok_or(unprocessable(
                    "Establish the trust anchor (PUT /v1/trust) before lifting the office.".to_string()
                ))?;

                // Authentic is not authorized: the walk also holds every office parcel within
                // its *signer's* privileges (admins change anything; everyone else only their
                // own keys). That is a content invariant, so it holds no matter which token
                // transported the chain. Its refusal is a 403 when the lift brought the overstep,
                // and a 422 when the office already held it.
                let new_office_state = audit_utils::verify_office_chain_memoized(&anchor, &request.new_head)
                    .map_err(|error| office_lift_refusal(error, office_head.as_deref()))?;

                // Newly revoked keys (active before, revoked after) become events. The
                // pre-lift roster is the office state already read above: on this branch
                // the office pallet *is* the target pallet, so its pre-lift head is
                // exactly the request's old head, which the pallet-head check above
                // already pinned this update to.
                let old_office_state = office_state_at(&office_head)?;

                for key in &new_office_state.keys {
                    let was_active = old_office_state.find_key(&key.key_id)
                        .map(|old| old.is_active())
                        .unwrap_or(true);

                    if key.retired_at.is_some() && was_active {
                        let mut event = HookEvent::new(EVENT_KEY_REVOKED);
                        event.warehouse = warehouse_id.clone();
                        event.operator = Some(key.operator.clone());
                        event.key_id = Some(key.key_id.clone());
                        event.detail = key.revocation_reason
                            .map(|reason| reason.as_str().to_string());
                        events.push(event);
                    }
                }
            } else if let Some(rules) = pallet_rules {
                // A meta pallet is checked by the warehouse's trust state, never skipped: under
                // trust every new parcel must be signed by an office key, in a warehouse that
                // never enrolled it must be unsigned and declare one operator, and either way it
                // stamps only its own cells and keeps the pallet's rules. The content check
                // subsumes the signature audit below.
                meta_tree::verify_meta_update(
                    &bare,
                    rules,
                    meta_tree::Admission::Admit,
                    anchor.as_ref(),
                    office_head.as_deref(),
                    request.old_head.as_deref(),
                    &request.new_head
                ).map_err(unprocessable)?;
            } else if let Some(anchor) = anchor {
                // A trusted warehouse accepts nothing a local audit would reject. This is the
                // user-pallet path (the office and the meta-tree pallets took the branches above). The office head is the one
                // already read above, not a second lookup.
                let office_head = office_head.ok_or(unprocessable(
                    "Trust is established but the office pallet is missing; lift the \
                    office first.".to_string()
                ))?;

                let office_state = audit_utils::verify_office_chain_memoized(&anchor, &office_head)
                    .map_err(|error| unprocessable(error.into()))?;

                // Incremental: everything reachable from old_head was verified when
                // old_head was committed, so only the new slice is audited.
                audit_utils::verify_pallet_history(
                    &request.new_head,
                    &anchor,
                    &office_state,
                    request.old_head.as_deref().as_slice()
                ).map_err(|refusal| unprocessable(refusal.into()))?;
            }

            ref_write_test_checkpoint(&name);

            // The compare-and-set against the request's old head, under the warehouse's ref lock, so a
            // local bay's writer that moved the ref since the check above is refused, not overwritten;
            // and so is one that moved the office or the anchor the decision above consumed.
            let preconditions = RefPreconditions {
                office: OfficePrecondition::At(office_head_read),
                anchor: match anchor_bytes {
                    Some(bytes) => AnchorPrecondition::Bytes(bytes),
                    None => AnchorPrecondition::Absent,
                },
                pallets: PalletsPrecondition::NotConsumed,
            };

            // Owed from before the write may land until it returns; its apply discharges it.
            took_owe.store(true, std::sync::atomic::Ordering::SeqCst);
            let owed = counted.0.owe_apply();
            let written = pallet_utils::update_pallet_head_verified(
                namespace, &bare, request.old_head.as_deref(), &request.new_head, &preconditions, &verified,
                request.tombstone.as_deref()
            );
            drop(owed);

            match written {
                Ok(()) => return Ok(events),
                // The office or the anchor the decision consumed moved under it, the pallet did
                // not: decide again against what stands now, rather than tell the client to lower a
                // pallet that never moved. A grant revoked meanwhile is then refused as a later
                // request is.
                Err(RefUpdateError::OfficeMoved { .. } | RefUpdateError::AnchorMoved)
                    if attempt < REF_UPDATE_ATTEMPTS => {
                    verified_closure = Some(verified);
                }
                Err(RefUpdateError::Moved(moved)) => {
                    return Err(pallet_moved_or_deleted(namespace, &bare, moved.found.as_deref(), request.old_head.as_deref()));
                }
                Err(RefUpdateError::OfficeMoved { found }) => return Err(office_moved(found.as_deref())),
                Err(RefUpdateError::AnchorMoved) => return Err(anchor_moved()),
                Err(moved @ (RefUpdateError::PalletsMoved { .. } | RefUpdateError::WitnessMoved { .. })) => {
                    return Err((StatusCode::CONFLICT, moved.to_string()));
                }
                // The fast-forward check above refuses such a move first; a conflict all the same.
                Err(backward @ RefUpdateError::Backward { .. }) => return Err((StatusCode::CONFLICT, backward.to_string())),
                Err(refused @ RefUpdateError::Incomplete { .. }) => {
                    return Err(unprocessable(refused.to_string()));
                }
                // A head naming redacted content is refused however often it is sent: the client
                // must write a head without it. Framed, so the client reads its code.
                Err(refused @ RefUpdateError::Redacted { .. }) => {
                    return Err(unprocessable(refused.into()));
                }
                Err(RefUpdateError::Deleted { pallet, head, acknowledged }) => {
                    return Err(pallet_deleted(&pallet, &head, acknowledged));
                }
                Err(RefUpdateError::Failed(e)) => return Err(damage_refusal(e)),
            }

        }
    });

    // The write and what follows it run apart from the request, so a client that hangs up never
    // leaves an apply the write owed undischarged and unasked for.
    let settled = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            match write.await {
                Ok(events) => {
                    for event in events {
                        emit_event(&state, event);
                    }

                    after_ref_write(&state, warehouse).await;
                    Ok(())
                }
                Err(error) => {
                    after_failed_ref_write(&state, warehouse, took.load(std::sync::atomic::Ordering::SeqCst)).await;
                    Err(error)
                }
            }
        }
    }).await;

    match settled {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(internal(format!("A ref write failed: {}", error))),
    }
}

/// `DELETE /v1/pallets/{name}` — remove a user pallet's ref, conditioned on the head the client
/// read (`old_head`). The removal leaves a tombstone holding that head (this root is served, so
/// it carries the mark): gc keeps the history, the handshake lists it, and re-creating the name
/// must descend from it and acknowledge it. Framed as [`post_ref_update`] is: the same
/// authorization (deleting a ref is moving it), decided again when the office or the anchor moved
/// under the write. Refused, as the local delete is, for the pallet a bay of this root stands on
/// (the default pallet first, which a clone checks out) and for one an open haul names.
async fn delete_ref(State(state): State<Arc<AppState>>,
                    Extension(principal): Extension<Principal>,
                    Path(params): Path<PathParams>,
                    Query(request): Query<RefDeleteRequest>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    let name = match param(&params, "name") {
        Ok(name) => name,
        Err(error) => return error_response(error),
    };

    // Parsed before the admission hook, as in [`post_ref_update`].
    let pallet_ref = match pallet_utils::PalletRef::parse(&name).map_err(unprocessable) {
        Ok(pallet_ref) => pallet_ref,
        Err(error) => return error_response(error),
    };

    if let Err(error) = check_admission(&state, &params, &principal, "ref_delete", Some(&name)).await {
        return error_response(error);
    }

    let warehouse_id = params.get("warehouse").cloned();

    let acting_operator = match &principal {
        Principal::Operator(identifier) => Some(identifier.clone()),
        _ => None,
    };

    let counted = RefWriteCounted::start(&warehouse);
    // Whether the write reached the point where it may land, and so owes an apply.
    let took = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let took_owe = Arc::clone(&took);
    let deletion = blocking(Arc::clone(&warehouse), move || {
        let counted = counted;
        if pallet_ref.namespace == pallet_utils::PalletNamespace::Meta {
            return Err(unprocessable(format!(
                "Meta pallet \"{}\" cannot be deleted: it is tracked metadata, not a line of work.",
                name
            )));
        }

        let bare = pallet_ref.name.clone();
        let old_head = request.old_head.clone();
        let mut attempt = 0;

        loop {
            attempt += 1;

            // Read once per attempt, and pinned by the removal's preconditions below, as
            // post_ref_update reads and pins them.
            let (anchor, anchor_bytes) = match office_utils::read_trust_anchor_with_bytes().map_err(internal)? {
                Some((anchor, bytes)) => (Some(anchor), Some(bytes)),
                None => (None, None),
            };
            let office_head = pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).map_err(internal)?;

            if let Principal::Operator(_) = &principal {
                let office_state = office_state_at(&office_head)?;
                authority_utils::may_move(&principal, &anchor, &office_state, &pallet_ref).map_err(forbidden)?;
            }

            let current = pallet_utils::get_pallet_head_in(pallet_utils::PalletNamespace::User, &bare).map_err(internal)?;

            if current.as_deref() != Some(old_head.as_str()) {
                // A retried delete whose first try landed (its answer lost) finds the tombstone it
                // wrote: the pallet is deleted at the head the client read, which is what it asked.
                if current.is_none() && already_deleted(&bare, &old_head)? {
                    return Ok(None);
                }

                return Err(pallet_moved_or_deleted(pallet_utils::PalletNamespace::User, &bare, current.as_deref(), Some(&old_head)));
            }

            if pallet_utils::read_position().map_err(internal)?.pallet == bare {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "Pallet \"{}\" is this remote's default pallet, which a new warehouse checks \
                        out; it cannot be deleted. Nothing was deleted.",
                        bare
                    ),
                ));
            }

            if let Some((which, _)) = bay_utils::bays_on(&bare).map_err(internal)?.into_iter().next() {
                return Err((
                    StatusCode::CONFLICT,
                    format!(
                        "A bay of this remote ({}) stands on pallet \"{}\"; it cannot be deleted. \
                        Nothing was deleted.",
                        which, bare
                    ),
                ));
            }

            // A read-time check, as the local delete's is: a haul opened after it still holds its
            // proposed head, and the tombstone the target's last one.
            forklift_core::util::haul_utils::refuse_an_open_haul(&bare, &forklift_core::util::haul_utils::read_hauls().map_err(internal)?)
                .map_err(|refusal| (StatusCode::CONFLICT, refusal))?;

            let preconditions = RefPreconditions {
                office: OfficePrecondition::At(office_head),
                anchor: match anchor_bytes {
                    Some(bytes) => AnchorPrecondition::Bytes(bytes),
                    None => AnchorPrecondition::Absent,
                },
                pallets: PalletsPrecondition::NotConsumed,
            };

            // Owed from before the removal may land until it returns, as for a ref update.
            took_owe.store(true, std::sync::atomic::Ordering::SeqCst);
            let owed = counted.0.owe_apply();
            let removal = pallet_utils::remove_pallet_head_in(
                pallet_utils::PalletNamespace::User, &bare, &old_head, &preconditions, None
            );
            drop(owed);

            let removed = match removal {
                Ok(()) => true,
                Err(RefUpdateError::OfficeMoved { .. } | RefUpdateError::AnchorMoved) if attempt < REF_UPDATE_ATTEMPTS => false,
                // A retry racing the try that landed finds the ref gone under the lock: its
                // tombstone at the head the client read says it is done.
                Err(RefUpdateError::Moved(moved)) if moved.found.is_none() && already_deleted(&bare, &old_head)? => return Ok(None),
                Err(RefUpdateError::Moved(moved)) => return Err(pallet_moved_or_deleted(
                    pallet_utils::PalletNamespace::User, &bare, moved.found.as_deref(), Some(&old_head)
                )),
                Err(RefUpdateError::OfficeMoved { found }) => return Err(office_moved(found.as_deref())),
                Err(RefUpdateError::AnchorMoved) => return Err(anchor_moved()),
                // A failed removal can still have landed, and then its tombstone stands: what the
                // ref holds now decides.
                Err(RefUpdateError::Failed(e)) => match pallet_utils::get_pallet_head_in(pallet_utils::PalletNamespace::User, &bare) {
                    Ok(None) => true,
                    _ => return Err(damage_refusal(e)),
                },
                // Refused however often it is sent, as the ref update answers it; a removal names
                // no new head, so this is not reached today.
                Err(refused @ RefUpdateError::Redacted { .. }) => return Err(unprocessable(refused.into())),
                Err(error) => return Err((StatusCode::CONFLICT, error.into())),
            };

            if removed {
                let mut event = HookEvent::new(EVENT_PALLET_DELETED);
                event.warehouse = warehouse_id.clone();
                event.operator = acting_operator.clone();
                event.pallet = Some(name.clone());
                event.old_head = Some(old_head.clone());

                return Ok(Some(event));
            }
        }
    });

    // Apart from the request, as a ref update's is ([`post_ref_update`]).
    let settled = tokio::spawn({
        let state = Arc::clone(&state);
        async move {
            match deletion.await {
                Ok(Some(event)) => {
                    emit_event(&state, event);

                    after_ref_write(&state, warehouse).await;
                    Ok(())
                }
                Ok(None) => {
                    after_failed_ref_write(&state, warehouse, took.load(std::sync::atomic::Ordering::SeqCst)).await;
                    Ok(())
                }
                Err(error) => {
                    after_failed_ref_write(&state, warehouse, took.load(std::sync::atomic::Ordering::SeqCst)).await;
                    Err(error)
                }
            }
        }
    }).await;

    match settled {
        Ok(Ok(())) => StatusCode::OK.into_response(),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(internal(format!("A pallet deletion failed: {}", error))),
    }
}

/// What follows a ref write this warehouse took. The write's answer waits for the apply of every
/// redaction the write let apply ([`apply_redactions`]), so content a write let apply is refused
/// before the write is answered, then cuts the downloads of the bundle the apply withdrew. The
/// deletion after is a redaction run's ([`run_redactions`]), and is not waited for. So is the
/// bundle rebuild: the accepted-lift counter's, or, when the write applied a redaction, the run's
/// after its deletion, so no build runs beside the drain.
///
/// An apply that fails, or cannot decide a redaction, is logged, and the write is answered all
/// the same: the ref is written, and the apply the write owed ([`WarehouseHandle::owe_apply`])
/// stays owed, so no content is served until the run's retry or a later write's apply discharges
/// it.
async fn after_ref_write(state: &Arc<AppState>, warehouse: Arc<WarehouseHandle>) {
    let (applied, apply) = apply_discharging(&warehouse).await;

    if applied {
        cut_bundle_downloads(&warehouse);
        warehouse.rebuild_after_drain.store(true, std::sync::atomic::Ordering::SeqCst);
    } else {
        maybe_rebuild_bundle(state, Arc::clone(&warehouse));
    }

    if let Err(error) = apply {
        tracing::error!(error, "applying the redactions failed; the redaction run retries it");
    }

    run_redactions(Arc::clone(state), warehouse);
}

/// What follows a ref write request here that moved no ref, answered with an error or as already
/// done. One that reached its write (`owed`) owes an apply ([`WarehouseHandle::owe_apply`]),
/// carried out here before the answer, as a write's is, so content is served again without
/// waiting for another write; one that fails asks for a redaction run, as a write does, and so
/// does one that settles it while a round has left the bundle rebuild for after it. One
/// refused before its write owes nothing and does nothing here: it neither scans the warehouse
/// nor cuts short the wait of a run retrying an apply or a deletion.
async fn after_failed_ref_write(state: &Arc<AppState>, warehouse: Arc<WarehouseHandle>, owed: bool) {
    if !owed || !warehouse.apply_owed() {
        return;
    }

    let (applied, apply) = apply_discharging(&warehouse).await;

    if applied {
        cut_bundle_downloads(&warehouse);
        warehouse.rebuild_after_drain.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    if let Err(error) = apply {
        tracing::error!(error, "applying the redactions failed; the redaction run retries it");
    }

    // A rebuild a round left for after the apply ([`finish_round`]) is a run's to carry out.
    if applied || warehouse.apply_owed() || warehouse.rebuild_after_drain.load(std::sync::atomic::Ordering::SeqCst) {
        run_redactions(Arc::clone(state), warehouse);
    }
}

/// How long the redaction deleter waits for a drained warehouse's requests to end before it gives
/// up and lets the warehouse be served again, its deletion owed. Every request refused meanwhile
/// is a `503`, so this bounds how long a warehouse is unavailable for one attempt.
const DRAIN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// How often a drain looks whether the warehouse's requests have ended.
const DRAIN_POLL: std::time::Duration = std::time::Duration::from_millis(20);

/// How long a redaction run waits for a running bundle rebuild to end before it drains anyway: a
/// rebuild holds the handle throughout and cannot be cut, so draining while one runs would only
/// make clients wait out a drain that cannot finish.
const REBUILD_WAIT: std::time::Duration = std::time::Duration::from_secs(600);

/// The first and longest wait before a redaction run retries a deletion it owes. Each owed
/// attempt doubles the wait; a ref write here starts a run at once.
const FIRST_RETRY: std::time::Duration = std::time::Duration::from_secs(30);
const LAST_RETRY: std::time::Duration = std::time::Duration::from_secs(3600);

/// The first and longest wait before a redaction run retries an apply it owes. Each owed attempt
/// doubles the wait, from shorter than a deletion's: no content is served while one is owed. One
/// owed for good (a head the scan cannot read) costs a scan every [`LAST_APPLY_RETRY`]; a ref
/// write here applies at once.
const APPLY_RETRY: std::time::Duration = std::time::Duration::from_secs(10);
const LAST_APPLY_RETRY: std::time::Duration = std::time::Duration::from_secs(300);

/// How long a content request waits for the applies owed here before it is refused with a `503`
/// ([`await_applies`]). Shorter than [`DRAIN_WAIT`], so a drain started meanwhile outlasts the
/// requests waiting here.
const APPLY_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
const _: () = assert!(APPLY_WAIT.as_secs() < DRAIN_WAIT.as_secs());

/// Start a redaction run for `warehouse`, or, when one runs already, have it go round again.
///
/// A round applies every redaction the warehouse's scan now finds unheld ([`apply_redactions`])
/// when an apply is owed (the handle's first, or one a write could not carry out), cuts the
/// downloads of the bundle that apply withdrew, deletes the applied and withheld content no census
/// root holds ([`delete_applied_content`]) and rebuilds the bundle an apply or a takedown
/// withdrew. A deletion that could not be carried out — the warehouse could not be drained, or
/// other maintenance holds its store — is owed, as is an apply that failed: the run tries again
/// later, waiting longer each time, while the warehouse is served. A run starts after each ref
/// write, once for each handle served, and when the withheld record gains a hash
/// ([`watch_withheld`]), so a deletion owed when a process stopped is carried out by the next one
/// to serve the warehouse.
fn run_redactions(state: Arc<AppState>, warehouse: Arc<WarehouseHandle>) {
    use std::sync::atomic::Ordering;

    // Asked for before the running run is looked at, so that run, ending, sees it. Only a run
    // waiting out a retry is woken: no permit is left behind to cut a later wait short.
    warehouse.redactions_asked.fetch_add(1, Ordering::SeqCst);
    warehouse.redact_again.store(true, Ordering::SeqCst);
    warehouse.redaction_wake.notify_waiters();

    if warehouse.redacting.swap(true, Ordering::SeqCst) {
        return;
    }

    tokio::spawn(async move {
        let mut warehouse = warehouse;
        let mut retry = FIRST_RETRY;
        let mut apply_retry = APPLY_RETRY;

        loop {
            warehouse.redact_again.store(false, Ordering::SeqCst);

            let owes = redaction_round(&state, &warehouse).await;
            let owed = owes.deletion || owes.apply;

            if !owes.deletion {
                retry = FIRST_RETRY;
            }
            if !owes.apply {
                apply_retry = APPLY_RETRY;
            }

            // Listening before the ask is looked at, so an ask after the look wakes the wait.
            let wake = Arc::clone(&warehouse.redaction_wake);
            let woken = wake.notified();
            tokio::pin!(woken);
            woken.as_mut().enable();

            if warehouse.redact_again.load(Ordering::SeqCst) {
                continue;
            }

            if !owed {
                warehouse.redacting.store(false, Ordering::SeqCst);

                // Asked for between the look above and the store: the ask found this run still
                // marked, so this run serves it, unless another has started since.
                if !warehouse.redact_again.load(Ordering::SeqCst) || warehouse.redacting.swap(true, Ordering::SeqCst) {
                    return;
                }

                continue;
            }

            // The wait holds no reference to the handle, so an idle warehouse may still be let go
            // meanwhile; the next handle served for it starts a run of its own.
            let weak = Arc::downgrade(&warehouse);
            drop(warehouse);

            // Owing nothing, the run ended above; so without a deletion, it owes an apply.
            let wait = match (owes.deletion, owes.apply) {
                (true, true) => retry.min(apply_retry),
                (true, false) => retry,
                (false, _) => apply_retry,
            };

            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = woken => {}
            }

            warehouse = match still_served(&state, &weak, false) {
                Served::Handle(warehouse) => warehouse,
                Served::Draining | Served::Gone => return,
            };

            if owes.deletion {
                retry = (retry * 2).min(LAST_RETRY);
            }
            if owes.apply {
                apply_retry = (apply_retry * 2).min(LAST_APPLY_RETRY);
            }
        }
    });
}

/// The handle `weak` names, when it is still the one served for its warehouse. Taken under the
/// registry mutex, as every reference to a served handle is (see [`resolve_handle`]): a handle
/// being let go is never taken back. With `undrained`, a handle being drained is not taken either,
/// as no request's is ([`drain`]): it is found by the served reference alone, so no reference to
/// it is taken meanwhile.
fn still_served(state: &AppState, weak: &std::sync::Weak<WarehouseHandle>, undrained: bool) -> Served {
    let Ok(registry) = state.warehouses.lock() else {
        return Served::Gone;
    };

    let served = match &state.mode {
        ServeMode::Single(served) => std::ptr::eq(Arc::as_ptr(served), weak.as_ptr()).then_some(served),
        ServeMode::Multi { .. } => registry.entries.values().map(|entry| &entry.handle)
            .find(|handle| std::ptr::eq(Arc::as_ptr(handle), weak.as_ptr())),
    };

    match served {
        None => Served::Gone,
        Some(handle) if undrained && handle.draining.load(std::sync::atomic::Ordering::SeqCst) => Served::Draining,
        Some(handle) => Served::Handle(Arc::clone(handle)),
    }
}

/// What [`still_served`] finds of a handle.
enum Served {
    /// It is served, and this is a reference to it.
    Handle(Arc<WarehouseHandle>),

    /// It is served, and being drained.
    Draining,

    /// It is no longer served.
    Gone,
}

/// How often a served warehouse's watch ([`watch_withheld`]) reads its withheld record. The
/// record is written by `forklift-server withhold`, another process, so this bounds how long a
/// bundle download of content just withheld keeps streaming, and how long its deletion waits.
const WITHHELD_POLL: std::time::Duration = std::time::Duration::from_secs(5);

/// Start serving `handle`, a handle served just now: read its withheld record as its watch's
/// baseline, start its first redaction run, then watch the record ([`watch_withheld`]). The
/// baseline is read first, so a hash withheld after it is the watch's to act on, at worst asking
/// for one more round. Requests are served before it is read, so a hash in it may have been
/// withheld while a bundle download already ran: when its bundle is withdrawn, the downloads are
/// cut, as the watch cuts them. A run that finds something withheld and no bundle published
/// rebuilds it, since withholding withdrew it whether or not a copy is here to delete.
fn start_serving(state: &Arc<AppState>, handle: &Arc<WarehouseHandle>) {
    let state = Arc::clone(state);
    let handle = Arc::clone(handle);

    tokio::spawn(async move {
        let baseline = blocking(Arc::clone(&handle), || {
            let withheld = read_withheld()?;
            let withdrawn = !withheld.is_empty() && !bundle_utils::get_latest_bundle_path().exists();

            Ok((withheld_hashes(&withheld), withdrawn))
        }).await;

        let seen = match baseline {
            Ok((seen, withdrawn)) => {
                if withdrawn {
                    cut_bundle_downloads(&handle);
                    handle.rebuild_after_drain.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Some(seen)
            }
            Err((_, error)) => {
                tracing::error!(error, "the withheld record could not be read; reading it again later");
                None
            }
        };

        let weak = Arc::downgrade(&handle);
        run_redactions(Arc::clone(&state), handle);

        watch_withheld(state, weak, seen).await;
    });
}

/// The hashes `withheld` names.
fn withheld_hashes(withheld: &WithheldSet) -> std::collections::BTreeSet<String> {
    withheld.entries().map(|(hash, _)| hash.clone()).collect()
}

/// Watch the withheld record of the handle `weak` names while it is served, every
/// [`WITHHELD_POLL`]. When the record withholds a hash `seen` did not (or, with no `seen`, when
/// it is first read withholding anything), the bundle downloads are cut, since a bundle published
/// before may hold the content, and a redaction round is asked for, which deletes the copies no
/// census root holds ([`delete_applied_content`]) and rebuilds the bundle after, as one after a
/// write that applied a redaction does.
///
/// Between reads the watch holds no reference to the handle, and it takes none while the handle is
/// drained, so a drain and an idle let-go are not held up by it; it ends once the handle is no
/// longer served. A record that cannot be read is read again at the next poll: the requests that
/// read it are refused meanwhile.
async fn watch_withheld(state: Arc<AppState>,
                        weak: std::sync::Weak<WarehouseHandle>,
                        mut seen: Option<std::collections::BTreeSet<String>>) {
    loop {
        tokio::time::sleep(WITHHELD_POLL).await;

        // A drained warehouse is skipped this poll; the round draining it deletes what it can.
        let warehouse = match still_served(&state, &weak, true) {
            Served::Handle(warehouse) => warehouse,
            Served::Draining => continue,
            Served::Gone => return,
        };

        let now = match blocking(Arc::clone(&warehouse), || read_withheld()).await {
            Ok(withheld) => withheld_hashes(&withheld),
            Err((_, error)) => {
                tracing::error!(error, "the withheld record could not be read; reading it again later");
                continue;
            }
        };

        let gained = match &seen {
            Some(seen) => now.iter().any(|hash| !seen.contains(hash)),
            None => !now.is_empty(),
        };
        seen = Some(now);

        if gained {
            cut_bundle_downloads(&warehouse);
            warehouse.rebuild_after_drain.store(true, std::sync::atomic::Ordering::SeqCst);
            run_redactions(Arc::clone(&state), warehouse);
        }
    }
}

/// What a redaction round ([`redaction_round`]) leaves owed, for a later round to try again.
struct RoundOwes {
    /// A deletion: it could not be carried out.
    deletion: bool,

    /// An apply: one is owed that this round did not discharge.
    apply: bool,
}

/// One round of a redaction run ([`run_redactions`]).
async fn redaction_round(state: &AppState, warehouse: &Arc<WarehouseHandle>) -> RoundOwes {
    use std::sync::atomic::Ordering;

    // A rebuild a write's apply called for, after this round's drain. Taken first, so the flag
    // set again by the end of the round says a write applied a redaction while it ran.
    let held_back = warehouse.rebuild_after_drain.swap(false, Ordering::SeqCst);

    // Only an apply owed: a write applies what it let apply before it is answered.
    let applied = match warehouse.apply_owed() {
        false => false,
        true => {
            let (applied, apply) = apply_discharging(warehouse).await;

            // Still owed: a later round retries it.
            if let Err(error) = apply {
                tracing::error!(error, "applying the redactions failed; retrying later");
            }

            applied
        }
    };

    // The apply withdrew the bundle, which may hold the content; downloads of it stop here.
    if applied {
        cut_bundle_downloads(warehouse);
    }

    let owed = delete_applied_content(state, warehouse).await;

    finish_round(state, warehouse, applied, held_back, owed);

    RoundOwes { deletion: owed, apply: warehouse.apply_owed() }
}

/// Rebuild the bundle after a redaction round's deletion, when it is due (the round `applied` a
/// redaction, or a write's apply called for it, `held_back`) or the drain held a rebuild back,
/// unless the run's next round would only find it in the way, and leaves it to that round:
/// - a write applied a redaction while the round ran, so the next round drains for it;
/// - an apply is owed, so no bundle is served until one settles it, and one that applies
///   withdraws the bundle again;
/// - the round applied a redaction and its deletion is `owed`, and a write asked for another
///   round, which drains for it at once. Only this round's own apply is left so: under traffic
///   that keeps both true, the next round rebuilds rather than leave the bundle withdrawn.
///
/// A write that applied nothing leaves the rebuild here, so a steady stream of them does not put
/// it off.
fn finish_round(state: &AppState, warehouse: &Arc<WarehouseHandle>, applied: bool, held_back: bool, owed: bool) {
    use std::sync::atomic::Ordering;

    // A rebuild the drain held back (`rebuild_again`) is carried on as one an apply calls for is.
    let due = applied || held_back || warehouse.rebuild_again.load(Ordering::SeqCst);

    // Each leaves a round to follow: the write asks for a run after setting the flag, and a run
    // asked again goes round again. That round's start takes the flag.
    let follows = warehouse.rebuild_after_drain.load(Ordering::SeqCst)
        || (applied && owed && warehouse.redact_again.load(Ordering::SeqCst));

    if follows || warehouse.apply_owed() {
        if due {
            warehouse.rebuild_after_drain.store(true, Ordering::SeqCst);

            // A refused write that settles the owed apply asks for a run when it sees the flag
            // ([`after_failed_ref_write`]); one settled before the flag was set leaves this run
            // to go round again for the rebuild.
            if !follows && !warehouse.apply_owed() {
                warehouse.redact_again.store(true, Ordering::SeqCst);
            }
        }
        return;
    }

    if due {
        rebuild_bundle(state, Arc::clone(warehouse), true);
    }
}

/// Delete the applied and withheld content this warehouse holds that no census root holds
/// ([`redaction_deleter::applied_copies`]), when there is any: drain the warehouse ([`drain`]),
/// then delete it ([`delete_drained`]) and serve the warehouse again. A handle that is not a
/// served warehouse's (a test fixture) holds no locks to delete under, and deletes nothing.
///
/// # Returns
/// Whether the deletion is owed: the census failed, or the warehouse could not be drained, or its
/// store was busy with other maintenance, or the deletion failed.
async fn delete_applied_content(state: &AppState, warehouse: &Arc<WarehouseHandle>) -> bool {
    if warehouse._serve_lock.is_none() {
        return false;
    }

    match blocking(Arc::clone(warehouse), || redaction_deleter::applied_copies().map_err(internal)).await {
        Ok(copies) if !copies.owes_deletion() => return false,
        Ok(_) => {}
        Err((_, error)) => {
            tracing::error!(error, "which redacted content is held could not be told; its deletion is owed");
            return true;
        }
    }

    // A rebuild running now would hold the drain open; it ends first.
    let rebuilding = tokio::time::Instant::now() + REBUILD_WAIT;
    while warehouse.bundling.load(std::sync::atomic::Ordering::SeqCst) && tokio::time::Instant::now() < rebuilding {
        tokio::time::sleep(DRAIN_POLL).await;
    }

    if !drain(state, warehouse, DRAIN_WAIT).await {
        tracing::warn!(
            root = %warehouse.root.display(),
            "requests did not end in time to drain the warehouse; deleting its redacted content is owed"
        );
        return true;
    }

    let worker = Arc::clone(warehouse);
    let (deletion, holds_store) = tokio::task::spawn_blocking(move || delete_drained(&worker)).await
        .unwrap_or_else(|error| (Err(format!("The deletion task failed: {}", error)), false));

    let owed = match deletion {
        Ok(deletion) => {
            tracing::info!(
                objects = deletion.objects.len(),
                chunks = deletion.chunks,
                bytes_freed = deletion.bytes_freed,
                held = deletion.held.len(),
                "deleted redacted content"
            );
            false
        }
        Err(error) => {
            tracing::warn!(error, "deleting redacted content is owed");
            true
        }
    };

    if !holds_store {
        retake_store_access(warehouse).await;
    }

    undrain(state, warehouse);

    owed
}

/// Mark `warehouse` drained, cut its bundle downloads, and wait up to `wait` ([`DRAIN_WAIT`]) for
/// every request on it to end.
///
/// From the mark on, every request that resolves the warehouse is refused, and every reference to
/// its handle is taken under the registry mutex (see [`resolve_handle`]), so once nothing holds
/// the handle but the registry (or single mode's own) and this run, nothing can again. A request
/// already answered whose body is in memory (an object, a batch, a subtree) dropped its reference
/// when the body was read: those bytes already left, and nothing here can call them back.
///
/// # Returns
/// Whether the warehouse is drained. When it is not, the mark is withdrawn.
async fn drain(state: &AppState, warehouse: &Arc<WarehouseHandle>, wait: std::time::Duration) -> bool {
    use std::sync::atomic::Ordering;

    let Ok(registry) = state.warehouses.lock() else {
        return false;
    };
    warehouse.draining.store(true, Ordering::SeqCst);
    drop(registry);

    // Requests waiting for an apply are refused now rather than hold the drain open.
    warehouse.applies_wake.notify_waiters();
    cut_bundle_downloads(warehouse);

    let deadline = tokio::time::Instant::now() + wait;

    loop {
        // The registry's (or single mode's) reference and this run's.
        if Arc::strong_count(warehouse) == 2 {
            return true;
        }

        if tokio::time::Instant::now() >= deadline {
            undrain(state, warehouse);
            return false;
        }

        tokio::time::sleep(DRAIN_POLL).await;
    }
}

/// Serve a drained warehouse again.
fn undrain(state: &AppState, warehouse: &WarehouseHandle) {
    let _registry = state.warehouses.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    warehouse.draining.store(false, std::sync::atomic::Ordering::SeqCst);
}

/// Delete the applied content of a drained warehouse that no bay here holds, under the store lock
/// and exclusive store access: the handle's shared store-access hold is traded for the exclusive
/// side for the deletion and taken back after it. The serve lock and the format lease stay held
/// throughout, so no other server and no upgrade comes in between. The caches of the warehouse's
/// objects are forgotten after, so nothing answers "present" for what was deleted.
///
/// # Returns
/// The deletion, or why it is owed (the store lock or the exclusive side was taken, or the
/// deletion failed), and whether the handle holds its shared store-access hold again: when it was
/// released and another process took the exclusive side before it was taken back, it does not.
fn delete_drained(warehouse: &WarehouseHandle) -> (Result<redaction_deleter::Deletion, String>, bool) {
    let _scope = StorageRootScope::enter(&warehouse.root);

    let Some(serve_lock) = warehouse._serve_lock.as_ref() else {
        return (Err("The warehouse is not served.".to_string()), true);
    };
    let mut store_access = warehouse.store_access.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    let deletion = (|| {
        // Taken before the shared hold is let go, so other maintenance refuses this attempt
        // without the hold ever being released for it.
        let store_lock = lock_utils::StoreLock::acquire()?;

        *store_access = None;

        let exclusive = StoreAccessLock::try_exclusive()?;

        redaction_deleter::delete_applied(&store_lock, serve_lock, &exclusive, &progress_utils::Progress::NONE)
    })();

    pack_utils::forget_warehouse();

    // What the deletion dropped and the journal it emptied change the candidates: read them again.
    *lock_gate(warehouse) = ChunkGate::default();

    if store_access.is_none() {
        match StoreAccessLock::try_shared() {
            Ok(hold) => *store_access = Some(hold),
            Err(error) => tracing::warn!(error, "the shared store-access hold could not be taken back yet"),
        }
    }

    (deletion, store_access.is_some())
}

/// Take a drained warehouse's shared store-access hold back, once whatever took the exclusive side
/// while it was let go is done. Until then the warehouse stays drained: serving it without the hold
/// would let that other process delete objects a request is relying on.
async fn retake_store_access(warehouse: &Arc<WarehouseHandle>) {
    for attempt in 1u64.. {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;

        // Said at once and every minute after: the warehouse answers 503 until this ends.
        if attempt % 60 == 1 {
            tracing::warn!(
                root = %warehouse.root.display(),
                seconds = attempt - 1,
                "another process holds the warehouse's store exclusively; it is refused until that ends"
            );
        }

        let worker = Arc::clone(warehouse);
        let retaken = tokio::task::spawn_blocking(move || {
            let _scope = StorageRootScope::enter(&worker.root);
            let mut store_access = worker.store_access.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

            match StoreAccessLock::try_shared() {
                Ok(hold) => {
                    // What the other process deleted is forgotten too.
                    pack_utils::forget_warehouse();
                    *store_access = Some(hold);
                    true
                }
                Err(_) => false,
            }
        }).await.unwrap_or(false);

        if retaken {
            return;
        }
    }
}

/// Apply every redaction of this warehouse's `@redactions` that its scan now finds unheld: a write
/// that moved the last head naming the content, or that recorded the redaction, is what lets one
/// apply. Each applied one is refused on every surface from then on. A warehouse with a remote
/// configured applies none here: it applies only on its remote's report, which this run does not
/// read, so a command run in the root (`lower`, `redact apply`) that reads it applies them.
///
/// # Returns
/// * `Ok(ApplyOutcome)`  - What it applied, and whether it decided every redaction.
/// * `Err(HandlerError)` - The redactions, the applied record or the configuration could not be
///                         read, or the apply could not be written.
async fn apply_redactions(warehouse: Arc<WarehouseHandle>) -> Result<ApplyOutcome, HandlerError> {
    blocking(warehouse, || {
        let before = read_applied()?;
        let read = redaction_utils::read_redactions().map_err(internal)?;

        if read.redactions.iter().all(|attributed| before.contains(&attributed.redaction.hash)) {
            return Ok(ApplyOutcome { applied: false, undecided: None });
        }

        let statuses = applied_redactions::apply(&read, None, None).map_err(internal)?;

        // Damage leaves a redaction undecided until the operator repairs it, so it is answered as
        // the damage, coded, not as a scan to retry.
        if let Some(damage) = statuses.iter().find_map(|status| match &status.state {
            applied_redactions::RedactionState::Undecided(why) if is_damage(why) => Some(why.clone()),
            _ => None,
        }) {
            return Err(damage_refusal(damage));
        }

        let undecided = statuses.iter().find_map(|status| match &status.state {
            applied_redactions::RedactionState::Undecided(why) => Some(format!(
                "Whether anything names the content of redaction {} could not be told: {}", status.hash, why
            )),
            _ => None,
        });

        Ok(ApplyOutcome { applied: read_applied()?.hashes().any(|hash| !before.contains(hash)), undecided })
    }).await
}

/// What [`apply_redactions`] did.
struct ApplyOutcome {
    /// Whether it applied a redaction, which withdrew the bundle.
    applied: bool,

    /// Why it could not tell whether a redaction is still named here, when it could not for one:
    /// that redaction may be one the warehouse should apply, and is left unapplied.
    undecided: Option<String>,
}

/// Apply ([`apply_redactions`]), and when it decides every redaction, discharge the applies owed
/// by the ref writes that returned before it began ([`WarehouseHandle::owe_apply`]). Its scan read
/// the warehouse after each of those writes had landed or failed, so whatever they let apply, it
/// applied.
///
/// # Returns
/// Whether it applied a redaction, and the error that left the applies owed, if one did.
async fn apply_discharging(warehouse: &Arc<WarehouseHandle>) -> (bool, Result<(), String>) {
    use std::sync::atomic::Ordering;

    let owed = warehouse.dischargeable();

    let outcome = match apply_redactions(Arc::clone(warehouse)).await {
        Ok(outcome) => outcome,
        Err((_, error)) => {
            if is_damage(&error) {
                *warehouse.apply_damage.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
                warehouse.applies_wake.notify_waiters();
            }

            return (false, Err(error));
        }
    };

    if let Some(why) = outcome.undecided {
        return (outcome.applied, Err(why));
    }

    *warehouse.apply_damage.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    warehouse.applies_discharged.fetch_max(owed, Ordering::SeqCst);
    warehouse.applies_wake.notify_waiters();

    (outcome.applied, Ok(()))
}

/// Wait, up to [`APPLY_WAIT`], until no apply is owed for `warehouse` (see
/// [`WarehouseHandle::applies_owed`]): content is answered only once every redaction a ref write
/// here may have let apply is applied, and so refused. Every request that answers with content
/// waits here first: an object, a batch, a subtree and the bundle.
///
/// # Returns
/// * `Ok(())`            - No apply is owed.
/// * `Err(HandlerError)` - A `503`: one is still owed at the bound, or the warehouse is drained, or
///                         the last apply stopped on damage ([`WarehouseHandle::apply_damage`]).
async fn await_applies(warehouse: &WarehouseHandle) -> Result<(), HandlerError> {
    let deadline = tokio::time::Instant::now() + APPLY_WAIT;

    loop {
        // Listening before the counters are looked at, so a discharge after the look wakes it.
        let woken = warehouse.applies_wake.notified();
        tokio::pin!(woken);
        woken.as_mut().enable();

        if warehouse.draining.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(draining_refusal());
        }

        if !warehouse.applies_closed() {
            return Ok(());
        }

        if let Some(damage) = warehouse.apply_damage.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone() {
            return Err(damage_refusal(damage));
        }

        tokio::select! {
            () = tokio::time::sleep_until(deadline) => return Err(applying_refusal()),
            () = woken => {}
        }
    }
}

/// The `503` of a content request while an apply is owed ([`await_applies`]).
fn applying_refusal() -> HandlerError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        "The warehouse is unavailable while it settles which content redacted from it is no longer \
        served. Try again shortly; if this persists, the server's log says why.".to_string(),
    )
}

/// Kick off a background bundle rebuild when the accepted-lift counter reaches the
/// configured threshold. At most one rebuild runs per warehouse at a time; the bundle
/// is written atomically, so serving continues undisturbed throughout.
fn maybe_rebuild_bundle(state: &AppState, warehouse: Arc<WarehouseHandle>) {
    use std::sync::atomic::Ordering;

    let Some(threshold) = state.rebuild_after_lifts else {
        return;
    };

    let lifts = warehouse.lifts_since_bundle.fetch_add(1, Ordering::SeqCst) + 1;

    if lifts < threshold {
        return;
    }

    rebuild_bundle(state, warehouse, false);
}

/// Kick off a background bundle rebuild, when rebuilds are configured and none runs for this
/// warehouse already. With `again` (a redaction was just applied, and the bundle withdrawn), a
/// rebuild already running is followed by another once it ends: it began before the redaction, so
/// it publishes nothing (see `bundle_utils::build_bundle`).
fn rebuild_bundle(state: &AppState, warehouse: Arc<WarehouseHandle>, again: bool) {
    use std::sync::atomic::Ordering;

    if state.rebuild_after_lifts.is_none() {
        return;
    }

    // Asked for before the running rebuild is looked at, so that rebuild, ending, sees it.
    if again {
        warehouse.rebuild_again.store(true, Ordering::SeqCst);
    }

    // A rebuild would hold the drain open; the redaction run that drains asks again after.
    if warehouse.draining.load(Ordering::SeqCst) {
        warehouse.rebuild_again.store(true, Ordering::SeqCst);
        return;
    }

    if warehouse.bundling.swap(true, Ordering::SeqCst) {
        return;
    }

    // This rebuild starts after whatever asked for another, so it serves that ask.
    warehouse.rebuild_again.store(false, Ordering::SeqCst);
    warehouse.lifts_since_bundle.store(0, Ordering::SeqCst);

    tokio::spawn(async move {
        loop {
            let worker = Arc::clone(&warehouse);

            let result = tokio::task::spawn_blocking(move || {
                let _scope = StorageRootScope::enter(&worker.root);

                bundle_utils::build_bundle()
            }).await;

            match result {
                Ok(Ok(stats)) => tracing::info!(
                    objects = stats.objects,
                    deltas = stats.deltas,
                    signatures = stats.signatures,
                    "rebuilt the bundle"
                ),
                Ok(Err(error)) => tracing::error!(error, "the bundle rebuild failed"),
                Err(error) => tracing::error!(%error, "the bundle rebuild task failed"),
            }

            warehouse.bundling.store(false, Ordering::SeqCst);

            // A drain began while this one ran: the ask stays for the redaction run to serve after.
            if warehouse.draining.load(Ordering::SeqCst) {
                break;
            }

            // Another was asked for while this one ran, and no other rebuild has started since.
            if !warehouse.rebuild_again.swap(false, Ordering::SeqCst) || warehouse.bundling.swap(true, Ordering::SeqCst) {
                break;
            }

            warehouse.lifts_since_bundle.store(0, Ordering::SeqCst);
        }
    });
}

/// `POST /v1/resolve` — operator identifiers → display names (DESIGN.html §8.12).
/// Resolution is server-mediated precisely so its policy is enforced and not
/// advisory: the server authenticates the caller, then asks its resolution hook (when
/// configured) which of the requested names this caller may see. No hook — or nothing
/// the caller may see — is an empty map, and the client shows the pseudonymous
/// identifiers. This only ever feeds display; it is never a verification input.
async fn post_resolve(State(state): State<Arc<AppState>>,
                      Extension(principal): Extension<Principal>,
                      Path(params): Path<PathParams>,
                      Json(request): Json<ResolveRequest<'static>>) -> Response {
    // A request against a warehouse that does not exist is a `404`, like every other
    // endpoint (resolution reads no warehouse state, but the route is warehouse-scoped).
    if let Err(error) = resolve_warehouse(&state, &params) {
        return error_response(error);
    }

    // Asked once per distinct identifier, and never for more than one request may name — on every
    // server, hook or not, so the limit is part of the request's contract rather than of how a
    // server is configured. The hook answer's cap scales with these identifiers, so they are
    // what bounds it.
    let identifiers: Vec<String> = match distinct_resolve_identifiers(&request.identifiers) {
        Ok(identifiers) => identifiers.into_iter().cloned().collect(),
        Err(refusal) => {
            tracing::warn!(refusal = %refusal, "refused a resolve request naming too many identifiers");
            return error_response(unprocessable(refusal));
        }
    };

    let Some(hook) = &state.resolution_hook else {
        // No directory is configured: the server knows no names. The client degrades
        // to the pseudonymous identifiers — never an error.
        return Json(ResolveResponse { names: BTreeMap::new() }).into_response();
    };

    // The caller travels to the hook so a policy-aware directory can tier its answer
    // (§8.12: guests resolve nothing, members only shared-warehouse peers, admins
    // everyone). This is exactly what a client-held shared secret could never do —
    // the reason resolution is server-mediated.
    let caller = match &principal {
        Principal::Operator(identifier) => Some(identifier.clone()),
        _ => None,
    };

    // What a directory can legitimately answer for these identifiers.
    let answer_cap = resolve_answer_cap(&identifiers);
    let hook_request = ResolutionHookRequest { caller, identifiers };

    let names = match post_hook(&state, hook, "resolution", &hook_request).await {
        Ok(response) if response.status().is_success() => {
            match body_utils::read_json_capped::<ResolutionHookResponse>(response, answer_cap).await {
                // Only what was asked is answered: a directory naming identifiers outside the
                // request does not reach the client. The identifiers are sorted and distinct.
                Ok(answer) => answer.names.into_iter()
                    .filter(|(id, _)| hook_request.identifiers.binary_search(id).is_ok()).collect(),
                Err(e) => {
                    tracing::warn!(error = ?e,
                        "the resolution hook's answer is unreadable, oversized or malformed; showing pseudonyms");
                    BTreeMap::new()
                }
            }
        }
        // The resolution failure policy is "show pseudonyms": a hook that is down or
        // answers badly resolves to nothing, never a failed request (unlike the hot
        // authentication/admission hooks, which fail closed).
        Ok(response) => {
            tracing::warn!(status = %response.status(), "the resolution hook refused; showing pseudonyms");
            BTreeMap::new()
        }
        Err(reason) => {
            tracing::warn!(reason, "the resolution hook is unreachable; showing pseudonyms");
            BTreeMap::new()
        }
    };

    Json(ResolveResponse { names }).into_response()
}

/// The body of a bundle download: `length` bytes of `file`, read by a task of its own that holds
/// the warehouse's handle until its last byte, so the warehouse is never let go while it streams.
///
/// The task stops the moment `cut` is cancelled, whether it is reading the file or waiting for a
/// slow client to take the next chunk, and the body ends in an error ([`CutBody`]): an
/// applied redaction cuts every download of the bundle it withdrew, which may hold the content, and
/// the redaction deleter's drain cuts whatever still streams. The client sees a download cut off,
/// and its resume finds the bundle gone or replaced.
fn bundle_body(file: tokio::fs::File,
               length: u64,
               warehouse: Arc<WarehouseHandle>,
               cut: tokio_util::sync::CancellationToken) -> Body {
    use tokio_stream::StreamExt;

    let (sender, receiver) = tokio::sync::mpsc::channel::<std::io::Result<Bytes>>(4);

    let body = CutBody {
        receiver,
        cut: Box::pin(cut.clone().cancelled_owned()),
        ended: false,
    };

    tokio::spawn(async move {
        let _warehouse = warehouse;
        let mut chunks = tokio_util::io::ReaderStream::new(tokio::io::AsyncReadExt::take(file, length));

        loop {
            let chunk = tokio::select! {
                biased;
                () = cut.cancelled() => return,
                chunk = chunks.next() => match chunk {
                    Some(chunk) => chunk,
                    None => return,
                },
            };

            tokio::select! {
                biased;
                () = cut.cancelled() => return,
                sent = sender.send(chunk) => if sent.is_err() {
                    // The client went away.
                    return;
                },
            }
        }
    });

    Body::from_stream(body)
}

/// A bundle download's body ([`bundle_body`]): the chunks its task reads, until the cut. The cut
/// is looked at before every chunk, here on the side the client reads, so it ends the body in an
/// error however many chunks are waiting: the connection is aborted, never closed as if the
/// download were whole.
struct CutBody {
    receiver: tokio::sync::mpsc::Receiver<std::io::Result<Bytes>>,
    cut: std::pin::Pin<Box<tokio_util::sync::WaitForCancellationFutureOwned>>,
    ended: bool,
}

impl tokio_stream::Stream for CutBody {
    type Item = std::io::Result<Bytes>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>,
                 context: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        use std::future::Future;

        if self.ended {
            return std::task::Poll::Ready(None);
        }

        if self.cut.as_mut().poll(context).is_ready() {
            self.ended = true;
            return std::task::Poll::Ready(Some(Err(std::io::Error::other("the bundle download was cut"))));
        }

        self.receiver.poll_recv(context)
    }
}

/// Cut every bundle download running from `warehouse` ([`bundle_body`]); those that start after
/// take the fresh token.
fn cut_bundle_downloads(warehouse: &WarehouseHandle) {
    let mut cut = warehouse.bundle_cut.lock().unwrap_or_else(std::sync::PoisonError::into_inner);

    cut.cancel();
    *cut = tokio_util::sync::CancellationToken::new();
}

/// `GET /v1/bundles/latest` — the most recent bundle, when one was built. The file is
/// streamed, never read into memory: bundles grow with the warehouse. The open handle
/// keeps serving the old file even if a rebuild replaces it mid-transfer.
///
/// A cut-off download resumes with `Range: bytes=N-`: the answer is a `206` with the bytes from
/// `N` on, under the strong tag of the file the handle opened, or `416` when `N` is at or past the
/// end. An `If-Range` naming any other tag gets the whole current bundle as a `200` instead (see
/// [`requested_range`]). Every answer states its length.
///
/// A file in another version than this build writes — one built before an upgrade — is answered
/// `404`, as if none were built, with a warning naming the rebuild: a client would only download it
/// to refuse it, then fall back to fetching objects anyway.
async fn get_bundle(State(state): State<Arc<AppState>>,
                    headers: HeaderMap,
                    Path(params): Path<PathParams>) -> Response {
    let warehouse = match resolve_warehouse(&state, &params) {
        Ok(warehouse) => warehouse,
        Err(error) => return error_response(error),
    };

    if let Err(error) = await_applies(&warehouse).await {
        return error_response(error);
    }

    // Taken before the bundle is opened: an apply withdraws the bundle before it cuts the
    // downloads, so a download that takes the token after the cut finds no bundle to open.
    let cut = warehouse.bundle_cut.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();

    // Resolved before a drain's mark, but took the token after its cut: the drain waits for this
    // request, so it stops here rather than stream past the cut.
    if warehouse.draining.load(std::sync::atomic::Ordering::SeqCst) {
        return error_response(draining_refusal());
    }

    // The path resolves synchronously under the scope; only the streaming is async.
    let path = {
        let _scope = StorageRootScope::enter(&warehouse.root);

        bundle_utils::get_latest_bundle_path()
    };

    let mut file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return error_response((StatusCode::NOT_FOUND, "No bundle has been built.".to_string()));
        }
        Err(e) => return error_response(internal(format!("Error while opening the bundle: {}", e))),
    };

    bundle_open_test_checkpoint(&warehouse.root);

    // The header is read from the handle that streams, so the version judged is the file sent.
    match bundle_is_current(&mut file).await {
        Ok(true) => {}
        Ok(false) => {
            tracing::warn!(
                root = %warehouse.root.display(),
                "the bundle is not in the format this server writes (built before an upgrade, or cut \
                short); answering 404 until it is rebuilt with `forklift-server bundle --root <root>` \
                (or the next automatic rebuild, when --rebuild-after-lifts is set)"
            );
            return error_response((
                StatusCode::NOT_FOUND, "No bundle in the current format has been built.".to_string()
            ));
        }
        Err(e) => return error_response(internal(format!("Error while reading the bundle: {}", e))),
    }

    // The validator and the length come from the handle that streams, never from the path: a
    // rebuild renamed over the path after the open must not lend this response its tag.
    let metadata = match file.metadata().await {
        Ok(metadata) => metadata,
        Err(e) => return error_response(internal(format!("Error while reading the bundle: {}", e))),
    };
    let length = metadata.len();
    let etag = bundle_etag(&metadata);
    let range = requested_range(&headers, etag.as_deref(), length);
    let offset = match range {
        BundleRange::Whole => 0,
        BundleRange::From(start) => start,
        BundleRange::Unsatisfiable => length,
    };

    if offset > 0 && offset < length {
        use tokio::io::AsyncSeekExt;

        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            return error_response(internal(format!("Error while reading the bundle: {}", e)));
        }
    }

    // It never sends more than the length it announced.
    let mut response = bundle_body(file, length - offset, warehouse, cut).into_response();
    let response_headers = response.headers_mut();

    response_headers.insert(axum::http::header::CONTENT_TYPE,
                            HeaderValue::from_static("application/octet-stream"));
    // Its packs are already compressed; the compression layer must not re-wrap them.
    response_headers.insert(axum::http::header::CONTENT_ENCODING, HeaderValue::from_static("identity"));
    response_headers.insert(axum::http::header::CONTENT_LENGTH, HeaderValue::from(length - offset));
    response_headers.insert(axum::http::header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));

    if let Some(etag) = etag {
        response_headers.insert(axum::http::header::ETAG,
                                HeaderValue::from_str(&etag).expect("a quoted run of hex digits"));
    }

    let content_range = match range {
        BundleRange::Whole => None,
        BundleRange::Unsatisfiable => {
            *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
            Some(format!("bytes */{}", length))
        }
        BundleRange::From(start) => {
            *response.status_mut() = StatusCode::PARTIAL_CONTENT;
            Some(format!("bytes {}-{}/{}", start, length - 1, length))
        }
    };

    if let Some(content_range) = content_range {
        response.headers_mut().insert(axum::http::header::CONTENT_RANGE,
                                      HeaderValue::from_str(&content_range).expect("digits and punctuation"));
    }

    response
}

/// The strong validator of the bundle `metadata` describes, as an opaque hash of its length, its
/// modification time at full precision and, on Unix, its inode: the tag names one built file
/// without showing a reader the store's inode numbers or build times. A rebuild publishes a newly
/// created file by rename and never rewrites the served one, so a rebuilt bundle gets a new tag
/// unless a reused inode meets an equal length and mtime; that costs a client one restart through
/// the bundle's digest, never a misread. `None` when the platform reports no modification time: no
/// tag is better than one a rebuild of equal length would share.
fn bundle_etag(metadata: &std::fs::Metadata) -> Option<String> {
    let modified = match metadata.modified().ok()?.duration_since(std::time::UNIX_EPOCH) {
        Ok(since) => format!("{}", since.as_nanos()),
        Err(before) => format!("-{}", before.duration().as_nanos()),
    };

    #[cfg(unix)]
    let inode = {
        use std::os::unix::fs::MetadataExt;

        metadata.ino().to_string()
    };
    #[cfg(not(unix))]
    let inode = String::new();

    let named = format!("{} {} {}", metadata.len(), modified, inode);
    let hex = blake3::hash(named.as_bytes()).to_hex();

    Some(format!("\"{}\"", &hex[..32]))
}

/// What part of the bundle a request is answered with.
#[derive(Debug, PartialEq)]
enum BundleRange {
    /// `200`, the whole file.
    Whole,
    /// `206`, the bytes from this offset on, which is inside the file.
    From(u64),
    /// `416`: the offset is at or past the end.
    Unsatisfiable,
}

/// The part of a bundle of `length` bytes, tagged `etag`, that a request asks for, by RFC 9110
/// §14.2 and §13.1.5. Only one range form is served: a single `Range: bytes=N-` (the unit in any
/// case, whitespace around the value ignored). It is honoured when there is no `If-Range`, or when
/// the one `If-Range` is exactly `etag`, the current strong tag. Anything else is `Whole`: a stale
/// or weak tag, a date, an `If-Range` with no tag to compare it with, another range form, or a
/// repeated `Range` or `If-Range` line.
fn requested_range(headers: &HeaderMap, etag: Option<&str>, length: u64) -> BundleRange {
    let mut ranges = headers.get_all(axum::http::header::RANGE).iter();
    let (Some(range), None) = (ranges.next(), ranges.next()) else {
        return BundleRange::Whole;
    };

    let mut conditions = headers.get_all(axum::http::header::IF_RANGE).iter();
    match (conditions.next(), conditions.next(), etag) {
        (None, _, _) => {}
        (Some(condition), None, Some(etag)) if condition.as_bytes() == etag.as_bytes() => {}
        _ => return BundleRange::Whole,
    }

    let Some(start) = range.to_str().ok().and_then(open_ended_range_start) else {
        return BundleRange::Whole;
    };

    if start < length { BundleRange::From(start) } else { BundleRange::Unsatisfiable }
}

/// `N` of a `bytes=N-` range value; `None` for any other value.
fn open_ended_range_start(value: &str) -> Option<u64> {
    let (unit, set) = value.trim().split_once('=')?;

    if !unit.trim().eq_ignore_ascii_case("bytes") {
        return None;
    }

    let digits = set.trim().strip_suffix('-')?;

    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }

    digits.parse().ok()
}

/// Whether the bundle `file` opens is in the version this build writes, leaving the handle back at
/// its first byte so the whole file streams.
async fn bundle_is_current(file: &mut tokio::fs::File) -> std::io::Result<bool> {
    use tokio::io::{AsyncReadExt, AsyncSeekExt};

    let mut prefix = vec![0u8; bundle_utils::BUNDLE_HEADER.len() + 1];
    let current = match file.read_exact(&mut prefix).await {
        Ok(_) => bundle_utils::is_current_bundle_prefix(&prefix),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => false,
        Err(e) => return Err(e),
    };

    file.seek(std::io::SeekFrom::Start(0)).await?;
    Ok(current)
}

/// Test seam: a `GET /v1/bundles/latest` on the armed warehouse root runs the armed closure once,
/// right after it opened the bundle, so a test can rename a rebuilt bundle over the path in exactly
/// that window. Keyed by root so tests serving bundles concurrently never meet.
#[cfg(test)]
static BUNDLE_OPEN_TEST_SEAMS: Mutex<BTreeMap<PathBuf, Box<dyn FnOnce() + Send>>> =
    Mutex::new(BTreeMap::new());

#[cfg(test)]
fn bundle_open_test_checkpoint(root: &std::path::Path) {
    let armed = BUNDLE_OPEN_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).remove(root);

    if let Some(run) = armed {
        run();
    }
}

#[cfg(not(test))]
fn bundle_open_test_checkpoint(_root: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use axum::routing::post;
    use forklift_core::globals::{StorageRootScope, FOLDER_NAME_FORKLIFT_ROOT};
    use forklift_core::model::operator::Operator;
    use forklift_core::model::tree_item::TreeItem;
    use forklift_core::util::{office_utils, scope_utils, warehouse_utils};
    use forklift_core::util::office_utils::{IdentityClass, OfficeState, Role, TrustAnchor, UserRecord};


    /// Establish `anchor` over whatever office this warehouse holds, as a fixture would without
    /// an enrollment ceremony.
    fn plant_trust_anchor(anchor: &TrustAnchor) -> Result<(), RefUpdateError> {
        let office = forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)?;

        office_utils::install_trust(&office_utils::TrustInstall {
            anchor,
            replaces: None,
            office: office.as_deref(),
            new_office: None,
            served: None,
            pallets: PalletsPrecondition::NotConsumed,
        }).map(|_| ())
    }
    /// The durable-taint machinery (module `taint_utils` in `forklift_core`) is activation-gated
    /// all-or-nothing: a process that taints or gates existence checks without ever healing them
    /// is strictly worse than today's baseline (a permanent in-process wedge, or a disk taint
    /// nobody consumes — see that module's doc comment). `forklift-server` does not wire the
    /// entry-heal (a long-lived, multi-warehouse process needs a per-request chokepoint of its own
    /// design, scoped out as a follow-up — see DESIGN.html §3.1.1), so it must never
    /// call that module's activation entry point either. A compile-time grep is cheap and precise
    /// here: the whole crate is two files, and the forbidden call is an exact, unmistakable
    /// string — this fails the moment either file starts calling it, without needing a runtime
    /// scenario. (Deliberately not spelled with its own `::` path syntax anywhere in this doc
    /// comment or the test below — see the test body for why.)
    #[test]
    fn this_crate_never_activates_the_durable_taint_machinery() {
        // The forbidden calls are assembled at runtime from separate pieces, never written as one
        // contiguous "module::function" token anywhere in this file — including in comments and
        // string literals — because `include_str!("server.rs")` below necessarily embeds this
        // very test's own source, and a literal needle here would trip over itself.
        let activate_needle = ["taint_utils", "activate"].join("::");
        let heal_needle = ["heal_utils", "heal_if_tainted"].join("::");

        let main_rs = include_str!("main.rs");
        let server_rs = include_str!("server.rs");

        for (name, source) in [("main.rs", main_rs), ("server.rs", server_rs)] {
            assert!(
                !source.contains(&activate_needle),
                "{} must never activate the durable-taint machinery — forklift-server has not \
                wired the entry-heal, so activating half the taint machinery would be strictly \
                worse than today's baseline (see taint_utils's module doc comment)",
                name
            );
            assert!(
                !source.contains(&heal_needle),
                "{} must never call the entry-heal while this crate never activates the taint \
                machinery — an unactivated call is currently harmless (a documented no-op), but \
                wiring it in without activation would be misleading dead code implying a \
                guarantee this crate does not provide",
                name
            );
        }
    }

    /// `HookEndpoint`'s `Debug` is hand-written specifically so this holds structurally rather
    /// than by nobody happening to print one with the secret in scope: the derived `Debug` this
    /// replaces would print `secret` in full. Checks both fields' formatted output, so a future
    /// edit cannot "fix" the struct literal without noticing the field name is still there too.
    #[test]
    fn hook_endpoint_debug_never_prints_the_secret() {
        let hook = HookEndpoint { url: "https://provider.example/hook".to_string(), secret: "s3cr3t-value".to_string() };
        let formatted = format!("{:?}", hook);

        assert!(!formatted.contains("s3cr3t-value"), "{}", formatted);
        assert!(formatted.contains("https://provider.example/hook"), "{}", formatted);
        assert!(formatted.contains("redacted"), "{}", formatted);
    }

    /// A classified refusal a core operation raised reaches this head as a sentinel-framed message;
    /// `error_body` threads its stable code and next step onto the wire (additive) with the de-framed
    /// human text in `error` — the raw frame never leaks (fixing the latent leak where a framed
    /// refusal string went verbatim into the body).
    #[test]
    fn error_body_threads_a_refusal_code_onto_the_wire() {
        let framed: String = scope_utils::chunked_transport_refusal("big.bin").into();
        assert!(framed.contains('\u{1f}'), "the core error reaches the head framed");

        let body = error_body(framed);
        assert_eq!(body.code.as_deref(), Some(scope_utils::CODE_CHUNKED_TRANSPORT_UNSUPPORTED));
        assert!(body.next_step.is_some());
        assert!(body.error.contains("big.bin"), "the de-framed human message: {}", body.error);
        assert!(!body.error.contains('\u{1f}'), "the raw frame never leaks: {:?}", body.error);
    }

    /// A plain (unframed) error passes through with no code — an older client ignores the absent
    /// field, and the body is byte-identical to before this change.
    #[test]
    fn error_body_leaves_a_plain_error_uncoded() {
        let body = error_body("something ordinary went wrong".to_string());
        assert_eq!(body.error, "something ordinary went wrong");
        assert!(body.code.is_none());
        assert!(body.next_step.is_none());
    }

    // ---------------------------------------------------------------------------------
    // Shared test plumbing
    // ---------------------------------------------------------------------------------

    /// A fresh `AppState` for a given serving mode, with no auth/hooks configured (tests
    /// override individual fields with struct-update syntax). `http` is built through
    /// [`build_hook_client`] — the same function [`serve`] uses — rather than a bare
    /// `reqwest::Client::new()`, which carries no timeout at all, so the handler tests run against
    /// the client production runs. The two never-answering-hook tests replace it with a client
    /// armed at [`TEST_HOOK_TIMEOUT`] so they do not wait out the production figure.
    ///
    /// `open: true` here is a test-fixture convenience only, never the production default (see
    /// `ServeOptions::open`'s doc comment and `serve`'s startup refusal). The `check_auth` section
    /// below overrides this explicitly, in both directions, to test the actual auth decision.
    fn base_state(mode: ServeMode) -> AppState {
        AppState {
            mode,
            token: None,
            operator_tokens: HashMap::new(),
            rebuild_after_lifts: None,
            warehouses: Mutex::new(WarehouseRegistry::new(MAX_LIVE_WAREHOUSES)),
            authentication_hook: None,
            admission_hook: None,
            events_hook: None,
            resolution_hook: None,
            http: build_hook_client().expect(
                "the hook HTTP client must build with this module's fixed config: only a \
                malformed TLS/proxy setting could fail this, and none is set here"
            ),
            authentication_cache: Mutex::new(HashMap::new()),
            authentication_cache_ttl: std::time::Duration::from_secs(60),
            open: true,
        }
    }

    fn single_mode_state(root: PathBuf) -> AppState {
        base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root))))
    }

    /// What the authentication layer hands a handler for an operator token. The handler tests
    /// call handlers directly, so they start from the authenticated principal; the layer itself
    /// is tested through [`router`].
    fn as_operator(identifier: &str) -> Extension<Principal> {
        Extension(Principal::Operator(identifier.to_string()))
    }

    fn headers_with_bearer(token: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", format!("Bearer {}", token).parse().unwrap());
        headers
    }

    fn headers_with_raw(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", value.parse().unwrap());
        headers
    }

    /// A unique scratch directory for one test (never shared across tests, so parallel
    /// tests never collide on disk).
    fn scratch_dir(name: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);

        let path = std::env::temp_dir().join(format!(
            "forklift-server-test-{}-{}-{}", name, std::process::id(), id
        ));
        let _ = forklift_core::util::file_utils::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Start an in-process HTTP server that answers every POST with a fixed status and
    /// body, recording the most recent request body it received and how many requests it
    /// has handled. The hook protocol is ordinary HTTP, so this is a faithful stand-in for
    /// a hosting provider's hook endpoint — no subprocess needed.
    async fn spawn_hook(status: StatusCode, body: &'static str)
        -> (String, Arc<Mutex<Vec<u8>>>, Arc<AtomicUsize>) {
        let received = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));

        let received_for_handler = Arc::clone(&received);
        let hits_for_handler = Arc::clone(&hits);

        let app = Router::new().route("/hook", post(move |bytes: Bytes| {
            let received = Arc::clone(&received_for_handler);
            let hits = Arc::clone(&hits_for_handler);

            async move {
                if let Ok(mut guard) = received.lock() {
                    *guard = bytes.to_vec();
                }
                hits.fetch_add(1, Ordering::SeqCst);

                (status, body).into_response()
            }
        }));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        (format!("http://{}/hook", addr), received, hits)
    }

    /// A URL nothing listens on (bound, then immediately released) — a deterministic way
    /// to exercise the "hook unreachable" fail-closed path without depending on a magic
    /// port number staying free.
    fn unreachable_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        format!("http://{}/hook", addr)
    }

    /// A hook endpoint that accepts the connection, makes a best-effort read of at most 4096
    /// bytes of the request (not necessarily the whole body — draining it fully is not load-
    /// bearing for this fixture, only holding the connection open with no response is), and then
    /// never answers — distinct from [`unreachable_url`], which refuses the connection outright
    /// and so never exercises the response-side wait at all. This is the fixture that actually proves
    /// `AppState.http` is timeout-armed: against `unreachable_url`, a client with no timeout at
    /// all still fails fast, on connection refusal, so that fixture alone cannot separate an armed
    /// client from an unbounded one. Parks on a channel-recv rather than sleeping a fixed
    /// duration, the same shape `forklift-core`'s own `SilentRemote` uses, so the connection stays
    /// open for exactly as long as the test needs it and no longer.
    struct SilentHook {
        url: String,
        _park: std::sync::mpsc::Sender<()>,
    }

    impl SilentHook {
        fn start() -> SilentHook {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = std::sync::mpsc::channel::<()>();

            std::thread::spawn(move || {
                use std::io::Read;

                if let Ok((mut stream, _)) = listener.accept() {
                    let mut buf = [0u8; 4096];
                    // Best-effort: draining the request is not load-bearing for this fixture
                    // (the client has already sent it by the time the server would notice a
                    // partial read), only holding the connection open with no response is.
                    let _ = stream.read(&mut buf);
                    let _ = rx.recv();
                    drop(stream);
                }
            });

            SilentHook { url: format!("http://{}/hook", addr), _park: tx }
        }
    }

    // ---------------------------------------------------------------------------------
    // The authentication layer — refused before the body is buffered
    // ---------------------------------------------------------------------------------

    /// Serve [`router`] on a real socket: what is under test is when the server stops waiting for
    /// bytes, which a request driven through the router in-process cannot show.
    async fn serve_router(state: Arc<AppState>, body_limit: usize) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = router(state, DefaultBodyLimit::max(body_limit));

        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        addr
    }

    /// Send a request head announcing `content_length` bytes, and none of them. Returns the open
    /// connection, so the caller decides what to send next.
    async fn send_head(addr: std::net::SocketAddr, method: &str, path: &str, token: Option<&str>,
                       content_length: usize) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;

        let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let authorization = token.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
        let head = format!(
            "{method} {path} HTTP/1.1\r\nHost: test\r\n{authorization}\
            Content-Type: application/json\r\nContent-Length: {content_length}\r\n\r\n"
        );
        stream.write_all(head.as_bytes()).await.unwrap();

        stream
    }

    /// The response's status line, failing the test if none comes while the request's body is
    /// still owed: that wait is exactly what a server reading the body first does.
    async fn status_line(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;

        let mut received = Vec::new();
        let mut chunk = [0u8; 1024];

        let read = tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while !received.windows(2).any(|pair| pair == b"\r\n") {
                let count = stream.read(&mut chunk).await.unwrap();
                assert!(count > 0, "the connection closed with no response");
                received.extend_from_slice(&chunk[..count]);
            }
        }).await;

        assert!(read.is_ok(), "no response while the body was owed: the server is waiting for it");

        let text = String::from_utf8_lossy(&received);
        text.lines().next().unwrap().to_string()
    }

    fn token_state(mode: ServeMode) -> Arc<AppState> {
        Arc::new(AppState { token: Some("the-token".to_string()), open: false, ..base_state(mode) })
    }

    fn verb_name(verb: Verb) -> &'static str {
        match verb {
            Verb::Get => "GET",
            Verb::Put => "PUT",
            Verb::Post => "POST",
            Verb::Delete => "DELETE",
        }
    }

    fn mount_name(mount: Mount) -> &'static str {
        match mount {
            Mount::Root => "root",
            Mount::Protocol => "protocol",
        }
    }

    fn modes_name(modes: Modes) -> &'static str {
        match modes {
            Modes::Both => "both",
            Modes::Multi => "multi",
        }
    }

    fn access_name(access: Access) -> &'static str {
        match access {
            Access::Authenticated => "authenticated",
            Access::Public => "public",
        }
    }

    /// A path a request can be sent to, with each `{name}` and `{*name}` filled in.
    fn concrete_path(path: &str) -> String {
        path.split('/')
            .map(|segment| match segment {
                s if s.starts_with("{*") => "a/b".to_string(),
                s if s.starts_with('{') => "x1".to_string(),
                s => s.to_string(),
            })
            .collect::<Vec<_>>()
            .join("/")
    }

    /// Every entry registered in one serving mode, with the concrete path a request reaches it on.
    fn registered_routes(multi: bool) -> Vec<(&'static RouteEntry, String)> {
        let prefix = if multi { "/warehouses/w1/v1" } else { "/v1" };

        ROUTES.iter()
            .filter(|entry| entry.modes.includes(multi))
            .map(|entry| {
                let path = concrete_path(entry.path);
                match entry.mount {
                    Mount::Root => (entry, path),
                    Mount::Protocol => (entry, format!("{prefix}{path}")),
                }
            })
            .collect()
    }

    /// Reverts: the handler-level check, or an authenticated entry registered without the layer —
    /// then the request waits for its body (or, with no principal, answers 500) instead of a 401.
    /// The public entries are asked too, so a layer over everything is caught as well.
    #[tokio::test]
    async fn every_route_refuses_a_bad_caller_with_the_body_still_owed() {
        let single = scratch_dir("auth-layer-single");
        let multi = scratch_dir("auth-layer-multi");
        let cases = [
            (token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(single)))), false),
            (token_state(ServeMode::Multi { base: multi }), true),
        ];
        let mut public = Vec::new();

        for (state, is_multi) in cases {
            let addr = serve_router(state, 1 << 20).await;

            for (entry, path) in registered_routes(is_multi) {
                let method = verb_name(entry.verb);

                if entry.access == Access::Public {
                    let mut stream = send_head(addr, method, &path, None, 0).await;
                    let status = status_line(&mut stream).await;

                    assert!(status.contains(" 200 "), "public {method} {path}: {status}");
                    public.push((is_multi, format!("{method} {path}")));
                    continue;
                }

                for token in [None, Some("not-the-token")] {
                    let mut stream = send_head(addr, method, &path, token, 1 << 19).await;
                    let status = status_line(&mut stream).await;

                    assert!(status.contains(" 401 "), "{method} {path} with {token:?}: {status}");
                }
            }
        }

        // A literal, not read from ROUTES or the table: making a route public is an edit here too.
        let expected = [(false, "GET /healthz"), (true, "GET /healthz")]
            .map(|(is_multi, route)| (is_multi, route.to_string()));

        assert_eq!(public, expected, "the public routes, asked in each mode");
    }

    const REMOTE_PROTOCOL: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/format/REMOTE_PROTOCOL.md");

    /// The rows of the table between `<!-- {name}:start -->` and `<!-- {name}:end -->`, each a list
    /// of its cells with the code quotes taken off. Panics when a marker is missing or a row does
    /// not have `columns` cells, so a damaged table is never read as a shorter one.
    fn delimited_rows(document: &str, name: &str, columns: usize) -> Vec<Vec<String>> {
        let start = format!("<!-- {name}:start -->");
        let end = format!("<!-- {name}:end -->");
        let begin = document.find(&start).unwrap_or_else(|| panic!("REMOTE_PROTOCOL.md has no {start}"));
        let body = &document[begin + start.len()..];
        let body = &body[..body.find(&end).unwrap_or_else(|| panic!("REMOTE_PROTOCOL.md has no {end}"))];

        body.lines()
            .filter(|line| line.starts_with('|'))
            .skip(2)
            .map(|line| {
                let cells: Vec<String> = line.trim().trim_matches('|').split('|')
                    .map(|cell| cell.trim().trim_matches('`').to_string())
                    .collect();
                assert_eq!(cells.len(), columns, "a {name} row with the wrong number of cells: {line}");
                cells
            })
            .collect()
    }

    /// Reverts: a route added to `ROUTES` and not to the table, a table row with no route, or an
    /// entry whose access the table states the other way. Each is its own assertion.
    #[test]
    fn the_route_table_is_the_route_list() {
        let document = std::fs::read_to_string(REMOTE_PROTOCOL).unwrap();
        let mut table = BTreeMap::new();

        for row in delimited_rows(&document, "routes", 5) {
            let key = (row[0].clone(), row[1].clone(), row[2].clone(), row[3].clone());
            assert!(table.insert(key, row[4].clone()).is_none(), "a route listed twice: {row:?}");
        }

        let mut list = BTreeMap::new();

        for entry in ROUTES {
            let key = (verb_name(entry.verb).to_string(), entry.path.to_string(),
                       mount_name(entry.mount).to_string(), modes_name(entry.modes).to_string());
            assert!(list.insert(key, access_name(entry.access).to_string()).is_none(),
                    "ROUTES holds {} {} twice", verb_name(entry.verb), entry.path);
        }

        assert!(table.len() > 10 && list.len() > 10, "a table or list read as nearly empty");

        let unlisted: Vec<_> = list.keys().filter(|key| !table.contains_key(*key)).collect();
        assert!(unlisted.is_empty(), "routes the REMOTE_PROTOCOL.md table lacks: {unlisted:?}");

        let unregistered: Vec<_> = table.keys().filter(|key| !list.contains_key(*key)).collect();
        assert!(unregistered.is_empty(), "table rows no route registers: {unregistered:?}");

        let disagreeing: Vec<_> = list.iter()
            .filter(|(key, access)| table.get(*key) != Some(*access))
            .map(|(key, access)| (key, access, &table[key]))
            .collect();
        assert!(disagreeing.is_empty(), "routes whose access the table states differently \
            (route, list, table): {disagreeing:?}");
    }

    /// Reverts: either prefix changed in the source and not in the mounts table, or the other way.
    #[test]
    fn the_mount_table_is_the_protocol_prefixes() {
        let document = std::fs::read_to_string(REMOTE_PROTOCOL).unwrap();
        let rows: BTreeSet<Vec<String>> = delimited_rows(&document, "mounts", 3).into_iter().collect();
        let expected: BTreeSet<Vec<String>> = [
            ["protocol", "single", SINGLE_PROTOCOL_PREFIX],
            ["protocol", "multi", MULTI_PROTOCOL_PREFIX],
        ].iter().map(|row| row.iter().map(|cell| cell.to_string()).collect()).collect();

        assert_eq!(rows, expected);
    }

    /// The axum version the registration method list below was read from: `Router`'s methods that
    /// register a route, a service, a nested router or a fallback, or reset one.
    const AXUM_VERSION_READ: &str = "0.8.9";
    const REGISTRATION_METHODS: &[&str] = &[
        "route", "route_service", "nest", "nest_service", "merge", "fallback", "fallback_service",
        "method_not_allowed_fallback", "reset_fallback",
    ];

    /// Every call of a registration method in non-test source, named by the function it is in. A
    /// call inside a `for` loop names the loop's iterable too (`route over ROUTES.iter.filter`), so
    /// the one allowed call is pinned to the list it reads, not only to where it sits. A macro's
    /// tokens are opaque to syn, so a registration method's name anywhere in a macro counts as a
    /// call, and so do `include!` and `#[path]`, which compile source this scan does not read.
    #[derive(Default)]
    struct Registrations {
        within: Vec<String>,
        iterables: Vec<String>,
        found: Vec<(String, String)>,
    }

    /// An expression as its chain of names: `ROUTES.iter.filter` for a method chain, `entry.path`
    /// for a field; `<expr>` for anything that is not a chain of calls and fields on a bare name.
    fn expression_shape(expression: &syn::Expr) -> String {
        match expression {
            syn::Expr::MethodCall(call) => format!("{}.{}", expression_shape(&call.receiver), call.method),
            syn::Expr::Field(field) => match &field.member {
                syn::Member::Named(name) => format!("{}.{name}", expression_shape(&field.base)),
                syn::Member::Unnamed(_) => "<expr>".to_string(),
            },
            syn::Expr::Path(path) if path.qself.is_none() && path.path.segments.len() == 1 =>
                path.path.segments[0].ident.to_string(),
            _ => "<expr>".to_string(),
        }
    }

    /// Every identifier in a token stream, groups included; literals and punctuation are not words.
    fn identifiers(buffer: syn::buffer::TokenBuffer) -> Vec<String> {
        fn walk(mut cursor: syn::buffer::Cursor, words: &mut Vec<String>) {
            while !cursor.eof() {
                if let Some((ident, next)) = cursor.ident() {
                    words.push(ident.to_string());
                    cursor = next;
                } else if let Some((inside, _, _, next)) = cursor.any_group() {
                    walk(inside, words);
                    cursor = next;
                } else if let Some((_, next)) = cursor.token_tree() {
                    cursor = next;
                } else {
                    break;
                }
            }
        }

        let mut words = Vec::new();
        walk(buffer.begin(), &mut words);
        words
    }

    fn is_test_only(attributes: &[syn::Attribute]) -> bool {
        attributes.iter().any(|attribute| {
            attribute.path().is_ident("cfg")
                && attribute.parse_args::<syn::Meta>().is_ok_and(|meta| meta.path().is_ident("test"))
        })
    }

    impl Registrations {
        /// A registration call inside a `for` loop also names its arguments, so the call is pinned
        /// to what it registers as well as to the list it walks.
        fn note_call(&mut self, method: &syn::Ident, arguments: &[&syn::Expr]) {
            let method = method.to_string();

            if self.iterables.is_empty() || !REGISTRATION_METHODS.contains(&method.as_str()) {
                self.note(&method);
            } else {
                let arguments: Vec<String> = arguments.iter().map(|argument| expression_shape(argument)).collect();
                self.record(format!("{method}({})", arguments.join(", ")));
            }
        }

        fn note(&mut self, method: &str) {
            if REGISTRATION_METHODS.contains(&method) {
                self.record(method.to_string());
            }
        }

        fn record(&mut self, what: String) {
            let within = self.within.last().cloned().unwrap_or_else(|| "<item>".to_string());
            let what = match self.iterables.last() {
                Some(iterable) => format!("{what} over {iterable}"),
                None => what,
            };
            self.found.push((within, what));
        }
    }

    impl<'ast> syn::visit::Visit<'ast> for Registrations {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !is_test_only(&item.attrs) {
                if item.attrs.iter().any(|attribute| attribute.path().is_ident("path")
                    || attribute.path().is_ident("cfg_attr") && matches!(&attribute.meta,
                        syn::Meta::List(list) if identifiers(syn::buffer::TokenBuffer::new2(list.tokens.clone())).iter().any(|word| word == "path")))
                {
                    self.record("#[path]".to_string());
                }
                syn::visit::visit_item_mod(self, item);
            }
        }

        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if !is_test_only(&item.attrs) {
                syn::visit::visit_item_impl(self, item);
            }
        }

        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if !is_test_only(&item.attrs) {
                self.within.push(item.sig.ident.to_string());
                syn::visit::visit_item_fn(self, item);
                self.within.pop();
            }
        }

        fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
            if !is_test_only(&item.attrs) {
                self.within.push(item.sig.ident.to_string());
                syn::visit::visit_impl_item_fn(self, item);
                self.within.pop();
            }
        }

        fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
            self.note_call(&call.method, &call.args.iter().collect::<Vec<_>>());
            syn::visit::visit_expr_method_call(self, call);
        }

        /// `use std::include as inc;` would hide an `inc!` from the `include` check.
        fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
            if rename.ident == "include" {
                self.record("use include as".to_string());
            }
            syn::visit::visit_use_rename(self, rename);
        }

        /// `Router::route(router, …)`, `<Router>::route(router, …)`, or the method taken as a value.
        fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
            if path.qself.is_some() || path.path.segments.len() > 1 {
                self.note(&path.path.segments.last().unwrap().ident.to_string());
            }
            syn::visit::visit_expr_path(self, path);
        }

        fn visit_expr_for_loop(&mut self, looped: &'ast syn::ExprForLoop) {
            self.visit_expr(&looped.expr);
            self.iterables.push(expression_shape(&looped.expr));
            self.visit_pat(&looped.pat);
            self.visit_block(&looped.body);
            self.iterables.pop();
        }

        fn visit_macro(&mut self, mac: &'ast syn::Macro) {
            let words = identifiers(syn::buffer::TokenBuffer::new2(mac.tokens.clone()));

            // An `include!` written inside another macro's tokens, such as a `macro_rules!` body.
            if mac.path.segments.last().is_some_and(|segment| segment.ident == "include")
                || words.iter().any(|word| word == "include")
            {
                self.record("include!".to_string());
            }
            for word in words {
                self.note(&word);
            }
            syn::visit::visit_macro(self, mac);
        }
    }

    fn registrations_in(source: &str) -> Vec<(String, String)> {
        let file = syn::parse_file(source).unwrap();
        let mut registrations = Registrations::default();
        syn::visit::Visit::visit_file(&mut registrations, &file);
        registrations.found
    }

    /// The scan itself: each way a route can be registered is found, and test code is not.
    #[test]
    fn the_registration_scan_finds_every_form() {
        let source = r#"
            fn inline(router: Router) -> Router { router.route("/a", get(a)).merge(other()) }
            fn by_path(router: Router) -> Router { Router::fallback(router, f) }
            fn by_qself(router: Router) -> Router { <Router>::method_not_allowed_fallback(router, f) }
            impl Server { fn method(self) -> Router { self.router.nest_service("/b", s) } }
            fn looped(mut router: Router) -> Router {
                for entry in ROUTES.iter().chain(EXTRA) { router = router.route(entry.path, h()); }
                router
            }
            fn in_macro(router: Router) -> Router { wrap!(router.route("/c", get(c))) }
            fn in_message() { tracing::debug!("cannot merge or route {name}", name = "nest"); }
            include!("elsewhere.rs");
            macro_rules! pull { ($file:literal) => { include!($file) } }
            use std::include as inc;
            #[path = "elsewhere.rs"]
            mod elsewhere;
            #[cfg_attr(unix, path = "elsewhere.rs")]
            mod conditional;
            #[cfg(test)]
            mod tests { fn hook() -> Router { Router::new().route("/hook", post(h)) } }
        "#;

        let found = registrations_in(source);

        assert_eq!(found, [
            ("inline", "merge"),
            ("inline", "route"),
            ("by_path", "fallback"),
            ("by_qself", "method_not_allowed_fallback"),
            ("method", "nest_service"),
            ("looped", "route(entry.path, <expr>) over ROUTES.iter.chain"),
            ("in_macro", "route"),
            ("<item>", "include!"),
            ("<item>", "include!"),
            ("<item>", "use include as"),
            ("<item>", "#[path]"),
            ("<item>", "#[path]"),
        ].map(|(within, what)| (within.to_string(), what.to_string())));
    }

    /// Reverts: any route, service, nested router or fallback registered anywhere in the crate's
    /// non-test source other than `register`'s one call and `router`'s two nests. Nothing can list
    /// what a built axum router holds, so a registration outside `ROUTES` is found here or nowhere.
    #[test]
    fn routes_are_registered_only_from_the_route_list() {
        let source_root = concat!(env!("CARGO_MANIFEST_DIR"), "/src");
        let mut pending = vec![PathBuf::from(source_root)];
        let mut found = Vec::new();
        let mut files = 0;

        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|entry| entry.unwrap().path()));
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                files += 1;
                let source = std::fs::read_to_string(&path).unwrap();

                for (within, method) in registrations_in(&source) {
                    found.push((path.file_name().unwrap().to_string_lossy().into_owned(), within, method));
                }
            }
        }

        assert!(files >= 2, "the scan read {files} source files");

        found.sort();
        let expected = [
            ("server.rs", "register", "route(entry.path, answer) over ROUTES.iter.filter"),
            ("server.rs", "router", "nest"),
            ("server.rs", "router", "nest"),
        ].map(|(file, within, method)| (file.to_string(), within.to_string(), method.to_string()));

        assert_eq!(found, expected, "a route registered outside ROUTES");
    }

    /// Reverts: a Cargo.lock that moves axum without re-reading `REGISTRATION_METHODS` from the new
    /// version's `Router`.
    #[test]
    fn the_registration_methods_were_read_from_the_pinned_axum() {
        let lock = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock")).unwrap();
        let versions: Vec<&str> = lock.split("[[package]]")
            .filter(|package| package.lines().any(|line| line.trim() == "name = \"axum\""))
            .filter_map(|package| package.lines().find_map(|line| line.trim().strip_prefix("version = ")))
            .map(|version| version.trim_matches('"'))
            .collect();

        assert_eq!(versions, [AXUM_VERSION_READ], "Cargo.lock pins another axum: re-read the methods \
            its Router registers through, update REGISTRATION_METHODS, then AXUM_VERSION_READ");
    }

    /// Reverts: an entry whose handler answers more than its own method, such as a chained
    /// `get(a).put(b)` or an `any`. The answer to a method no entry lists is a 405 whose `Allow`
    /// names exactly the methods `ROUTES` holds on that path, `GET` bringing `HEAD`.
    #[tokio::test]
    async fn every_path_answers_exactly_its_listed_methods() {
        let single = scratch_dir("allow-single");
        let multi = scratch_dir("allow-multi");
        let cases = [
            (token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(single)))), false),
            (token_state(ServeMode::Multi { base: multi }), true),
        ];
        let client = reqwest::Client::new();
        let unlisted = reqwest::Method::from_bytes(b"PROOFLOAD").unwrap();
        let mut asked = 0;

        for (state, is_multi) in cases {
            let addr = serve_router(state, 1 << 20).await;
            let mut listed: BTreeMap<String, BTreeSet<&str>> = BTreeMap::new();

            for (entry, path) in registered_routes(is_multi) {
                let methods = listed.entry(path).or_default();
                methods.insert(verb_name(entry.verb));

                if entry.verb == Verb::Get {
                    methods.insert("HEAD");
                }
            }

            for (path, methods) in listed {
                let response = client.request(unlisted.clone(), format!("http://{addr}{path}"))
                    .send().await.unwrap();
                let allow: BTreeSet<String> = response.headers().get("allow")
                    .map(|value| value.to_str().unwrap().split(',').map(|m| m.trim().to_string()).collect())
                    .unwrap_or_default();
                let methods: BTreeSet<String> = methods.into_iter().map(str::to_string).collect();

                assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED, "{path}");
                assert_eq!(allow, methods, "{path} answers other methods than ROUTES lists");
                asked += 1;
            }
        }

        assert!(asked > 20, "only {asked} paths asked");
    }

    /// Reverts: `register` registering an entry outside its mount or serving mode, such as a filter
    /// that drops `entry.modes.includes(multi)` (multi-only routes served by a single server) or
    /// `entry.mount == mount` (the protocol served at the root, `/healthz` under the prefix). Every
    /// listed path, asked with its own method at each place the list does not register it, is a 404.
    #[tokio::test]
    async fn no_path_answers_where_the_list_does_not_register_it() {
        let single = scratch_dir("absent-single");
        let multi = scratch_dir("absent-multi");
        let cases = [
            (token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(single)))), false),
            (token_state(ServeMode::Multi { base: multi }), true),
        ];
        let client = reqwest::Client::new();
        let mut asked = 0;

        for (state, is_multi) in cases {
            let addr = serve_router(state, 1 << 20).await;
            let registered: BTreeSet<String> = registered_routes(is_multi).into_iter().map(|(_, path)| path).collect();

            for entry in ROUTES {
                let path = concrete_path(entry.path);

                for place in [path.clone(), format!("/v1{path}"), format!("/warehouses/w1/v1{path}")] {
                    if registered.contains(&place) {
                        continue;
                    }
                    let method = reqwest::Method::from_bytes(verb_name(entry.verb).as_bytes()).unwrap();
                    let response = client.request(method, format!("http://{addr}{place}")).send().await.unwrap();

                    assert_eq!(response.status(), StatusCode::NOT_FOUND,
                        "{} {place} answers in {} mode", verb_name(entry.verb), if is_multi { "multi" } else { "single" });
                    asked += 1;
                }
            }
        }

        assert!(asked > 40, "only {asked} places asked");
    }

    /// The other half of the same layer: the right caller gets through, and the server does wait
    /// for that caller's body. Without this, a server refusing everything would pass the test
    /// above. Reverts: a layer that drops the principal — the handler's extractor then refuses.
    #[tokio::test]
    async fn an_admitted_upload_is_read_in_full_and_stored() {
        use tokio::io::AsyncWriteExt;

        let root = scratch_dir("auth-layer-admitted");
        let _scope = StorageRootScope::enter(&root);
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 1 << 20).await;

        let bytes: &[u8] = &object_utils::blob_bytes(b"an object the admitted caller uploads in two writes");
        let hash = object_utils::hash_object_bytes(bytes);
        let path = format!("/v1/objects/{hash}");

        let mut stream = send_head(addr, "PUT", &path, Some("the-token"), bytes.len()).await;
        stream.write_all(&bytes[..10]).await.unwrap();
        stream.flush().await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        stream.write_all(&bytes[10..]).await.unwrap();

        let status = status_line(&mut stream).await;
        assert!(status.contains(" 201 "), "{status}");
        assert!(file_utils::does_object_exist(&hash).unwrap());
    }

    /// The body cap still applies to admitted callers. Reverts: `body_limit` dropped from the stack.
    #[tokio::test]
    async fn an_admitted_upload_over_the_cap_is_refused() {
        use tokio::io::AsyncWriteExt;

        let root = scratch_dir("auth-layer-cap");
        let _scope = StorageRootScope::enter(&root);
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 1024).await;

        let hash = object_utils::hash_object_bytes(b"whatever");
        let mut stream = send_head(addr, "PUT", &format!("/v1/objects/{hash}"), Some("the-token"), 4096).await;
        stream.write_all(&[0u8; 4096]).await.unwrap();
        let status = status_line(&mut stream).await;

        assert!(status.contains(" 413 "), "{status}");
    }

    /// A client mid-upload must still be told why, not see a dead connection: the refusal arrives
    /// while it is writing a body the server will not keep. Reverts: the discard removed — the
    /// client's write can then break before it reads the 401; that is a race, so this may stay green.
    #[tokio::test]
    async fn a_client_uploading_with_a_bad_token_reads_the_refusal() {
        let root = scratch_dir("auth-layer-client");
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 64 << 20).await;
        let client = reqwest::Client::new();

        for _ in 0..20 {
            let response = client.put(format!("http://{addr}/v1/objects/abc"))
                .bearer_auth("not-the-token")
                .body(vec![0u8; 32 << 20])
                .send().await
                .expect("the refusal must reach the client as a response, not a transport error");

            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
    }

    /// A refused caller who never sends the body it announced does not keep the connection.
    /// Reverts: the discard's time bound — the connection then stays open and this times out.
    #[tokio::test]
    async fn a_refused_connection_owing_its_body_is_closed() {
        use tokio::io::AsyncReadExt;

        let root = scratch_dir("auth-layer-close");
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 1 << 20).await;

        let mut stream = send_head(addr, "PUT", "/v1/objects/abc", None, 1 << 19).await;
        assert!(status_line(&mut stream).await.contains(" 401 "));

        let mut rest = Vec::new();
        let closed = tokio::time::timeout(
            REFUSED_BODY_DISCARD_TIME * 3, stream.read_to_end(&mut rest)
        ).await;

        assert!(closed.is_ok(), "the connection outlived the discard's time bound");
    }

    /// A refused body is read past the socket's buffers, and the connection closes before an
    /// oversized one ends, by whichever bound comes first. Reverts: the discard removed — writes
    /// fail almost at once, under the lower limit; both bounds removed — every write succeeds.
    #[tokio::test]
    async fn a_refused_body_is_read_and_then_cut_off() {
        use tokio::io::AsyncWriteExt;

        const CHUNK: usize = 1 << 20;
        const ANNOUNCED: usize = 512 * CHUNK;

        let root = scratch_dir("auth-layer-cutoff");
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 1 << 20).await;

        let mut stream = send_head(addr, "PUT", "/v1/objects/abc", None, ANNOUNCED).await;
        assert!(status_line(&mut stream).await.contains(" 401 "));

        let chunk = vec![0u8; CHUNK];
        let mut written = 0;

        let cut_off = tokio::time::timeout(REFUSED_BODY_DISCARD_TIME * 6, async {
            while written < ANNOUNCED {
                if stream.write_all(&chunk).await.is_err() {
                    return true;
                }

                written += CHUNK;
            }

            false
        }).await;

        assert_eq!(cut_off, Ok(true), "wrote {written} bytes");
        assert!(written > 16 * CHUNK, "only {written} bytes were read before the connection closed");
    }

    /// The byte bound stops a body that never ends. Reverts: the bound removed — this times out.
    #[tokio::test]
    async fn the_discard_stops_at_its_byte_bound() {
        let endless = tokio_stream::iter(std::iter::repeat_with(
            || Ok::<_, std::io::Error>(Bytes::from_static(b"0123456789"))
        ));
        let discard = discard_refused_body(
            Body::from_stream(endless), std::time::Duration::from_secs(3600), 1000
        );

        tokio::time::timeout(std::time::Duration::from_secs(10), discard).await
            .expect("the discard must stop at its byte bound");
    }

    #[tokio::test]
    async fn healthz_stays_open() {
        let root = scratch_dir("auth-layer-healthz");
        let addr = serve_router(token_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root)))), 1024).await;

        let mut stream = send_head(addr, "GET", "/healthz", None, 0).await;
        let status = status_line(&mut stream).await;

        assert!(status.contains(" 200 "), "{status}");
    }

    // ---------------------------------------------------------------------------------
    // check_auth
    // ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn no_auth_configured_and_explicitly_open_is_fully_open() {
        let state = AppState { open: true, ..single_mode_state(PathBuf::from("/unused")) };
        let principal = check_auth(&state, &HeaderMap::new()).await.unwrap();
        assert!(principal == Principal::Open);
    }

    /// The fail-open finding this fix closes: an empty auth config (no token, no operator
    /// tokens, no authentication hook) must never be silently read as "serve openly" — that is
    /// exactly the state a present-but-wrong-typed config value (`token = 12345`) or a mistyped
    /// hook key used to collapse into via `parse_config`'s old lenient `.and_then(as_str)`
    /// readers. `check_auth` itself must refuse a request in this state unless `open` is
    /// explicitly `true`; `serve`'s startup check (this module, above) is the belt to this
    /// braces — it refuses to construct a server in this configuration at all.
    #[tokio::test]
    async fn no_auth_configured_and_not_open_is_unauthorized() {
        let state = AppState { open: false, ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &HeaderMap::new()).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn the_static_token_authenticates_as_static() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let principal = check_auth(&state, &headers_with_bearer("secret")).await.unwrap();
        assert!(principal == Principal::Static);
    }

    /// Pins the equality semantics the constant-time compare must preserve: a same-length
    /// near-miss still refuses. This is the regression test for the review finding that the
    /// static-token compare was a naive (timing-leaky) `==`; a constant-time comparator that
    /// were subtly wrong (e.g. always agreeing once lengths match) would slip through every
    /// other test here, since they all use tokens of different lengths.
    #[tokio::test]
    async fn an_equal_length_near_miss_token_is_unauthorized() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &headers_with_bearer("secrft")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    /// Belt-and-braces for the empty-credential fail-open finding: even if `state.token` were
    /// somehow `Some("")` — `serve`'s options filter is meant to make this unreachable in
    /// practice, but this pins the deeper guard directly, in case a future construction path for
    /// `AppState` reintroduces it — a request presenting the literal header
    /// `Authorization: Bearer ` (empty credential, no bytes after the scheme) must still be
    /// refused, not authenticate as `Principal::Static` via `tokens_match("", "")`.
    #[tokio::test]
    async fn an_empty_bearer_credential_never_authenticates_even_against_an_empty_static_token() {
        let state = AppState {
            token: Some(String::new()),
            ..single_mode_state(PathBuf::from("/unused"))
        };
        let error = check_auth(&state, &headers_with_bearer("")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    /// The whitespace-only sibling (PR #124 round 3, F1): even if `state.token` were somehow
    /// `Some("   ")` — again, upstream filtering is meant to make this unreachable in practice —
    /// a request presenting an all-whitespace bearer credential must still be refused, not
    /// authenticate via `tokens_match("   ", "   ")`. This is the h2c/HPACK case the finding
    /// raised directly: this belt-and-braces guard does not assume any transport trims trailing
    /// whitespace off header values before `check_auth` ever sees them.
    #[tokio::test]
    async fn a_whitespace_only_bearer_credential_never_authenticates_even_against_a_whitespace_only_static_token() {
        let state = AppState {
            token: Some("   ".to_string()),
            ..single_mode_state(PathBuf::from("/unused"))
        };
        let error = check_auth(&state, &headers_with_bearer("   ")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[test]
    fn tokens_match_requires_an_exact_byte_for_byte_match() {
        assert!(tokens_match("secret", "secret"));
        assert!(!tokens_match("secret", "secrft"), "a same-length near-miss must refuse");
        assert!(!tokens_match("secret", "secre"), "a shorter guess must refuse");
        assert!(!tokens_match("secret", "secrets"), "a longer guess must refuse");
        assert!(tokens_match("", ""));
    }

    #[tokio::test]
    async fn a_known_operator_token_authenticates_as_that_operator() {
        let mut operator_tokens = HashMap::new();
        operator_tokens.insert("tok-bob".to_string(), "bob".to_string());
        let state = AppState { operator_tokens, ..single_mode_state(PathBuf::from("/unused")) };

        let principal = check_auth(&state, &headers_with_bearer("tok-bob")).await.unwrap();
        assert!(principal == Principal::Operator("bob".to_string()));
    }

    #[tokio::test]
    async fn an_unknown_token_with_no_hook_is_unauthorized() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &headers_with_bearer("guess")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_missing_authorization_header_is_unauthorized_when_auth_is_configured() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &HeaderMap::new()).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_non_bearer_scheme_is_unauthorized() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };
        let error = check_auth(&state, &headers_with_raw("Basic secret")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    /// RFC 7235 §2.1: `auth-scheme` is case-insensitive, so `bearer`/`BEARER`/mixed-case must
    /// authenticate exactly like `Bearer`. The credential (token) half stays case-sensitive — a
    /// same-length, wrong-case token must still refuse.
    #[tokio::test]
    async fn the_bearer_scheme_is_accepted_case_insensitively_but_the_token_stays_case_sensitive() {
        let state = AppState { token: Some("secret".to_string()), ..single_mode_state(PathBuf::from("/unused")) };

        for scheme in ["bearer", "Bearer", "BEARER", "BeArEr"] {
            let principal = check_auth(&state, &headers_with_raw(&format!("{scheme} secret")))
                .await
                .unwrap_or_else(|_| panic!("scheme {} must authenticate", scheme));
            assert!(principal == Principal::Static);
        }

        let error = check_auth(&state, &headers_with_raw("Bearer SECRET")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED, "the token's case must still matter");
    }

    #[tokio::test]
    async fn a_hook_answer_authenticates_and_is_cached() {
        let (url, _received, hits) = spawn_hook(StatusCode::OK, r#"{"identifier":"bob"}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let first = check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert!(first == Principal::Operator("bob".to_string()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        // A second request with the same token must reuse the cached answer, not
        // re-consult the hook (the whole point of the cache on this hot path).
        let second = check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert!(second == Principal::Operator("bob".to_string()));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_zero_ttl_cache_never_serves_a_stale_answer() {
        let (url, _received, hits) = spawn_hook(StatusCode::OK, r#"{"identifier":"bob"}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            authentication_cache_ttl: std::time::Duration::from_secs(0),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), 2, "a zero-second TTL must re-check every time");
    }

    #[tokio::test]
    async fn a_hook_non_success_status_is_unauthorized() {
        let (url, _received, _hits) = spawn_hook(StatusCode::FORBIDDEN, "nope").await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::UNAUTHORIZED);
    }

    /// The bound the two never-answering-hook tests arm their client at, in place of
    /// [`HOOK_CLIENT_TIMEOUT`], so neither waits ten seconds.
    const TEST_HOOK_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1_500);

    /// The client [`build_hook_client`] builds (the one [`serve`] uses) is armed at the production
    /// figure, read off the client itself rather than waited out: the tests that watch a hook time
    /// out arm [`TEST_HOOK_TIMEOUT`] instead, so this is the check on the production figure.
    /// `reqwest`'s `Debug` output names the armed timeout; if a `reqwest` upgrade changes that
    /// format, this fails rather than passing vacuously.
    #[test]
    fn the_hook_client_is_armed_at_the_production_timeout() {
        let client = format!("{:?}", build_hook_client().unwrap());
        assert!(client.contains("TotalTimeout: 10s"), "the hook client is not armed at 10s: {}", client);
    }

    #[tokio::test]
    async fn an_unreachable_authentication_hook_fails_closed() {
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url: unreachable_url(), secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// `an_unreachable_authentication_hook_fails_closed` above pins the connection-refused path,
    /// which fails fast regardless of whether `AppState.http` carries any timeout at all. This pins
    /// the response-side wait instead: a hook that accepts the connection and then never answers
    /// must still fail closed when the client's timeout fires, rather than hang. Which figure
    /// production arms is `the_hook_client_is_armed_at_the_production_timeout`'s check, not this
    /// one's.
    ///
    /// The client is armed at [`TEST_HOOK_TIMEOUT`] through [`hook_client_armed_at`], the
    /// construction [`build_hook_client`] uses, so the test waits a second and a half rather than
    /// production's ten; `the_hook_client_is_armed_at_the_production_timeout` checks the
    /// production figure without waiting it out. `lower_bound`/`upper_bound` are literals, not
    /// derived from [`TEST_HOOK_TIMEOUT`], so a mutation to the armed figure moves the real
    /// elapsed time and not the bounds with it. `lower_bound` (1.2s) rejects a client armed at
    /// half the figure (0.75s), and `upper_bound` (5s) leaves 3.5s for a loaded runner's
    /// scheduling while still rejecting production's 10s; a client armed at a multiple of its
    /// figure is the production-timeout test's to catch. `outer_ceiling` (15s) is a backstop past both, so a
    /// genuinely unbounded regression fails this test's own panic (naming the real cause) rather
    /// than hanging the suite.
    #[tokio::test]
    async fn an_authentication_hook_that_never_answers_fails_closed_near_the_hook_timeout() {
        let hook = SilentHook::start();
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url: hook.url.clone(), secret: "s".to_string() }),
            http: hook_client_armed_at(TEST_HOOK_TIMEOUT).unwrap(),
            ..single_mode_state(PathBuf::from("/unused"))
        };
        let lower_bound = std::time::Duration::from_millis(1_200);
        let upper_bound = std::time::Duration::from_secs(5);
        let outer_ceiling = std::time::Duration::from_secs(15);

        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            outer_ceiling, check_auth(&state, &headers_with_bearer("tok"))
        ).await;
        let elapsed = started.elapsed();

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "check_auth hung past the test's own {:?} outer ceiling against a hook that \
                never answers — AppState.http is not timeout-armed", outer_ceiling
            ))
            .err()
            .expect("a hook that never answers must fail closed, not appear to succeed");

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            elapsed >= lower_bound && elapsed < upper_bound,
            "elapsed {:?} is outside [{:?}, {:?}) — the client that fired was armed with some \
            other bound than the one `hook_client_armed_at` was given (TEST_HOOK_TIMEOUT, 1.5s)",
            elapsed, lower_bound, upper_bound
        );
    }

    #[tokio::test]
    async fn a_malformed_authentication_hook_answer_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, "not json").await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// F4 of PR #124 round 2: a well-formed but blank identifier must fail closed exactly like
    /// malformed JSON — `Principal::Operator("")` is not a real identity, and unlike the token
    /// side (fail-closed via an exact, untrimmed byte comparison — see
    /// `an_empty_bearer_credential_never_authenticates_even_against_an_empty_static_token`),
    /// nothing later compares an identifier byte-exactly against a configured value; it is
    /// recorded (`caller`, `operator` in admission/event payloads) as if it were a legitimate
    /// actor.
    #[tokio::test]
    async fn a_hook_answer_with_an_empty_identifier_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"identifier":""}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// Whitespace-only is exactly as much "not an identity" as empty — this is the whitespace
    /// half the token side never needs (an exact byte comparison already refuses any
    /// whitespace-padded guess).
    #[tokio::test]
    async fn a_hook_answer_with_a_whitespace_only_identifier_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"identifier":"   "}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_auth(&state, &headers_with_bearer("tok")).await.err().unwrap();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The check must reject only *all-whitespace* identifiers, not merely trim and accept —
    /// an identifier with incidental leading/trailing whitespace around real content is still a
    /// real identity and must authenticate, verbatim (untrimmed): the office lookup matches an
    /// operator's identifier by exact equality against the tracked office roster, so silently
    /// trimming it here would make this check the reason a legitimately-enrolled " bob " never
    /// matches.
    #[tokio::test]
    async fn a_hook_answer_with_incidental_whitespace_around_a_real_identifier_still_authenticates() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"identifier":" bob "}"#).await;
        let state = AppState {
            authentication_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let principal = check_auth(&state, &headers_with_bearer("tok")).await.unwrap();
        assert!(principal == Principal::Operator(" bob ".to_string()));
    }

    // ---------------------------------------------------------------------------------
    // check_admission
    // ---------------------------------------------------------------------------------

    #[tokio::test]
    async fn no_admission_hook_always_admits() {
        let state = single_mode_state(PathBuf::from("/unused"));
        let result = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn the_admission_hook_can_allow() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":true}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let result = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn the_admission_hook_denies_with_its_reason() {
        let (url, _received, _hits) = spawn_hook(
            StatusCode::OK, r#"{"allow":false,"reason":"quota exceeded"}"#
        ).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert_eq!(error.1, "quota exceeded");
    }

    #[tokio::test]
    async fn a_denial_without_a_reason_uses_the_default_message() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":false}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("admission policy"), "{}", error.1);
    }

    #[tokio::test]
    async fn an_unreachable_admission_hook_fails_closed() {
        let state = AppState {
            admission_hook: Some(HookEndpoint { url: unreachable_url(), secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    /// The admission-hook sibling of
    /// `an_authentication_hook_that_never_answers_fails_closed_near_the_hook_timeout` — same gap,
    /// same fixture, same literal bounds and the same reasoning for why they are literals rather
    /// than derived from [`TEST_HOOK_TIMEOUT`]; see that test's own doc.
    #[tokio::test]
    async fn an_admission_hook_that_never_answers_fails_closed_near_the_hook_timeout() {
        let hook = SilentHook::start();
        let state = AppState {
            admission_hook: Some(HookEndpoint { url: hook.url.clone(), secret: "s".to_string() }),
            http: hook_client_armed_at(TEST_HOOK_TIMEOUT).unwrap(),
            ..single_mode_state(PathBuf::from("/unused"))
        };
        let lower_bound = std::time::Duration::from_millis(1_200);
        let upper_bound = std::time::Duration::from_secs(5);
        let outer_ceiling = std::time::Duration::from_secs(15);

        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            outer_ceiling,
            check_admission(
                &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
            )
        ).await;
        let elapsed = started.elapsed();

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "check_admission hung past the test's own {:?} outer ceiling against a hook that \
                never answers — AppState.http is not timeout-armed", outer_ceiling
            ))
            .unwrap_err();

        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
        assert!(
            elapsed >= lower_bound && elapsed < upper_bound,
            "elapsed {:?} is outside [{:?}, {:?}) — the client that fired was armed with some \
            other bound than the one `hook_client_armed_at` was given (TEST_HOOK_TIMEOUT, 1.5s)",
            elapsed, lower_bound, upper_bound
        );
    }

    #[tokio::test]
    async fn a_non_success_admission_status_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::INTERNAL_SERVER_ERROR, "boom").await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn malformed_admission_json_fails_closed() {
        let (url, _received, _hits) = spawn_hook(StatusCode::OK, "not json").await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let error = check_admission(
            &state, &PathParams::new(), &Principal::Operator("bob".to_string()), "upload", None
        ).await.unwrap_err();
        assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn the_admission_request_carries_the_tenant_operator_and_pallet() {
        let (url, received, _hits) = spawn_hook(StatusCode::OK, r#"{"allow":true}"#).await;
        let state = AppState {
            admission_hook: Some(HookEndpoint { url, secret: "s".to_string() }),
            ..single_mode_state(PathBuf::from("/unused"))
        };

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "tenant-a".to_string());

        check_admission(
            &state, &params, &Principal::Operator("bob".to_string()), "ref_update", Some("main")
        ).await.unwrap();

        let body = received.lock().unwrap().clone();
        let value: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(value["action"], "ref_update");
        assert_eq!(value["warehouse"], "tenant-a");
        assert_eq!(value["operator"], "bob");
        assert_eq!(value["pallet"], "main");
    }

    // ---------------------------------------------------------------------------------
    // validate_warehouse_id / parse_operator_tokens
    // ---------------------------------------------------------------------------------

    #[test]
    fn validate_warehouse_id_rejects_the_obvious_bad_shapes() {
        assert!(validate_warehouse_id("").is_err(), "empty");
        assert!(validate_warehouse_id(&"a".repeat(101)).is_err(), "too long");
        assert!(validate_warehouse_id(".hidden").is_err(), "leading dot");
        assert!(validate_warehouse_id("-flag").is_err(), "leading dash");
        assert!(validate_warehouse_id("tenant/a").is_err(), "path separator");
        assert!(validate_warehouse_id("tenant a").is_err(), "space");
        assert!(validate_warehouse_id("Acme").is_err(), "uppercase");
        assert!(validate_warehouse_id("nul").is_err(), "reserved device name");
    }

    #[test]
    fn validate_warehouse_id_accepts_ordinary_ids() {
        assert!(validate_warehouse_id("tenant-a").is_ok());
        assert!(validate_warehouse_id("tenant_a.1").is_ok());
        assert!(validate_warehouse_id(&"a".repeat(100)).is_ok(), "exactly 100 is fine");
    }

    #[test]
    fn parses_a_valid_operator_token_file() {
        let dir = scratch_dir("tokens-ok");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = \"alice\"\n\"tok-b\" = \"bob\"\n").unwrap();

        let tokens = parse_operator_tokens(path.to_str().unwrap()).unwrap();
        assert_eq!(tokens.get("tok-a"), Some(&"alice".to_string()));
        assert_eq!(tokens.get("tok-b"), Some(&"bob".to_string()));

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_missing_token_file() {
        let path = std::env::temp_dir().join("forklift-server-test-tokens-does-not-exist.toml");
        let _ = forklift_core::util::file_utils::remove_file(&path);
        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn rejects_a_token_file_that_is_not_valid_toml() {
        let dir = scratch_dir("tokens-bad-toml");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "not [ valid toml").unwrap();

        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_token_file_with_no_operators_table() {
        let dir = scratch_dir("tokens-no-table");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "not_operators = 1\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("[operators]"), "{}", error);
        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_a_token_mapped_to_a_non_string_value() {
        let dir = scratch_dir("tokens-non-string");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = 42\n").unwrap();

        assert!(parse_operator_tokens(path.to_str().unwrap()).is_err());
        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// The operator-tokens counterpart of the empty-`--token` fail-open finding: an empty
    /// string key is not a credential. Left unrejected, `state.operator_tokens.get("")` would
    /// match a request carrying the literal header `Authorization: Bearer ` (empty credential),
    /// authenticating it as whatever identifier the empty key maps to.
    #[test]
    fn rejects_a_token_file_with_an_empty_token_key() {
        let dir = scratch_dir("tokens-empty-key");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"\" = \"alice\"\n\"tok-b\" = \"bob\"\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("empty"), "{}", error);
        assert!(error.contains("alice"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// The round-3 sibling (F1): a whitespace-only token key is exactly as much "not a
    /// credential" as an empty one, for the identical reason `merge_static_token`'s static token
    /// now checks whitespace too — either no client can ever present it (over a transport that
    /// trims trailing whitespace off header values, silently locking "alice" out forever), or one
    /// can present a trivially-guessable all-whitespace credential.
    #[test]
    fn rejects_a_token_file_with_a_whitespace_only_token_key() {
        let dir = scratch_dir("tokens-whitespace-key");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"   \" = \"alice\"\n\"tok-b\" = \"bob\"\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("whitespace"), "{}", error);
        assert!(error.contains("alice"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// F4 of PR #124 round 2: `"tok-a" = ""` used to parse fine — only the token (key) side was
    /// checked for emptiness, never the identifier (value). An empty identifier is not merely
    /// unenrolled (which the office lookup already 403s): it flows into admission/event payloads
    /// and `post_resolve`'s `caller` as a legitimate-looking actor.
    #[test]
    fn rejects_a_token_mapped_to_a_blank_identifier() {
        let dir = scratch_dir("tokens-blank-identifier");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = \"\"\n\"tok-b\" = \"bob\"\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("blank"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// Whitespace-only is exactly as much "not an identity" as empty — the same blank rule
    /// `rejects_a_token_file_with_a_whitespace_only_token_key` now applies to the token (key)
    /// side too (PR #124 round 3, F1 unified them); what still differs is normalization, not
    /// presence — see `a_token_file_identifier_with_incidental_whitespace_is_stored_untrimmed`
    /// below for the half of the identity rule this test does not cover.
    #[test]
    fn rejects_a_token_mapped_to_a_whitespace_only_identifier() {
        let dir = scratch_dir("tokens-whitespace-identifier");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = \"   \"\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(error.contains("blank"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// F6 of PR #124 round 3: the matching control `authenticate_via_hook`'s own identity rule
    /// already has
    /// (`a_hook_answer_with_incidental_whitespace_around_a_real_identifier_still_authenticates`),
    /// missing here until now. Without it, a future edit changing `identifier.to_string()` to
    /// `identifier.trim().to_string()` above would silently break exact-equality matching against
    /// the tracked office roster (the office lookup) for every legitimately whitespace-padded
    /// identifier, with no test in this file catching it: the two rejection tests above only
    /// exercise *entirely*-blank identifiers, never one with real content plus incidental
    /// surrounding whitespace.
    #[test]
    fn a_token_file_identifier_with_incidental_whitespace_is_stored_untrimmed() {
        let dir = scratch_dir("tokens-identifier-whitespace-preserved");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"tok-a\" = \" bob \"\n").unwrap();

        let tokens = parse_operator_tokens(path.to_str().unwrap()).unwrap();
        assert_eq!(tokens.get("tok-a").map(String::as_str), Some(" bob "));

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// PR #124 round 4, F5: the symmetric control for the *key* side of the same claim — the
    /// comment above (`"a real, non-blank token key is still matched byte-for-byte, verbatim …
    /// nothing here trims it"`) has never had a test pinning it, only the two rejection tests
    /// above (which exercise *entirely*-blank keys). Without this, a future
    /// `token.trim().to_string()` edit at the `tokens.insert(...)` call below would silently make
    /// every whitespace-padded operator token permanently unauthenticatable — `check_auth` looks
    /// up the presented, untrimmed header value via `state.operator_tokens.get(token)`, so a
    /// stored, trimmed key can never match again — with no test in this file catching it.
    #[test]
    fn a_token_file_key_with_incidental_whitespace_is_stored_untrimmed() {
        let dir = scratch_dir("tokens-key-whitespace-preserved");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\" tok a \" = \"bob\"\n").unwrap();

        let tokens = parse_operator_tokens(path.to_str().unwrap()).unwrap();
        assert_eq!(tokens.get(" tok a ").map(String::as_str), Some("bob"));

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// The blank-identifier error must never echo the token — unlike the empty-*token* error
    /// above (safe to echo, since the token there is itself empty), the token in this scenario
    /// is a real, live credential, and printing it would leak it into the server's own
    /// startup-failure output and logs.
    #[test]
    fn a_blank_identifier_error_never_echoes_the_token() {
        let dir = scratch_dir("tokens-blank-identifier-no-leak");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n\"super-secret-token\" = \"\"\n").unwrap();

        let error = parse_operator_tokens(path.to_str().unwrap()).unwrap_err();
        assert!(!error.contains("super-secret-token"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    // ---------------------------------------------------------------------------------
    // resolve_warehouse
    // ---------------------------------------------------------------------------------

    #[test]
    fn single_mode_always_resolves_the_one_served_handle() {
        let root = scratch_dir("resolve-single");
        let state = single_mode_state(root.clone());

        let first = resolve_handle(&state, &PathParams::new()).unwrap();
        let second = resolve_handle(&state, &PathParams::new()).unwrap();
        assert!(Arc::ptr_eq(&first, &second));
        assert_eq!(first.root, root);

        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    #[test]
    fn multi_mode_404s_for_an_unknown_warehouse() {
        let base = scratch_dir("resolve-multi-404");
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "ghost".to_string());

        let error = resolve_handle(&state, &params).err().unwrap();
        assert_eq!(error.0, StatusCode::NOT_FOUND);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A name whose folder cannot be listed is not judged empty or full: the lookup answers it as
    /// a check that failed, never with a remedy.
    ///
    /// Revert: an unreadable folder reads as not empty → 409 → red.
    #[cfg(unix)]
    #[test]
    fn multi_mode_500s_for_a_folder_it_cannot_list() {
        use std::os::unix::fs::PermissionsExt;
        let base = scratch_dir("resolve-multi-unlistable");
        let shut = base.join("shut");
        std::fs::create_dir_all(&shut).unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o300)).unwrap();
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let listable = std::fs::read_dir(&shut).is_ok();
        let error = resolve_handle(&state, &warehouse_params("shut")).err().unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o700)).unwrap();
        if !listable {
            assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", error.1);
        }

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A tenant folder without a warehouse marker exists, so it is answered 409, never the 404
    /// whose "create it first" the create would then refuse; nothing about it is served.
    ///
    /// Revert: the lookup answers only an unmarked folder, or none → the rest fall to the 404 → red.
    #[test]
    fn multi_mode_409s_for_an_unmarked_warehouse() {
        let base = scratch_dir("resolve-multi-unmarked");
        std::fs::create_dir_all(base.join("old").join(FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap();
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let error = resolve_handle(&state, &warehouse_params("old")).err().unwrap();
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);
        assert!(error.1.starts_with("\"old\" exists on this server but is not a usable warehouse"), "{}", error.1);
        assert!(!error.1.contains(&*base.to_string_lossy()), "no server path reaches the client: {}", error.1);
        assert!(!serve_lock_exists(&base, "old"));

        // Nor is anything else there that is not a warehouse: a bay's folder, or a marker this
        // build cannot read.
        let bay = base.join("bay").join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&bay).unwrap();
        std::fs::write(bay.join(bay_utils::FILE_NAME_BAY_REDIRECT), b"x").unwrap();
        mark_warehouse(&base.join("newer"));
        std::fs::write(base.join("newer").join(FOLDER_NAME_FORKLIFT_ROOT).join(warehouse_utils::FILE_NAME_WAREHOUSE_MARKER), b"/forklift-warehouse 99\n").unwrap();
        let error = resolve_handle(&state, &warehouse_params("bay")).err().unwrap();
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);
        // One that cannot be judged is offered no remedy that would destroy it.
        let error = resolve_handle(&state, &warehouse_params("newer")).err().unwrap();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", error.1);

        // A file, or a folder with content, would refuse a creation too; an empty folder would not.
        std::fs::write(base.join("file"), b"x").unwrap();
        std::fs::create_dir_all(base.join("full")).unwrap();
        std::fs::write(base.join("full").join("data.txt"), b"x").unwrap();
        std::fs::create_dir_all(base.join("empty")).unwrap();
        for (id, status) in [("file", StatusCode::CONFLICT), ("full", StatusCode::CONFLICT), ("empty", StatusCode::NOT_FOUND)] {
            let error = resolve_handle(&state, &warehouse_params(id)).err().unwrap();
            assert_eq!(error.0, status, "{}: {}", id, error.1);
        }

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_422s_for_an_invalid_warehouse_id() {
        let base = scratch_dir("resolve-multi-422");
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "../escape".to_string());

        let error = resolve_handle(&state, &params).err().unwrap();
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_422s_for_a_mixed_case_warehouse_id() {
        let base = scratch_dir("resolve-multi-422-case");
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "Acme".to_string());

        let error = resolve_handle(&state, &params).err().unwrap();
        assert_eq!(error.0, StatusCode::UNPROCESSABLE_ENTITY);
        assert!(error.1.contains("the lowercase \"acme\" is valid"), "{}", error.1);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_caches_the_handle_across_requests() {
        let base = scratch_dir("resolve-multi-cache");
        mark_warehouse(&base.join("wh1"));
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), "wh1".to_string());

        let first = resolve_handle(&state, &params).unwrap();
        let second = resolve_handle(&state, &params).unwrap();
        assert!(Arc::ptr_eq(&first, &second), "the same warehouse must reuse one handle");

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn multi_mode_keeps_tenants_isolated() {
        let base = scratch_dir("resolve-multi-tenants");
        mark_warehouse(&base.join("wh1"));
        mark_warehouse(&base.join("wh2"));
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let mut params1 = PathParams::new();
        params1.insert("warehouse".to_string(), "wh1".to_string());
        let mut params2 = PathParams::new();
        params2.insert("warehouse".to_string(), "wh2".to_string());

        let handle1 = resolve_handle(&state, &params1).unwrap();
        let handle2 = resolve_handle(&state, &params2).unwrap();

        assert!(!Arc::ptr_eq(&handle1, &handle2), "two tenants must never share a handle");
        assert_ne!(handle1.root, handle2.root);
        assert_eq!(state.warehouses.lock().unwrap().entries.len(), 2);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    // ---------------------------------------------------------------------------------
    // The live-warehouse cap: letting idle warehouses go
    // ---------------------------------------------------------------------------------

    /// Makes `root` a bare warehouse: its `.forklift` folder, holding only the marker.
    fn mark_warehouse(root: &std::path::Path) {
        mark_warehouse_as(root, "full");
    }

    /// Mark `root` as a bare warehouse whose marker records `mode` (or any other entry a test wants
    /// refused).
    fn mark_warehouse_as(root: &std::path::Path, mode: &str) {
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(forklift.join("config")).unwrap();
        std::fs::write(forklift.join(warehouse_utils::FILE_NAME_WAREHOUSE_MARKER),
            warehouse_utils::framed_marker_bytes(&format!("format 1\n{}\n", mode))).unwrap();
    }

    /// A multi-mode state over `base` that keeps at most `cap` warehouses live while any is idle,
    /// with each of `ids` present as a (bare) warehouse.
    fn capped_state(base: &std::path::Path, cap: usize, ids: &[&str]) -> AppState {
        for id in ids {
            mark_warehouse(&base.join(id));
        }
        let state = base_state(ServeMode::Multi { base: base.to_path_buf() });
        *state.warehouses.lock().unwrap() = WarehouseRegistry::new(cap);
        state
    }

    fn warehouse_params(id: &str) -> PathParams {
        let mut params = PathParams::new();
        params.insert("warehouse".to_string(), id.to_string());
        params
    }

    fn live_ids(state: &AppState) -> std::collections::BTreeSet<String> {
        state.warehouses.lock().unwrap().entries.keys().cloned().collect()
    }

    fn serve_lock_exists(base: &std::path::Path, id: &str) -> bool {
        base.join(id).join(FOLDER_NAME_FORKLIFT_ROOT).join("serve.lock").exists()
    }

    /// One warehouse in another format is answered as unavailable, without this server's paths,
    /// and every other warehouse is still served.
    ///
    /// Revert: resolve past the format check → the other format is served as this build's → red.
    #[test]
    fn a_warehouse_in_another_format_is_unavailable_and_the_others_are_served() {
        let base = scratch_dir("other-format");
        let state = capped_state(&base, 8, &["fine", "newer", "older", "halfway"]);
        for (id, marker) in [("newer", "format 2\nfull\n"), ("older", "format 0\nfull\n"), ("halfway", "format 1\nfull\nupgrading 2 0\n")] {
            std::fs::write(base.join(id).join(FOLDER_NAME_FORKLIFT_ROOT).join(warehouse_utils::FILE_NAME_WAREHOUSE_MARKER),
                warehouse_utils::framed_marker_bytes(marker)).unwrap();
            let error = resolve_handle(&state, &warehouse_params(id)).err().unwrap();
            assert_eq!(error.0, StatusCode::SERVICE_UNAVAILABLE, "{}: {}", id, error.1);
            assert!(!error.1.contains(base.to_str().unwrap()), "{}: {}", id, error.1);
        }

        resolve_handle(&state, &warehouse_params("fine")).unwrap();
        assert_eq!(live_ids(&state), ["fine".to_string()].into());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A warehouse whose format cannot be checked is a server fault, never answered as another
    /// format: a lost format lock the server cannot make again is a 500.
    ///
    /// Revert: answer every failure of the format check as another format → 503 → red.
    #[test]
    #[cfg(unix)]
    fn a_format_that_cannot_be_checked_is_a_server_fault() {
        use std::os::unix::fs::PermissionsExt;

        let base = scratch_dir("format-unchecked");
        let state = capped_state(&base, 8, &["w"]);
        let forklift = base.join("w").join(FOLDER_NAME_FORKLIFT_ROOT);
        let _ = forklift_core::util::file_utils::remove_file(forklift.join("format.lock"));
        std::fs::set_permissions(&forklift, std::fs::Permissions::from_mode(0o555)).unwrap();

        let error = resolve_handle(&state, &warehouse_params("w")).err().unwrap();
        std::fs::set_permissions(&forklift, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", error.1);

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A served warehouse keeps its format lease until it is let go: an upgrade cannot start under
    /// it, and can once the server has stopped serving it.
    ///
    /// Revert: let `serving` drop the lease it took → the exclusive lock lands while served → red.
    #[test]
    fn a_served_warehouse_holds_its_format_until_it_is_let_go() {
        let base = scratch_dir("format-lease");
        let state = capped_state(&base, 1, &["w1", "w2"]);
        let exclusive = |id: &str| {
            let file = std::fs::File::open(base.join(id).join(FOLDER_NAME_FORKLIFT_ROOT).join("format.lock")).unwrap();
            file.try_lock().is_ok()
        };

        drop(resolve_handle(&state, &warehouse_params("w1")).unwrap());
        assert!(!exclusive("w1"), "serving w1 holds its format");

        drop(resolve_handle(&state, &warehouse_params("w2")).unwrap());
        assert!(exclusive("w1"), "w1 was let go");
        assert!(!exclusive("w2"));

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A warehouse a request still holds is never let go, however full the registry; an idle one
    /// is, and letting it go releases its serve lock.
    ///
    /// Revert: `make_room` stops requiring `strong_count == 1` → the held `w1` is let go while its
    /// handle (and serve lock) lives on, so resolving `w1` again is refused by its own stale serve
    /// lock instead of returning the held handle → red.
    #[test]
    fn a_busy_warehouse_is_never_let_go_and_an_idle_one_is() {
        let base = scratch_dir("cap-busy-idle");
        let state = capped_state(&base, 1, &["w1", "w2", "w3"]);

        let held = resolve_handle(&state, &warehouse_params("w1")).unwrap();

        // Nothing is idle, so w2 is admitted over the cap rather than refused.
        drop(resolve_handle(&state, &warehouse_params("w2")).unwrap());
        assert_eq!(live_ids(&state), ["w1", "w2"].map(String::from).into());

        // w2 is idle now, w1 is not: serving w3 lets exactly w2 go.
        drop(resolve_handle(&state, &warehouse_params("w3")).unwrap());
        assert_eq!(live_ids(&state), ["w1", "w3"].map(String::from).into());
        assert!(!serve_lock_exists(&base, "w2"), "a warehouse let go releases its serve lock");
        assert!(serve_lock_exists(&base, "w1"));

        let again = resolve_handle(&state, &warehouse_params("w1"))
            .unwrap_or_else(|e| panic!("the busy warehouse must still be live: {:?}", e));
        assert!(Arc::ptr_eq(&held, &again), "one handle per warehouse, still the held one");

        // A warehouse that was let go is served again, under a new serve lock.
        drop((held, again));
        resolve_handle(&state, &warehouse_params("w2")).unwrap();
        assert!(serve_lock_exists(&base, "w2"));

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A warehouse that cannot be served (here: under `gc`, which holds its serve lock) is refused
    /// without letting a live one go for it.
    ///
    /// Revert: `resolve_warehouse` makes room before serving → the idle `w1` is let go for a
    /// warehouse that is then refused → red.
    #[test]
    fn a_refused_warehouse_costs_no_live_one_its_place() {
        let base = scratch_dir("cap-refused");
        let state = capped_state(&base, 1, &["w1", "busy"]);
        std::fs::write(base.join("busy").join(FOLDER_NAME_FORKLIFT_ROOT).join("serve.lock"), b"1").unwrap();

        drop(resolve_handle(&state, &warehouse_params("w1")).unwrap());
        let refused = resolve_handle(&state, &warehouse_params("busy")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE, "{}", refused.1);
        assert_eq!(live_ids(&state), ["w1"].map(String::from).into());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Restrict the fetch scope of the warehouse at `root`, as `clone --only src` does.
    fn make_sparse(root: &std::path::Path) {
        mark_warehouse_as(root, "sparse");
        let _scope = StorageRootScope::enter(root);
        scope_utils::set_fetch_scope(&scope_utils::PathScope::from_prefixes(["src"])).unwrap();
    }

    /// A sparse warehouse is never served, and the refusal is a 409 naming it as sparse rather than
    /// the 503 a busy one gets. A full warehouse beside it is served.
    ///
    /// Revert: drop the sparse check from `serving` → `sparse` is served and becomes live → red.
    #[test]
    fn a_sparse_warehouse_is_refused_with_a_409() {
        let base = scratch_dir("sparse-multi");
        let state = capped_state(&base, 4, &["full", "sparse"]);
        make_sparse(&base.join("sparse"));

        // A scope prune holding the store exclusively does not mask the answer as "busy".
        let pruning = {
            let _scope = StorageRootScope::enter(&base.join("sparse"));
            StoreAccessLock::try_exclusive().expect("exclusive hold")
        };
        let refused = resolve_handle(&state, &warehouse_params("sparse")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        drop(pruning);

        let refused = resolve_handle(&state, &warehouse_params("sparse")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.starts_with("Warehouse \"sparse\" is a partial warehouse"), "{}", refused.1);
        assert!(refused.1.contains("without --only or --cache"), "{}", refused.1);
        assert!(!serve_lock_exists(&base, "sparse"), "a refused warehouse keeps no serve lock");

        drop(resolve_handle(&state, &warehouse_params("full")).unwrap());
        assert_eq!(live_ids(&state), ["full"].map(String::from).into());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// The single-root path gets the same refusal from the same funnel, for a sparse root and a
    /// cache, and a fetch scope or warehouse marker that cannot be read is refused rather than read
    /// as full.
    ///
    /// Revert: drop the sparse check from `serving` → the sparse root is served → red. Revert:
    /// leave the cache mode out of `content_may_be_absent` → the cache root is served → red. Revert:
    /// read an unreadable fetch scope or marker as full (`unwrap_or(false)`) → the garbled root or
    /// the cache is served → red. Revert: read a sparse root without its fetch scope as full → the
    /// `gone` root is served → red.
    #[test]
    fn serving_refuses_a_partial_root_and_an_unreadable_scope_or_marker() {
        let base = scratch_dir("sparse-single");

        let full = base.join("full");
        mark_warehouse(&full);
        drop(WarehouseHandle::serving(full.clone(), false).expect("a full root is served"));

        let sparse = base.join("sparse");
        mark_warehouse(&sparse);
        make_sparse(&sparse);
        let refused = WarehouseHandle::serving(sparse.clone(), false).err().expect("a sparse root is refused");
        assert!(matches!(refused, ServingRefusal::Partial), "{:?}", refused);
        let message = refused.message("\"/srv/wh\"");
        assert!(message.starts_with("\"/srv/wh\" is a partial warehouse"), "{}", message);

        // A cache holds its full fetch scope, so only its marker says it may lack content.
        let cache = base.join("cache");
        mark_warehouse_as(&cache, "cache");
        let refused = WarehouseHandle::serving(cache.clone(), false).err().expect("a cache root is refused");
        assert!(matches!(refused, ServingRefusal::Partial), "{:?}", refused);

        // A marker whose mode this build cannot read is refused rather than read as full.
        mark_warehouse_as(&cache, "shallow");
        let refused = WarehouseHandle::serving(cache, false).err().expect("an unreadable marker is refused");
        assert!(matches!(refused, ServingRefusal::Unreadable(_)), "{:?}", refused);

        let garbled = base.join("garbled");
        mark_warehouse(&garbled);
        make_sparse(&garbled);
        {
            let _scope = StorageRootScope::enter(&garbled);
            std::fs::write(scope_utils::fetch_scope_path(), b"not a scope file").unwrap();
        }
        let refused = WarehouseHandle::serving(garbled, false).err().expect("an unreadable scope is refused");
        assert!(matches!(refused, ServingRefusal::Unreadable(_)), "{:?}", refused);

        // The mode is recorded in the marker, so neither deleting a sparse root's fetch scope nor
        // placing one in a full root makes either servable or reads either as full.
        let gone = base.join("gone");
        mark_warehouse(&gone);
        make_sparse(&gone);
        forklift_core::util::file_utils::remove_file(gone.join(FOLDER_NAME_FORKLIFT_ROOT).join("config").join("fetch-scope")).unwrap();
        let refused = WarehouseHandle::serving(gone, false).err().expect("a sparse root without its scope is refused");
        assert!(matches!(&refused, ServingRefusal::Unreadable(reason) if reason.contains("is missing")), "{:?}", refused);

        let stray = base.join("stray");
        mark_warehouse(&stray);
        std::fs::write(stray.join(FOLDER_NAME_FORKLIFT_ROOT).join("config").join("fetch-scope"), b"").unwrap();
        let refused = WarehouseHandle::serving(stray, false).err().expect("a full root with a fetch scope is refused");
        assert!(matches!(&refused, ServingRefusal::Unreadable(reason) if reason.contains("holds a fetch scope")), "{:?}", refused);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Write the sealed-object record of the warehouse at `root` as the header and one frame per op,
    /// with no origin in sight: the record is read, never inferred from the mode.
    fn write_sealed(root: &std::path::Path, ops: &[(sealed_utils::SealOp, &str)]) {
        let _scope = StorageRootScope::enter(root);
        let mut bytes = sealed_utils::header_bytes();
        for (op, hash) in ops {
            bytes.extend(sealed_utils::encode_frame(*op, hash).unwrap());
        }
        std::fs::write(sealed_utils::sealed_path(), bytes).unwrap();
    }

    /// A root marked full whose sealed-object record holds an object is refused, as a partial one
    /// is: it holds that object by hash alone. One whose record replays to nothing is served, and
    /// one whose record cannot be read is refused rather than read as empty.
    ///
    /// Revert: drop the sealed check from `serving` → `sealed` is served → red. Revert: refuse a
    /// root that merely has a record file → `emptied` (a record and its removal) is refused → red.
    /// Revert: read an unreadable record as empty (`unwrap_or_default`) → `garbled` is served →
    /// red. Revert: drop the origin check → `origined` is served → red. A torn append after frames
    /// that replay to nothing reads as nothing recorded, so `emptied` is served with one.
    #[test]
    fn serving_refuses_a_full_root_whose_sealed_record_holds_an_object() {
        use sealed_utils::SealOp;
        let base = scratch_dir("sealed-single");
        let one = "1".repeat(64);
        let two = "2".repeat(64);

        let sealed = base.join("sealed");
        mark_warehouse(&sealed);
        write_sealed(&sealed, &[(SealOp::Record, &one), (SealOp::Record, &two), (SealOp::Remove, &two)]);
        let refused = WarehouseHandle::serving(sealed.clone(), false).err().expect("a sealed root is refused");
        assert!(matches!(refused, ServingRefusal::Sealed(1)), "{:?}", refused);
        let message = refused.message("\"/srv/wh\"");
        assert!(message.starts_with("\"/srv/wh\" records 1 object it holds by hash alone"), "{}", message);
        assert!(message.contains("clone the warehouse it was copied from again, from its server, into a new root"), "{}", message);
        assert!(!sealed.join(FOLDER_NAME_FORKLIFT_ROOT).join("serve.lock").exists(), "a refused root keeps no serve lock");

        let emptied = base.join("emptied");
        mark_warehouse(&emptied);
        write_sealed(&emptied, &[(SealOp::Record, &one), (SealOp::Remove, &one)]);
        {
            let _scope = StorageRootScope::enter(&emptied);
            let mut torn = sealed_utils::encode_frame(SealOp::Record, &two).unwrap();
            torn.truncate(torn.len() / 2);
            let mut bytes = std::fs::read(sealed_utils::sealed_path()).unwrap();
            bytes.extend(torn);
            std::fs::write(sealed_utils::sealed_path(), bytes).unwrap();
        }
        drop(WarehouseHandle::serving(emptied, false).expect("a record that replays to nothing is served"));

        let garbled = base.join("garbled");
        mark_warehouse(&garbled);
        {
            let _scope = StorageRootScope::enter(&garbled);
            std::fs::write(sealed_utils::sealed_path(), b"/forklift-sealed 999\n").unwrap();
        }
        let refused = WarehouseHandle::serving(garbled, false).err().expect("an unreadable record is refused");
        assert!(matches!(&refused, ServingRefusal::Unreadable(reason) if reason.contains("sealed-object record")), "{:?}", refused);

        // An origin is what lets a new entry be recorded, so a full root that records one is
        // refused even with nothing recorded yet.
        let origined = base.join("origined");
        mark_warehouse(&origined);
        {
            let _scope = StorageRootScope::enter(&origined);
            origin_utils::write_for_tests("http://origin.example");
        }
        let refused = WarehouseHandle::serving(origined, false).err().expect("a root recording an origin is refused");
        assert!(matches!(refused, ServingRefusal::Origin), "{:?}", refused);
        let message = refused.message("Warehouse \"origined\"");
        assert!(message.starts_with("Warehouse \"origined\" records an origin (.forklift/origin)"), "{}", message);
        assert!(message.contains("Clone that origin again into a new root"), "{}", message);
        assert!(!message.contains("origin.example"), "the origin URL never reaches a client: {}", message);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A full root lacking content its unavailable record lists is refused, counting only the
    /// absent entries: one whose object it holds again is served. One whose record cannot be read
    /// is refused as unreadable, and a multi-warehouse server answers the refusal with a 409.
    ///
    /// Revert: drop the check → the root lacking the content is served → red; count every entry →
    /// the root holding its content is refused → red; read a garbled record as empty → served → red;
    /// map `Unavailable` to a 500 → red on its status.
    #[test]
    fn serving_refuses_a_root_lacking_content_its_unavailable_record_lists() {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::model::blob::Blob;
        let base = scratch_dir("unavailable");
        let absent = "1".repeat(64);
        let entry = |hash: &str| (hash.to_string(), "DMCA notice 5".to_string());

        let lacking = base.join("lacking");
        mark_warehouse(&lacking);
        let present = {
            let _scope = StorageRootScope::enter(&lacking);
            let mut blob = LooseObjectBuilder::build_blob(&Blob { content: b"came back".to_vec() });
            blob.store().unwrap();
            unavailable::record([entry(&absent), entry(&blob.hash)]).unwrap();
            blob.hash
        };
        let refused = WarehouseHandle::serving(lacking.clone(), false).err().expect("a root lacking content is refused");
        assert!(matches!(refused, ServingRefusal::Unavailable(1)), "{:?}", refused);
        let message = refused.message("\"/srv/wh\"");
        assert!(message.starts_with("\"/srv/wh\" lacks 1 file its record of unavailable content lists"), "{}", message);
        assert!(message.contains("Load the missing files' bytes into this warehouse"), "{}", message);
        assert!(!lacking.join(FOLDER_NAME_FORKLIFT_ROOT).join("serve.lock").exists(), "a refused root keeps no serve lock");

        let back = base.join("back");
        mark_warehouse(&back);
        {
            let _scope = StorageRootScope::enter(&back);
            let mut blob = LooseObjectBuilder::build_blob(&Blob { content: b"came back".to_vec() });
            blob.store().unwrap();
            unavailable::record([entry(&present)]).unwrap();
        }
        drop(WarehouseHandle::serving(back, false).expect("a root holding every listed object is served"));

        let garbled = base.join("garbled");
        mark_warehouse(&garbled);
        std::fs::write(garbled.join(FOLDER_NAME_FORKLIFT_ROOT).join("unavailable"), b"/forklift-unavailable 999\n").unwrap();
        let refused = WarehouseHandle::serving(garbled, false).err().expect("an unreadable record is refused");
        assert!(matches!(&refused, ServingRefusal::Unreadable(reason) if reason.contains("cannot get")), "{:?}", refused);

        let multi = base.join("multi");
        let state = capped_state(&multi, 4, &["lacking"]);
        {
            let _scope = StorageRootScope::enter(&multi.join("lacking"));
            unavailable::record([entry(&absent)]).unwrap();
        }
        let refused = resolve_handle(&state, &warehouse_params("lacking")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.starts_with("Warehouse \"lacking\" lacks 1 file"), "{}", refused.1);

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A multi-warehouse server refuses a sealed root, and one that records an origin, with the 409
    /// a partial one gets, keeps no serve lock for either and makes neither live; a full one beside
    /// them is served.
    ///
    /// Revert: map `Sealed` (or `Origin`) to the 500 of an unreadable root → red on its status.
    #[test]
    fn a_multi_server_refuses_a_sealed_warehouse_with_a_409() {
        let base = scratch_dir("sealed-multi");
        let state = capped_state(&base, 4, &["full", "sealed", "origined"]);
        write_sealed(&base.join("sealed"), &[(sealed_utils::SealOp::Record, &"3".repeat(64))]);
        {
            let _scope = StorageRootScope::enter(&base.join("origined"));
            origin_utils::write_for_tests("http://origin.example");
        }

        let refused = resolve_handle(&state, &warehouse_params("sealed")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.starts_with("Warehouse \"sealed\" records 1 object it holds by hash alone"), "{}", refused.1);
        assert!(!serve_lock_exists(&base, "sealed"), "a refused warehouse keeps no serve lock");

        let refused = resolve_handle(&state, &warehouse_params("origined")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.starts_with("Warehouse \"origined\" records an origin (.forklift/origin)"), "{}", refused.1);
        assert!(!serve_lock_exists(&base, "origined"), "a refused warehouse keeps no serve lock");

        drop(resolve_handle(&state, &warehouse_params("full")).unwrap());
        assert_eq!(live_ids(&state), ["full"].map(String::from).into());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A multi-warehouse server refuses an unfinished clone (503) as single mode does: it has
    /// not finished fetching, so what it holds cannot be trusted yet. A fetch scope that cannot be
    /// read is a 500.
    ///
    /// Revert: drop the clone gate from `serving` → `unfinished` is served → red.
    #[test]
    fn a_multi_server_refuses_an_unfinished_franchise_and_an_unreadable_scope() {
        let base = scratch_dir("unfinished-multi");
        let state = capped_state(&base, 4, &["unfinished", "garbled"]);

        let forklift = base.join("unfinished").join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::write(franchise_utils::record_path(&forklift), b"torn").unwrap();
        let refused = resolve_handle(&state, &warehouse_params("unfinished")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE, "{}", refused.1);
        assert!(refused.1.starts_with("Warehouse \"unfinished\" is temporarily unavailable"), "{}", refused.1);
        assert!(!refused.1.contains(&*base.to_string_lossy()), "no server path reaches the client: {}", refused.1);
        assert!(!serve_lock_exists(&base, "unfinished"), "a refused warehouse keeps no serve lock");

        make_sparse(&base.join("garbled"));
        {
            let _scope = StorageRootScope::enter(&base.join("garbled"));
            std::fs::write(scope_utils::fetch_scope_path(), b"not a scope file").unwrap();
        }
        let refused = resolve_handle(&state, &warehouse_params("garbled")).err().expect("refused");
        assert_eq!(refused.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", refused.1);

        assert!(live_ids(&state).is_empty());
        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// The idle warehouse let go is the least recently resolved one.
    ///
    /// Revert: `get` stops marking the entry as used → `a` is the oldest insert and is let go
    /// instead of `b` → red.
    #[test]
    fn the_least_recently_used_idle_warehouse_goes_first() {
        let base = scratch_dir("cap-lru");
        let state = capped_state(&base, 2, &["a", "b", "c"]);

        drop(resolve_handle(&state, &warehouse_params("a")).unwrap());
        drop(resolve_handle(&state, &warehouse_params("b")).unwrap());
        drop(resolve_handle(&state, &warehouse_params("a")).unwrap());
        drop(resolve_handle(&state, &warehouse_params("c")).unwrap());

        assert_eq!(live_ids(&state), ["a", "c"].map(String::from).into());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// The handshake reports a withheld hash's deletion owed while the store holds a copy, and
    /// done once it holds none.
    ///
    /// Revert: report every withheld hash deleted → the stored one is not `deletion_owed`.
    #[test]
    fn the_handshake_owes_a_withheld_deletion_while_a_copy_is_stored() {
        let root = scratch_dir("withheld-handshake");
        let _scope = StorageRootScope::enter(&root);
        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();

        let bytes: &[u8] = &object_utils::blob_bytes(b"a withheld copy still stored");
        let stored = object_utils::hash_object_bytes(bytes);
        object_utils::store_object_bytes(&stored, bytes).unwrap();
        let gone = object_utils::hash_object_bytes(&object_utils::blob_bytes(b"never stored here"));
        withholding::withhold(&stored, "statement").unwrap();
        withholding::withhold(&gone, "statement").unwrap();

        let states: HashMap<String, ServedWithholdingState> = served_withheld().unwrap().into_iter()
            .map(|served| (served.hash, served.state)).collect();
        assert!(matches!(states[&stored], ServedWithholdingState::DeletionOwed));
        assert!(matches!(states[&gone], ServedWithholdingState::Deleted));

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A withheld recipe the store no longer holds is still owed while the chunk journal owes a
    /// chunk of it the store holds, as a deletion cut short between its passes leaves it.
    ///
    /// Revert: judge a withheld hash by its object alone → the recipe reads `deleted`.
    #[test]
    fn the_handshake_owes_a_withheld_deletion_while_a_chunk_of_it_is_owed() {
        let root = scratch_dir("withheld-handshake-chunks");
        let _scope = StorageRootScope::enter(&root);
        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();

        let chunk_bytes: &[u8] = &object_utils::blob_bytes(b"a chunk the journal still owes");
        let chunk = object_utils::hash_object_bytes(chunk_bytes);
        object_utils::store_object_bytes(&chunk, chunk_bytes).unwrap();
        let recipe = object_utils::hash_object_bytes(&object_utils::blob_bytes(b"a deleted recipe"));
        withholding::withhold(&recipe, "statement").unwrap();
        let journal = forklift_core::util::record_utils::to_bytes(
            "redactions-chunks-owed", 1, [format!("{} {}", recipe, chunk)]
        );
        std::fs::write(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("redactions-chunks-owed"), journal).unwrap();
        assert_eq!(redaction_deleter::recipes_owing_chunks().unwrap(), [recipe.clone()].into());

        let served = served_withheld().unwrap();

        assert_eq!(served.len(), 1);
        assert!(matches!(served[0].state, ServedWithholdingState::DeletionOwed));

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// Letting a warehouse go forgets its cached packs. Once it is not live, another process may
    /// delete packed objects (a full repack), and a cached pack set is trusted on a hit, so a kept
    /// one would answer "present" for an object that is gone — to a client deciding what to
    /// upload, and to the closure check of a ref update.
    ///
    /// Revert: `let_go` stops calling `pack_utils::invalidate_cache` → the deleted object still
    /// reads present → red.
    #[test]
    fn a_warehouse_let_go_forgets_its_packs() {
        let base = scratch_dir("cap-packs");
        let state = capped_state(&base, 1, &["w1", "w2"]);
        let root = base.join("w1");

        let hash = {
            let _scope = StorageRootScope::enter(&root);
            warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();
            let bytes: &[u8] = &object_utils::blob_bytes(b"an object only a pack holds");
            let hash = object_utils::hash_object_bytes(bytes);
            object_utils::store_object_bytes(&hash, bytes).unwrap();
            pack_utils::compact(false, false, false, &forklift_core::util::progress_utils::Progress::NONE)
                .unwrap();
            hash
        };
        let pack_folder = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects").join("pack");
        assert!(std::fs::read_dir(&pack_folder).unwrap().next().is_some(), "the object is packed");

        let present = |state: &AppState| {
            let handle = resolve_handle(state, &warehouse_params("w1")).unwrap();
            let _scope = StorageRootScope::enter(&handle.root);
            file_utils::does_object_exist(&hash).unwrap()
        };
        assert!(present(&state), "served, the packed object is present");

        // w1 is idle, so serving w2 lets it go; then its pack disappears, as a repack run by
        // another process would make it.
        drop(resolve_handle(&state, &warehouse_params("w2")).unwrap());
        assert!(!live_ids(&state).contains("w1"));
        for entry in std::fs::read_dir(&pack_folder).unwrap() {
            forklift_core::util::file_utils::remove_file(entry.unwrap().path()).unwrap();
        }

        assert!(!present(&state), "served again, the deleted object must read missing");

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Letting a warehouse go forgets the objects and trees this process has read from it too: a
    /// ref update's closure check loads trees through those caches, and both answer a hit without
    /// looking at the disk.
    ///
    /// Revert: `pack_utils::forget_warehouse` stops calling `file_utils::forget_cached_objects`
    /// (or `object_utils::forget_cached_trees`) → the deleted blob (or tree) still loads → red.
    #[test]
    fn a_warehouse_let_go_forgets_the_objects_it_read() {
        let base = scratch_dir("cap-objects");
        let state = capped_state(&base, 1, &["w1", "w2"]);
        let root = base.join("w1");
        {
            let _scope = StorageRootScope::enter(&root);
            warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();
        }

        let (blob, tree) = {
            let handle = resolve_handle(&state, &warehouse_params("w1")).unwrap();
            let _scope = StorageRootScope::enter(&handle.root);
            let bytes: &[u8] = &object_utils::blob_bytes(b"an object read while served");
            let blob = object_utils::hash_object_bytes(bytes);
            object_utils::store_object_bytes(&blob, bytes).unwrap();
            let tree = object_utils::empty_tree_hash().unwrap();
            file_utils::retrieve_object_by_hash(&blob).unwrap();
            object_utils::load_tree_shared(&tree).unwrap();
            (blob, tree)
        };

        // w1 goes idle and is let go; then its objects disappear, as `gc` would make them.
        drop(resolve_handle(&state, &warehouse_params("w2")).unwrap());
        assert!(!live_ids(&state).contains("w1"));
        let objects = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("objects");
        forklift_core::util::file_utils::remove_dir_all(&objects).unwrap();
        std::fs::create_dir(&objects).unwrap();

        let handle = resolve_handle(&state, &warehouse_params("w1")).unwrap();
        let _scope = StorageRootScope::enter(&handle.root);
        assert!(file_utils::retrieve_object_by_hash(&blob).is_err(), "the deleted blob must not load");
        assert!(object_utils::load_tree_shared(&tree).is_err(), "the deleted tree must not load");

        drop((_scope, handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Wait, up to five seconds, until nothing but the registry holds warehouse `id`'s handle and
    /// no redaction run is running for it: the tasks a request leaves behind (a download's, a
    /// redaction run's) let go of it only once they are scheduled.
    async fn settle(state: &AppState, id: &str) {
        for _ in 0..500 {
            let settled = state.warehouses.lock().unwrap().entries.get(id).is_none_or(|entry| {
                Arc::strong_count(&entry.handle) == 1 && !entry.handle.redacting.load(Ordering::SeqCst)
            });

            if settled {
                return;
            }

            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        panic!("warehouse {} did not settle", id);
    }

    /// A bundle download holds its warehouse until the body is dropped, so the warehouse is not
    /// let go mid-stream.
    ///
    /// Revert: `get_bundle` streams the bare file again → the handle drops when the handler
    /// returns and `w1` is let go while its body is alive → red.
    #[tokio::test]
    async fn a_bundle_download_keeps_its_warehouse_live() {
        let base = scratch_dir("cap-bundle");
        let state = Arc::new(capped_state(&base, 1, &["w1", "w2", "w3"]));
        {
            let _scope = StorageRootScope::enter(&base.join("w1"));
            let path = bundle_utils::get_latest_bundle_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, format!("{}\nbundle bytes", bundle_utils::BUNDLE_HEADER)).unwrap();
        }

        // Its first redaction run, which also holds it, ends before the download starts.
        drop(resolve_warehouse(&state, &warehouse_params("w1")).unwrap());
        settle(&state, "w1").await;

        let response = get_bundle(State(Arc::clone(&state)), HeaderMap::new(),
                                  Path(warehouse_params("w1"))).await;
        assert_eq!(response.status(), StatusCode::OK);

        drop(resolve_handle(&state, &warehouse_params("w2")).unwrap());
        assert!(live_ids(&state).contains("w1"), "a streaming download keeps its warehouse live");

        drop(response);
        settle(&state, "w1").await;
        drop(resolve_handle(&state, &warehouse_params("w3")).unwrap());
        assert!(!live_ids(&state).contains("w1"), "once the body is gone, the warehouse is idle");

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A drained warehouse is refused with a `503` that says when to try again, and served again
    /// once the drain ends.
    ///
    /// Revert: resolve without looking at the drain mark → the request is served → red. Drop the
    /// `Retry-After` → red.
    #[tokio::test]
    async fn a_drained_warehouse_is_refused_until_it_is_served_again() {
        let base = scratch_dir("drain-refused");
        let state = capped_state(&base, 4, &["w1"]);
        let handle = resolve_handle(&state, &warehouse_params("w1")).unwrap();

        assert!(drain(&state, &handle, std::time::Duration::from_secs(5)).await, "nothing else holds it");
        let refused = resolve_handle(&state, &warehouse_params("w1")).err().expect("refused while drained");
        assert_eq!(refused.0, StatusCode::SERVICE_UNAVAILABLE);
        let response = error_response(refused);
        assert_eq!(response.headers().get(axum::http::header::RETRY_AFTER).unwrap(), &RETRY_AFTER_SECONDS.to_string());

        undrain(&state, &handle);
        let again = resolve_handle(&state, &warehouse_params("w1")).expect("served again");
        assert!(Arc::ptr_eq(&again, &handle), "the same handle, never a second one");

        drop((again, handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A drain waits for the requests holding the warehouse, and past its bound gives up and serves
    /// the warehouse again; once they end, it drains.
    ///
    /// Revert: drain without waiting for the handle to go idle → the first drain succeeds → red.
    /// Keep the mark when the drain gives up → the warehouse stays refused → red.
    #[tokio::test]
    async fn a_drain_waits_for_requests_and_gives_up_at_its_bound() {
        let base = scratch_dir("drain-bound");
        let state = capped_state(&base, 4, &["w1"]);
        let handle = resolve_handle(&state, &warehouse_params("w1")).unwrap();
        let request = resolve_handle(&state, &warehouse_params("w1")).unwrap();

        assert!(!drain(&state, &handle, std::time::Duration::from_millis(100)).await, "a request holds it");
        drop(resolve_handle(&state, &warehouse_params("w1")).expect("served again after the drain gave up"));

        drop(request);
        assert!(drain(&state, &handle, std::time::Duration::from_secs(5)).await, "the request ended");
        undrain(&state, &handle);

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// The deleter trades the served warehouse's shared store-access hold for the exclusive side,
    /// and takes the shared hold back after: from then on another process's deleter is refused
    /// again. While another holder shares the store, the deletion is owed and the hold is kept.
    ///
    /// Revert: leave the shared hold released after the deletion → a deleter runs beside the served
    /// warehouse → red. Keep the shared hold while trying the exclusive side → the deletion is
    /// always owed → red.
    #[tokio::test]
    async fn the_deleter_takes_the_shared_hold_back_after_it() {
        let base = scratch_dir("deleter-hold");
        std::fs::create_dir_all(base.join("w1")).unwrap();
        {
            let _scope = StorageRootScope::enter(&base.join("w1"));
            warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();
        }
        let state = capped_state(&base, 4, &[]);
        let handle = resolve_handle(&state, &warehouse_params("w1")).unwrap();
        let _scope = StorageRootScope::enter(&handle.root);

        let (deletion, holds_store) = delete_drained(&handle);
        deletion.expect("nothing else holds the store");
        assert!(holds_store);
        assert!(StoreAccessLock::try_exclusive().is_err(), "the served warehouse holds the store again");

        let other = StoreAccessLock::try_shared().unwrap();
        let (deletion, holds_store) = delete_drained(&handle);
        assert!(deletion.is_err(), "owed while another holder shares the store");
        assert!(holds_store);
        drop(other);
        assert!(StoreAccessLock::try_exclusive().is_err(), "the served warehouse still holds the store");

        drop((_scope, handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Cutting a warehouse's bundle downloads ends one whose client takes nothing in an error,
    /// never as if it were whole; one started after the cut is whole.
    ///
    /// Revert: stream the file without the cut → the stalled download is whole → red. End the cut
    /// body quietly → it reads as complete → red.
    #[tokio::test]
    async fn a_cut_ends_a_stalled_bundle_download_short() {
        let base = scratch_dir("bundle-cut");
        let state = Arc::new(capped_state(&base, 4, &["w1"]));
        let bundle = {
            let _scope = StorageRootScope::enter(&base.join("w1"));
            let path = bundle_utils::get_latest_bundle_path();
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            let mut bundle = format!("{}\n", bundle_utils::BUNDLE_HEADER).into_bytes();
            bundle.resize(4 * 1024 * 1024, b'x');
            std::fs::write(&path, &bundle).unwrap();
            bundle
        };

        let stalled = get_bundle(State(Arc::clone(&state)), HeaderMap::new(), Path(warehouse_params("w1"))).await;
        assert_eq!(stalled.status(), StatusCode::OK);
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        cut_bundle_downloads(&resolve_handle(&state, &warehouse_params("w1")).unwrap());
        let received = axum::body::to_bytes(stalled.into_body(), usize::MAX).await;
        assert!(received.is_err(), "the cut download ends in an error, never as if whole");

        let whole = get_bundle(State(Arc::clone(&state)), HeaderMap::new(), Path(warehouse_params("w1"))).await;
        assert_eq!(axum::body::to_bytes(whole.into_body(), usize::MAX).await.unwrap().len(), bundle.len());

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A handle served for the first time starts a redaction run of its own, once, so a deletion
    /// owed when the warehouse was last served is carried out without waiting for a ref write. It
    /// starts once the withheld record's watch has read its baseline ([`start_serving`]).
    ///
    /// Revert: start runs only after ref writes → no round is asked → red.
    #[tokio::test]
    async fn a_warehouse_served_anew_starts_one_redaction_run() {
        let base = scratch_dir("first-run");
        let state = Arc::new(capped_state(&base, 4, &["w1"]));

        let handle = resolve_warehouse(&state, &warehouse_params("w1")).unwrap();
        drop(resolve_warehouse(&state, &warehouse_params("w1")).unwrap());
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        while handle.redactions_asked.load(Ordering::SeqCst) == 0 && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(handle.redactions_asked.load(Ordering::SeqCst), 1, "one run, at the first serve");

        drop(handle);
        settle(&state, "w1").await;
        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// An apply that fails is owed to the redaction run, from a ref write's and from a round's
    /// own: the run retries it rather than leave the content served until another write.
    ///
    /// Revert: discharge a failed apply's owes → nothing is owed after the write → red. Report a
    /// round's apply as settled whatever it did → the failed round owes nothing → red.
    #[tokio::test]
    async fn a_failed_apply_is_owed_to_the_redaction_run() {
        let root = scratch_dir("apply-owed");
        mark_warehouse(&root);
        // A record this build cannot read fails every apply.
        std::fs::write(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("redactions-applied"),
                       b"/forklift-redactions-applied 2\n").unwrap();
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        // A run is under way, so the write starts none.
        handle.redacting.store(true, Ordering::SeqCst);

        drop(handle.owe_apply());
        after_ref_write(&state, Arc::clone(&handle)).await;
        assert!(handle.applies_closed(), "the write's failed apply is owed");

        assert!(redaction_round(&state, &handle).await.apply, "the round that fails it again owes it");
        assert!(handle.applies_closed(), "and it stays owed for the next round");

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// An apply discharges only the owes of writes that returned before it began: one still
    /// writing may land after the apply read the warehouse, so its owe outlives that apply.
    ///
    /// Revert: discharge every owe taken (`applies_owed` alone) → the write still in flight is
    /// discharged → red.
    #[tokio::test]
    async fn an_apply_leaves_a_write_in_flight_owed() {
        let root = scratch_dir("apply-in-flight");
        mark_warehouse(&root);
        let handle = Arc::new(WarehouseHandle::new(root.clone()));

        drop(handle.owe_apply());
        let writing = handle.owe_apply();
        let (_, applied) = apply_discharging(&handle).await;
        applied.unwrap();
        assert!(handle.applies_closed(), "the write in flight is still owed");

        drop(writing);
        apply_discharging(&handle).await.1.unwrap();
        assert!(!handle.applies_closed(), "an apply after it returned discharges it");

        drop(handle);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A content request waiting on an owed apply is answered as soon as one discharges it, and a
    /// drain answers it at once rather than at the wait's bound.
    ///
    /// Revert: leave the waiters unwoken by a discharge → it waits out the bound and is refused →
    /// red. Leave them unwoken by the drain → it waits past the 1 s the test allows → red.
    #[tokio::test]
    async fn a_content_wait_ends_on_a_discharge_or_a_drain() {
        let root = scratch_dir("apply-wait");
        mark_warehouse(&root);
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        let bound = std::time::Duration::from_secs(1);

        drop(handle.owe_apply());
        let waiting = tokio::spawn({
            let handle = Arc::clone(&handle);
            async move { await_applies(&handle).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        apply_discharging(&handle).await.1.unwrap();
        let answered = tokio::time::timeout(bound, waiting).await.expect("woken by the discharge");
        assert!(answered.unwrap().is_ok(), "content is served once nothing is owed");

        drop(handle.owe_apply());
        let waiting = tokio::spawn({
            let handle = Arc::clone(&handle);
            async move { await_applies(&handle).await }
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // The drain waits for the waiter to let go of the handle, which it does once woken.
        let (_, refused) = tokio::join!(drain(&state, &handle, std::time::Duration::from_millis(200)),
                                        tokio::time::timeout(bound, waiting));
        let refused = refused.expect("woken by the drain").unwrap().unwrap_err();
        assert_eq!(refused, draining_refusal());

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// Asking for a redaction round wakes only a run waiting out a retry: it leaves no permit that
    /// would cut the run's next wait short.
    ///
    /// Revert: wake with `notify_one` → the permit stays and a later wait ends at once → red.
    #[tokio::test]
    async fn an_ask_leaves_no_wake_behind() {
        let root = scratch_dir("wake-permit");
        mark_warehouse(&root);
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        handle.redacting.store(true, Ordering::SeqCst);

        run_redactions(Arc::clone(&state), Arc::clone(&handle));

        let waited = tokio::time::timeout(std::time::Duration::from_millis(50), handle.redaction_wake.notified()).await;
        assert!(waited.is_err(), "no wake was left behind");

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A bundle in another version than this build writes is answered `404`, so a client falls
    /// back to its object walk without downloading it; a current one is served whole, from its
    /// first byte, although its header was read to judge it.
    ///
    /// Revert: serve whatever file is there → the previous-version file answers `200` → red. Drop
    /// the seek back to the start → the current file's body loses its header line → red.
    #[tokio::test]
    async fn a_bundle_in_a_previous_version_is_answered_not_found() {
        let base = scratch_dir("bundle-version");
        let state = Arc::new(capped_state(&base, 2, &["w1"]));
        let path = {
            let _scope = StorageRootScope::enter(&base.join("w1"));
            bundle_utils::get_latest_bundle_path()
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();

        for previous in ["forklift-bundle 2026-07-13\nnative packs", "forklift-bundle 2026-07-11\nrecords", "short"] {
            std::fs::write(&path, previous).unwrap();
            let response = get_bundle(State(Arc::clone(&state)), HeaderMap::new(),
                                      Path(warehouse_params("w1"))).await;
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{:?}", previous);
        }

        let current = format!("{}\nthe rest of the bundle", bundle_utils::BUNDLE_HEADER);
        std::fs::write(&path, &current).unwrap();
        let response = get_bundle(State(Arc::clone(&state)), HeaderMap::new(),
                                  Path(warehouse_params("w1"))).await;
        assert_eq!(response.status(), StatusCode::OK);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], current.as_bytes(), "the whole file is served from its first byte");

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// One bundle warehouse `w1` under `base`, and the path its bundle is served from.
    fn bundle_state(base: &std::path::Path) -> (Arc<AppState>, PathBuf) {
        let state = Arc::new(capped_state(base, 2, &["w1"]));
        let path = {
            let _scope = StorageRootScope::enter(&base.join("w1"));
            bundle_utils::get_latest_bundle_path()
        };
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        (state, path)
    }

    /// Publishes `bytes` as the bundle the way a rebuild does: a new file renamed over the path.
    fn publish_bundle(path: &std::path::Path, bytes: &[u8]) {
        let temp = path.with_extension("rebuild");
        std::fs::write(&temp, bytes).unwrap();
        file_utils::rename(&temp, path).unwrap();
    }

    /// `GET /v1/bundles/latest` with `headers` (name, value), answered: status, headers and body.
    async fn get_bundle_with(state: &Arc<AppState>,
                             headers: &[(&'static str, &str)]) -> (StatusCode, HeaderMap, Vec<u8>) {
        let mut request = HeaderMap::new();
        for (name, value) in headers {
            request.append(*name, HeaderValue::from_str(value).unwrap());
        }

        let response = get_bundle(State(Arc::clone(state)), request, Path(warehouse_params("w1"))).await;
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        (status, headers, body.to_vec())
    }

    fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
        headers.get(name).map(|value| value.to_str().unwrap())
    }

    /// A whole bundle carries a strong tag, its length and `Accept-Ranges`; a `Range: bytes=N-`
    /// under that tag, or under no `If-Range` at all, gets a `206` with exactly bytes `N..` of the
    /// file, `Content-Range` and the length of what is left.
    ///
    /// Revert: ignore `Range` → the resumed request answers `200` with the whole file → red.
    #[tokio::test]
    async fn a_ranged_bundle_request_under_the_current_tag_gets_the_rest() {
        let base = scratch_dir("bundle-range");
        let (state, path) = bundle_state(&base);
        let bundle = format!("{}\nthe rest of the bundle", bundle_utils::BUNDLE_HEADER).into_bytes();
        let length = bundle.len();
        publish_bundle(&path, &bundle);

        let (status, headers, body) = get_bundle_with(&state, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, bundle);
        assert_eq!(header(&headers, "content-length"), Some(length.to_string().as_str()));
        assert_eq!(header(&headers, "accept-ranges"), Some("bytes"));
        assert_eq!(header(&headers, "content-range"), None);
        let etag = header(&headers, "etag").expect("a whole bundle carries a tag").to_string();
        assert!(etag.len() == 34 && etag.starts_with('"') && etag.ends_with('"')
                    && etag[1..33].bytes().all(|byte| byte.is_ascii_hexdigit()),
                "a strong tag of 32 hex digits: {}", etag);

        let start = 7;
        let range = format!("bytes={}-", start);
        let (status, headers, body) =
            get_bundle_with(&state, &[("range", &range), ("if-range", &etag)]).await;
        assert_eq!(status, StatusCode::PARTIAL_CONTENT);
        assert_eq!(body, &bundle[start..], "exactly the bytes from the offset asked for");
        assert_eq!(header(&headers, "content-range"),
                   Some(format!("bytes {}-{}/{}", start, length - 1, length).as_str()));
        assert_eq!(header(&headers, "content-length"), Some((length - start).to_string().as_str()));
        assert_eq!(header(&headers, "etag"), Some(etag.as_str()));

        // The last byte alone is still a range, and so is the whole file asked for from byte 0; so
        // is one asked for with no `If-Range`, and one spelled with another case and spaces.
        let last = format!("bytes={}-", length - 1);
        let spelled = format!(" Bytes = {}- ", start);
        let unconditional: [(&str, &[(&'static str, &str)], usize); 4] = [
            ("the last byte", &[("range", &last), ("if-range", &etag)], length - 1),
            ("the first byte on", &[("range", "bytes=0-"), ("if-range", &etag)], 0),
            ("no If-Range", &[("range", "bytes=5-")], 5),
            ("another spelling", &[("range", &spelled), ("if-range", &etag)], start),
        ];

        for (case, request, from) in unconditional {
            let (status, headers, body) = get_bundle_with(&state, request).await;
            assert_eq!(status, StatusCode::PARTIAL_CONTENT, "{}", case);
            assert_eq!(body, &bundle[from..], "{}", case);
            assert_eq!(header(&headers, "content-range"),
                       Some(format!("bytes {}-{}/{}", from, length - 1, length).as_str()), "{}", case);
        }

        // An offset at or past the end, under the current tag or none, is not satisfiable.
        let at_end = format!("bytes={}-", length);
        let past_end = format!("bytes={}-", length + 5);
        for request in [&[("range", at_end.as_str()), ("if-range", etag.as_str())][..],
                        &[("range", past_end.as_str())][..]] {
            let (status, headers, body) = get_bundle_with(&state, request).await;
            assert_eq!(status, StatusCode::RANGE_NOT_SATISFIABLE, "{:?}", request);
            assert!(body.is_empty(), "{:?}", request);
            assert_eq!(header(&headers, "content-range"), Some(format!("bytes */{}", length).as_str()));
            assert_eq!(header(&headers, "content-length"), Some("0"));
            assert_eq!(header(&headers, "etag"), Some(etag.as_str()));
        }

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// An `If-Range` must name the current tag: after a rebuild, under a stale or weak tag, past
    /// the end under a stale tag, in another range form, or with a repeated `Range` or `If-Range`
    /// line, the answer is `200` with the whole current bundle and its own tag.
    ///
    /// Revert: honour `Range` without comparing `If-Range` → the stale-tag request answers `206`
    /// → red.
    #[tokio::test]
    async fn a_ranged_bundle_request_under_another_tag_gets_the_whole_current_bundle() {
        let base = scratch_dir("bundle-stale-range");
        let (state, path) = bundle_state(&base);
        let first = format!("{}\nthe first build", bundle_utils::BUNDLE_HEADER).into_bytes();
        publish_bundle(&path, &first);
        let (_, headers, _) = get_bundle_with(&state, &[]).await;
        let first_tag = header(&headers, "etag").unwrap().to_string();

        let rebuilt = format!("{}\nthe rebuilt bundle, longer", bundle_utils::BUNDLE_HEADER).into_bytes();
        publish_bundle(&path, &rebuilt);

        let weak = format!("W/{}", first_tag);
        let (_, headers, _) = get_bundle_with(&state, &[]).await;
        let current_tag = header(&headers, "etag").unwrap().to_string();
        assert_ne!(current_tag, first_tag, "a rebuild gets a new tag");
        let weak_current = format!("W/{}", current_tag);
        let past_end = format!("bytes={}-", rebuilt.len());
        let cases: [(&str, &[(&'static str, &str)]); 10] = [
            ("the tag before the rebuild", &[("range", "bytes=5-"), ("if-range", &first_tag)]),
            ("a tag never served", &[("range", "bytes=5-"), ("if-range", "\"stale\"")]),
            ("a weak tag", &[("range", "bytes=5-"), ("if-range", &weak)]),
            ("the current tag, weak", &[("range", "bytes=5-"), ("if-range", &weak_current)]),
            ("a date", &[("range", "bytes=5-"), ("if-range", "Sun, 25 Oct 2026 08:53:01 GMT")]),
            ("past the end under a stale tag", &[("range", &past_end), ("if-range", &first_tag)]),
            ("a closed range", &[("range", "bytes=5-9"), ("if-range", &current_tag)]),
            ("two ranges", &[("range", "bytes=5-,7-"), ("if-range", &current_tag)]),
            ("a repeated Range", &[("range", "bytes=5-"), ("range", "bytes=7-")]),
            ("a repeated If-Range", &[("range", "bytes=5-"), ("if-range", &current_tag),
                                      ("if-range", &current_tag)]),
        ];

        for (case, request) in cases {
            let (status, headers, body) = get_bundle_with(&state, request).await;
            assert_eq!(status, StatusCode::OK, "{}", case);
            assert_eq!(body, rebuilt, "{}: the whole current bundle", case);
            assert_eq!(header(&headers, "etag"), Some(current_tag.as_str()), "{}", case);
            assert_eq!(header(&headers, "content-length"), Some(rebuilt.len().to_string().as_str()), "{}", case);
            assert_eq!(header(&headers, "content-range"), None, "{}", case);
        }

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// With no tag to compare an `If-Range` with, a conditional range is never honoured; an
    /// unconditional one still is.
    ///
    /// Revert: skip the `If-Range` check when there is no tag → the conditional range is honoured
    /// → red.
    #[test]
    fn a_conditional_range_without_a_tag_gets_the_whole_bundle() {
        let mut headers = HeaderMap::new();
        headers.insert("range", HeaderValue::from_static("bytes=4-"));
        assert_eq!(requested_range(&headers, None, 10), BundleRange::From(4));

        headers.insert("if-range", HeaderValue::from_static("\"any\""));
        assert_eq!(requested_range(&headers, None, 10), BundleRange::Whole);
        assert_eq!(requested_range(&headers, Some("\"any\""), 10), BundleRange::From(4));
    }

    /// When a rebuild of equal length is renamed over the path after `get_bundle` opened the old
    /// file, the response streams the old file under the old file's tag, which differs from the
    /// rebuild's. On Unix the two files share their modification time as well, so only the inode
    /// tells them apart.
    ///
    /// Reverts: take the tag from the path's metadata → the response carries the rebuild's tag →
    /// red. Take it from the length alone → the two tags are equal → red. Drop the inode (Unix) →
    /// equal length and mtime give equal tags → red.
    #[tokio::test]
    async fn a_bundle_rebuilt_while_its_request_is_served_keeps_the_tag_of_the_bytes_sent() {
        let base = scratch_dir("bundle-swap");
        let (state, path) = bundle_state(&base);
        let old = format!("{}\nthe old build", bundle_utils::BUNDLE_HEADER).into_bytes();
        let new = format!("{}\nthe new build", bundle_utils::BUNDLE_HEADER).into_bytes();
        assert_eq!(old.len(), new.len());
        publish_bundle(&path, &old);
        let (_, headers, _) = get_bundle_with(&state, &[]).await;
        let old_tag = header(&headers, "etag").unwrap().to_string();

        let root = resolve_handle(&state, &warehouse_params("w1")).unwrap().root.clone();
        let swap_path = path.clone();
        let swap = move || {
            let old_modified = std::fs::metadata(&swap_path).unwrap().modified().unwrap();
            let temp = swap_path.with_extension("rebuild");
            std::fs::write(&temp, &new).unwrap();
            // Unix: the same mtime, so only the inode separates the files. Elsewhere there is no
            // inode term, so the rebuild is a second later, as any real rebuild is.
            let modified = if cfg!(unix) {
                old_modified
            } else {
                old_modified + std::time::Duration::from_secs(1)
            };
            std::fs::File::options().write(true).open(&temp).unwrap().set_modified(modified).unwrap();
            file_utils::rename(&temp, &swap_path).unwrap();
        };
        BUNDLE_OPEN_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).insert(root, Box::new(swap));

        let (status, headers, body) = get_bundle_with(&state, &[]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, old, "the open handle streams the old file");
        assert_eq!(header(&headers, "etag"), Some(old_tag.as_str()), "under the old file's tag");

        let (_, headers, body) = get_bundle_with(&state, &[]).await;
        assert_eq!(body, format!("{}\nthe new build", bundle_utils::BUNDLE_HEADER).into_bytes());
        assert_ne!(header(&headers, "etag"), Some(old_tag.as_str()), "the rebuild has a tag of its own");

        drop(state);
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn the_live_warehouse_cap_is_half_the_open_file_limit_within_bounds() {
        assert_eq!(live_warehouse_cap(Some(256)), 128);
        assert_eq!(live_warehouse_cap(Some(1000)), 500);
        assert_eq!(live_warehouse_cap(Some(1_048_576)), MAX_LIVE_WAREHOUSES);
        assert_eq!(live_warehouse_cap(Some(u64::MAX)), MAX_LIVE_WAREHOUSES, "RLIM_INFINITY");
        assert_eq!(live_warehouse_cap(Some(1)), 1, "never zero");
        assert_eq!(live_warehouse_cap(None), MAX_LIVE_WAREHOUSES);
    }

    // ---------------------------------------------------------------------------------
    // create_warehouse
    // ---------------------------------------------------------------------------------

    fn creation_litter(base: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(base).unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with(CREATING_PREFIX))
            .collect()
    }

    #[test]
    fn a_new_warehouse_is_created_once_and_then_exists() {
        let base = scratch_dir("create-new");

        assert_eq!(create_warehouse(&base, "w").unwrap(), Creation::Created);
        assert!(base.join("w").join(FOLDER_NAME_FORKLIFT_ROOT).is_dir());
        {
            let _scope = StorageRootScope::enter(&base.join("w"));
            assert_eq!(pallet_utils::read_position().unwrap().pallet, pallet_utils::DEFAULT_PALLET_NAME,
                       "the warehouse arrives fully prepared");
        }
        assert_eq!(create_warehouse(&base, "w").unwrap(), Creation::Existed);
        assert!(creation_litter(&base).is_empty(), "nothing is left behind: {:?}", creation_litter(&base));

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// An empty folder at the id is taken over; a folder with anything in it, or a file, is refused
    /// and left as it was. The file case is the Windows one: a rename there replaces a file, which
    /// is how this test failed on Windows before the check ahead of the rename.
    #[test]
    fn an_empty_folder_is_taken_over_and_anything_else_is_refused() {
        let base = scratch_dir("create-existing");

        std::fs::create_dir(base.join("empty")).unwrap();
        assert_eq!(create_warehouse(&base, "empty").unwrap(), Creation::Created);
        assert!(base.join("empty").join(FOLDER_NAME_FORKLIFT_ROOT).is_dir());

        std::fs::create_dir(base.join("busy")).unwrap();
        std::fs::write(base.join("busy").join("notes.txt"), b"keep me").unwrap();
        let error = create_warehouse(&base, "busy").unwrap_err();
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);
        assert_eq!(std::fs::read(base.join("busy").join("notes.txt")).unwrap(), b"keep me");
        assert!(!base.join("busy").join(FOLDER_NAME_FORKLIFT_ROOT).exists(), "never adopted");

        // A `.forklift` without a warehouse marker, empty or not, is neither an existing warehouse
        // nor marked into one. Revert: `holds_warehouse` taking an unmarked folder → `Existed` → red.
        for (id, content) in [("hollow", None), ("old", Some("objects"))] {
            let forklift = base.join(id).join(FOLDER_NAME_FORKLIFT_ROOT);
            std::fs::create_dir_all(content.map_or(forklift.clone(), |content| forklift.join(content))).unwrap();
            let error = create_warehouse(&base, id).err().unwrap_or_else(|| panic!("{} must not read as a warehouse", id));
            assert_eq!(error.0, StatusCode::CONFLICT, "{}: {}", id, error.1);
            assert!(!forklift.join(warehouse_utils::FILE_NAME_WAREHOUSE_MARKER).exists(), "{}: never marked", id);
        }

        std::fs::write(base.join("file"), b"not a folder").unwrap();
        assert_eq!(create_warehouse(&base, "file").err().expect("a file is refused").0, StatusCode::CONFLICT);
        assert_eq!(std::fs::read(base.join("file")).unwrap(), b"not a folder", "never replaced");

        assert!(creation_litter(&base).is_empty(), "nothing is left behind: {:?}", creation_litter(&base));

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A bay's working directory holds a `.forklift` folder too. The multi-mode lookup does not
    /// serve it, and creation neither reports it as an existing warehouse nor adopts it. Neither
    /// answers 404: that would send the client to a creation that refuses.
    #[test]
    fn a_bay_under_the_base_is_neither_served_nor_taken_for_a_warehouse() {
        let base = scratch_dir("bay-under-base");
        let forklift = base.join("bay").join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        std::fs::write(forklift.join(bay_utils::FILE_NAME_BAY_REDIRECT), b"redirect").unwrap();
        let state = base_state(ServeMode::Multi { base: base.clone() });

        let error = resolve_handle(&state, &warehouse_params("bay")).err().expect("a bay must not be served");
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);

        let error = create_warehouse(&base, "bay").err().expect("a bay must not read as an existing warehouse");
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);
        assert_eq!(std::fs::read_dir(&forklift).unwrap().count(), 1, "the bay's folder is left as it was");
        assert!(creation_litter(&base).is_empty(), "nothing is left behind: {:?}", creation_litter(&base));

        // A `.forklift` that is neither (a redirect that is a folder) cannot be judged: a lookup
        // answers it as a server fault with no remedy, and a creation finds the name taken.
        std::fs::create_dir_all(base.join("broken").join(FOLDER_NAME_FORKLIFT_ROOT).join(bay_utils::FILE_NAME_BAY_REDIRECT)).unwrap();
        let error = resolve_handle(&state, &warehouse_params("broken")).err().expect("a broken entry must not be served");
        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR, "{}", error.1);
        let error = create_warehouse(&base, "broken").err().expect("a broken entry must not read as a warehouse");
        assert_eq!(error.0, StatusCode::CONFLICT, "{}", error.1);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// A killed earlier process with this one's id can leave its staging folder at the very name
    /// the next creation draws, when the startup sweep could not remove it. The creation steps
    /// past it, and the litter is neither adopted nor touched.
    #[test]
    fn a_dead_process_s_staging_folder_at_the_next_name_is_stepped_past() {
        let base = scratch_dir("create-litter");
        // A counter of the test's own: other tests drawing from the shared one would move the
        // next name past the litter, and the test would pass without stepping past anything.
        let counter = std::sync::atomic::AtomicU64::new(0);
        let litter: Vec<_> = (0..8)
            .map(|n| base.join(format!("{}w-{}-{}", CREATING_PREFIX, std::process::id(), n)))
            .collect();
        for folder in &litter {
            std::fs::create_dir(folder).unwrap();
            std::fs::write(folder.join("left.txt"), b"litter").unwrap();
        }

        assert_eq!(create_warehouse_numbered(&base, "w", &counter).unwrap(), Creation::Created);
        assert!(base.join("w").join(FOLDER_NAME_FORKLIFT_ROOT).is_dir());
        assert!(!base.join("w").join("left.txt").exists(), "the litter was adopted");
        for folder in &litter {
            assert_eq!(std::fs::read(folder.join("left.txt")).unwrap(), b"litter", "litter is left as it was");
        }

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// Of many creations racing for one new id, exactly one reports `Created`. Supplementary to
    /// the rename deciding it: a check-then-prepare implementation fails this most runs, not
    /// every run.
    #[test]
    fn racing_creations_of_one_id_create_it_exactly_once() {
        let base = scratch_dir("create-race");
        let barrier = Arc::new(std::sync::Barrier::new(16));

        let outcomes: Vec<Creation> = (0..16)
            .map(|_| {
                let (base, barrier) = (base.clone(), Arc::clone(&barrier));
                std::thread::spawn(move || {
                    barrier.wait();
                    create_warehouse(&base, "raced").unwrap()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect();

        assert_eq!(outcomes.iter().filter(|o| **o == Creation::Created).count(), 1, "{:?}", outcomes);
        assert!(creation_litter(&base).is_empty(), "nothing is left behind: {:?}", creation_litter(&base));

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    #[test]
    fn the_startup_sweep_removes_only_interrupted_creations() {
        let base = scratch_dir("create-sweep");
        std::fs::create_dir_all(base.join(format!("{}w-1-0", CREATING_PREFIX)).join(FOLDER_NAME_FORKLIFT_ROOT))
            .unwrap();
        mark_warehouse(&base.join("w"));
        std::fs::write(base.join(format!("{}stray", CREATING_PREFIX)), b"").unwrap();
        std::fs::write(base.join(lock_utils::FILE_NAME_BASE_SERVE_LOCK), b"").unwrap();

        sweep_interrupted_creations(&base);

        assert!(creation_litter(&base).is_empty());
        assert!(base.join("w").join(FOLDER_NAME_FORKLIFT_ROOT).is_dir());
        assert!(base.join(lock_utils::FILE_NAME_BASE_SERVE_LOCK).exists());

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    // ---------------------------------------------------------------------------------
    // the office lookup / require_uploader (transport authorization)
    // ---------------------------------------------------------------------------------

    /// Serializes every test that touches key generation/signing: `sign_utils` resolves
    /// its private-key directory from the `FORKLIFT_KEYS_DIR` environment variable on
    /// every call, which is process-global (not scoped by `StorageRootScope`). Holding
    /// this lock for such a test's whole body keeps two tests from ever pointing the
    /// same process at different key directories at once. Tests that never sign
    /// anything (most of this file) never take it and run fully in parallel.
    static KEYS_ENV_LOCK: Mutex<()> = Mutex::new(());

    /// A real, cryptographically valid office chain: a genesis admin (self-endorsed
    /// identity root, exactly like `office enroll`), with trust established. Building it
    /// through the same `office_utils`/`sign_utils` calls the CLI uses means the chain
    /// this produces is exactly as verifiable as a real one — not a shortcut that only
    /// happens to satisfy the office lookup.
    struct OfficeFixture {
        root: PathBuf,
        _scope: StorageRootScope,
        // `None` when a caller is holding `KEYS_ENV_LOCK` itself in an outer scope
        // that must outlive this fixture (see `build`'s doc comment) — otherwise
        // `Some`, and the fixture releases the lock as part of its own drop, in field
        // order, right after `Drop::drop` restores `FORKLIFT_KEYS_DIR`.
        _keys_lock: Option<std::sync::MutexGuard<'static, ()>>,
        admin: Operator,
        admin_key_id: String,
        // What `FORKLIFT_KEYS_DIR` held before this fixture pointed it at its own
        // (about-to-be-deleted) directory — restored in `Drop` so a later test in this
        // binary never inherits a path this fixture already removed. `FORKLIFT_KEYS_DIR`
        // is process-global (see `KEYS_ENV_LOCK`), so leaving it pointed at a removed
        // directory would flake the next signer that runs without the lock.
        previous_keys_dir: Option<String>,
    }

    impl OfficeFixture {
        fn genesis(name: &str) -> OfficeFixture {
            let keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            OfficeFixture::build(name, Some(keys_lock))
        }

        /// The body of `genesis`, taking the `KEYS_ENV_LOCK` guard as a parameter
        /// instead of acquiring one itself, so a caller can choose who ends up
        /// owning it:
        ///
        /// - `genesis` passes `Some(lock)` (the normal case): the fixture takes
        ///   ownership and holds it for its own lifetime, releasing it in
        ///   field-drop order right after `Drop::drop` restores `FORKLIFT_KEYS_DIR`
        ///   — unchanged from before.
        /// - A caller that must keep observing `FORKLIFT_KEYS_DIR` (both its own
        ///   writes and this fixture's) after the fixture is gone passes `None` and
        ///   keeps its own guard alive in an outer scope instead. This is what the
        ///   restore-on-drop falsifier below does: it asserts on `FORKLIFT_KEYS_DIR`
        ///   both right after construction and after `drop(fixture)`, and every one
        ///   of those assertions — not just construction — needs the lock held, or
        ///   a concurrent fixture from another test in this binary can be
        ///   mid-flight and change the value out from under the assertion. Handing
        ///   the fixture `None` means `drop(fixture)` only drops the fixture, not
        ///   the lock; the caller's own guard, still alive in its own scope, keeps
        ///   every assertion covered and is released only when that scope ends.
        fn build(name: &str, keys_lock: Option<std::sync::MutexGuard<'static, ()>>) -> OfficeFixture {
            let root = scratch_dir(name);
            let keys_dir = root.join("keys");
            std::fs::create_dir_all(&keys_dir).unwrap();
            let previous_keys_dir = std::env::var("FORKLIFT_KEYS_DIR").ok();
            std::env::set_var("FORKLIFT_KEYS_DIR", &keys_dir);

            let scope = StorageRootScope::enter(&root);
            warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();

            let admin = Operator { identifier: "alice".to_string() };
            let (key_id, public_key) = forklift_core::util::sign_utils::generate_keypair(&admin.identifier).unwrap();
            let pop = office_utils::sign_key_pop(&key_id, &public_key, &admin.identifier).unwrap();
            let root_key = office_utils::endorse_key(&public_key, &admin.identifier, &key_id, &pop, 1_700_000_000).unwrap();

            let state = OfficeState {
                users: vec![UserRecord {
                    identifier: admin.identifier.clone(),
                    enrolled_at: 1_700_000_000,
                    role: Role::Admin,
                    pallets: Vec::new(),
                    identity_root: key_id.clone(),
                    class: IdentityClass::Human,
                    supervisor: None,
                }],
                keys: vec![root_key],
            };

            let genesis = office_utils::stack_office_parcel(
                &state, &admin, "genesis".to_string(), &key_id,
                forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().as_deref(),
                &RefPreconditions::NONE,
            ).unwrap();

            plant_trust_anchor(&TrustAnchor {
                genesis,
                enabled_at: 1_700_000_000,
                boundary: Vec::new(),
                prior_genesis: None,
                adopts: None,
            }).unwrap();

            OfficeFixture {
                root, _scope: scope, _keys_lock: keys_lock, admin, admin_key_id: key_id,
                previous_keys_dir,
            }
        }

        /// Admit an additional user onto the chain (mirrors `office admit`).
        /// Returns the admitted user's own key id, so a caller that needs to sign as
        /// them (a falsifier pushing a real parcel under their identity) doesn't have
        /// to re-derive it.
        fn admit(&self,
                identifier: &str,
                role: Role,
                pallets: Vec<String>,
                class: IdentityClass,
                supervisor: Option<String>) -> String {
            let mut state = office_utils::read_office_state().unwrap();

            let (key_id, public_key) = forklift_core::util::sign_utils::generate_keypair(identifier).unwrap();
            let pop = office_utils::sign_key_pop(&key_id, &public_key, identifier).unwrap();
            let key = office_utils::endorse_key(
                &public_key, identifier, &self.admin_key_id, &pop, 1_700_000_001
            ).unwrap();

            state.users.push(UserRecord {
                identifier: identifier.to_string(),
                enrolled_at: 1_700_000_001,
                role,
                pallets,
                identity_root: key_id.clone(),
                class,
                supervisor,
            });
            state.keys.push(key);

            office_utils::stack_office_parcel(
                &state, &self.admin, format!("admit {}", identifier), &self.admin_key_id,
                forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().as_deref(),
                &RefPreconditions::NONE,
            ).unwrap();

            key_id
        }
    }

    impl Drop for OfficeFixture {
        fn drop(&mut self) {
            match &self.previous_keys_dir {
                Some(previous) => std::env::set_var("FORKLIFT_KEYS_DIR", previous),
                None => std::env::remove_var("FORKLIFT_KEYS_DIR"),
            }
            let _ = forklift_core::util::file_utils::remove_dir_all(&self.root);
        }
    }

    /// Falsifies the leak this fixture used to have: `FORKLIFT_KEYS_DIR` is
    /// process-global, and this fixture points it at a directory its own `Drop`
    /// deletes. Without restoring the prior value first, the process would be left
    /// with `FORKLIFT_KEYS_DIR` pointing at a removed path after the fixture goes
    /// away — flaking any later signer in this binary that runs without
    /// `KEYS_ENV_LOCK`. Covers both directions: a prior value to restore, and no
    /// prior value (must end up unset again, not set to `""`).
    ///
    /// Every assertion below — including the ones after `drop(fixture)` — runs while
    /// this test's own `keys_lock` is held. Passing `OfficeFixture::build` `None`
    /// keeps that guard here instead of handing it to the fixture, so `drop(fixture)`
    /// releases only the fixture, not the lock: a concurrent fixture from another
    /// test in this binary cannot become mid-flight and change `FORKLIFT_KEYS_DIR`
    /// out from under an assertion. (An earlier version of this test handed the lock
    /// to the fixture and read `FORKLIFT_KEYS_DIR` after dropping it — unlocked. That
    /// version's own assertions could flake for the exact reason this test exists.)
    #[test]
    fn dropping_office_fixture_restores_the_previous_keys_dir_env_var() {
        const SENTINEL: &str = "/does/not/exist/forklift-restore-sentinel";

        // Case 1: FORKLIFT_KEYS_DIR pointed somewhere else before the fixture.
        {
            let keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::set_var("FORKLIFT_KEYS_DIR", SENTINEL);
            let fixture = OfficeFixture::build("restore-check-was-set", None);
            assert_ne!(
                std::env::var("FORKLIFT_KEYS_DIR").ok(), Some(SENTINEL.to_string()),
                "fixture construction should have repointed FORKLIFT_KEYS_DIR at its own directory"
            );
            drop(fixture);
            assert_eq!(
                std::env::var("FORKLIFT_KEYS_DIR").ok(), Some(SENTINEL.to_string()),
                "dropping the fixture must restore FORKLIFT_KEYS_DIR to what it held before construction"
            );
            drop(keys_lock);
        }

        // Case 2: FORKLIFT_KEYS_DIR was not set at all before the fixture.
        {
            let keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            std::env::remove_var("FORKLIFT_KEYS_DIR");
            let fixture = OfficeFixture::build("restore-check-was-unset", None);
            drop(fixture);
            assert!(
                std::env::var("FORKLIFT_KEYS_DIR").is_err(),
                "dropping the fixture must leave FORKLIFT_KEYS_DIR unset if it started unset"
            );
            drop(keys_lock);
        }
    }

    /// The office lookup does not read the anchor and office state for itself — its
    /// production callers pass in the values their own audit consumed. These tests
    /// have no audit of their own to agree with, so they read fresh, exactly as
    /// `require_uploader` now does, and pass the result straight through.
    fn office_user_of_now(identifier: &str) -> Result<Option<office_utils::UserRecord>, HandlerError> {
        let anchor = office_utils::read_trust_anchor().map_err(internal)?;
        let office_state = office_utils::read_office_state().map_err(internal)?;
        authority_utils::office_user_of(identifier, &anchor, &office_state).map_err(forbidden)
    }

    #[test]
    fn no_trust_anchor_means_office_user_of_is_none() {
        let root = scratch_dir("office-none-anchor");
        let _scope = StorageRootScope::enter(&root);
        std::fs::create_dir_all(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        assert!(office_user_of_now("anyone").unwrap().is_none());
        assert!(require_uploader(&Principal::Operator("anyone".to_string())).is_ok());

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    #[test]
    fn an_established_anchor_with_no_office_yet_is_the_bootstrap_window() {
        // Trust set, but the office pallet itself has not been lifted (no key generation
        // involved, so this test needs no `FORKLIFT_KEYS_DIR` scoping).
        let root = scratch_dir("office-bootstrap-window");
        let _scope = StorageRootScope::enter(&root);
        std::fs::create_dir_all(root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        plant_trust_anchor(&TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        }).unwrap();

        assert!(office_user_of_now("anyone").unwrap().is_none());
        assert!(require_uploader(&Principal::Operator("anyone".to_string())).is_ok());

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    #[test]
    fn require_uploader_bypasses_the_office_for_static_and_open_principals() {
        // No storage scope at all: neither variant ever reaches the office.
        assert!(require_uploader(&Principal::Static).is_ok());
        assert!(require_uploader(&Principal::Open).is_ok());
    }

    #[test]
    fn an_enrolled_reader_is_found_with_their_role_and_identity_class() {
        let fixture = OfficeFixture::genesis("office-reader");
        fixture.admit(
            "bob", Role::Reader, Vec::new(), IdentityClass::Agent, Some("alice".to_string())
        );

        let user = office_user_of_now("bob").unwrap().expect("bob must be enrolled");
        assert_eq!(user.role, Role::Reader);
        assert_eq!(user.class, IdentityClass::Agent);
        assert_eq!(user.supervisor.as_deref(), Some("alice"));

        let error = require_uploader(&Principal::Operator("bob".to_string())).unwrap_err();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("readers cannot upload"), "{}", error.1);
    }

    #[test]
    fn an_unenrolled_operator_is_forbidden_once_an_office_exists() {
        let fixture = OfficeFixture::genesis("office-unenrolled");

        let error = office_user_of_now("mallory").err().unwrap();
        assert_eq!(error.0, StatusCode::FORBIDDEN);
        assert!(error.1.contains("is not enrolled"), "{}", error.1);

        drop(fixture);
    }

    #[test]
    fn require_uploader_allows_writers_and_admins() {
        let fixture = OfficeFixture::genesis("office-writer-admin");
        fixture.admit(
            "carol", Role::Writer, vec!["main".to_string()], IdentityClass::Bot, None
        );

        assert!(require_uploader(&Principal::Operator("carol".to_string())).is_ok());
        assert!(require_uploader(&Principal::Operator(fixture.admin.identifier.clone())).is_ok());
    }

    /// An operator whose token authenticates but whom the office does not enroll may not upload:
    /// the office lookup's refusal is the upload's refusal, before any byte is stored. The same
    /// object then lands for an enrolled writer, so the 403 is the gate and not the object.
    // Revert: `require_uploader` swallows the lookup's refusal (`Err(_) => Ok(())`) → 201 → red.
    #[tokio::test]
    async fn an_unenrolled_operator_may_not_upload_an_object() {
        let fixture = OfficeFixture::genesis("put-object-unenrolled");
        fixture.admit("carol", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None);

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let bytes = object_utils::blob_bytes(b"an object an unenrolled operator tries to upload");
        let hash = object_utils::hash_object_bytes(&bytes);
        let mut params = PathParams::new();
        params.insert("hash".to_string(), hash.clone());

        let response = put_object(
            State(Arc::clone(&state)), as_operator("mallory"), Path(params.clone()),
            Bytes::from(bytes.clone())
        ).await;

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_text(response).await;
        assert!(body.contains("is not enrolled in this warehouse's office."), "{}", body);
        assert!(!file_utils::does_object_exist(&hash).unwrap(), "a refused upload must store nothing");

        let response = put_object(
            State(state), as_operator("carol"), Path(params), Bytes::from(bytes)
        ).await;

        assert_eq!(response.status(), StatusCode::CREATED);
    }

    // ---------------------------------------------------------------------------------
    // post_ref_update — the transport-authorization gate
    // ---------------------------------------------------------------------------------

    fn ref_update_params(name: &str) -> PathParams {
        let mut params = PathParams::new();
        params.insert("name".to_string(), name.to_string());
        params
    }

    #[tokio::test]
    async fn a_reader_may_not_transport_the_office_meta_pallet() {
        let fixture = OfficeFixture::genesis("post-ref-reader-office");
        fixture.admit("dana", Role::Reader, Vec::new(), IdentityClass::Human, None);

        let state = Arc::new(single_mode_state(fixture.root.clone()));
        let params = ref_update_params("@office");
        let body = RefUpdateRequest { old_head: None, new_head: "a".repeat(64), tombstone: None };

        let response = post_ref_update(
            State(state), as_operator("dana"), Path(params), Json(body)
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_writer_outside_their_grant_may_not_move_a_pallet() {
        let fixture = OfficeFixture::genesis("post-ref-writer-outside-grant");
        fixture.admit(
            "erin", Role::Writer, vec!["only-this-one".to_string()], IdentityClass::Human, None
        );

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let params = ref_update_params("someone-elses-pallet");
        let body = RefUpdateRequest { old_head: None, new_head: "a".repeat(64), tombstone: None };

        let response = post_ref_update(
            State(state), as_operator("erin"), Path(params), Json(body)
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn a_writer_inside_their_grant_clears_the_authorization_gate() {
        let fixture = OfficeFixture::genesis("post-ref-writer-inside-grant");
        fixture.admit(
            "frank", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None
        );

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let params = ref_update_params("main");
        // A `new_head` that was never uploaded: this must fail *later*, on object
        // presence, never on authorization — the FORBIDDEN branch must not fire for an
        // operator who is allowed to write this pallet.
        let body = RefUpdateRequest { old_head: None, new_head: "b".repeat(64), tombstone: None };

        let response = post_ref_update(
            State(state), as_operator("frank"), Path(params), Json(body)
        ).await;

        let status = response.into_response().status();
        assert_ne!(status, StatusCode::FORBIDDEN, "an in-grant writer must clear the auth gate");
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "it must instead fail on the missing object");
    }

    /// FORK-77: a pallet name outside the portable grammar never reaches the store. `Main`
    /// would alias `main` on a case-folding disk, so the ref update is refused by name before
    /// authorization or object presence is even consulted.
    #[tokio::test]
    async fn a_ref_update_to_a_mixed_case_pallet_is_refused() {
        let fixture = OfficeFixture::genesis("post-ref-mixed-case");
        fixture.admit(
            "frank", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None
        );

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let body = RefUpdateRequest { old_head: None, new_head: "b".repeat(64), tombstone: None };
        let response = post_ref_update(
            State(state), as_operator("frank"), Path(ref_update_params("Main")), Json(body)
        ).await.into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let error: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let message = error["error"].as_str().unwrap_or_default();
        assert!(message.contains("\"Main\" is not a valid pallet name"), "{}", message);
        assert!(message.contains("the lowercase \"main\" is valid"), "{}", message);
    }

    /// Lift an office parcel promoting "bob" (a writer) to admin, signed by `signer`'s key and
    /// stacked on the fixture's current office head, as the admin, and return the
    /// status. The admin may transport the office, so any refusal comes from its content check.
    async fn lift_bob_promotion(fixture: &OfficeFixture, signer: &Operator, signer_key_id: &str) -> StatusCode {
        let head = forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME)
            .unwrap()
            .expect("the fixture's office head");

        let mut state = office_utils::read_office_state().unwrap();
        state.users.iter_mut().find(|user| user.identifier == "bob").unwrap().role = Role::Admin;

        let promotion = office_utils::build_and_sign_office_parcel(
            &state, signer, "promote bob".to_string(), signer_key_id, vec![head.clone()]
        ).unwrap();

        post_ref_update(
            State(office_state(fixture)), as_operator(&fixture.admin.identifier),
            Path(ref_update_params("@office")),
            Json(RefUpdateRequest { old_head: Some(head), new_head: promotion, tombstone: None })
        ).await.into_response().status()
    }

    /// FORK-220: an office parcel that oversteps its signer's privileges is a 403, even now that
    /// the chain walk (not a second check) finds it. The admin-signed control shows the domain is
    /// non-empty and the 403 is not the transport gate.
    #[tokio::test]
    async fn a_writer_promoting_themselves_is_forbidden_and_an_admin_promoting_them_is_not() {
        let fixture = OfficeFixture::genesis("post-ref-self-promotion");
        let bob_key_id = fixture.admit("bob", Role::Writer, Vec::new(), IdentityClass::Human, None);
        let bob = Operator { identifier: "bob".to_string() };

        assert_eq!(lift_bob_promotion(&fixture, &bob, &bob_key_id).await, StatusCode::FORBIDDEN);
        assert_eq!(lift_bob_promotion(&fixture, &fixture.admin, &fixture.admin_key_id).await, StatusCode::OK);
    }

    /// FORK-220: a 403 names the lifter's own overstep, so an overstep the warehouse already
    /// held is not pinned on a legitimate lift on top of it: that refusal is a 422. The held
    /// self-promotion is planted past the server, as a store written before the check would be.
    #[tokio::test]
    async fn a_held_overstep_under_a_legitimate_lift_is_unprocessable_not_forbidden() {
        let fixture = OfficeFixture::genesis("post-ref-held-overstep");
        let bob_key_id = fixture.admit("bob", Role::Writer, Vec::new(), IdentityClass::Human, None);
        let bob = Operator { identifier: "bob".to_string() };

        let mut promoted = office_utils::read_office_state().unwrap();
        promoted.users.iter_mut().find(|user| user.identifier == "bob").unwrap().role = Role::Admin;
        let base = forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap();
        let held = office_utils::stack_office_parcel(
            &promoted, &bob, "bob promotes himself".to_string(), &bob_key_id, base.as_deref(),
            &RefPreconditions::NONE,
        ).unwrap();

        // The admin demotes Bob again: legitimate in itself.
        let mut demoted = promoted.clone();
        demoted.users.iter_mut().find(|user| user.identifier == "bob").unwrap().role = Role::Writer;
        let lifted = office_utils::build_and_sign_office_parcel(
            &demoted, &fixture.admin, "demote bob".to_string(), &fixture.admin_key_id, vec![held.clone()]
        ).unwrap();

        let response = post_ref_update(
            State(office_state(&fixture)), as_operator(&fixture.admin.identifier),
            Path(ref_update_params("@office")),
            Json(RefUpdateRequest { old_head: Some(held.clone()), new_head: lifted, tombstone: None })
        ).await.into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let message = String::from_utf8_lossy(&body);
        assert!(message.contains(&held), "{}", message);
        assert!(message.contains("already in this warehouse's office"), "{}", message);
    }

    /// FORK-220: an office parcel that is not authentic stays a 422: one signed by a key the office
    /// does not track.
    #[tokio::test]
    async fn an_office_parcel_signed_by_an_untracked_key_is_unprocessable() {
        let fixture = OfficeFixture::genesis("post-ref-untracked-office-signer");
        fixture.admit("bob", Role::Writer, Vec::new(), IdentityClass::Human, None);

        let stranger = Operator { identifier: "mallory".to_string() };
        let (stranger_key_id, _) = forklift_core::util::sign_utils::generate_keypair("mallory").unwrap();

        assert_eq!(
            lift_bob_promotion(&fixture, &stranger, &stranger_key_id).await,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }

    // ---------------------------------------------------------------------------------
    // FORK-95 slice 1 falsifiers: the move gate consumes the audit's own values
    // instead of reading a second, independent copy of its own.
    // ---------------------------------------------------------------------------------

    /// Build, store and sign a minimal (empty-tree) parcel — content the audit will
    /// accept without needing a real working copy — and return its hash. Unlike
    /// `office_utils::stack_office_parcel`, this never advances any ref: a falsifier
    /// needs the object *present* (so `does_object_exist` and the deep audit have
    /// something real to check) without the pallet already having moved, so it can
    /// drive the move itself through `post_ref_update`.
    fn store_signed_parcel(actor: &Operator,
                           signing_key_id: &str,
                           parents: Vec<String>,
                           description: &str) -> String {
        use forklift_core::enums::dir_entry_type::DirEntryType;

        let root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        store_signed_parcel_over(actor, signing_key_id, parents, description, &root_tree)
    }

    /// [`store_signed_parcel`] over `root_tree`, stored with it.
    fn store_signed_parcel_over(actor: &Operator,
                                signing_key_id: &str,
                                parents: Vec<String>,
                                description: &str,
                                root_tree: &TreeItem) -> String {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::parcel_action_type::ParcelActionType;
        use forklift_core::model::parcel::Parcel;
        use forklift_core::model::parcel_action::ParcelAction;

        let mut root_object = LooseObjectBuilder::build_tree(root_tree);
        root_object.store().unwrap();

        let timestamp = chrono::Utc::now();
        let parcel = Parcel {
            tree_hash: root_object.hash,
            parents,
            actions: vec![
                ParcelAction {
                    operator: actor.clone(),
                    action: ParcelActionType::Author,
                    description: None,
                    timestamp,
                },
                ParcelAction {
                    operator: actor.clone(),
                    action: ParcelActionType::Stack,
                    description: None,
                    timestamp,
                },
            ],
            description: Some(description.to_string()),
        };

        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();

        let signature = forklift_core::util::sign_utils::sign_parcel_hash(
            signing_key_id, &parcel_object.hash
        ).unwrap();
        forklift_core::util::sign_utils::store_parcel_signature(&parcel_object.hash, &signature).unwrap();

        parcel_object.hash
    }

    /// A push of a head whose root holds the metadata folder is refused, under trust too: every
    /// client command that would check it out refuses, so the pallet would be stuck there.
    // Revert: drop the materializable-head gate from `post_ref_update` → the crafted head lands.
    #[tokio::test]
    async fn a_ref_update_to_a_head_holding_the_metadata_folder_is_refused() {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::dir_entry_type::DirEntryType;

        let fixture = OfficeFixture::genesis("post-ref-metadata-root");
        let quinn_key_id = fixture.admit("quinn", Role::Writer, vec!["crafted".to_string()], IdentityClass::Human, None);
        let quinn = Operator { identifier: "quinn".to_string() };
        let state = single_mode_state(fixture.root.clone());

        let metadata = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        let mut metadata_object = LooseObjectBuilder::build_tree(&metadata);
        metadata_object.store().unwrap();
        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(FOLDER_NAME_FORKLIFT_ROOT.to_string(), metadata_object.hash, DirEntryType::Tree));
        let crafted = store_signed_parcel_over(&quinn, &quinn_key_id, Vec::new(), "crafted", &root_tree);

        let response = post_ref_update(
            State(Arc::new(state)), as_operator("quinn"), Path(ref_update_params("crafted")),
            Json(RefUpdateRequest { old_head: None, new_head: crafted.clone(), tombstone: None }),
        ).await.into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_text(response).await;
        assert!(body.contains(r#"holds \".forklift\""#), "{}", body);
        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("crafted").unwrap(), None);
        let _ = crafted;
    }

    /// Store an unsigned parcel over an empty tree and return its hash: a pallet head whose history
    /// is complete, for a test that needs only some head.
    fn store_bare_parcel(description: &str) -> String {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::dir_entry_type::DirEntryType;
        use forklift_core::model::parcel::Parcel;

        let mut root_object = LooseObjectBuilder::build_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree));
        root_object.store().unwrap();

        let mut parcel_object = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: root_object.hash,
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some(description.to_string()),
        });
        parcel_object.store().unwrap();
        parcel_object.hash
    }

    /// Restores `FORKLIFT_KEYS_DIR` when the test's scope ends, including on panic.
    /// A falsifier that does its job *fails*, and a straight-line restore placed after
    /// the assertion never runs on the one path that matters — leaving the process-global
    /// key directory pointing at a scratch root that is about to be deleted, for every
    /// later test in this binary. `OfficeFixture` carries the same guard for the same
    /// reason; this test cannot use the fixture because it must run with the office
    /// *not* lifted.
    struct KeysDirRestore(Option<String>);

    impl Drop for KeysDirRestore {
        fn drop(&mut self) {
            match self.0.take() {
                Some(previous) => std::env::set_var("FORKLIFT_KEYS_DIR", previous),
                None => std::env::remove_var("FORKLIFT_KEYS_DIR"),
            }
        }
    }

    /// Clears the armed seam when the test's scope ends — including on panic, so a
    /// timed-out or failed rendezvous cannot leave a live arm for the next test in
    /// this binary to consume silently.
    struct OfficeLookupSeamGuard;

    impl Drop for OfficeLookupSeamGuard {
        fn drop(&mut self) {
            *OFFICE_LOOKUP_TEST_SEAM.lock().unwrap_or_else(|e| e.into_inner()) = None;
        }
    }

    /// Arm the move gate's checkpoint for one identifier, returning a disarm
    /// guard plus the "reached" receiver and "proceed" sender the test drives the
    /// rendezvous with. Keyed by identifier so an unrelated concurrent lookup cannot
    /// consume the arm — but the key is only sound while no other test pushes as the
    /// same operator, so seam tests use an identifier of their own.
    fn arm_office_lookup_checkpoint(identifier: &str)
        -> (OfficeLookupSeamGuard, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel();

        *OFFICE_LOOKUP_TEST_SEAM.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((identifier.to_string(), reached_tx, gate_rx));

        (OfficeLookupSeamGuard, reached_rx, gate_tx)
    }

    /// Falsifier: a push whose grant is revoked while it is audited is refused, as a request
    /// arriving after the revocation would be, rather than committed on the office it read.
    ///
    /// Not a wall-clock race: the revocation is driven into the checkpoint the move gate reaches
    /// right after fixing its authorization answer, a real admin-signed office parcel moving the
    /// office ref while quinn's push holds a decision that still grants her "main". Her write then
    /// finds the office moved, and the update decides again against the office that stands now,
    /// in which she is a reader.
    ///
    /// A fake, never-uploaded `new_head` (the trick several sibling tests use to stop short of the
    /// deep audit) can't stand in here: it fails on object presence before authorization would ever
    /// matter, so a wrong implementation would be refused too. This needs a push that can really
    /// commit if authorization wrongly lets it through — hence a real, signed, uploaded parcel.
    // Reverts: answer the write's `OfficeMoved` without deciding again → the office-moved 409;
    // the final write passes `OfficePrecondition::NotConsumed` → 200 on a revoked grant.
    //
    // Multi-threaded runtime, not the default current-thread one: the test body below blocks its
    // own thread synchronously on `reached.recv_timeout` while `push` (spawned, not awaited yet)
    // needs an executor thread free to poll it — on a current-thread runtime the test's own
    // blocking wait would starve the only thread able to make `push` progress at all, and the
    // checkpoint would never fire.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_grant_revoked_while_its_push_is_audited_is_refused_on_the_new_office() {
        let fixture = OfficeFixture::genesis("post-ref-revoke-race");
        let quinn_key_id = fixture.admit(
            "quinn-seam", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None
        );
        let quinn = Operator { identifier: "quinn-seam".to_string() };
        let new_head = store_signed_parcel(&quinn, &quinn_key_id, Vec::new(), "quinn-seam's push");

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let (_seam, reached, gate) = arm_office_lookup_checkpoint("quinn-seam");

        let params = ref_update_params("main");
        let body = RefUpdateRequest { old_head: None, new_head, tombstone: None };
        let push = tokio::spawn(post_ref_update(
            State(Arc::clone(&state)), as_operator("quinn-seam"), Path(params), Json(body)
        ));

        reached.recv_timeout(std::time::Duration::from_secs(5))
            .expect("the move gate must reach its test checkpoint");

        let mut office_state = office_utils::read_office_state().unwrap();
        office_state.users.iter_mut()
            .find(|user| user.identifier == "quinn-seam")
            .expect("quinn-seam must be enrolled")
            .role = Role::Reader;
        office_utils::stack_office_parcel(
            &office_state, &fixture.admin, "revoke quinn".to_string(), &fixture.admin_key_id,
            forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().as_deref(),
            &RefPreconditions::NONE,
        ).unwrap();

        gate.send(()).expect("the push's checkpoint must still be waiting on the gate");
        let response = push.await.unwrap().into_response();

        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("main").unwrap(), None,
            "a revoked grant must not move the pallet");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        let body = body_text(response).await;
        assert!(body.contains("\\\"quinn-seam\\\" (reader) may not move pallet \\\"main\\\"."), "{}", body);
        assert_eq!(ref_update_attempts(&fixture.root, "main"), 2,
            "the refusal comes from deciding again against the new office");
    }

    /// Falsifier 2 (reverted → red): the office-head `Option` this fix reads once has
    /// two arms, and the `None` arm — trust established, office not yet lifted, the
    /// ordinary bootstrap window reached on every enrollment — must still let that
    /// first office lift commit. C9's own prose names the wrong implementation this
    /// guards against: naming only the hash-taking accessor leaves nothing to call in
    /// the `None` case, tempting an unwrap that panics right where the bootstrap
    /// window is not an error at all.
    ///
    /// This is the "must still commit" half of the over-tightened direction: nothing
    /// races here (there is no concurrent writer), so a naive implementation cannot
    /// blame a stale read — the only way to make this push fail is to get the `None`
    /// arm itself wrong, which is exactly the mistake this test is built to catch.
    // `KEYS_ENV_LOCK` is deliberately held across the request under test: it serialises
    // mutation of the process-global `FORKLIFT_KEYS_DIR`, so dropping it before the await
    // would let a concurrent test in this binary swap the key directory mid-request. The
    // guard protects an env var, never an async resource, so it cannot deadlock the runtime.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn the_bootstrap_window_office_lift_still_commits() {
        let root = scratch_dir("post-ref-bootstrap-lift");
        let _scope = StorageRootScope::enter(&root);
        let _keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keys_dir = root.join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        // Declared after `keys_lock`, so it restores while the lock is still held.
        let _keys_restore = KeysDirRestore(std::env::var("FORKLIFT_KEYS_DIR").ok());
        std::env::set_var("FORKLIFT_KEYS_DIR", &keys_dir);

        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();

        let admin = Operator { identifier: "alice".to_string() };
        let (key_id, public_key) =
            forklift_core::util::sign_utils::generate_keypair(&admin.identifier).unwrap();
        let pop = office_utils::sign_key_pop(&key_id, &public_key, &admin.identifier).unwrap();
        let root_key = office_utils::endorse_key(
            &public_key, &admin.identifier, &key_id, &pop, 1_700_000_000
        ).unwrap();

        // Trust is established; the office pallet itself has never been lifted —
        // `get_meta_pallet_head(OFFICE_PALLET_NAME)` is `None` here, the bootstrap
        // window this test targets. `build_and_sign_office_parcel` builds the same
        // tree shape a real `office enroll` produces, stored and signed, without
        // `stack_office_genesis`'s side effect of also advancing the ref — this test
        // must drive that move itself, through `post_ref_update`, to exercise its own
        // handling of the bootstrap window. The anchor names this parcel as its
        // genesis (built first, exactly as `OfficeFixture::build` orders it), or the
        // office-chain audit refuses a chain that does not descend from it.
        let genesis_state = OfficeState {
            users: vec![UserRecord {
                identifier: admin.identifier.clone(),
                enrolled_at: 1_700_000_000,
                role: Role::Admin,
                pallets: Vec::new(),
                identity_root: key_id.clone(),
                class: IdentityClass::Human,
                supervisor: None,
            }],
            keys: vec![root_key],
        };
        let genesis_hash = office_utils::build_and_sign_office_parcel(
            &genesis_state, &admin, "genesis".to_string(), &key_id, Vec::new()
        ).unwrap();

        plant_trust_anchor(&TrustAnchor {
            genesis: genesis_hash.clone(),
            enabled_at: 1_700_000_000,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: None,
        }).unwrap();

        let state = Arc::new(single_mode_state(root.clone()));

        let params = ref_update_params("@office");
        let body = RefUpdateRequest { old_head: None, new_head: genesis_hash, tombstone: None };

        let response = post_ref_update(
            State(state), as_operator("alice"), Path(params), Json(body)
        ).await;

        assert_eq!(
            response.into_response().status(), StatusCode::OK,
            "the first office lift, in the ordinary bootstrap window, must commit"
        );

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        // `_keys_lock` is NOT dropped here. It must outlive `_keys_restore`, which runs at
        // scope end — releasing the lock first would let another test acquire it and set
        // its own FORKLIFT_KEYS_DIR, only for this test's restore to clobber it mid-run.
        // Declaration order (lock, then restore) gives the reverse drop order that is
        // correct on both the passing and the panicking path.
    }

    /// Clears one armed write seam when the test's scope ends, including on panic, so a failed
    /// test cannot leave a live arm behind.
    struct WriteSeamGuard((PathBuf, String));

    impl Drop for WriteSeamGuard {
        fn drop(&mut self) {
            REF_WRITE_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.0);
        }
    }

    /// Arm the write seam for `name` in the warehouse at `root` with `run`, which every checkpoint
    /// for that pair runs until the returned guard drops.
    fn arm_write_seam(root: &std::path::Path, name: &str, run: impl FnMut() + Send + 'static) -> WriteSeamGuard {
        let key = (root.join(FOLDER_NAME_FORKLIFT_ROOT), name.to_string());
        REF_WRITE_TEST_SEAMS.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), Box::new(run));

        WriteSeamGuard(key)
    }

    /// Arm the write seam for `name` in the warehouse at `root` to park its first checkpoint until
    /// the returned sender sends (or drops), signalling the returned receiver when it parks. Later
    /// checkpoints run through.
    fn park_first_write(root: &std::path::Path, name: &str)
        -> (WriteSeamGuard, std::sync::mpsc::Receiver<()>, std::sync::mpsc::Sender<()>) {
        let (reached_tx, reached_rx) = std::sync::mpsc::channel();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let mut parked = false;

        let guard = arm_write_seam(root, name, move || {
            if !std::mem::replace(&mut parked, true) {
                let _ = reached_tx.send(());
                let _ = gate_rx.recv_timeout(std::time::Duration::from_secs(30));
            }
        });

        (guard, reached_rx, gate_tx)
    }

    /// How many times ref updates to `name` in the warehouse at `root` decided.
    fn ref_update_attempts(root: &std::path::Path, name: &str) -> u32 {
        let key = (root.join(FOLDER_NAME_FORKLIFT_ROOT), name.to_string());

        REF_UPDATE_ATTEMPTS_TEST_COUNT.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied().unwrap_or(0)
    }

    async fn body_text(response: Response) -> String {
        String::from_utf8(axum::body::to_bytes(response.into_body(), usize::MAX).await.unwrap().to_vec()).unwrap()
    }

    // Reverts: the final write passes `OfficePrecondition::NotConsumed` → the push lands on the
    // first attempt, judged against an office that no longer stands; the attempt bound raised to
    // four → four attempts.
    #[tokio::test]
    async fn a_push_whose_office_moves_after_every_audit_is_refused_after_three_attempts() {
        let fixture = OfficeFixture::genesis("post-ref-office-moved");
        let quinn_key_id = fixture.admit("quinn", Role::Writer, vec!["office-moved".to_string()], IdentityClass::Human, None);
        let quinn = Operator { identifier: "quinn".to_string() };
        let new_head = store_signed_parcel(&quinn, &quinn_key_id, Vec::new(), "quinn's push");
        let state = Arc::new(single_mode_state(fixture.root.clone()));

        // A local bay of this warehouse stacks a real office parcel between every audit and its
        // write, so each decision reads an office that verifies and is gone by the write.
        let admin = fixture.admin.clone();
        let admin_key_id = fixture.admin_key_id.clone();
        let _seam = arm_write_seam(&fixture.root, "office-moved", move || {
            let office_state = office_utils::read_office_state().unwrap();
            office_utils::stack_office_parcel(
                &office_state, &admin, "a bay moves the office".to_string(), &admin_key_id,
                forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().as_deref(),
                &RefPreconditions::NONE,
            ).unwrap();
        });

        let response = post_ref_update(
            State(state), as_operator("quinn"), Path(ref_update_params("office-moved")),
            Json(RefUpdateRequest { old_head: None, new_head, tombstone: None }),
        ).await.into_response();

        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("office-moved").unwrap(), None,
            "the pallet must be unchanged");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let moved = forklift_core::util::pallet_utils::get_meta_pallet_head(OFFICE_PALLET_NAME).unwrap().unwrap();
        let body = body_text(response).await;
        assert!(body.contains(&format!("The office pallet moved during the audit: its head is now {}.", moved)), "{}", body);
        assert_eq!(ref_update_attempts(&fixture.root, "office-moved"), 3);
    }

    // Revert: the final write passes `AnchorPrecondition::NotConsumed` → the push lands, judged
    // against an anchor that no longer stands.
    #[tokio::test]
    async fn a_push_whose_anchor_changes_after_every_audit_is_refused_after_three_attempts() {
        let fixture = OfficeFixture::genesis("post-ref-anchor-moved");
        let quinn_key_id = fixture.admit("quinn", Role::Writer, vec!["anchor-moved".to_string()], IdentityClass::Human, None);
        let quinn = Operator { identifier: "quinn".to_string() };
        let new_head = store_signed_parcel(&quinn, &quinn_key_id, Vec::new(), "quinn's push");
        let state = Arc::new(single_mode_state(fixture.root.clone()));

        // The anchor's bytes change between every audit and its write.
        let trust = fixture.root.join(FOLDER_NAME_FORKLIFT_ROOT).join("trust");
        let _seam = arm_write_seam(&fixture.root, "anchor-moved", move || {
            let mut bytes = std::fs::read(&trust).unwrap();
            bytes.extend_from_slice(b"# changed\n");
            std::fs::write(&trust, bytes).unwrap();
        });

        let response = post_ref_update(
            State(state), as_operator("quinn"), Path(ref_update_params("anchor-moved")),
            Json(RefUpdateRequest { old_head: None, new_head, tombstone: None }),
        ).await.into_response();

        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("anchor-moved").unwrap(), None,
            "the pallet must be unchanged");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = body_text(response).await;
        assert!(body.contains("The trust anchor changed during the audit; re-read the warehouse and retry."), "{}", body);
        assert_eq!(ref_update_attempts(&fixture.root, "anchor-moved"), 3);
    }

    /// A ref update to `name` from an unborn head, by an open principal, as a spawned task.
    fn spawn_open_push(state: &Arc<AppState>, name: &str, new_head: String) -> tokio::task::JoinHandle<Response> {
        tokio::spawn(post_ref_update(
            State(Arc::clone(state)), Extension(Principal::Open), Path(ref_update_params(name)),
            Json(RefUpdateRequest { old_head: None, new_head, tombstone: None }),
        ))
    }

    // Revert: a mutex shared by every ref update around post_ref_update's blocking closure → the
    // second push waits behind the parked one and the bounded wait runs out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parked_push_does_not_hold_up_a_push_to_another_pallet() {
        let (root, _scope) = trust_warehouse("post-ref-parked-other-pallet");
        let parked_head = store_bare_parcel("parked");
        let free_head = store_bare_parcel("free");
        let state = Arc::new(single_mode_state(root.clone()));

        let (_seam, reached, gate) = park_first_write(&root, "parked");
        let parked = spawn_open_push(&state, "parked", parked_head.clone());
        reached.recv_timeout(std::time::Duration::from_secs(5)).expect("the first push must park at its write");

        let free = tokio::time::timeout(std::time::Duration::from_secs(5), spawn_open_push(&state, "free", free_head.clone()))
            .await
            .expect("a push to another pallet must not wait for the parked one")
            .unwrap();
        assert_eq!(free.status(), StatusCode::OK);

        gate.send(()).unwrap();
        assert_eq!(parked.await.unwrap().status(), StatusCode::OK);
        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("parked").unwrap(), Some(parked_head));
        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("free").unwrap(), Some(free_head));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A ref write whose client hangs up while it is under way still discharges the apply it owed
    /// once it lands, so content is served again without waiting for another write.
    ///
    /// Revert: settle the write in the request's own future → the dropped request never applies,
    /// and the apply stays owed → red.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_write_whose_client_hangs_up_still_discharges_its_apply() {
        let (root, _scope) = trust_warehouse("post-ref-hung-up");
        let head = store_bare_parcel("hung-up");
        let state = Arc::new(single_mode_state(root.clone()));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();

        let (_seam, reached, gate) = park_first_write(&root, "hung-up");
        let push = spawn_open_push(&state, "hung-up", head.clone());
        reached.recv_timeout(std::time::Duration::from_secs(5)).expect("the push must park at its write");
        push.abort();
        assert!(push.await.unwrap_err().is_cancelled());
        gate.send(()).unwrap();

        let mut open = false;
        for _ in 0..250 {
            if forklift_core::util::pallet_utils::get_pallet_head("hung-up").unwrap().is_some() && !handle.applies_closed() {
                open = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(open, "the write landed and its apply was discharged");

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A round reports no apply owed for an owe only a ref write still in flight holds: that
    /// write's own apply discharges it, so the run does not poll for it.
    ///
    /// Revert: report the round's apply owed while any owe is open → red.
    #[tokio::test]
    async fn a_round_owes_no_apply_a_write_in_flight_owes() {
        let root = scratch_dir("round-in-flight");
        mark_warehouse(&root);
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        handle.redacting.store(true, Ordering::SeqCst);

        let writing = handle.owe_apply();
        assert!(!redaction_round(&state, &handle).await.apply, "the write in flight owes it");
        assert!(handle.applies_closed(), "and it is not discharged");

        drop((writing, handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A round rebuilds the bundle a write's apply called for even when another round follows, as a
    /// steady stream of writes makes one follow every round; it carries the rebuild on to the next
    /// round only when that round drains or applies first: a write applied a redaction while it
    /// ran, its deletion is owed with another round asked for, or an apply is owed.
    ///
    /// A refused write that settles an owed apply asks for the run a carried rebuild needs.
    ///
    /// Revert: leave the rebuild to the next round whenever one follows → the first round does not
    /// rebuild → red. Ignore a write's apply, an owed deletion or an owed apply → that case
    /// rebuilds → red. Drop the call on deferring → it is not carried on → red. Carry a carried
    /// rebuild on again → red. A refused write ignoring the carried rebuild → no run → red. Carry
    /// only an apply's rebuild → the held-back one is not carried → red.
    #[tokio::test]
    async fn a_round_rebuilds_the_bundle_unless_a_write_applied_meanwhile() {
        let root = scratch_dir("round-rebuild");
        mark_warehouse(&root);
        let mut state = base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone()))));
        state.rebuild_after_lifts = Some(1000);
        let state = Arc::new(state);
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        handle.redacting.store(true, Ordering::SeqCst);
        handle.bundling.store(true, Ordering::SeqCst);

        // A write applied before the round and another asked for a run: the round rebuilds. The
        // rebuild marked running is asked again, so the round's ask shows as `rebuild_again`.
        handle.rebuild_after_drain.store(true, Ordering::SeqCst);
        handle.redact_again.store(true, Ordering::SeqCst);
        redaction_round(&state, &handle).await;
        assert!(handle.rebuild_again.swap(false, Ordering::SeqCst), "the round rebuilds");
        assert!(!handle.rebuild_after_drain.load(Ordering::SeqCst), "and settles the write's call");

        // A write applied while the round ran: its rebuild is the next round's.
        handle.rebuild_after_drain.store(true, Ordering::SeqCst);
        finish_round(&state, &handle, true, false, false);
        assert!(!handle.rebuild_again.load(Ordering::SeqCst), "the next round rebuilds");
        assert!(handle.rebuild_after_drain.load(Ordering::SeqCst), "and is still called for");

        // The round applied, its deletion is owed and another round follows at once: that round
        // drains first. A rebuild already carried on to the round is not carried again.
        handle.rebuild_after_drain.store(false, Ordering::SeqCst);
        finish_round(&state, &handle, true, false, true);
        assert!(!handle.rebuild_again.load(Ordering::SeqCst), "the drain goes first");
        assert!(handle.rebuild_after_drain.swap(false, Ordering::SeqCst), "and the rebuild is carried on");
        finish_round(&state, &handle, false, true, true);
        assert!(handle.rebuild_again.swap(false, Ordering::SeqCst), "a carried rebuild is not put off again");

        // An apply is owed: no bundle is served until one settles it.
        handle.rebuild_after_drain.store(false, Ordering::SeqCst);
        handle.redact_again.store(false, Ordering::SeqCst);
        let owe = handle.owe_apply();
        drop(owe);
        finish_round(&state, &handle, true, false, false);
        assert!(!handle.rebuild_again.load(Ordering::SeqCst), "the apply goes first");
        assert!(handle.rebuild_after_drain.load(Ordering::SeqCst), "and the rebuild is carried on");
        assert!(!handle.redact_again.load(Ordering::SeqCst), "for the run settling the apply");

        // So is one the drain held back, with nothing applied.
        handle.rebuild_after_drain.store(false, Ordering::SeqCst);
        handle.rebuild_again.store(true, Ordering::SeqCst);
        finish_round(&state, &handle, false, false, false);
        assert!(handle.rebuild_after_drain.load(Ordering::SeqCst), "a held-back rebuild is carried on");
        handle.rebuild_again.store(false, Ordering::SeqCst);

        // A refused write that settles the apply without applying anything asks for that run.
        handle.redacting.store(true, Ordering::SeqCst);
        handle.redactions_asked.store(0, Ordering::SeqCst);
        after_failed_ref_write(&state, Arc::clone(&handle), true).await;
        assert!(!handle.apply_owed(), "the write settles the apply");
        assert_eq!(handle.redactions_asked.load(Ordering::SeqCst), 1, "and asks for the run");

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A ref write refused or answered as done asks for a redaction run only when it reached its
    /// write and the apply it carries out fails: one with nothing owed, with only another write's
    /// owe in flight, or refused before its write while an apply stays owed, never wakes a run
    /// waiting out a retry.
    ///
    /// Revert: ask for a run whenever an owe is open → the refusal beside a write in flight asks →
    /// red. Ignore whether the request reached its write → the early refusal asks → red. Ask for
    /// none after a failed apply → nothing retries it → red.
    #[tokio::test]
    async fn a_refused_write_asks_for_a_run_only_when_its_apply_fails() {
        let root = scratch_dir("refused-write");
        mark_warehouse(&root);
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        handle.redacting.store(true, Ordering::SeqCst);

        after_failed_ref_write(&state, Arc::clone(&handle), true).await;
        let writing = handle.owe_apply();
        after_failed_ref_write(&state, Arc::clone(&handle), true).await;
        assert_eq!(handle.redactions_asked.load(Ordering::SeqCst), 0, "nothing for a run to apply");
        drop(writing);

        // A record this build cannot read fails every apply, so one stays owed.
        std::fs::write(root.join(FOLDER_NAME_FORKLIFT_ROOT).join("redactions-applied"),
                       b"/forklift-redactions-applied 2\n").unwrap();
        after_failed_ref_write(&state, Arc::clone(&handle), false).await;
        assert_eq!(handle.redactions_asked.load(Ordering::SeqCst), 0, "a request refused before its write");
        after_failed_ref_write(&state, Arc::clone(&handle), true).await;
        assert_eq!(handle.redactions_asked.load(Ordering::SeqCst), 1, "the failed apply is the run's");
        assert!(handle.applies_closed());

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A content request refused for what it asks is refused at once, owed apply or not: only a
    /// request that would be answered with content waits for one.
    ///
    /// Revert: wait for the applies before the request is checked → it waits out the bound and is
    /// answered `503` → red.
    #[tokio::test]
    async fn a_malformed_content_request_is_refused_without_waiting_for_an_apply() {
        let root = scratch_dir("malformed-wait");
        mark_warehouse(&root);
        let state = Arc::new(base_state(ServeMode::Single(Arc::new(WarehouseHandle::new(root.clone())))));
        let handle = resolve_handle(&state, &PathParams::new()).unwrap();
        drop(handle.owe_apply());

        let mut params = PathParams::new();
        params.insert("hash".to_string(), "not-a-hash".to_string());
        let answer = tokio::time::timeout(std::time::Duration::from_secs(2),
                                          get_object(State(Arc::clone(&state)), Path(params)))
            .await.expect("refused at once");
        assert_eq!(answer.status(), StatusCode::UNPROCESSABLE_ENTITY);

        drop((handle, state));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    // Revert: one mutex around both post_ref_update's and put_trust's blocking closures → the trust
    // PUT waits behind the parked push and the bounded wait runs out.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_parked_push_does_not_hold_up_trust_establishment() {
        let (root, _scope) = trust_warehouse("post-ref-parked-trust");
        let parked_head = store_bare_parcel("parked");
        let state = Arc::new(single_mode_state(root.clone()));

        let (_seam, reached, gate) = park_first_write(&root, "parked");
        let parked = spawn_open_push(&state, "parked", parked_head);
        reached.recv_timeout(std::time::Duration::from_secs(5)).expect("the push must park at its write");

        let trust = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            put_trust(State(Arc::clone(&state)), Extension(Principal::Open), Path(PathParams::new()), Json(first_anchor(Vec::new()))),
        ).await.expect("trust establishment must not wait for the parked push").into_response();
        assert_eq!(trust.status(), StatusCode::CREATED);

        gate.send(()).unwrap();
        parked.await.unwrap();
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    // Reverts: the final write expects the pallet's current head instead of the request's old head
    // → the loser overwrites the winner (200); the loop also deciding again on `Moved` → a second
    // attempt for the loser.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_loser_of_two_pushes_from_one_head_is_refused_after_one_attempt() {
        let (root, _scope) = trust_warehouse("post-ref-same-pallet");
        let loser_head = store_bare_parcel("loser");
        let winner_head = store_bare_parcel("winner");
        let state = Arc::new(single_mode_state(root.clone()));

        let (_seam, reached, gate) = park_first_write(&root, "contested");
        let loser = spawn_open_push(&state, "contested", loser_head);
        reached.recv_timeout(std::time::Duration::from_secs(5)).expect("the loser must park at its write");

        let winner = spawn_open_push(&state, "contested", winner_head.clone()).await.unwrap();
        assert_eq!(winner.status(), StatusCode::OK);

        gate.send(()).unwrap();
        let loser = loser.await.unwrap();

        assert_eq!(forklift_core::util::pallet_utils::get_pallet_head("contested").unwrap(), Some(winner_head.clone()),
            "the winner's head must stand");
        assert_eq!(loser.status(), StatusCode::CONFLICT);
        let body = body_text(loser).await;
        assert!(body.contains(&winner_head), "the refusal names the head that won: {}", body);
        assert_eq!(ref_update_attempts(&root, "contested"), 2, "one attempt each");
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// A first anchor for an empty warehouse, adopting nothing.
    fn first_anchor(boundary: Vec<String>) -> TrustAnchorDto {
        TrustAnchorDto { genesis: "0".repeat(64), enabled_at: 1, boundary, prior_genesis: None, adopts: None }
    }

    /// A parcel on `parent`, so a head on it descends from `parent`.
    fn store_child_parcel(parent: &str, description: &str) -> String {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::dir_entry_type::DirEntryType;
        use forklift_core::model::parcel::Parcel;

        let mut root_object = LooseObjectBuilder::build_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree));
        root_object.store().unwrap();

        let mut parcel_object = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: root_object.hash,
            parents: vec![parent.to_string()],
            actions: Vec::new(),
            description: Some(description.to_string()),
        });
        parcel_object.store().unwrap();
        parcel_object.hash
    }

    /// Deleting a pallet over the protocol, the tombstone it leaves, and what that tombstone
    /// changes for the handshake, holds and a re-creation.
    mod pallet_delete {
        use super::*;
        use forklift_core::util::tombstone_utils;

        /// A served root (marked as `serving` marks it) whose bay stands on "main", with pallet
        /// "side" at a head of its own; returns that head.
        fn served_with_side(name: &str) -> (PathBuf, StorageRootScope, String) {
            let (root, scope) = trust_warehouse(name);
            let main = store_bare_parcel("main");
            let side = store_bare_parcel("side");
            pallet_utils::update_pallet_head("main", None, &main, &RefPreconditions::NONE).unwrap();
            pallet_utils::update_pallet_head("side", None, &side, &RefPreconditions::NONE).unwrap();
            pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(main))).unwrap();
            assert!(tombstone_utils::mark_served(false).unwrap());

            (root, scope, side)
        }

        async fn delete(root: &std::path::Path, name: &str, old_head: &str) -> Response {
            delete_ref(
                State(Arc::new(single_mode_state(root.to_path_buf()))), Extension(Principal::Open), Path(ref_update_params(name)),
                Query(RefDeleteRequest { old_head: old_head.to_string() }),
            ).await.into_response()
        }

        async fn create(root: &std::path::Path, name: &str, new_head: &str, tombstone: Option<&str>) -> Response {
            post_ref_update(
                State(Arc::new(single_mode_state(root.to_path_buf()))), Extension(Principal::Open), Path(ref_update_params(name)),
                Json(RefUpdateRequest { old_head: None, new_head: new_head.to_string(), tombstone: tombstone.map(str::to_string) }),
            ).await.into_response()
        }

        /// Revert: drop the `mark_served` call from `serving` → the served root carries no mark →
        /// red.
        #[test]
        fn serving_a_root_marks_it_served() {
            let root = scratch_dir("serving-marks");
            mark_warehouse(&root);

            drop(WarehouseHandle::serving(root.clone(), false).expect("a full root is served"));

            let _scope = StorageRootScope::enter(&root);
            assert!(tombstone_utils::is_served().unwrap(), "a served root carries the mark");
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: read an absent served mark as never served → the gate serves the root and
        /// writes a fresh mark over the tombstones → red. Revert: answer damage as `internal` in
        /// `damage_refusal` → a 500 → red.
        #[tokio::test]
        async fn a_root_whose_mark_was_lost_beside_its_tombstones_is_not_served() {
            let (root, scope, side) = served_with_side("mark-lost");
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);
            std::fs::remove_file(forklift_core::globals::forklift_root().join("served")).unwrap();
            drop(scope);

            let refused = WarehouseHandle::serving(root.clone(), false).err().expect("the root is refused");
            assert!(matches!(&refused, ServingRefusal::Unreadable(reason) if reason.contains("served mark")), "{:?}", refused);

            let (status, message) = damage_refusal(match refused { ServingRefusal::Unreadable(reason) => reason, _ => unreachable!() });
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
            assert!(forklift_core::error::deframe(&message).is_some_and(|(code, _, _)| code == "tombstones_damaged"), "{}", message);

            let _scope = StorageRootScope::enter(&root);
            assert!(!forklift_core::globals::forklift_root().join("served").exists(), "no mark is written over the damage");
            drop(_scope);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        #[tokio::test]
        async fn a_delete_removes_the_ref_and_the_handshake_lists_its_tombstone() {
            let (root, _scope, side) = served_with_side("delete-lands");

            let response = delete(&root, "side", &side).await;

            assert_eq!(response.status(), StatusCode::OK, "{}", body_text(response).await);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);

            let info = get_warehouse(State(Arc::new(single_mode_state(root.clone()))), Path(PathParams::new()))
                .await.into_response();
            let info: WarehouseInfo = serde_json::from_str(&body_text(info).await).unwrap();
            assert_eq!(info.tombstones, Some(BTreeMap::from([("side".to_string(), side)])));
            assert!(!info.pallets.contains_key("side"), "a deleted pallet is not listed");
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: map the handshake's list read through `internal` → a 500 with no code → red.
        /// Revert: add `Retry-After` to every 503 → the damage answer tells the client to wait →
        /// red.
        #[tokio::test]
        async fn a_handshake_over_a_damaged_list_refuses_with_its_code_and_no_retry() {
            use forklift_core::util::record_utils;
            let (root, _scope, side) = served_with_side("handshake-damaged");
            let list = forklift_core::globals::forklift_root().join("tombstones");
            let (first, second) = (format!("{} gone", side), format!("{} gone", "b".repeat(64)));

            let damages: [(&str, Option<Vec<u8>>); 4] = [
                ("not text", Some(b"\xff\xfe not text".to_vec())),
                ("a bad entry", Some(record_utils::to_bytes("tombstones", 2, ["nothash gone"]))),
                ("a duplicate", Some(record_utils::to_bytes("tombstones", 2, [first.as_str(), second.as_str()]))),
                ("an absent list", None),
            ];

            for (what, bytes) in damages {
                match bytes {
                    Some(bytes) => std::fs::write(&list, bytes).unwrap(),
                    None => std::fs::remove_file(&list).unwrap(),
                }

                let response = get_warehouse(State(Arc::new(single_mode_state(root.clone()))), Path(PathParams::new()))
                    .await.into_response();

                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{}", what);
                assert!(response.headers().get(axum::http::header::RETRY_AFTER).is_none(), "{}: no wait mends it", what);
                let body = body_text(response).await;
                assert!(body.contains(r#""code":"tombstones_damaged""#), "{}: {}", what, body);
                assert!(body.contains("forklift-server tombstone repair --root <warehouse>"), "{}: {}", what, body);
            assert!(!body.contains("repair --root /") && !body.contains("repair --root '"), "{}: no server path: {}", what, body);
            }
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: map the moved ref to `pallet_moved` alone → the refusal says "Lower and retry"
        /// → red.
        #[tokio::test]
        async fn a_lift_onto_a_pallet_deleted_here_names_the_deletion_not_a_lower() {
            let (root, _scope, side) = served_with_side("delete-then-lift");
            let on_it = store_child_parcel(&side, "on side");
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);

            let response = post_ref_update(
                State(Arc::new(single_mode_state(root.clone()))), Extension(Principal::Open), Path(ref_update_params("side")),
                Json(RefUpdateRequest { old_head: Some(side.clone()), new_head: on_it, tombstone: None }),
            ).await.into_response();

            assert_eq!(response.status(), StatusCode::CONFLICT);
            let body = body_text(response).await;
            assert!(body.contains("was deleted on this remote") && !body.contains("Lower and retry"), "{}", body);
            assert!(body.contains("tombstone drop --root <warehouse> side"), "{}", body);
            assert!(body.contains(r#""code":"pallet_deleted_on_remote""#), "{}", body);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: drop the replay's tombstone check → the retry reads as a moved ref → red.
        #[tokio::test]
        async fn a_retried_delete_that_already_landed_answers_ok_and_another_head_still_conflicts() {
            let (root, _scope, side) = served_with_side("delete-replay");
            let main = pallet_utils::get_pallet_head("main").unwrap().unwrap();
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);

            let replay = delete(&root, "side", &side).await;
            assert_eq!(replay.status(), StatusCode::OK, "{}", body_text(replay).await);

            let other = delete(&root, "side", &main).await;
            assert_eq!(other.status(), StatusCode::CONFLICT, "a delete at a head the tombstone does not hold is refused");
            assert_eq!(tombstone_utils::read_tombstones().unwrap().named, BTreeMap::from([("side".to_string(), side)]));
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: drop the meta refusal → `@tags` is removed (or the request fails otherwise) →
        /// red. Revert: drop the default-pallet refusal → "main" is deleted → red.
        #[tokio::test]
        async fn a_delete_refuses_a_meta_pallet_a_moved_ref_and_the_default_pallet() {
            let (root, _scope, side) = served_with_side("delete-refusals");
            let tags = store_bare_parcel("tags");
            pallet_utils::update_meta_pallet_head("tags", None, &tags, &RefPreconditions::NONE).unwrap();
            let main = pallet_utils::get_pallet_head("main").unwrap().unwrap();

            let meta = delete(&root, "@tags", &tags).await;
            assert_eq!(meta.status(), StatusCode::UNPROCESSABLE_ENTITY);

            let moved = delete(&root, "side", &main).await;
            assert_eq!(moved.status(), StatusCode::CONFLICT);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(side), "a moved ref stays");

            let default = delete(&root, "main", &main).await;
            assert_eq!(default.status(), StatusCode::CONFLICT);
            assert!(body_text(default).await.contains("default pallet"));
            assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(main));

            assert_eq!(pallet_utils::get_meta_pallet_head("tags").unwrap(), Some(tags));
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty(), "no refusal leaves a tombstone");
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: drop the early refusal and pass no tombstone to the write → the unacknowledged
        /// creation lands → red. Revert: drop only the early refusal → the write refuses it all the
        /// same (the rule lives in the ref core), so this stays green: the early refusal is not
        /// the guard.
        #[tokio::test]
        async fn re_creating_a_deleted_pallet_must_acknowledge_its_tombstone_and_descend_from_it() {
            let (root, _scope, side) = served_with_side("delete-recreate");
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);
            let on_it = store_child_parcel(&side, "on side");
            let unrelated = store_bare_parcel("unrelated");

            let unacknowledged = create(&root, "side", &on_it, None).await;
            assert_eq!(unacknowledged.status(), StatusCode::CONFLICT);
            let body = body_text(unacknowledged).await;
            assert!(body.contains("acknowledge the deletion"), "{}", body);
            assert!(body.contains(r#""code":"pallet_deleted_on_remote""#), "a client branches on the code: {}", body);

            let unrelated = create(&root, "side", &unrelated, Some(&side)).await;
            assert_eq!(unrelated.status(), StatusCode::CONFLICT);
            assert!(body_text(unrelated).await.contains("does not descend"));
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);

            let acknowledged = create(&root, "side", &on_it, Some(&side)).await;
            assert_eq!(acknowledged.status(), StatusCode::OK, "{}", body_text(acknowledged).await);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(on_it));
            assert!(tombstone_utils::read_tombstones().unwrap().is_empty(), "the re-creation clears the tombstone");
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A re-creation whose history is not all uploaded is refused as missing history (`422`),
        /// before any walk of it.
        ///
        /// Revert: run the early tombstone check before the closure check → the walk fails on the
        /// absent parent → `500` → red.
        #[tokio::test]
        async fn a_re_creation_with_missing_history_is_a_422() {
            let (root, _scope, side) = served_with_side("delete-recreate-missing");
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);
            let absent = store_child_parcel(&side, "never uploaded");
            let on_absent = store_child_parcel(&absent, "uploaded");
            let (folder, file) = file_utils::get_path_for_object(&absent).unwrap();
            forklift_core::util::file_utils::remove_file(std::path::Path::new(&folder).join(file)).unwrap();

            let response = create(&root, "side", &on_absent, Some(&side)).await;

            assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
            // Revert: answer the closure refusal as plain text → no code → a lift cannot tell it
            // apart and never uploads again → red.
            let body = body_text(response).await;
            assert!(body.contains(r#""code":"remote_history_incomplete""#), "a client branches on the code: {}", body);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// Revert: walk nothing for an absent ref in `post_holds` → the deleted head reads as not
        /// held → red.
        #[tokio::test]
        async fn holds_are_answered_from_a_deleted_pallets_tombstone() {
            let (root, _scope, side) = served_with_side("delete-holds");
            assert_eq!(delete(&root, "side", &side).await.status(), StatusCode::OK);

            let response = post_holds(
                State(Arc::new(single_mode_state(root.clone()))), Path(ref_update_params("side")),
                Json(HoldsRequest { candidates: vec![side.clone()] }),
            ).await.into_response();

            let holds: HoldsResponse = serde_json::from_str(&body_text(response).await).unwrap();
            assert_eq!(holds.holds.get(&side), Some(&HoldAnswer::Held));
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }
    }

    /// The push gate on a user pallet's head: a name no client can check out is refused, judged
    /// against the old head or the tombstone this warehouse itself stores.
    mod materializable_heads {
        use super::*;
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::dir_entry_type::DirEntryType;
        use forklift_core::model::blob::Blob;
        use forklift_core::model::parcel::Parcel;
        use forklift_core::util::tombstone_utils;

        fn file(name: &str, content: &str) -> TreeItem {
            let mut blob = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
            blob.store().unwrap();
            TreeItem::new(name.to_string(), blob.hash, DirEntryType::Normal)
        }

        /// A stored tree named `name` holding `children`.
        fn dir(name: &str, children: Vec<TreeItem>) -> TreeItem {
            let mut tree = TreeItem::new(name.to_string(), String::new(), DirEntryType::Tree);
            for child in children {
                tree.add_child(child);
            }
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            tree.hash = object.hash;
            tree
        }

        /// A stored parcel on `parents` whose root holds `children`.
        fn parcel(parents: &[&str], children: Vec<TreeItem>) -> String {
            let root = dir("", children);
            let mut object = LooseObjectBuilder::build_parcel(&Parcel {
                tree_hash: root.hash,
                parents: parents.iter().map(|parent| parent.to_string()).collect(),
                actions: Vec::new(),
                description: Some("gate".to_string()),
            });
            object.store().unwrap();
            object.hash
        }

        /// A served root whose bay stands on "main".
        fn served(name: &str) -> (PathBuf, StorageRootScope) {
            let (root, scope) = trust_warehouse(name);
            let main = store_bare_parcel("main");
            pallet_utils::update_pallet_head("main", None, &main, &RefPreconditions::NONE).unwrap();
            pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(main))).unwrap();
            assert!(tombstone_utils::mark_served(false).unwrap());

            (root, scope)
        }

        /// Plant `head` on user pallet `name` past the gate, as a writer from before it would have.
        fn plant(name: &str, head: &str) {
            pallet_utils::update_pallet_head(name, None, head, &RefPreconditions::NONE).unwrap();
        }

        async fn update(root: &std::path::Path, name: &str, old_head: Option<&str>, new_head: &str,
                        tombstone: Option<&str>) -> (StatusCode, String) {
            let response = post_ref_update(
                State(Arc::new(single_mode_state(root.to_path_buf()))), Extension(Principal::Open), Path(ref_update_params(name)),
                Json(RefUpdateRequest {
                    old_head: old_head.map(str::to_string),
                    new_head: new_head.to_string(),
                    tombstone: tombstone.map(str::to_string),
                }),
            ).await.into_response();
            let status = response.status();

            (status, body_text(response).await)
        }

        /// Revert: drop the gate → both crafted heads land → red. A clean head beside them lands,
        /// so the refusals are the names', not the fixture's.
        #[tokio::test]
        async fn a_head_holding_the_metadata_folder_at_any_depth_is_refused() {
            let (root, _scope) = served("gate-any-depth");
            let at_root = parcel(&[], vec![dir(".forklift", vec![file("config", "x")])]);
            let below = parcel(&[], vec![dir("docs", vec![file("a", "1"), file(".ForkLift", "x")])]);
            let clean = parcel(&[], vec![dir("docs", vec![file("a", "1")])]);

            let (status, body) = update(&root, "side", None, &at_root, None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert!(body.contains(r#"holds \".forklift\""#), "{}", body);

            let (status, body) = update(&root, "side", None, &below, None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert!(body.contains(r#"holds \"docs/.ForkLift\""#), "the path is named whole: {}", body);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);

            let (status, body) = update(&root, "side", None, &clean, None).await;
            assert_eq!(status, StatusCode::OK, "{}", body);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A creation names a wedged parcel as its tombstone when none stands, and builds on it
        /// without changing its tree. Judged against the request's field, nothing would be loaded.
        ///
        /// Revert: take the base from `request.tombstone` → the creation lands → red.
        #[tokio::test]
        async fn a_creation_is_judged_against_the_stored_tombstone_never_the_request() {
            let (root, _scope) = served("gate-free-tombstone");
            let wedged_tree = vec![file(".forklift", "x"), file("a", "1")];
            let wedged = parcel(&[], wedged_tree.clone());
            let on_it = parcel(&[&wedged], wedged_tree);

            let (status, body) = update(&root, "side", None, &on_it, Some(&wedged)).await;

            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert!(body.contains(r#"holds \".forklift\""#), "{}", body);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A wedged head deleted here comes back by a restore, whose tree is its stored tombstone's,
        /// so nothing is loaded; the pallet is then repaired like any wedged head.
        ///
        /// Revert: judge a creation against nothing even when a tombstone stands → the restore is
        /// refused → red.
        #[tokio::test]
        async fn a_restore_of_a_wedged_tombstone_head_is_accepted() {
            let (root, _scope) = served("gate-restore");
            let wedged = parcel(&[], vec![file(".forklift", "x")]);
            plant("side", &wedged);
            let deleted = delete_ref(
                State(Arc::new(single_mode_state(root.clone()))), Extension(Principal::Open), Path(ref_update_params("side")),
                Query(RefDeleteRequest { old_head: wedged.clone() }),
            ).await.into_response();
            assert_eq!(deleted.status(), StatusCode::OK);

            let (status, body) = update(&root, "side", None, &wedged, Some(&wedged)).await;

            assert_eq!(status, StatusCode::OK, "{}", body);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(wedged));
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A pallet planted with a bad name in `d`: an edit in `d` is refused until a repair removes
        /// it, and an edit in `e` passes.
        ///
        /// Revert: skip a name the base already held → the edit in `d` lands → red. Revert: load
        /// every subtree whatever the base → the edit in `e` is refused → red.
        #[tokio::test]
        async fn an_edit_beside_an_old_bad_name_is_refused_and_one_elsewhere_passes() {
            let (root, _scope) = served("gate-old-name");
            let e = dir("e", vec![file("g", "1")]);
            let wedged = parcel(&[], vec![dir("d", vec![file(".forklift", "x"), file("f", "1")]), e.clone()]);
            plant("side", &wedged);

            let beside = parcel(&[&wedged], vec![dir("d", vec![file(".forklift", "x"), file("f", "2")]), e]);
            let (status, body) = update(&root, "side", Some(&wedged), &beside, None).await;
            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert!(body.contains(r#"holds \"d/.forklift\""#), "{}", body);

            let elsewhere = parcel(&[&wedged], vec![dir("d", vec![file(".forklift", "x"), file("f", "1")]), dir("e", vec![file("g", "2")])]);
            let (status, body) = update(&root, "side", Some(&wedged), &elsewhere, None).await;
            assert_eq!(status, StatusCode::OK, "{}", body);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(elsewhere));
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A merge over a repaired head that takes its older, wedged parent's tree. The closure walk
        /// skips that tree as its parent's; the gate judges it against the old head alone.
        ///
        /// Revert: also skip a tree equal to a parent's → the merge lands → red.
        #[tokio::test]
        async fn a_merge_carrying_an_older_wedged_tree_is_refused() {
            let (root, _scope) = served("gate-merge");
            let wedged_tree = vec![file(".forklift", "x"), file("a", "1")];
            let wedged = parcel(&[], wedged_tree.clone());
            let repaired = parcel(&[&wedged], vec![file("a", "1")]);
            plant("side", &repaired);
            let merge = parcel(&[&repaired, &wedged], wedged_tree);

            let (status, body) = update(&root, "side", Some(&repaired), &merge, None).await;

            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(repaired));
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }

        /// A changed tree copied to a second path: at `a` its unchanged subtree `s` is the base's,
        /// at `b` there is no base, so the bad name inside `s` is new there.
        ///
        /// Revert: remember checked trees by hash alone → `b` is skipped as already checked → the
        /// head lands → red.
        #[tokio::test]
        async fn a_copied_tree_owes_what_its_new_path_never_held() {
            let (root, _scope) = served("gate-copy");
            let s = dir("s", vec![file(".forklift", "x")]);
            let wedged = parcel(&[], vec![dir("a", vec![s.clone(), file("x", "1")])]);
            plant("side", &wedged);
            let copied = parcel(&[&wedged], vec![dir("a", vec![s.clone(), file("x", "2")]), dir("b", vec![s, file("x", "2")])]);

            let (status, body) = update(&root, "side", Some(&wedged), &copied, None).await;

            assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{}", body);
            assert!(body.contains(r#"holds \"b/s/.forklift\""#), "{}", body);
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }
    }

    /// A storage-only warehouse entered on this thread, for a test of `PUT /v1/trust`.
    fn trust_warehouse(name: &str) -> (PathBuf, StorageRootScope) {
        let root = scratch_dir(name);
        mark_warehouse(&root);
        let scope = StorageRootScope::enter(&root);

        (root, scope)
    }

    // Revert: drop the office-shape check from `require_adopted_history` → the anchor is stored.
    #[tokio::test]
    async fn a_first_anchor_adopting_anything_but_office_history_is_refused_and_writes_nothing() {
        use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
        use forklift_core::enums::dir_entry_type::DirEntryType;
        use forklift_core::model::parcel::Parcel;

        let (root, _scope) = trust_warehouse("put-trust-adopts-non-office");
        let state = Arc::new(single_mode_state(root.clone()));

        let mut tree = LooseObjectBuilder::build_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree));
        tree.store().unwrap();
        let mut parcel = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree.hash,
            parents: Vec::new(),
            actions: Vec::new(),
            description: None,
        });
        parcel.store().unwrap();

        let anchor = TrustAnchorDto { adopts: Some(parcel.hash.clone()), ..first_anchor(Vec::new()) };
        let response = put_trust(State(state), Extension(Principal::Open), Path(PathParams::new()), Json(anchor))
            .await
            .into_response();

        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = body_text(response).await;
        assert!(body.contains("not office history") && body.contains(&parcel.hash), "{}", body);
        assert!(!root.join(".forklift").join("trust").exists(), "nothing is written");

        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    // Revert: the first anchor installs with `OfficePrecondition::NotConsumed` → the anchor lands
    // over an office it never saw.
    #[tokio::test]
    async fn a_first_anchor_whose_office_was_born_meanwhile_is_refused_and_writes_nothing() {
        let (root, _scope) = trust_warehouse("put-trust-office-born");
        let state = Arc::new(single_mode_state(root.clone()));
        let _seam = arm_write_seam(&root, TRUST_WRITE_SEAM, || {
            forklift_core::util::pallet_utils::update_meta_pallet_head(
                OFFICE_PALLET_NAME, None, &"e".repeat(64), &RefPreconditions::NONE,
            ).unwrap();
        });

        let response = put_trust(State(state), Extension(Principal::Open), Path(PathParams::new()), Json(first_anchor(Vec::new())))
            .await.into_response();

        assert_eq!(office_utils::read_trust_anchor().unwrap(), None, "no anchor may be written");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    // Revert: the first anchor installs with `PalletsPrecondition::NotConsumed` → the anchor lands
    // with a boundary that does not vouch for "main".
    #[tokio::test]
    async fn a_first_anchor_whose_boundary_misses_a_head_is_refused_and_writes_nothing() {
        let (root, _scope) = trust_warehouse("put-trust-boundary");
        let head = store_bare_parcel("main");
        forklift_core::util::pallet_utils::update_pallet_head("main", None, &head, &RefPreconditions::NONE).unwrap();
        let state = Arc::new(single_mode_state(root.clone()));

        let response = put_trust(State(state), Extension(Principal::Open), Path(PathParams::new()), Json(first_anchor(vec!["2".repeat(64)])))
            .await.into_response();

        assert_eq!(office_utils::read_trust_anchor().unwrap(), None, "no anchor may be written");
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let body = body_text(response).await;
        assert!(body.contains("Pallet \\\"main\\\" here is at") && body.contains("boundary does not hold"), "{}", body);

        let accepted = put_trust(
            State(Arc::new(single_mode_state(root.clone()))), Extension(Principal::Open), Path(PathParams::new()),
            Json(first_anchor(vec![head])),
        ).await.into_response();
        assert!(accepted.status().is_success(), "a boundary holding every head installs: {}", accepted.status());
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// Two first-contact trust PUTs, the first (of [`first_anchor`]) parked at its install while
    /// the second (of `second_anchor`) installs; with `move_office`, a bay then also stacks the
    /// office before the first resumes, so its install is refused rather than found in place.
    /// Returns the parked PUT's status and body, and the second's status.
    async fn racing_first_contacts(name: &str, second_anchor: TrustAnchorDto, move_office: bool)
                                   -> (StatusCode, String, StatusCode) {
        let (root, _scope) = trust_warehouse(name);
        let state = Arc::new(single_mode_state(root.clone()));

        let (_seam, reached, gate) = park_first_write(&root, TRUST_WRITE_SEAM);
        let parked = tokio::spawn(put_trust(
            State(Arc::clone(&state)), Extension(Principal::Open), Path(PathParams::new()), Json(first_anchor(Vec::new())),
        ));
        reached.recv_timeout(std::time::Duration::from_secs(5)).expect("the first PUT must park at its install");

        let second = put_trust(State(Arc::clone(&state)), Extension(Principal::Open), Path(PathParams::new()), Json(second_anchor.clone()))
            .await.into_response().status();

        if move_office {
            forklift_core::util::pallet_utils::update_meta_pallet_head(
                OFFICE_PALLET_NAME, None, &"e".repeat(64), &RefPreconditions::NONE,
            ).unwrap();
        }

        gate.send(()).unwrap();
        let parked = parked.await.unwrap().into_response();
        let parked_status = parked.status();
        let parked_body = body_text(parked).await;

        assert!(office_utils::read_trust_anchor().unwrap().is_some_and(|anchor| TrustAnchorDto::from(&anchor) == second_anchor),
            "the second PUT's anchor must be installed");
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);

        (parked_status, parked_body, second)
    }

    /// Two identical first-contact trust PUTs: [`racing_first_contacts`]'s statuses.
    async fn identical_first_contacts(name: &str, move_office: bool) -> (StatusCode, StatusCode) {
        let (parked, _, second) = racing_first_contacts(name, first_anchor(Vec::new()), move_office).await;

        (parked, second)
    }

    // Only a `201` emits EVENT_TRUST_ESTABLISHED (put_trust's `Ok(Some(event))` arm answers both),
    // so one `201` between the two is exactly one event.
    // Revert: put_trust answers `TrustInstalled::AlreadyInPlace` as it answers `Written` → the
    // parked PUT also answers 201, a second event.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_racing_first_contacts_establish_trust_once() {
        assert_eq!(identical_first_contacts("put-trust-identical", false).await, (StatusCode::OK, StatusCode::CREATED));
    }

    // Revert: put_trust answers a refused install without re-reading the anchor → the parked PUT
    // gets the office-moved 409 for an anchor that is the one it asked for.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn identical_racing_first_contacts_establish_trust_once_when_the_office_moves_between() {
        assert_eq!(identical_first_contacts("put-trust-identical-moved", true).await, (StatusCode::OK, StatusCode::CREATED));
    }

    // The loser of two different first contacts gets the answer a request arriving after the
    // winner gets, not an invitation to retry.
    // Revert: put_trust answers a refused install whose re-read anchor it is no re-genesis of with
    // the install's own refusal → the anchor-moved "retry" 409.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_loser_of_two_different_first_contacts_meets_the_one_way_door() {
        let other = TrustAnchorDto { enabled_at: 2, ..first_anchor(Vec::new()) };
        let (parked, body, second) = racing_first_contacts("put-trust-different", other, false).await;

        assert_eq!((parked, second), (StatusCode::CONFLICT, StatusCode::CREATED));
        assert!(body.contains("trust is a one-way door"), "{}", body);
    }

    /// The office fixture's state, for the meta-pallet falsifiers.
    fn office_state(fixture: &OfficeFixture) -> Arc<AppState> {
        Arc::new(single_mode_state(fixture.root.clone()))
    }

    /// Falsifier: a meta pallet name outside the known list is refused, even when its history
    /// is signed by an admin and would pass the signature-only check older names get.
    #[tokio::test]
    async fn an_unknown_meta_pallet_name_is_refused() {
        let fixture = OfficeFixture::genesis("post-ref-unknown-meta");
        let new_head = store_signed_parcel(&fixture.admin, &fixture.admin_key_id, Vec::new(), "@foo");

        let response = post_ref_update(
            State(office_state(&fixture)), as_operator(&fixture.admin.identifier),
            Path(ref_update_params("@foo")), Json(RefUpdateRequest { old_head: None, new_head, tombstone: None })
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Falsifier: a `@kv` head whose cell is stamped with a key other than the one that signed
    /// its parcel is refused by the content check, though every signature in it verifies.
    #[tokio::test]
    async fn a_kv_cell_stamped_by_another_key_is_refused() {
        use forklift_core::util::{kv_utils, meta_tree};

        let fixture = OfficeFixture::genesis("post-ref-kv-forged-stamp");
        let bob_key_id = fixture.admit("bob", Role::Writer, Vec::new(), IdentityClass::Human, None);
        let office = audit_utils::verified_local_office().unwrap();

        // Signed by alice, stamped as bob: the writer's own check refuses it after the parcel
        // is stored, so the signing closure keeps the hash for the push.
        let forged = std::cell::RefCell::new(None);
        let signer = meta_tree::MetaSigner::new(&fixture.admin_key_id, |hash| {
            *forged.borrow_mut() = Some(hash.to_string());
            forklift_core::util::sign_utils::sign_parcel_hash(&fixture.admin_key_id, hash)
        });
        let edit = meta_tree::set_cell(
            &kv_utils::cell_path(&"c".repeat(64), "ci"), &meta_tree::Stamp::Signer(bob_key_id.clone()),
            kv_utils::KV_TEXT_SCHEMA, &kv_utils::text_value("green")
        ).unwrap();

        let writer = meta_tree::MetaWriter::Signed { office, signer };
        let refused = meta_tree::stack_meta_edits(
            kv_utils::KV_PALLET_NAME, &kv_utils::KvRules, &writer, &[edit], &fixture.admin, "forged"
        );
        assert!(refused.is_err(), "the writer must refuse its own forged stamp");
        drop(writer);

        let new_head = forged.into_inner().expect("the forged parcel was signed");
        let response = post_ref_update(
            State(office_state(&fixture)), as_operator(&fixture.admin.identifier),
            Path(ref_update_params("@kv")), Json(RefUpdateRequest { old_head: None, new_head, tombstone: None })
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::UNPROCESSABLE_ENTITY);
    }

    /// Falsifier: a remote without an office refuses `@kv`, where any other meta pallet would be
    /// taken unverified.
    // `KEYS_ENV_LOCK` is held across the request for the reason the bootstrap test gives.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn kv_without_an_office_is_refused() {
        let root = scratch_dir("post-ref-kv-no-office");
        let _scope = StorageRootScope::enter(&root);
        let _keys_lock = KEYS_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let keys_dir = root.join("keys");
        std::fs::create_dir_all(&keys_dir).unwrap();
        let _keys_restore = KeysDirRestore(std::env::var("FORKLIFT_KEYS_DIR").ok());
        std::env::set_var("FORKLIFT_KEYS_DIR", &keys_dir);

        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();

        let actor = Operator { identifier: "alice".to_string() };
        let (key_id, _) = forklift_core::util::sign_utils::generate_keypair(&actor.identifier).unwrap();
        let new_head = store_signed_parcel(&actor, &key_id, Vec::new(), "@kv without an office");

        let response = post_ref_update(
            State(Arc::new(single_mode_state(root.clone()))), Extension(Principal::Open),
            Path(ref_update_params("@kv")), Json(RefUpdateRequest { old_head: None, new_head, tombstone: None })
        ).await;

        assert_eq!(response.into_response().status(), StatusCode::UNPROCESSABLE_ENTITY);

        drop(_scope);
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        // The lock outlives `_keys_restore` by declaration order, as in the bootstrap test.
    }

    /// Coverage (over-tightened direction, second half): two writers pushing to two
    /// *different* pallets, concurrently, must each commit on their own — neither
    /// push's audit may be forced to re-run, or refused, because of the other's
    /// activity. `office_head`/`office_state`/`anchor` are read fresh into local
    /// variables inside each request's own closure (never shared or cached across
    /// requests), so there is no channel through which one push could observe or be
    /// refused for another's *unrelated* pallet: both must land as 200s. (That neither
    /// waits for the other is pinned by `a_parked_push_does_not_hold_up_a_push_to_another_pallet`.)
    ///
    /// Unlike the other two falsifiers, this one is not known to discriminate a
    /// plausible wrong implementation of *this* diff — see this test's mention in the
    /// handoff report for why: slice 1 never introduces anything shared across
    /// requests or pallets for a mistake to leak through, so there is no "revert this
    /// hunk" story that turns it red. It stands as coverage against a *different*
    /// class of regression (a cache or shared state introduced by a future change).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_pushes_to_different_pallets_do_not_force_each_other_into_a_re_audit() {
        let fixture = OfficeFixture::genesis("post-ref-unrelated-pallets");
        let quinn_key_id = fixture.admit(
            "quinn", Role::Writer, vec!["main".to_string()], IdentityClass::Human, None
        );
        let roger_key_id = fixture.admit(
            "roger", Role::Writer, vec!["side".to_string()], IdentityClass::Human, None
        );

        let quinn = Operator { identifier: "quinn".to_string() };
        let roger = Operator { identifier: "roger".to_string() };
        let quinn_head = store_signed_parcel(&quinn, &quinn_key_id, Vec::new(), "quinn's push");
        let roger_head = store_signed_parcel(&roger, &roger_key_id, Vec::new(), "roger's push");

        let state = Arc::new(single_mode_state(fixture.root.clone()));

        let quinn_push = tokio::spawn(post_ref_update(
            State(Arc::clone(&state)),
            as_operator("quinn"),
            Path(ref_update_params("main")),
            Json(RefUpdateRequest { old_head: None, new_head: quinn_head, tombstone: None }),
        ));
        let roger_push = tokio::spawn(post_ref_update(
            State(Arc::clone(&state)),
            as_operator("roger"),
            Path(ref_update_params("side")),
            Json(RefUpdateRequest { old_head: None, new_head: roger_head, tombstone: None }),
        ));

        let quinn_status = quinn_push.await.unwrap().into_response().status();
        let roger_status = roger_push.await.unwrap().into_response().status();

        assert_eq!(quinn_status, StatusCode::OK, "quinn's push to \"main\" must commit on its own");
        assert_eq!(roger_status, StatusCode::OK, "roger's push to \"side\" must commit on its own");
    }

    // ---------------------------------------------------------------------------------
    // serve: the startup refusal to ever infer Principal::Open
    // ---------------------------------------------------------------------------------

    /// A minimal `ServeOptions` with no auth of any kind and `open: false` — every field a
    /// test overrides with struct-update syntax to exercise one dimension of the startup
    /// auth gate at a time.
    fn bare_serve_options() -> ServeOptions {
        ServeOptions {
            root: Some("/does/not/exist/and/does/not/matter".to_string()),
            warehouses: None,
            addr: "127.0.0.1:0".to_string(),
            token: None,
            tokens: None,
            blank_token_supplied: false,
            max_body_mb: None,
            rebuild_after_lifts: None,
            authentication_hook: None,
            admission_hook: None,
            events_hook: None,
            resolution_hook: None,
            authentication_cache_secs: None,
            open: false,
        }
    }

    /// The startup half of the fail-open fix: `serve` must refuse before ever binding a
    /// listener or constructing an `AppState` when no auth is configured and `open` is not
    /// set — naming the remedy, not silently starting `Principal::Open`. The auth gate runs
    /// before root resolution (see `serve`'s ordering), so an invalid root does not mask this:
    /// if the gate were bypassed, this would fail with "Error while resolving" instead.
    #[tokio::test]
    async fn serve_refuses_to_start_with_no_auth_and_no_open() {
        let error = serve(bare_serve_options()).await.unwrap_err();

        assert!(error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("--open"), "{}", error);
        // Locks the "absent" phrasing distinctly from the "blank" phrasing
        // `serve_with_an_empty_token_does_not_pass_the_auth_gate` pins below (F2 of PR #124
        // round 2) — no token was supplied at all here, blank or otherwise.
        assert!(error.contains("no --token/token"), "{}", error);
    }

    /// The explicit opt-in must still work: `open: true` with nothing else configured must
    /// pass the auth gate rather than be refused. Distinguished from a refusal by which error
    /// comes back — the deliberately-invalid root's resolution error, not the auth one — since
    /// fully starting the server (binding, serving, shutting down) is out of scope for a fast
    /// unit test.
    #[tokio::test]
    async fn serve_with_open_true_passes_the_auth_gate() {
        let options = ServeOptions { open: true, ..bare_serve_options() };
        let error = serve(options).await.unwrap_err();

        assert!(!error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("Error while resolving"), "{}", error);
    }

    /// The same bypass, via a configured static token instead of `open` — confirms the gate
    /// looks at all three auth sources (token, operator tokens, hook), not just `open`.
    #[tokio::test]
    async fn serve_with_a_token_passes_the_auth_gate_even_without_open() {
        let options = ServeOptions { token: Some("secret".to_string()), ..bare_serve_options() };
        let error = serve(options).await.unwrap_err();

        assert!(!error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("Error while resolving"), "{}", error);
    }

    /// The fail-open finding: `--token ""` (or `token = ""` in the config file) must not count
    /// as a configured token. Before the fix, `options.token.is_some()` was `true` for
    /// `Some("")`, so this bypassed the gate entirely and the server would have started fully
    /// open to any request carrying `Authorization: Bearer ` (empty credential).
    ///
    /// `blank_token_supplied: true` here mirrors what `main.rs::merge_static_token` would have
    /// computed for this scenario — this test constructs `ServeOptions` directly, bypassing
    /// that merge, so it sets the signal by hand. F2 of PR #124 round 2: the refusal must name
    /// this case as "a blank --token/token", not the misdiagnosing "no --token/token" (which
    /// `serve_refuses_to_start_with_no_auth_and_no_open` pins for the genuinely-absent case).
    #[tokio::test]
    async fn serve_with_an_empty_token_does_not_pass_the_auth_gate() {
        let options = ServeOptions {
            token: Some(String::new()),
            blank_token_supplied: true,
            ..bare_serve_options()
        };
        let error = serve(options).await.unwrap_err();

        assert!(error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("a blank --token/token"), "{}", error);
        assert!(!error.contains("no --token/token"), "{}", error);
    }

    /// The whitespace-only sibling (PR #124 round 3, F1): `--token "   "` must be treated exactly
    /// like `--token ""` at this gate too — `blank_token_supplied` is set the same way
    /// `main.rs::merge_static_token` would set it for this scenario. PR #124 round 4, F1: this
    /// test used to assert the same "an empty --token/token" wording as the empty-string case
    /// above, which was true of the code (a single hardcoded clause covered both) but not of the
    /// scenario — an operator who set `token = "   "` was told their token was an empty string.
    /// Now asserts the corrected, case-covering wording instead of pinning that misdiagnosis.
    #[tokio::test]
    async fn serve_with_a_whitespace_only_token_does_not_pass_the_auth_gate() {
        let options = ServeOptions {
            token: Some("   ".to_string()),
            blank_token_supplied: true,
            ..bare_serve_options()
        };
        let error = serve(options).await.unwrap_err();

        assert!(error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("a blank --token/token"), "{}", error);
        assert!(!error.contains("no --token/token"), "{}", error);
    }

    /// The empty-`[operators]`-table case must be named distinctly from "no --tokens/tokens
    /// file" (F4): the operator did pass `--tokens`, the file just defines no entries. Telling
    /// them to pass a flag they already passed sends them looking in the wrong place.
    #[tokio::test]
    async fn serve_with_an_empty_tokens_file_names_that_case_distinctly() {
        let dir = scratch_dir("serve-empty-tokens-file");
        let path = dir.join("tokens.toml");
        std::fs::write(&path, "[operators]\n").unwrap();

        let options = ServeOptions {
            tokens: Some(path.to_str().unwrap().to_string()),
            ..bare_serve_options()
        };
        let error = serve(options).await.unwrap_err();

        assert!(error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("defines no entries"), "{}", error);
        assert!(!error.contains("no --tokens/tokens file"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&dir);
    }

    /// PR #124 round 4, F4: a whitespace-only `authentication_url` used to pass the hook
    /// completeness gate (`is_empty()` only), which then made `auth_configured` true and let the
    /// server start believing authentication was configured — only to 503 every request at
    /// `authenticate_via_hook`, since a blank URL can never be posted to. Fail-closed, so not a
    /// hole, but the wrong refusal point: this must be caught here, at startup, naming the hook,
    /// rather than at the first request with no diagnostic at start time.
    ///
    /// Uses `--warehouses` (an existing, empty scratch folder) rather than `bare_serve_options`'s
    /// deliberately-nonexistent `--root`: the hook-completeness gate runs *after* root/warehouses
    /// resolution (see `serve`'s ordering), so a fake root would fail with "Error while
    /// resolving" before ever reaching the check this test targets.
    ///
    /// PR #124 round 5, F3: because this test (unlike almost every other `serve` test in this
    /// file) points at a real, existing `--warehouses` folder, the blank-hook refusal is the
    /// only thing standing between `serve` and actually binding and entering the axum server
    /// loop — if that refusal ever regressed, `.unwrap_err()` below would never be reached and
    /// this test would hang forever instead of failing, so a regression here would show up in CI
    /// as a timeout, not a red assertion. Verified empirically (PR #124 round 5 review): with
    /// the guard removed, this test hangs rather than failing. `tokio::time::timeout` turns that
    /// hang into a fast, named failure in both directions.
    #[tokio::test]
    async fn serve_refuses_a_whitespace_only_hook_url() {
        let base = scratch_dir("serve-hook-url-whitespace");
        let options = ServeOptions {
            root: None,
            warehouses: Some(base.to_str().unwrap().to_string()),
            authentication_hook: Some(HookEndpoint {
                url: "   ".to_string(),
                secret: "s".to_string(),
            }),
            ..bare_serve_options()
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), serve(options)).await;

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "serve() did not return within 5s — the blank-hook-URL startup refusal has \
                regressed and serve is binding and running the axum server loop instead"
            ))
            .unwrap_err();
        assert!(error.contains("has a blank URL or secret"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// The secret-side sibling: a whitespace-only secret is weaker than a real one but must
    /// still be refused as "not configured", the same rule the URL side now gets (F4 above).
    /// Same hang-shape hazard as `serve_refuses_a_whitespace_only_hook_url` above (F3): a real
    /// `--warehouses` folder means a regressed guard hangs the test instead of failing it.
    #[tokio::test]
    async fn serve_refuses_a_whitespace_only_hook_secret() {
        let base = scratch_dir("serve-hook-secret-whitespace");
        let options = ServeOptions {
            root: None,
            warehouses: Some(base.to_str().unwrap().to_string()),
            authentication_hook: Some(HookEndpoint {
                url: "https://provider.example/hooks/auth".to_string(),
                secret: "   ".to_string(),
            }),
            ..bare_serve_options()
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), serve(options)).await;

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "serve() did not return within 5s — the blank-hook-secret startup refusal has \
                regressed and serve is binding and running the axum server loop instead"
            ))
            .unwrap_err();
        assert!(error.contains("has a blank URL or secret"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    /// PR #124 round 5, F2: a non-blank but non-absolute hook URL (scheme omitted) passes the
    /// blank check above yet produces exactly the failure that check exists to catch at
    /// startup — `reqwest::Url::parse` (called both here and, transitively, by `post_hook` via
    /// `state.http.post`) rejects a schemeless string as a relative URL without a base. Same
    /// real-`--warehouses`-folder hang hazard as the two tests above (F3): wrapped in a timeout
    /// so a regressed guard fails fast instead of hanging.
    #[tokio::test]
    async fn serve_refuses_a_non_absolute_hook_url() {
        let base = scratch_dir("serve-hook-url-non-absolute");
        let options = ServeOptions {
            root: None,
            warehouses: Some(base.to_str().unwrap().to_string()),
            authentication_hook: Some(HookEndpoint {
                url: "provider.example/hooks/auth".to_string(),
                secret: "s".to_string(),
            }),
            ..bare_serve_options()
        };
        let outcome = tokio::time::timeout(std::time::Duration::from_secs(5), serve(options)).await;

        let error = outcome
            .unwrap_or_else(|_| panic!(
                "serve() did not return within 5s — the non-absolute-hook-URL startup refusal \
                has regressed and serve is binding and running the axum server loop instead"
            ))
            .unwrap_err();
        assert!(error.contains("is not a valid absolute URL"), "{}", error);
        assert!(error.contains("provider.example/hooks/auth"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
    }

    // ---------------------------------------------------------------------------------
    // startup_bind_warning
    // ---------------------------------------------------------------------------------

    #[test]
    fn a_loopback_bind_never_warns_regardless_of_auth() {
        let v4: std::net::SocketAddr = "127.0.0.1:9418".parse().unwrap();
        let v6: std::net::SocketAddr = "[::1]:9418".parse().unwrap();

        assert!(startup_bind_warning(&v4, true).is_none());
        assert!(startup_bind_warning(&v4, false).is_none());
        assert!(startup_bind_warning(&v6, true).is_none());
        assert!(startup_bind_warning(&v6, false).is_none());
    }

    #[test]
    fn a_non_loopback_bind_warns_whether_or_not_a_token_is_configured() {
        let bound: std::net::SocketAddr = "0.0.0.0:9418".parse().unwrap();

        let with_auth = startup_bind_warning(&bound, true).expect("a warning with auth configured");
        assert!(with_auth.contains("plaintext"), "{}", with_auth);

        let without_auth = startup_bind_warning(&bound, false).expect("a warning with no auth");
        assert!(without_auth.contains("Principal::Open"), "{}", without_auth);
    }

    // ---------------------------------------------------------------------------------
    // body_limit_bytes (F5 of PR #124 round 2): the magnitude half of the overflow class
    // ---------------------------------------------------------------------------------

    #[test]
    fn body_limit_bytes_converts_an_ordinary_value() {
        assert_eq!(body_limit_bytes(Some(4096)).unwrap(), 4096 * 1024 * 1024);
    }

    #[test]
    fn body_limit_bytes_defaults_to_the_object_ceiling_when_unset() {
        assert_eq!(body_limit_bytes(None).unwrap(), object_utils::MAX_OBJECT_BYTES);
    }

    /// The exact boundary the finding names: `2^44` MiB is the smallest `max_body_mb` whose
    /// byte count overflows a `u64` (`2^44 * 2^20 == 2^64`). `(mb as usize) * 1024 * 1024` — the
    /// code this replaces — silently wraps to exactly `0` here on a 64-bit target (rejecting
    /// every request body) instead of refusing to start.
    #[test]
    fn body_limit_bytes_refuses_an_overflowing_value_naming_the_key() {
        let error = body_limit_bytes(Some(1u64 << 44)).unwrap_err();
        assert!(error.contains("max_body_mb"), "{}", error);
    }

    #[test]
    fn body_limit_bytes_accepts_the_largest_value_that_does_not_overflow() {
        // The largest `mb` whose `* 1024 * 1024` still fits in a `u64` (one MiB short of the
        // overflowing boundary above) must still convert cleanly, not be caught by an
        // off-by-one in the overflow check.
        let mb = (1u64 << 44) - 1;
        assert_eq!(body_limit_bytes(Some(mb)).unwrap(), (mb as usize) * 1024 * 1024);
    }

    /// PR #124 round 4, F6: `max_body_mb`'s sign check only ever rejected negative values, so
    /// `Some(0)` sailed through, round-tripped losslessly through the cast, and reached
    /// `DefaultBodyLimit::max(0)` — the server starts normally and then 413s every request
    /// carrying a body, a 1-byte lift included. Non-negative but nonsensical, the same class
    /// `authentication_cache_secs`'s ceiling closes for that field.
    #[test]
    fn body_limit_bytes_refuses_zero_naming_the_key() {
        let error = body_limit_bytes(Some(0)).unwrap_err();
        assert!(error.contains("max_body_mb"), "{}", error);
    }

    #[test]
    fn body_limit_bytes_accepts_the_smallest_nonzero_value() {
        // The boundary right above the refusal above: `1` MiB must still convert cleanly, not be
        // caught by an off-by-one in the zero check.
        assert_eq!(body_limit_bytes(Some(1)).unwrap(), 1024 * 1024);
    }

    // ---------------------------------------------------------------------------------
    // authentication_cache_ttl (PR #124 round 5, F4): the ceiling re-enforced at the
    // `ServeOptions` boundary, not just in `main.rs`'s TOML reader
    // ---------------------------------------------------------------------------------

    #[test]
    fn authentication_cache_ttl_defaults_to_60s_when_unset() {
        assert_eq!(authentication_cache_ttl(None).unwrap(), std::time::Duration::from_secs(60));
    }

    #[test]
    fn authentication_cache_ttl_accepts_the_ceiling() {
        assert_eq!(
            authentication_cache_ttl(Some(MAX_AUTHENTICATION_CACHE_SECS)).unwrap(),
            std::time::Duration::from_secs(MAX_AUTHENTICATION_CACHE_SECS)
        );
    }

    #[test]
    fn authentication_cache_ttl_refuses_a_value_over_the_ceiling() {
        let error = authentication_cache_ttl(Some(MAX_AUTHENTICATION_CACHE_SECS + 1)).unwrap_err();
        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("ceiling"), "{}", error);
    }

    /// The finding itself: a `ServeOptions` built directly (not through `main.rs`'s TOML
    /// reader — the only path `bounded_authentication_cache_secs` guards) with an
    /// over-the-ceiling `authentication_cache_secs` must still be refused, by `serve` itself.
    /// Before this fix, this construction sailed straight past every check and reached
    /// `Duration::from_secs` uncontested, making SERVER.md's and HOOK_PROTOCOL.md's "refuses to
    /// start over 24h" claim false for any caller other than the config-file path. Uses
    /// `bare_serve_options`'s deliberately-nonexistent `--root`: this check runs before root
    /// resolution, so no real warehouse folder — and no hang risk — is needed to observe it.
    #[tokio::test]
    async fn serve_refuses_an_authentication_cache_secs_over_the_ceiling() {
        let options = ServeOptions {
            authentication_cache_secs: Some(MAX_AUTHENTICATION_CACHE_SECS + 1),
            ..bare_serve_options()
        };
        let error = serve(options).await.unwrap_err();

        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("ceiling"), "{}", error);
        assert!(!error.contains("No authentication is configured"), "{}", error);
    }
}
