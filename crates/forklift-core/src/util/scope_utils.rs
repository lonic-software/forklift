//! Task-scoped sparse workspaces (DESIGN.html §7.6) — the local scope record and the
//! three-valued classifier every scope-aware walk branches on. Both layers are live: a bay's
//! checkout scope, and the warehouse fetch scope a sparse clone records (empty = the
//! store holds everything).
//!
//! A scoped bay materializes and operates on only chosen path subtrees of the user
//! pallet's working tree. Scope is **local only, never tracked** (it is a property of
//! *this* checkout, not the project — tracking it would push it onto collaborators, git
//! sparse-checkout's leaky middle ground). It lives in two path-prefix files:
//!
//! * the **bay checkout scope** (`<bay_root>/scope`, bay-local) — what this bay
//!   materializes and stacks; always a subset (⊆) of the fetch scope, and
//! * the **warehouse fetch scope** (`config/fetch-scope`, shared) — what the warehouse
//!   has fetched at all. A full clone leaves it unset (= full) and only the bay scope
//!   restricts behavior; a sparse (`clone --only`) warehouse records the fetched prefixes
//!   here, and `scope fetch` widens them.
//!
//! The classifier is **three-valued, not boolean**: a boolean
//! `in_scope` conflates two situations a walk must treat differently — a *spine* directory
//! (an ancestor of an in-scope path, walked but with its out-of-scope siblings copied by
//! hash) and a *fully in-scope* directory (descended normally). See [`ScopeClass`].
//!
//! **Meta pallets (`@office` and `.forklift/meta/*`) are never scoped**: this
//! module classifies *user-pallet content paths* only; meta-pallet fetch/audit/materialize
//! code keeps calling the existing, unscoped functions unchanged.

use std::path::{Path, PathBuf};
use crate::globals::{self, bay_root};
use crate::error::{CoreError, RefusalCode};
use crate::util::record_utils::{self, RecordError};
use crate::util::warehouse_utils::{self, WarehouseMode};

/// The bay-local file that records a bay's checkout scope (under `<bay_root>/`).
const FILE_NAME_BAY_SCOPE: &str = "scope";

/// The shared configuration folder (matches `config_utils`' own folder name) and the
/// warehouse fetch-scope file inside it.
const FOLDER_NAME_CONFIG: &str = "config";
const FILE_NAME_FETCH_SCOPE: &str = "fetch-scope";

/// The [`record_utils`] kind and version of every non-full scope file. Format 2 put the root
/// ignore file in every scope, refusing the unversioned files before it; format 3 escapes each
/// prefix, so one may hold a line break. A file in either earlier format is refused on read.
/// The header opens with "/", which no stored prefix can (prefixes are trimmed of it), so an
/// unversioned file can never be mistaken for a versioned one.
const SCOPE_RECORD: &str = "scope";
const SCOPE_VERSION: u32 = 3;

/// The stable machine-branchable refusal codes this feature contributes to the §7.4 error
/// taxonomy, as their code strings — kept as re-exports of the typed [`RefusalCode`] (the source
/// of truth lives in [`crate::error`]) so existing references keep resolving. A scope refusal is a
/// typed [`CoreError::Refusal`] the head classifies by [`RefusalCode`]; see [`refusal`].
pub const CODE_OUT_OF_SCOPE: &str = RefusalCode::OutOfScope.as_str();
pub const CODE_OUT_OF_SCOPE_CONFLICT: &str = RefusalCode::OutOfScopeConflict.as_str();
pub const CODE_SCOPE_PATH_TYPE_CHANGED: &str = RefusalCode::ScopePathTypeChanged.as_str();
pub const CODE_SPARSE_WORKSPACE: &str = RefusalCode::SparseWorkspace.as_str();
pub const CODE_NON_ORIGIN_REMOTE: &str = RefusalCode::NonOriginRemote.as_str();
pub const CODE_NARROW_UNCLEAN: &str = RefusalCode::NarrowUnclean.as_str();
pub const CODE_SCOPE_PRUNE_BLOCKED: &str = RefusalCode::ScopePruneBlocked.as_str();

/// Large-file chunk transport (§9.4b): a chunked file cannot ride a bundle, and lifting one to a
/// remote that does not advertise chunking support would let its `gc` collect the chunks. Its own
/// refusal code, re-exported from the typed [`RefusalCode`].
pub const CODE_CHUNKED_TRANSPORT_UNSUPPORTED: &str = RefusalCode::ChunkedTransportUnsupported.as_str();

/// A **grandfathered** object above the whole-object ceiling (authored before the ceiling
/// existed) is readable and checkout-able locally forever —
/// the ceiling gates writes and imports only — but nothing accepts it on the wire, and there is no
/// migration that would preserve its signed identity (a blob's hash is pinned inside a signed
/// tree; re-chunking it would mint a different hash and so a different, unsigned tree). Refusing
/// client-side, before anything is written into a bundle or sent over a lift, is the honest
/// failure at the source. Re-exported from the typed [`RefusalCode`].
pub const CODE_OVERSIZED_TRANSPORT_UNSUPPORTED: &str = RefusalCode::OversizedTransportUnsupported.as_str();

/// A lift whose commit needs more than one paginated batch (§9.4b Stage 3, W3) — more than
/// `MAX_MISSING_BATCH` distinct staged objects — requires a remote that understands the additive
/// `more` field, which shipped *with* chunking support, not before it: a pre-chunking staging head
/// has no such field and ignores an unrecognized one (`#[serde(default)]` reads it as `false`), so
/// it sweeps its staging prefix after the very first batch, silently discarding whatever a later
/// batch still needed staged. This is not specific to any chunked file — a plain lift touching
/// enough distinct small tracked files hits the identical cap — so the check (and this code) is
/// keyed on the staged object count alone, checked right after negotiation and before a single
/// byte is uploaded (a naive commit-time check would waste the whole upload first). Re-exported
/// from the typed [`RefusalCode`].
pub const CODE_COMMIT_PAGINATION_UNSUPPORTED: &str = RefusalCode::CommitPaginationUnsupported.as_str();

/// The bridge-shim framing that lets a still-`Result<_, String>` segment carry a refusal across
/// the migration frontier — re-exported from [`crate::error`], which owns the encoding now. See
/// the [`crate::error`] module docs.
pub use crate::error::{REFUSAL_PREFIX, REFUSAL_FIELD_SEPARATOR};

/// Where a user-pallet content path sits relative to the checkout scope (design
/// §3.1). Every scope-aware walk branches on the three cases, never on a single predicate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeClass {
    /// At or under a configured in-scope prefix, or the root ignore file, which every scope
    /// holds: fully materialized — descend, materialize, verify and merge exactly as a full
    /// workspace would. Nothing below needs re-classifying. Not every in-scope path is under a
    /// directory prefix — the ignore file is a file at a spine level — so a walk that refuses a
    /// type change asks [`PathScope::requires_directory`], never this variant.
    InScope,

    /// A strict ancestor of at least one in-scope prefix: must be walked (something below it
    /// is in scope), but its *other* entries at this level (files, and subtrees not on the
    /// path to any in-scope leaf) are [`ScopeClass::OutOfScope`] and must be carried forward
    /// by hash, never descended.
    Spine,

    /// Neither: sealed by the hash already committed in the parent spine tree object. Never
    /// loaded, never descended, never materialized.
    OutOfScope,
}

