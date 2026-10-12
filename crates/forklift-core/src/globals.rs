use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

pub const FOLDER_NAME_FORKLIFT_ROOT: &str = ".forklift";
pub const FOLDER_NAME_OBJECTS_ROOT: &str =  "objects";
pub const FOLDER_NAME_INVENTORY_ROOT: &str = "inventory";

/// The folder under the forklift root that holds the commit-graph shards (design note §B):
/// a derived, self-healing cache of the parcel DAG. Warehouse-global like `objects`, not
/// bay-local — ancestry is a property of the shared history, not of a bay's staging.
pub const FOLDER_NAME_GRAPH_ROOT: &str = "graph";

/// The folder under the forklift root that holds each named bay's local state (§7.5).
pub const FOLDER_NAME_BAYS_ROOT: &str = "bays";
pub const BYTE_NEW_LINE: u8 = 10;
pub const BYTE_SPACE: u8 = 32;
pub const BYTE_END_OF_TEXT: u8 = 3;
pub const BYTE_NULL: u8 = 0;

thread_local! {
    /// The warehouse root for storage-path resolution on this thread, when one is
    /// entered (see `StorageRootScope`).
    static STORAGE_ROOT: RefCell<Option<PathBuf>> = const { RefCell::new(None) };

    /// Bumped whenever this thread's storage-root *scope* changes (a `StorageRootScope`
    /// enter or exit). Half of the [`scope_fingerprint`] that lets storage-path helpers
    /// memoize the resolved root instead of rebuilding it on every object read.
    static SCOPE_GENERATION: Cell<u64> = const { Cell::new(0) };
}

/// Bumped whenever the process-global bay context changes ([`set_bay_context`]) — the other
/// half of [`scope_fingerprint`]. Global (not thread-local) so a bay change invalidates every
/// thread's memoized root, matching the bay context's own process-global scope.
static BAY_GENERATION: AtomicU64 = AtomicU64::new(0);

/// Note this thread just changed its storage-root scope, so any memoized root is stale.
fn bump_scope_generation() {
    SCOPE_GENERATION.with(|generation| generation.set(generation.get().wrapping_add(1)));
}

/// A fingerprint of everything [`forklift_root`] depends on — the thread's scope generation and
/// the global bay generation. It changes iff the resolved root could have changed, so a helper
/// that caches `(fingerprint, root)` can return the cached root while the fingerprint holds and
/// safely recompute the moment it does not. Cheap: a thread-local read and one relaxed atomic
/// load, versus the `RwLock` read and path allocations `forklift_root` does.
pub fn scope_fingerprint() -> (u64, u64) {
    (
        SCOPE_GENERATION.with(|generation| generation.get()),
        BAY_GENERATION.load(Ordering::Relaxed),
    )
}

/// An entered storage-root scope: while it is alive, `forklift_root()` resolves under
/// the given warehouse root on this thread. The previous scope is restored on drop.
///
/// The scope is thread-local and strictly synchronous: never hold a guard across an
/// `.await` (the task may resume on another thread), and never assume spawned tasks
/// (`TaskExecutor` workers included) inherit it. The CLI does not need scopes at all —
/// it enters the warehouse by changing the working directory once. The server head
/// enters a scope inside each blocking storage closure, which is what lets one process
/// serve more than one warehouse root.
pub struct StorageRootScope {
    previous: Option<PathBuf>,
}

impl StorageRootScope {
    /// Enter a storage-root scope for `root` on this thread.
    pub fn enter(root: &Path) -> StorageRootScope {
        let previous = STORAGE_ROOT.with(|cell| cell.replace(Some(normalize_root(root))));
        bump_scope_generation();

        StorageRootScope { previous }
    }
}