/// A set of in-scope path prefixes, used as either scope: a checkout's (what a bay or the main
/// tree writes to disk) or the warehouse's fetch scope (what its store holds).
///
/// An empty set means **full scope** — no restriction, so [`classify`](Self::classify)
/// returns [`ScopeClass::InScope`] for every path, and every scope-aware code path collapses to
/// the unscoped behavior. A full scope is recorded as the empty file; whether a scope file may be
/// absent is decided by whose it is ([`read_fetch_scope`], [`read_main_tree_scope`],
/// [`read_bay_scope`]), never by reading an absence as full.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathScope {
    /// Normalized in-scope prefixes: warehouse path keys (no leading/trailing slash, `/`
    /// separators), sorted and de-duplicated, none empty. Empty vec = full scope.
    prefixes: Vec<String>,
}

impl PathScope {
    /// The full (unrestricted) scope — every path is in scope.
    pub fn full() -> PathScope {
        PathScope { prefixes: Vec::new() }
    }

    /// Build a scope from raw in-scope prefixes, normalizing them: an empty (root) prefix
    /// means "materialize everything", so it collapses the whole scope to full; the rest are
    /// trimmed of `/`, de-duplicated and sorted for a canonical on-disk form.
    pub fn from_prefixes<I, S>(prefixes: I) -> PathScope
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut normalized: Vec<String> = Vec::new();

        for prefix in prefixes {
            let prefix = prefix.into();
            let trimmed = prefix.trim_matches('/').to_string();

            // A root prefix means the whole tree — that is full scope, so no restriction.
            if trimmed.is_empty() {
                return PathScope::full();
            }

            normalized.push(trimmed);
        }

        normalized.sort();
        normalized.dedup();

        PathScope { prefixes: normalized }
    }

    /// Whether this scope is full (unrestricted). A full scope makes every scope-aware walk
    /// a no-op that behaves exactly as an unscoped workspace does.
    pub fn is_full(&self) -> bool {
        self.prefixes.is_empty()
    }

    /// The in-scope prefixes (empty when full).
    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }

    /// The paths every non-full scope holds in scope without declaring them as prefixes: the
    /// root ignore file. Empty for the full scope, which holds everything anyway.
    pub fn implicit_members(&self) -> &'static [&'static str] {
        if self.prefixes.is_empty() {
            &[]
        } else {
            &[crate::util::file_utils::FILENAME_IGNORE]
        }
    }

    /// Classify a file entry at a spine level, for a walker that carries out-of-scope files by
    /// hash: `Ok(true)` when it is out of scope (sealed), `Ok(false)` when it is in scope (the
    /// root ignore file — handled like any in-scope file), and the type-changed refusal when the
    /// scope expects a directory there.
    ///
    /// # Arguments
    /// * `path` - The file entry's warehouse path key.
    pub fn spine_file_is_sealed(&self, path: &str) -> Result<bool, CoreError> {
        if self.requires_directory(path) {
            return Err(type_changed_refusal(path));
        }

        Ok(self.classify(path) == ScopeClass::OutOfScope)
    }

    /// Classify a user-pallet content path against this scope (design §3.1). See
    /// [`ScopeClass`] for the three cases.
    ///
    /// # Arguments
    /// * `path` - A warehouse path key (the empty string is the root).
    pub fn classify(&self, path: &str) -> ScopeClass {
        if self.prefixes.is_empty() {
            return ScopeClass::InScope;
        }

        // In scope: at or under some in-scope prefix, or an implicit member (the root ignore file,
        // so a sparse checkout always holds the rules that apply to it). A directory of that name
        // is in scope whole, as on a full warehouse, where reading the rules refuses it.
        // `requires_directory` and `subset_of` reason over the declared prefixes only.
        for prefix in self.prefixes.iter().map(String::as_str).chain(self.implicit_members().iter().copied()) {
            if is_at_or_under(path, prefix) {
                return ScopeClass::InScope;
            }
        }

        // Spine: a strict ancestor of some in-scope prefix (the root is an ancestor of every
        // non-root prefix, so a scoped workspace's root is always spine).
        for prefix in &self.prefixes {
            if is_strictly_under(prefix, path) {
                return ScopeClass::Spine;
            }
        }

        ScopeClass::OutOfScope
    }

    /// Whether the scope requires `path` to be a directory for it to stay coherent: `path` is
    /// a spine ancestor of an in-scope prefix, or is itself an in-scope prefix (which was
    /// declared as a directory). A tree that has a *file* at such a path is a type change the
    /// sparse scope cannot reason about (§3.1) — the caller refuses with `scope_path_type_changed`.
    pub fn requires_directory(&self, path: &str) -> bool {
        self.classify(path) == ScopeClass::Spine || self.prefixes.iter().any(|prefix| prefix == path)
    }

    /// Whether this scope is a subset (⊆) of `other` — every path this scope materializes is
    /// in scope for `other`. The bay checkout scope must always be ⊆ the warehouse
    /// fetch scope (design §3.1): a bay can never materialize what the warehouse never fetched.
    pub fn subset_of(&self, other: &PathScope) -> bool {
        if other.is_full() {
            return true;
        }

        if self.is_full() {
            // A full bay is not a subset of a restricted fetch scope.
            return false;
        }

        self.prefixes.iter().all(|prefix| other.classify(prefix) == ScopeClass::InScope)
    }

    /// Serialize the scope to its on-disk form, a [`record_utils`] file of kind [`SCOPE_RECORD`]:
    /// one prefix per line, escaped, so a prefix may hold a line break or edge whitespace. A
    /// full scope serializes to the empty string.
    fn to_bytes(&self) -> Vec<u8> {
        if self.prefixes.is_empty() {
            return Vec::new();
        }

        record_utils::to_bytes(SCOPE_RECORD, SCOPE_VERSION, &self.prefixes)
    }

    /// Parse a scope from its on-disk form; an error is phrased to follow "The scope file
    /// \"<path>\"". An empty file is the full scope. Anything else must be a [`record_utils`]
    /// file at [`SCOPE_VERSION`] naming at least one prefix: a file without the header was written
    /// under the older meaning of a scope, which left the root ignore file out, and reading it
    /// under today's meaning would report that file as removed and commit the removal — so it is
    /// refused, as is a header naming a format this build does not know, and a header with no
    /// prefix after it, which no write produces and which would otherwise widen a sparse scope to
    /// full.
    ///
    /// # Arguments
    /// * `bytes`  - The file's contents.
    /// * `remedy` - The way out for a file in the older format, which depends on whose file it is.
    fn from_bytes(bytes: &[u8], remedy: &str) -> Result<PathScope, String> {
        if bytes.is_empty() {
            return Ok(PathScope::full());
        }

        let lines = record_utils::from_bytes(bytes, SCOPE_RECORD, SCOPE_VERSION).map_err(|e| match e {
            RecordError::Unversioned => format!(
                "was written by an older version of Forklift, whose sparse checkouts left out \
                the root ignore file, so it cannot be read safely. {}",
                remedy
            ),
            RecordError::OtherVersion(ref header) => {
                let older = header.rsplit(' ').next()
                    .and_then(|version| version.trim().parse::<u32>().ok())
                    .is_some_and(|version| version < SCOPE_VERSION);
                if older {
                    format!("{} An older version of Forklift wrote it. {}", e.describe(), remedy)
                } else {
                    format!("{} Use the Forklift that wrote it.", e.describe())
                }
            }
            _ => e.describe(),
        })?;

        let scope = PathScope::from_prefixes(lines);

        if scope.is_full() {
            return Err(format!(
                "has a format header but no prefix narrower than the warehouse root, so it is not \
                a scope this version of Forklift writes. {}",
                remedy
            ));
        }

        Ok(scope)
    }
}

/// Whether `path` is `prefix` itself or lies under it — a pure path test, unlike
/// [`PathScope::classify`], which also holds the root ignore file in scope.
pub fn is_at_or_under(path: &str, prefix: &str) -> bool {
    path == prefix || is_strictly_under(path, prefix)
}

/// Whether `child` is a strict descendant of the `ancestor` directory (not equal to it).
/// The empty ancestor is the root, under which every non-root path lives.
pub fn is_strictly_under(child: &str, ancestor: &str) -> bool {
    if ancestor.is_empty() {
        return !child.is_empty();
    }

    child.len() > ancestor.len()
        && child.as_bytes()[ancestor.len()] == b'/'
        && child.starts_with(ancestor)
}

/// The path of the active bay's materialization-scope file.
pub fn bay_scope_path() -> PathBuf {
    bay_root().join(FILE_NAME_BAY_SCOPE)
}

/// The path of the shared warehouse fetch-scope file.
pub fn fetch_scope_path() -> PathBuf {
    globals::forklift_root().join(FOLDER_NAME_CONFIG).join(FILE_NAME_FETCH_SCOPE)
}

/// Read a scope file, with an absent file as `None`: what an absence means depends on whose scope
/// it is and on the warehouse's mode, so each reader decides.
///
/// # Arguments
/// * `path`   - The scope file.
/// * `remedy` - The way out when the file is in the older, unversioned format.
fn read_scope_file_if_present(path: &Path, remedy: &str) -> Result<Option<PathScope>, String> {
    match std::fs::read(path) {
        Ok(bytes) => PathScope::from_bytes(&bytes, remedy).map(Some).map_err(|reason| {
            format!("The scope file \"{}\" {}", path.to_string_lossy(), reason)
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("Error while reading the scope file \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// Whether a scope file is present, without reading it: a scope file found where none belongs is
/// refused whatever it holds.
fn scope_file_is_present(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Error while reading the scope file \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// Write a scope file atomically (its parent folder must already exist).
fn write_scope_file(path: &Path, scope: &PathScope) -> Result<(), String> {
    crate::util::file_utils::write_file_atomically(path, &scope.to_bytes())
}

/// The active checkout's scope: the main tree's ([`read_main_tree_scope`]) or the
/// active bay's ([`read_bay_scope`]), checked the same way.
pub fn current_scope() -> Result<PathScope, String> {
    match globals::active_bay() {
        Some(_) => read_bay_scope_at(&bay_scope_path()),
        None => read_main_tree_scope(),
    }
}

/// Whether the active bay is scoped (has a non-full checkout scope). A quick gate for
/// the verbs that behave differently — or refuse — in a scoped workspace.
pub fn is_scoped() -> Result<bool, String> {
    Ok(!current_scope()?.is_full())
}

/// The warehouse fetch scope: full in a full or cache warehouse, and the fetched prefixes in a
/// sparse one. The file is read against the mode the warehouse marker records, never on its own:
/// a full or cache warehouse must hold no fetch-scope file and a sparse one a non-full one, so
/// deleting the file cannot make a sparse warehouse read as full.
pub fn read_fetch_scope() -> Result<PathScope, String> {
    Ok(read_fetch_state()?.1)
}

/// The warehouse's mode and its fetch scope, checked against each other (see [`read_fetch_scope`]).
fn read_fetch_state() -> Result<(WarehouseMode, PathScope), String> {
    let mode = warehouse_utils::warehouse_mode()?;
    let path = fetch_scope_path();

    if mode != WarehouseMode::Sparse {
        // Only a sparse warehouse is ever given a fetch scope, so one here was put in place by
        // hand. It is refused whatever it holds rather than parsed, and deleting it is safe: the
        // mode, not the file, says what the warehouse holds.
        if !scope_file_is_present(&path)? {
            return Ok((mode, PathScope::full()));
        }
        let kind = mode_kind(mode);
        return Err(format!(
            "The warehouse is recorded as {} but holds a fetch scope (\"{}\"), which only a sparse \
             warehouse has, so Forklift cannot tell what it has fetched. Delete that file: the warehouse \
             is {} without it.",
            kind, path.to_string_lossy(), kind
        ));
    }

    let missing = |found: &str| format!(
        "The warehouse is recorded as sparse but its fetch scope (\"{}\") {}, so Forklift cannot tell \
         what it has fetched. {}",
        path.to_string_lossy(), found, REMEDY_REFRANCHISE
    );
    match read_scope_file_if_present(&path, REMEDY_REFRANCHISE)? {
        Some(scope) if !scope.is_full() => Ok((mode, scope)),
        Some(_) => Err(missing("names no path")),
        None => Err(missing("is missing")),
    }
}

/// How a refusal names a warehouse that is not sparse.
fn mode_kind(mode: WarehouseMode) -> &'static str {
    if mode == WarehouseMode::Full { "full" } else { "a cache" }
}

/// The checkout scope of a named bay. Read from any checkout so a warehouse-level
/// operation — scope prune — can intersect every bay's scope before freeing anything, not only
/// the invoking bay's.
///
/// A bay's scope is its own, since a full warehouse may hold a scoped bay, so every bay records
/// it, "every path" included, and a bay with no scope file is refused in every mode: reading the
/// absence as full would check out paths the bay never held. In a sparse warehouse the scope must
/// also name a path and lie within the fetch scope.
///
/// # Arguments
/// * `name` - The bay's name (as listed by `bay_utils::list_bays`).
pub fn read_bay_scope(name: &str) -> Result<PathScope, String> {
    read_bay_scope_at(&crate::util::bay_utils::bay_state_dir(name).join(FILE_NAME_BAY_SCOPE))
}

fn read_bay_scope_at(path: &Path) -> Result<PathScope, String> {
    let refused = |found: &str| format!(
        "The bay's scope (\"{}\") {}, so Forklift cannot tell which paths the bay checks out. {}",
        path.to_string_lossy(), found, REMEDY_READD_BAY
    );
    let Some(scope) = read_scope_file_if_present(path, REMEDY_READD_BAY)? else {
        return Err(refused("is missing"));
    };

    let fetch = read_fetch_scope()?;

    if !fetch.is_full() {
        if scope.is_full() {
            return Err(refused("names no path, which only a bay of a warehouse that is not sparse may do"));
        }
        if !scope.subset_of(&fetch) {
            return Err(refused("is not within the warehouse's fetch scope"));
        }
    }

    Ok(scope)
}

/// The checkout scope of the warehouse's main working tree. The main tree keeps its
/// bay-local state directly under the shared root, so its scope file is at a fixed location every
/// checkout can read — needed by scope prune, which must intersect the main tree's scope too, not
/// only the invoking checkout's and the named bays'.
///
/// Unlike a bay's, the main tree's scope follows the warehouse's mode: only a sparse clone
/// scopes it. A full or cache warehouse's main tree has no scope file and checks out every path; a
/// file found there was put in place by hand and is refused whatever it holds. A sparse
/// warehouse's main tree must have one that names a path within the fetch scope, so deleting it
/// cannot make the main tree read as holding every path.
pub fn read_main_tree_scope() -> Result<PathScope, String> {
    let (mode, fetch) = read_fetch_state()?;
    let path = globals::forklift_root().join(FILE_NAME_BAY_SCOPE);

    if mode != WarehouseMode::Sparse {
        if !scope_file_is_present(&path)? {
            return Ok(PathScope::full());
        }
        return Err(format!(
            "The warehouse is recorded as {} but its main tree holds a scope (\"{}\"), which only a \
             sparse warehouse's main tree has, so Forklift cannot tell which paths it checks out. Delete \
             that file: the main tree checks out every path without it.",
            mode_kind(mode), path.to_string_lossy()
        ));
    }

    let refused = |found: &str| format!(
        "The warehouse is recorded as sparse but its main tree's scope (\"{}\") {}, so Forklift cannot \
         tell which paths the main tree checks out. {}",
        path.to_string_lossy(), found, REMEDY_REFRANCHISE
    );
    match read_scope_file_if_present(&path, REMEDY_REFRANCHISE)? {
        None => Err(refused("is missing")),
        Some(scope) if scope.is_full() => Err(refused("names no path")),
        Some(scope) if !scope.subset_of(&fetch) => Err(refused("is not within the warehouse's fetch scope")),
        Some(scope) => Ok(scope),
    }
}

/// The way out for a warehouse's fetch scope or main-tree scope Forklift cannot use: both are
/// written when the warehouse is prepared, so it is cloned again.
const REMEDY_REFRANCHISE: &str = "Clone the remote again, into a new directory.";
/// The way out for a bay's scope Forklift cannot use. A removed bay keeps its pallet, so its name
/// stays taken and the new bay needs another.
const REMEDY_READD_BAY: &str = "Remove the bay with \"forklift bay remove\" and add a new one under another name; \
    copy any uncommitted work across by hand.";

/// Set the active checkout's scope, refusing one the readers would refuse: a
/// scope that is not ⊆ the warehouse fetch scope (a checkout can never materialize what the
/// warehouse never fetched, design §3.1), a full one in a sparse warehouse, and any scope for the
/// main tree of a warehouse that is not sparse ([`read_main_tree_scope`]).
///
/// # Arguments
/// * `scope` - The checkout scope to record for the active checkout.
pub fn set_bay_scope(scope: &PathScope) -> Result<(), String> {
    let main_tree = globals::active_bay().is_none();
    if main_tree && warehouse_utils::warehouse_mode()? != WarehouseMode::Sparse {
        return Err("Only a sparse warehouse's main tree records a scope.".to_string());
    }

    set_bay_scope_at(&bay_scope_path(), scope)
}

/// [`set_bay_scope`] for the bay whose state dir is `dir`, before its context is entered. Every bay
/// records its scope, a full one included ([`read_bay_scope`]).
pub fn set_bay_scope_in(dir: &std::path::Path, scope: &PathScope) -> Result<(), String> {
    set_bay_scope_at(&dir.join(FILE_NAME_BAY_SCOPE), scope)
}

fn set_bay_scope_at(path: &std::path::Path, scope: &PathScope) -> Result<(), String> {
    let fetch = read_fetch_scope()?;

    if !fetch.is_full() && scope.is_full() {
        return Err(
            "A checkout of a sparse warehouse must be scoped; it cannot check out every path.".to_string()
        );
    }

    if !scope.subset_of(&fetch) {
        return Err(
            "The requested checkout scope is not within the warehouse's fetch scope; \
            a bay cannot materialize a path the warehouse has not fetched.".to_string()
        );
    }

    if let Some(parent) = path.parent() {
        crate::util::file_utils::create_folder_if_not_exists(parent)?;
    }

    write_scope_file(path, scope)
}

/// Replace a sparse warehouse's fetch scope (shared across bays): `scope fetch` unions new prefixes
/// into it and `scope prune` drops some. Refused in a warehouse that is not sparse, and for a full
/// scope, which no sparse warehouse may record: whether a warehouse is sparse is its mode, fixed
/// when it was prepared ([`warehouse_utils::prepare_warehouse`] writes the first fetch scope).
///
/// # Arguments
/// * `scope` - The fetch scope to record for the warehouse.
pub fn set_fetch_scope(scope: &PathScope) -> Result<(), String> {
    if warehouse_utils::warehouse_mode()? != WarehouseMode::Sparse {
        return Err("Only a sparse warehouse records a fetch scope.".to_string());
    }

    if scope.is_full() {
        return Err("A sparse warehouse's fetch scope must name at least one path.".to_string());
    }

    write_scope_file(&fetch_scope_path(), scope)
}

/// Write a new sparse warehouse's first fetch scope and main-tree scope into its `.forklift`
/// folder `forklift`, before its marker exists (`prepare_warehouse` alone). Its configuration
/// folder must already exist.
///
/// # Arguments
/// * `forklift` - The `.forklift` folder being prepared.
/// * `scope`    - The fetch scope, which is also the main tree's. Never full.
pub(crate) fn record_sparse_scopes(forklift: &Path, scope: &PathScope) -> Result<(), String> {
    if scope.is_full() {
        return Err("A sparse warehouse's fetch scope must name at least one path.".to_string());
    }

    let bytes = scope.to_bytes();

    for path in [forklift.join(FOLDER_NAME_CONFIG).join(FILE_NAME_FETCH_SCOPE), forklift.join(FILE_NAME_BAY_SCOPE)] {
        crate::util::file_utils::replace_file_atomically(&path, &bytes)
            .map_err(|e| format!("Error while recording the scope \"{}\": {}", path.to_string_lossy(), e))?;
    }

    Ok(())
}

/// Whether the store may lack content its history names: the warehouse's mode is not full (it is
/// sparse or a cache). A quick gate for the operations that need the whole store, or that must
/// stay with the remote that holds what this one lacks: the origin-only lift guard, mirror, and
/// serving the warehouse. The fetch scope is checked against the mode on the way.
pub fn content_may_be_absent() -> Result<bool, String> {
    Ok(read_fetch_state()?.0 != WarehouseMode::Full)
}

/// Whether the warehouse was cloned with `--cache`: its mode is cache.
pub fn is_cache_mode() -> Result<bool, String> {
    Ok(read_fetch_state()?.0 == WarehouseMode::Cache)
}

/// Build a classified scope refusal (design §7.4 taxonomy) as a typed [`CoreError::Refusal`]: the
/// stable [`RefusalCode`], the human `message` and a machine-actionable `next_step`. The head
/// classifies it into its exit code by the code — no string parsing. `forklift-core` never prints
/// and cannot depend on the head's error type, so this typed value is the seam. See
/// [`crate::error`] for how it crosses a still-String segment.
///
/// # Arguments
/// * `code`      - The stable [`RefusalCode`].
/// * `message`   - The human explanation.
/// * `next_step` - The machine-actionable recovery step.
pub fn refusal(code: RefusalCode, message: impl Into<String>, next_step: impl Into<String>) -> CoreError {
    CoreError::refusal(code, message, next_step)
}

/// Decode a sentinel-framed refusal string into `(code, message, next_step)`. Re-exported from
/// [`crate::error`], which owns the framing now; retained here for the call sites (the wire's
/// `error_of`, existing tests) that reference the scope path.
pub use crate::error::deframe as decode_refusal;

/// A ready-made `scope_path_type_changed` refusal for a spine path whose entry flipped
/// between a directory and a file at the target revision (design §3.1). The spine's whole
/// job is to carry the hash forward assuming the path is still a directory; a type flip
/// breaks that assumption in a way a sparse workspace cannot safely reason about.
pub fn type_changed_refusal(path: &str) -> CoreError {
    let next_step = "Re-scope the bay (or widen it to include the path), or resolve in a full workspace.";

    refusal(
        RefusalCode::ScopePathTypeChanged,
        format!(
            "The path \"{}\" is no longer a directory (or is now one) at this revision; the \
            sparse scope is no longer valid there. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `out_of_scope` refusal for a path argument outside the bay's scope.
pub fn out_of_scope_refusal(path: &str) -> CoreError {
    let next_step = "Widen the bay's scope to include the path, or run the command in a full workspace.";

    refusal(
        RefusalCode::OutOfScope,
        format!("The path \"{}\" is outside this bay's checkout scope. {}", path, next_step),
        next_step,
    )
}

/// A ready-made `out_of_scope_conflict` refusal for a merge where an out-of-scope entry (a
/// subtree, file or symlink this bay never materialized) changed on *both* sides against the
/// merge base. A one-sided out-of-scope change is resolved by hash without the
/// content; a genuine two-sided conflict cannot be — the bay has no content to reconcile — so
/// it refuses rather than guess.
pub fn out_of_scope_conflict_refusal(path: &str) -> CoreError {
    let next_step = "Widen the bay's scope to include the path and retry, or resolve the merge in a full workspace.";

    refusal(
        RefusalCode::OutOfScopeConflict,
        format!(
            "The path \"{}\" changed on both sides outside this bay's checkout scope; a \
            scoped bay cannot merge it without the content. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `sparse_workspace` refusal for a whole-tree verb that does not yet support
/// running in a scoped bay.
pub fn sparse_workspace_refusal(verb: &str, next_step: &str) -> CoreError {
    refusal(
        RefusalCode::SparseWorkspace,
        format!("\"{}\" is not supported in a scoped (sparse) bay yet. {}", verb, next_step),
        next_step,
    )
}

/// A ready-made `narrow_unclean` refusal: the subtree `scope narrow` was asked to drop still holds
/// uncommitted work. `scope narrow` de-materializes files unconditionally once it decides to act — it
/// has no working-directory-preserving path the way `shift`'s target-tree walk does — so it
/// refuses up front rather than silently discard staged, unstaged or untracked content. This
/// matches `shift`'s own precedent for untracked collisions: never delete something the
/// operator has not committed anywhere. No override in this round — stack, restore, park or
/// move the blocking paths out of the way, then narrow again.
///
/// # Arguments
/// * `path`      - The subtree that was asked to be narrowed away.
/// * `blocked_by` - What kind of uncommitted work is blocking it (for the message).
pub fn narrow_unclean_refusal(path: &str, blocked_by: &str) -> CoreError {
    let next_step = format!(
        "Stack or restore the changes under \"{}\" (or move untracked files out of the way), \
        then run \"forklift scope narrow\" again.",
        path
    );

    refusal(
        RefusalCode::NarrowUnclean,
        format!(
            "\"{}\" has {} that scope narrow would otherwise delete; it refuses to discard \
            uncommitted work. {}",
            path, blocked_by, next_step
        ),
        next_step,
    )
}

/// A ready-made `scope_prune_blocked` refusal: scope prune would free content that a checkout
/// still materializes. A prune narrows the shared warehouse fetch scope and deletes the freed
/// objects, so it can never leave a checkout materializing a path the warehouse no longer
/// fetches — that would break the checkout the next time it read the now-absent content. It
/// refuses up front, naming the blocking checkout(s), rather than corrupt them.
///
/// # Arguments
/// * `path`      - The path the prune was asked to free.
/// * `blockers`  - The checkout(s) still materializing it (for the message).
pub fn scope_prune_blocked_refusal(path: &str, blockers: &str) -> CoreError {
    let next_step = format!(
        "In {}, drop the in-scope paths that overlap \"{}\" with \"forklift scope narrow\" \
        (checkout-local, it frees nothing), or remove a bay whose whole scope overlaps, then prune.",
        blockers, path
    );

    refusal(
        RefusalCode::ScopePruneBlocked,
        format!(
            "\"{}\" is still in the checkout scope of {}; pruning it would break that checkout. {}",
            path, blockers, next_step
        ),
        next_step,
    )
}

/// A ready-made `chunked_transport_unsupported` refusal for writing a large file stored in chunks
/// **into a bundle**. Chunk transport ships per object (a chunked file's chunks ride the loose
/// GET/PUT byte plane, negotiated one hash at a time), but **no bundle ever carries a chunk** by
/// design: trees reference only recipes, so a bundle's closure walk never descends into a recipe,
/// and a chunk is structurally absent from every bundle. A bundle could still move a chunked
/// file's recipe as bytes, but the result would be a bundle over a file whose chunks it cannot
/// contain — unmaterializable wherever it lands. Refusing up front, naming the path, is the honest
/// failure; a chunked file reaches a peer over the wire per object (clone, lower and scope fetch all fetch
/// the recipe, then the chunks loose), never inside a bundle.
///
/// # Arguments
/// * `path` - The warehouse path of the chunked file that blocked the bundle.
pub fn chunked_transport_refusal(path: &str) -> CoreError {
    let next_step = "Move this warehouse's history to the peer over the wire (lower/clone/lift \
        to a chunk-aware remote), which carries chunked files per object; a bundle cannot.".to_string();

    refusal(
        RefusalCode::ChunkedTransportUnsupported,
        format!(
            "\"{}\" is a large file stored in chunks, and a bundle never carries a chunked file's \
            chunks. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `chunked_transport_unsupported` refusal for lifting a chunked file to a remote
/// **that does not support chunked large files** (its handshake omits the `chunking` capability —
/// an older head). Such a head serves and stores the recipe like any other object, but its `gc`
/// cannot see that a recipe's chunks are reachable only through it and would silently collect them
/// (the §9.4b B1 data-loss bug), so a chunk-aware client refuses to lift chunked content there —
/// client-side, before any negotiation or upload, naming the path. Checked for every chunked entry
/// the lift closure visits, so it also catches one that is unchanged-but-newly-reachable. Plain
/// (non-chunked) content lifts to such a head unaffected.
///
/// # Arguments
/// * `path` - The warehouse path of the chunked file that blocked the lift.
pub fn chunked_remote_refusal(path: &str) -> CoreError {
    let next_step = "Upgrade the remote to a version that supports chunked large files, or keep \
        this file under the chunking threshold.".to_string();

    refusal(
        RefusalCode::ChunkedTransportUnsupported,
        format!(
            "\"{}\" is a large file stored in chunks, and this remote does not support chunked \
            large files. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `commit_pagination_unsupported` refusal for a lift whose commit would need more
/// than one paginated batch against a remote that does not advertise chunking support (the
/// capability the additive `more` field shipped alongside — see
/// [`CODE_COMMIT_PAGINATION_UNSUPPORTED`]). Checked right after upload-target negotiation, which
/// already knows the exact staged count, and before a single byte is uploaded — a refused lift
/// never wastes the (potentially enormous) upload only to fail confusingly at commit time.
///
/// # Arguments
/// * `staged_count` - How many distinct objects `upload-targets` staged for this lift.
pub fn commit_pagination_unsupported_refusal(staged_count: usize) -> CoreError {
    let next_step = "Upgrade the remote to a version that supports paginated commits.".to_string();

    refusal(
        RefusalCode::CommitPaginationUnsupported,
        format!(
            "This lift needs to stage {} objects, above the {}-object cap a single commit batch \
            supports, and this remote does not advertise support for paginated commits. {}",
            staged_count, crate::model::remote::MAX_MISSING_BATCH, next_step
        ),
        next_step,
    )
}

/// A ready-made `oversized_transport_unsupported` refusal for sending an object above the
/// whole-object ceiling to a remote or into a bundle — the honest failure the maintainer settled
/// on for a **grandfathered** giant (an object authored before `MAX_OBJECT_BYTES` existed). It stays fully readable and checkout-able locally forever
/// (the ceiling gates writes and imports, never reads), but no migration exists that preserves its
/// signed identity, so it can never move to a remote or into a bundle: every bundle reader
/// refuses it, the record-stream one on its declared length before reading a byte. Refusing here — before anything is written into a bundle
/// stream or sent over the wire on a lift — is the honest failure at the source.
///
/// # Arguments
/// * `what` - What is being refused (a path, a hash, or both — whatever the caller has in hand).
/// * `len`  - The object's actual byte length.
pub fn oversized_transport_refusal(what: &str, len: u64) -> CoreError {
    let next_step = "This object predates the whole-object size limit. It stays readable and \
        checkout-able locally, but no migration exists that would preserve its signed identity, \
        so it cannot be sent to a remote or into a bundle.".to_string();

    refusal(
        RefusalCode::OversizedTransportUnsupported,
        format!(
            "{} is {} bytes, above the {}-byte whole-object ceiling. {}",
            what, len, crate::util::object_utils::MAX_OBJECT_BYTES, next_step
        ),
        next_step,
    )
}

/// Refuse to transport (bundle or lift) an object above the whole-object ceiling, when the caller
/// already has its exact byte length in hand (a bundle writer about to emit a record, or a lift
/// about to put bytes on the wire). A no-op for anything at or under the ceiling.
///
/// # Arguments
/// * `what` - What is being refused, for [`oversized_transport_refusal`] (a path, a hash, or both).
/// * `len`  - The object's actual byte length.
///
/// # Returns
/// * `Ok(())`         - If `len` is within the ceiling.
/// * `Err(CoreError)` - The `oversized_transport_unsupported` refusal, otherwise.
pub fn refuse_if_over_object_ceiling(what: &str, len: usize) -> Result<(), CoreError> {
    if len <= crate::util::object_utils::MAX_OBJECT_BYTES {
        return Ok(());
    }

    Err(oversized_transport_refusal(what, len as u64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_scope_classifies_everything_in_scope() {
        let scope = PathScope::full();

        assert!(scope.is_full());
        for path in ["", "src", "src/api", "src/api/x.rs", "README.md"] {
            assert_eq!(scope.classify(path), ScopeClass::InScope, "path: {}", path);
        }
    }

    #[test]
    fn classify_is_three_valued_over_a_single_prefix() {
        let scope = PathScope::from_prefixes(["src/api"]);

        // Spine: the root and the intermediate directory on the path to the in-scope leaf.
        assert_eq!(scope.classify(""), ScopeClass::Spine);
        assert_eq!(scope.classify("src"), ScopeClass::Spine);

        // In scope: the prefix itself and everything under it.
        assert_eq!(scope.classify("src/api"), ScopeClass::InScope);
        assert_eq!(scope.classify("src/api/handlers"), ScopeClass::InScope);
        assert_eq!(scope.classify("src/api/handlers/get.rs"), ScopeClass::InScope);

        // Out of scope: siblings of the spine and of the in-scope prefix, and unrelated roots.
        assert_eq!(scope.classify("src/web"), ScopeClass::OutOfScope);
        assert_eq!(scope.classify("src/lib.rs"), ScopeClass::OutOfScope);
        assert_eq!(scope.classify("README.md"), ScopeClass::OutOfScope);

        // A name that merely shares the prefix string is not under it.
        assert_eq!(scope.classify("src/apix"), ScopeClass::OutOfScope);
    }

    #[test]
    fn the_root_ignore_file_is_in_every_scope_without_being_a_prefix() {
        let scope = PathScope::from_prefixes(["src/api"]);

        // The root ignore file is in scope, though no prefix names it — but it is not a declared
        // prefix, so the scope never requires it to be a directory, and it widens nothing else.
        assert_eq!(scope.classify(".forkliftignore"), ScopeClass::InScope);
        assert!(!scope.requires_directory(".forkliftignore"));
        assert_eq!(scope.implicit_members(), &[".forkliftignore"]);
        assert!(PathScope::full().implicit_members().is_empty());
        assert!(!scope.spine_file_is_sealed(".forkliftignore").unwrap());
        assert!(scope.spine_file_is_sealed("README.md").unwrap());
        assert!(scope.spine_file_is_sealed("src").is_err(), "a file where a spine directory is expected");
        assert_eq!(scope.prefixes(), &["src/api".to_string()]);
        assert_eq!(scope.classify("README.md"), ScopeClass::OutOfScope);
        assert_eq!(scope.classify(".forkliftignorex"), ScopeClass::OutOfScope);
        assert_eq!(scope.classify("src/.forkliftignore"), ScopeClass::OutOfScope);

        // A path test that does not go through the scope sees only the prefix.
        // Every implicit member, not just today's one, is a root-level file the spine walkers
        // keep as in scope, and never a place the scope expects a directory.
        for member in scope.implicit_members() {
            assert!(!member.contains('/'), "{} must sit at the root, a spine level", member);
            assert_eq!(scope.classify(member), ScopeClass::InScope, "{}", member);
            assert!(!scope.requires_directory(member), "{}", member);
            assert!(!scope.spine_file_is_sealed(member).unwrap(), "{}", member);
        }

        assert!(is_at_or_under("src/api/x.rs", "src/api"));
        assert!(is_at_or_under("src/api", "src/api"));
        assert!(!is_at_or_under(".forkliftignore", "src/api"));
        assert!(!is_at_or_under("src", "src/api"));
    }

    #[test]
    fn classify_handles_multiple_prefixes() {
        let scope = PathScope::from_prefixes(["src/api", "docs"]);

        assert_eq!(scope.classify(""), ScopeClass::Spine);
        assert_eq!(scope.classify("src"), ScopeClass::Spine);
        assert_eq!(scope.classify("src/api"), ScopeClass::InScope);
        assert_eq!(scope.classify("docs"), ScopeClass::InScope);
        assert_eq!(scope.classify("docs/guide.md"), ScopeClass::InScope);
        assert_eq!(scope.classify("src/web"), ScopeClass::OutOfScope);
        assert_eq!(scope.classify("tests"), ScopeClass::OutOfScope);
    }

    #[test]
    fn prefixes_are_normalized_and_a_root_prefix_is_full() {
        let scope = PathScope::from_prefixes(["/src/api/", "src/api", "docs/"]);
        assert_eq!(scope.prefixes(), &["docs".to_string(), "src/api".to_string()]);

        // A root prefix collapses to full scope.
        assert!(PathScope::from_prefixes(["/"]).is_full());
        assert!(PathScope::from_prefixes([""]).is_full());
    }

    #[test]
    fn scope_round_trips_through_its_on_disk_form() {
        let scope = PathScope::from_prefixes(["src/api", "docs"]);
        let bytes = scope.to_bytes();

        assert_eq!(PathScope::from_bytes(&bytes, "").unwrap(), scope);

        // The full scope round-trips as the empty file.
        assert!(PathScope::full().to_bytes().is_empty());
        assert!(PathScope::from_bytes(b"", "").unwrap().is_full());
        // Only the empty file is: whitespace is no longer trimmed away, so a file of blank lines
        // has no header and is refused.
        assert!(PathScope::from_bytes(b"\n  \n", "").is_err());
    }

    #[test]
    fn a_scope_file_without_its_format_header_is_refused() {
        // A non-full scope is written under its format header.
        let bytes = PathScope::from_prefixes(["src"]).to_bytes();
        assert!(bytes.starts_with(b"/forklift-scope 3\n"), "{:?}", String::from_utf8_lossy(&bytes));

        // The unversioned form an older Forklift wrote is refused, naming the way out.
        let legacy = PathScope::from_bytes(b"src\n", "Do the remedy.").unwrap_err();
        assert!(legacy.contains("older version of Forklift"), "{}", legacy);
        assert!(legacy.ends_with("Do the remedy."), "{}", legacy);

        // So is a format this build does not know.
        let unknown = PathScope::from_bytes(b"/forklift-scope 4\nsrc\n", "").unwrap_err();
        assert!(unknown.contains("/forklift-scope 4"), "{}", unknown);

        // Format 2 wrote prefixes raw, so a prefix holding a line break read back as two.
        let raw = PathScope::from_bytes(b"/forklift-scope 2\nsrc\n", "Do the remedy.").unwrap_err();
        assert!(raw.contains("/forklift-scope 2") && raw.contains("Do the remedy."), "{}", raw);
        assert!(!raw.contains("Use the Forklift that wrote it"), "{}", raw);

        // Line endings rewritten to CRLF still read as the older format they are.
        let crlf = PathScope::from_bytes(b"/forklift-scope 2\r\nsrc\r\n", "Do the remedy.").unwrap_err();
        assert!(crlf.contains("Do the remedy."), "{}", crlf);

        // And a header with no prefix after it, which would otherwise read as the full scope.
        let bare = PathScope::from_bytes(b"/forklift-scope 3\n", "Do the remedy.").unwrap_err();
        assert!(bare.contains("no prefix narrower than the warehouse root"), "{}", bare);
        let rooted = PathScope::from_bytes(b"/forklift-scope 3\nsrc\n/\n", "").unwrap_err();
        assert!(rooted.contains("no prefix narrower than the warehouse root"), "{}", rooted);
        assert!(bare.ends_with("Do the remedy."), "{}", bare);
    }

    #[test]
    fn subset_invariant_holds_for_bay_within_fetch() {
        let fetch = PathScope::from_prefixes(["src"]);

        // A bay ⊆ the fetch scope.
        assert!(PathScope::from_prefixes(["src/api"]).subset_of(&fetch));
        assert!(PathScope::from_prefixes(["src"]).subset_of(&fetch));

        // A bay reaching outside the fetch scope is not a subset.
        assert!(!PathScope::from_prefixes(["src/api", "docs"]).subset_of(&fetch));
        assert!(!PathScope::full().subset_of(&fetch));

        // Everything is a subset of the full fetch scope.
        assert!(PathScope::from_prefixes(["anything"]).subset_of(&PathScope::full()));
        assert!(PathScope::full().subset_of(&PathScope::full()));
    }

    /// Destructure a refusal into `(code, message, next_step)` for the assertions below — the
    /// constructors now return a typed [`CoreError::Refusal`], so tests read the fields directly
    /// instead of decoding a string.
    fn parts(error: &CoreError) -> (RefusalCode, &str, &str) {
        match error {
            CoreError::Refusal { code, message, next_step, .. } => (*code, message, next_step),
            CoreError::Other(other) => panic!("expected a refusal, got Other({:?})", other),
        }
    }

    #[test]
    fn refusal_carries_its_typed_code_and_text() {
        let refusal = refusal(RefusalCode::OutOfScope, "message here", "do this next");
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::OutOfScope);
        assert_eq!(message, "message here");
        assert_eq!(next_step, "do this next");

        // A plain error is not a refusal.
        assert_eq!(CoreError::from("something ordinary went wrong"), CoreError::Other("something ordinary went wrong".to_string()));
    }

    #[test]
    fn a_narrow_unclean_refusal_carries_the_stable_code_and_names_the_path() {
        let refusal = narrow_unclean_refusal("docs", "untracked file(s) (docs/draft.md)");
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::NarrowUnclean);
        assert!(message.contains("docs"), "the path is named: {}", message);
        assert!(message.contains("untracked file(s)"), "the blocker is named: {}", message);
        assert!(next_step.contains("docs"), "the recovery names the path: {}", next_step);
    }

    #[test]
    fn a_chunked_transport_refusal_carries_the_stable_code_and_names_the_path() {
        let refusal = chunked_transport_refusal("big.bin");
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::ChunkedTransportUnsupported);
        assert!(message.contains("big.bin"), "the path is named: {}", message);
        assert!(message.contains("chunks"), "the message explains why: {}", message);
        assert!(!next_step.is_empty());
    }

    #[test]
    fn an_oversized_transport_refusal_carries_the_stable_code_and_names_the_ceiling() {
        let refusal = oversized_transport_refusal(
            "\"big.bin\" (object aaaa)", crate::util::object_utils::MAX_OBJECT_BYTES as u64 + 1
        );
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::OversizedTransportUnsupported);
        assert!(message.contains("big.bin"), "what is named: {}", message);
        assert!(message.contains("ceiling"), "the message explains why: {}", message);
        assert!(next_step.contains("signed identity"), "the recovery states no migration exists: {}", next_step);
    }

    #[test]
    fn refuse_if_over_object_ceiling_is_a_no_op_at_or_under_the_ceiling() {
        assert!(refuse_if_over_object_ceiling("object x", crate::util::object_utils::MAX_OBJECT_BYTES).is_ok());
        assert!(refuse_if_over_object_ceiling("object x", 0).is_ok());
    }

    #[test]
    fn refuse_if_over_object_ceiling_refuses_one_byte_over() {
        let error = refuse_if_over_object_ceiling(
            "object x", crate::util::object_utils::MAX_OBJECT_BYTES + 1
        ).expect_err("one byte over the ceiling must refuse");

        let (code, _, _) = parts(&error);
        assert_eq!(code, RefusalCode::OversizedTransportUnsupported);
    }

    #[test]
    fn refusal_sanitizes_a_control_character_in_interpolated_text() {
        // A path sourced from disk (a tree/inventory entry), not `WarehousePath::from_user_input`,
        // can carry the framing separator itself. Sanitizing at construction keeps the message
        // render-safe and frame-safe, with the control character stripped rather than dropped.
        let hostile_path = "src/\u{1f}api";

        let refusal = out_of_scope_refusal(hostile_path);
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::OutOfScope);
        assert!(!message.contains('\u{1f}'), "message still carries the control char: {:?}", message);
        assert!(!next_step.contains('\u{1f}'), "next_step still carries the control char: {:?}", next_step);
        assert!(message.contains("src/ api"), "control char should be replaced, not vanish: {:?}", message);
    }

    #[test]
    fn a_scope_prune_blocked_refusal_carries_the_stable_code_and_names_the_blocker() {
        let refusal = scope_prune_blocked_refusal("docs", "bay \"reviewer\"");
        let (code, message, next_step) = parts(&refusal);

        assert_eq!(code, RefusalCode::ScopePruneBlocked);
        assert!(message.contains("docs"), "the path is named: {}", message);
        assert!(message.contains("bay \"reviewer\""), "the blocker is named: {}", message);
        assert!(next_step.contains("scope narrow"), "the recovery says how to unblock: {}", next_step);
    }

    #[test]
    fn prefixes_holding_line_breaks_or_edge_spaces_round_trip() {
        let scope = PathScope::from_prefixes(["a\nb", " docs ", "c\r", "d\\ne"]);

        let read_back = PathScope::from_bytes(&scope.to_bytes(), "").unwrap();

        assert_eq!(read_back, scope);
    }

    use crate::util::warehouse_utils::mark_for_test;

    fn scratch(name: &str) -> (PathBuf, globals::StorageRootScope) {
        let root = std::env::temp_dir().join(format!("forklift-scope-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT).join(FOLDER_NAME_CONFIG)).unwrap();
        let scope = globals::StorageRootScope::enter(&root);
        (root, scope)
    }

    /// The mode the marker records decides partiality, and the fetch scope is read against it.
    ///
    /// Revert: read `content_may_be_absent` from the fetch-scope file alone → the cache reads as
    /// full; or read an absent marker as full → the first assertion goes green on `Ok`.
    #[test]
    fn the_recorded_mode_decides_whether_content_may_be_absent() {
        let (root, _scope) = scratch("mode");

        let error = content_may_be_absent().unwrap_err();
        assert!(error.contains("holds no warehouse marker"), "{}", error);

        mark_for_test(WarehouseMode::Full);
        assert!(!content_may_be_absent().unwrap());
        assert!(!is_cache_mode().unwrap());
        assert!(read_fetch_scope().unwrap().is_full());

        mark_for_test(WarehouseMode::Cache);
        assert!(content_may_be_absent().unwrap());
        assert!(is_cache_mode().unwrap());
        assert!(read_fetch_scope().unwrap().is_full());

        mark_for_test(WarehouseMode::Sparse);
        set_fetch_scope(&PathScope::from_prefixes(["docs"])).unwrap();
        assert!(content_may_be_absent().unwrap());
        assert!(!is_cache_mode().unwrap());
        assert_eq!(read_fetch_scope().unwrap(), PathScope::from_prefixes(["docs"]));

        std::fs::remove_dir_all(&root).ok();
    }

    /// Every pairing of mode and fetch-scope file but the two a clone writes is refused, by
    /// every reader, naming the way out; none reads as full.
    ///
    /// Revert: map a sparse warehouse's missing fetch scope to full → its row goes `Ok`; or skip
    /// the check for a full or cache mode → the stray-file rows go `Ok`.
    #[test]
    fn a_fetch_scope_that_does_not_match_the_mode_is_refused() {
        let (root, _scope) = scratch("mode-mismatch");
        let path = fetch_scope_path();
        let sparse = PathScope::from_prefixes(["docs"]).to_bytes();

        // A stray file in a full warehouse or a cache is refused unparsed, so a garbled one says
        // "delete it" too.
        for (mode, file, found, remedy) in [
            (WarehouseMode::Sparse, None, "is missing", REMEDY_REFRANCHISE),
            (WarehouseMode::Sparse, Some(Vec::new()), "names no path", REMEDY_REFRANCHISE),
            (WarehouseMode::Full, Some(sparse.clone()), "holds a fetch scope", "Delete that file"),
            (WarehouseMode::Full, Some(Vec::new()), "holds a fetch scope", "Delete that file"),
            (WarehouseMode::Full, Some(b"not a scope file".to_vec()), "holds a fetch scope", "Delete that file"),
            (WarehouseMode::Cache, Some(sparse.clone()), "holds a fetch scope", "Delete that file"),
        ] {
            mark_for_test(mode);
            match &file {
                Some(bytes) => std::fs::write(&path, bytes).unwrap(),
                None => { let _ = std::fs::remove_file(&path); }
            }

            for error in [read_fetch_scope().unwrap_err(), content_may_be_absent().unwrap_err(), is_cache_mode().unwrap_err()] {
                assert!(error.contains(found), "{:?} {:?}: {}", mode, file, error);
                assert!(error.contains(remedy), "{:?} {:?}: {}", mode, file, error);
            }
        }

        std::fs::remove_dir_all(&root).ok();
    }

    /// Only a sparse warehouse's fetch scope can be replaced, and never by a full one.
    ///
    /// Revert: drop either check in `set_fetch_scope` → the matching write lands.
    #[test]
    fn set_fetch_scope_refuses_a_warehouse_that_is_not_sparse_and_a_full_scope() {
        let (root, _scope) = scratch("set-fetch-scope");
        let docs = PathScope::from_prefixes(["docs"]);

        for mode in [WarehouseMode::Full, WarehouseMode::Cache] {
            mark_for_test(mode);
            assert!(set_fetch_scope(&docs).unwrap_err().contains("Only a sparse warehouse"), "{:?}", mode);
            assert!(!fetch_scope_path().exists(), "{:?}", mode);
        }

        mark_for_test(WarehouseMode::Sparse);
        assert!(set_fetch_scope(&PathScope::full()).unwrap_err().contains("at least one path"));
        assert!(!fetch_scope_path().exists());

        std::fs::remove_dir_all(&root).ok();
    }
}