/// Strip a `\\?\` verbatim prefix from `path`, yielding the ordinary form. A no-op on
/// other platforms, and on a path that is not verbatim.
///
/// **The one copy of this rule.** `std::fs::canonicalize` returns *verbatim* paths on
/// Windows (`\\?\C:\…`, or `\\?\UNC\server\share`), and three unrelated things in this
/// repo break on them, each for its own reason:
///
/// * [`normalize_root`] — verbatim paths take their separators **literally**, so a forward
///   slash becomes a filename character and the storage layer's `\\?\C:\…\.forklift/objects`
///   is rejected as invalid (os error 123).
/// * `commands::alias`'s shim — `cmd.exe` cannot execute a `\\?\`-prefixed path at all
///   ("The system cannot find the path specified").
/// * `config_utils`'s not-valid refusal — the one message whose whole job is naming a file
///   for a human to open in an editor.
///
/// Stripping unconditionally is deliberate (not e.g. via the `dunce` crate, which keeps the
/// verbatim form for long paths): the forward-slash model above is incompatible with *any*
/// verbatim root, and the other two callers want the human/shell-legible spelling regardless
/// of length. This was a private copy here plus a second one in `commands::alias`, the two
/// kept in step by a comment asking the next editor to do so; it is one function now so that
/// there is nothing to keep in step.
#[cfg(windows)]
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();

    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        PathBuf::from(format!(r"\\{}", rest))
    } else if let Some(rest) = text.strip_prefix(r"\\?\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

#[cfg(not(windows))]
pub fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    path.to_path_buf()
}

/// Normalize a storage root to a form the storage layer's path model can build on — see
/// [`strip_verbatim_prefix`], which owns the rule and the reason.
fn normalize_root(root: &Path) -> PathBuf {
    strip_verbatim_prefix(root)
}

impl Drop for StorageRootScope {
    fn drop(&mut self) {
        let previous = self.previous.take();
        STORAGE_ROOT.with(|cell| *cell.borrow_mut() = previous);
        bump_scope_generation();
    }
}

/// Capture this thread's entered storage-root scope, so a worker thread spawned to share
/// the work can adopt the *same* warehouse root. Storage-root scopes are thread-local and
/// are **not** inherited by spawned threads (see [`StorageRootScope`]), so a fan-out that
/// reads objects must re-enter this on each worker:
///
/// ```ignore
/// let scope_root = globals::current_scope_root();
/// std::thread::scope(|scope| {
///     scope.spawn(|| {
///         let _scope = scope_root.as_deref().map(StorageRootScope::enter);
///         // object reads here now resolve under the caller's warehouse root
///     });
/// });
/// ```
///
/// `None` means resolution is by working directory (the CLI) or by the process-global bay
/// context — both of which a spawned thread already sees — so the worker adopts nothing.
pub fn current_scope_root() -> Option<PathBuf> {
    STORAGE_ROOT.with(|cell| cell.borrow().clone())
}

/// A bay context (§7.5): the shared `.forklift` a bay borrows, and the bay's name. A bay's
/// working directory is *not* the warehouse, so unlike the main tree its cwd cannot locate
/// the shared `.forklift` — this records it. Process-global (not thread-local) on purpose:
/// the CLI's parallel walks (`TaskExecutor` workers) run on threads that do not inherit
/// thread-locals, and they must see the same shared root and bay.
struct BayContext {
    shared_forklift: PathBuf,
    name: String,
}

static BAY_CONTEXT: RwLock<Option<BayContext>> = RwLock::new(None);

/// Enter a bay for the rest of this process (the CLI sets this once, after discovery, when
/// it was invoked inside a bay). `shared_forklift` is the warehouse's `.forklift` folder
/// the bay shares; `name` is the bay.
pub fn set_bay_context(shared_forklift: PathBuf, name: String) {
    *BAY_CONTEXT.write().expect("the bay context lock is poisoned") =
        Some(BayContext { shared_forklift: normalize_root(&shared_forklift), name });
    BAY_GENERATION.fetch_add(1, Ordering::Relaxed);
}

/// The active bay's name (`None` when running in the main working tree).
pub fn active_bay() -> Option<String> {
    BAY_CONTEXT.read().expect("the bay context lock is poisoned")
        .as_ref().map(|context| context.name.clone())
}

/// The `.forklift` folder of the active warehouse. Shared across bays: objects, refs
/// (pallets/meta), trust and configuration all live here.
///
/// Resolution order: a bay's shared `.forklift` (its cwd is the bay, not the warehouse);
/// otherwise the entered storage-root scope (the server); otherwise relative to the
/// working directory (the main CLI, which changes into the warehouse root at startup).
///
/// Every *shared* storage path must derive from this function, never from
/// `FOLDER_NAME_FORKLIFT_ROOT` directly — that keeps the storage layer root-relocatable.
/// Bay-*local* paths derive from [`bay_root`] instead.
pub fn forklift_root() -> PathBuf {
    if let Some(context) = BAY_CONTEXT.read().expect("the bay context lock is poisoned").as_ref() {
        return context.shared_forklift.clone();
    }

    STORAGE_ROOT.with(|cell| match cell.borrow().as_ref() {
        Some(root) => root.join(FOLDER_NAME_FORKLIFT_ROOT),
        None => PathBuf::from(FOLDER_NAME_FORKLIFT_ROOT),
    })
}

/// The root for *bay-local* storage — the inventory, current pallet, lock, parked and
/// consolidation state — so bays share one object store and one set of refs while each
/// keeps its own working state. The main working tree keeps this state directly in
/// `.forklift/`; a named bay keeps it under `.forklift/bays/<name>/`.
pub fn bay_root() -> PathBuf {
    match active_bay() {
        None => forklift_root(),
        Some(name) => forklift_root().join(FOLDER_NAME_BAYS_ROOT).join(name),
    }
}

/// The root folder of the active warehouse itself (see [`forklift_root`]) — for the
/// few root-level files that live next to `.forklift`, like the ignore file.
pub fn warehouse_root() -> PathBuf {
    STORAGE_ROOT.with(|cell| match cell.borrow().as_ref() {
        Some(root) => root.clone(),
        None => PathBuf::from("."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_root_is_relative_without_a_scope() {
        assert_eq!(forklift_root(), PathBuf::from(".forklift"));
    }

    /// The leg of [`strip_verbatim_prefix`] that holds on every platform: a path that is not
    /// verbatim comes back untouched. Worth pinning on its own now that three unrelated
    /// callers — the storage root, the Windows shim and `config_utils`'s hand-edit refusal —
    /// pass ordinary paths through it on the way to something a user or a shell reads.
    #[test]
    fn an_ordinary_path_passes_through_unchanged() {
        for path in ["/warehouses/alpha", "relative/warehouse.toml", "."] {
            assert_eq!(strip_verbatim_prefix(Path::new(path)), PathBuf::from(path));
        }
    }

    /// The stripping itself, which only Windows can exercise — `canonicalize` returns the
    /// verbatim form there and nowhere else, and the function is deliberately a no-op
    /// elsewhere, so a cross-platform version of this test would assert the opposite thing on
    /// each platform. Runs on the Windows CI job; the `\\?\UNC\` case is included because it
    /// is the one that needs a different rewrite rather than a plain prefix removal.
    #[cfg(windows)]
    #[test]
    fn a_verbatim_path_loses_its_prefix() {
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\C:\warehouses\alpha")),
            PathBuf::from(r"C:\warehouses\alpha")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"\\?\UNC\server\share\alpha")),
            PathBuf::from(r"\\server\share\alpha")
        );
        assert_eq!(
            strip_verbatim_prefix(Path::new(r"C:\warehouses\alpha")),
            PathBuf::from(r"C:\warehouses\alpha")
        );
    }

    #[test]
    fn scopes_apply_nest_and_restore() {
        let _outer = StorageRootScope::enter(Path::new("/warehouses/alpha"));
        assert_eq!(forklift_root(), PathBuf::from("/warehouses/alpha/.forklift"));

        {
            let _inner = StorageRootScope::enter(Path::new("/warehouses/beta"));
            assert_eq!(forklift_root(), PathBuf::from("/warehouses/beta/.forklift"));
        }

        assert_eq!(forklift_root(), PathBuf::from("/warehouses/alpha/.forklift"));
    }

    #[test]
    fn scopes_are_thread_local() {
        let _scope = StorageRootScope::enter(Path::new("/warehouses/alpha"));

        let other = std::thread::spawn(|| forklift_root()).join().unwrap();

        assert_eq!(other, PathBuf::from(".forklift"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_verbatim_prefix_is_stripped_on_entry() {
        // A verbatim root would make the forward-slash subpaths the storage layer
        // appends (`.forklift/objects`) invalid; the scope must strip the prefix so
        // `forklift_root()` is an ordinary path Windows accepts `/` in.
        let _drive = StorageRootScope::enter(Path::new(r"\\?\C:\wh"));
        assert_eq!(forklift_root(), PathBuf::from(r"C:\wh").join(FOLDER_NAME_FORKLIFT_ROOT));

        let _unc = StorageRootScope::enter(Path::new(r"\\?\UNC\server\share\wh"));
        assert_eq!(forklift_root(), PathBuf::from(r"\\server\share\wh").join(FOLDER_NAME_FORKLIFT_ROOT));
    }
}
