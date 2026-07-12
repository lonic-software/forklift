//! Task-scoped sparse workspaces (DESIGN.html §7.6) — the local scope record and the
//! three-valued classifier every scope-aware walk branches on. Both layers are live: a bay's
//! materialization scope, and the warehouse fetch scope a sparse franchise records (empty = the
//! store holds everything).
//!
//! A scoped bay materializes and operates on only chosen path subtrees of the user
//! pallet's working tree. Scope is **local only, never tracked** (it is a property of
//! *this* checkout, not the project — tracking it would push it onto collaborators, git
//! sparse-checkout's leaky middle ground). It lives in two path-prefix files:
//!
//! * the **bay materialization scope** (`<bay_root>/scope`, bay-local) — what this bay
//!   materializes and stacks; always a subset (⊆) of the fetch scope, and
//! * the **warehouse fetch scope** (`config/fetch-scope`, shared) — what the warehouse
//!   has fetched at all. A full franchise leaves it unset (= full) and only the bay scope
//!   restricts behavior; a sparse (`franchise --only`) warehouse records the fetched prefixes
//!   here, and `expand` widens them.
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

/// The bay-local file that records a bay's materialization scope (under `<bay_root>/`).
const FILE_NAME_BAY_SCOPE: &str = "scope";

/// The shared configuration folder (matches `config_utils`' own folder name) and the
/// warehouse fetch-scope file inside it.
const FOLDER_NAME_CONFIG: &str = "config";
const FILE_NAME_FETCH_SCOPE: &str = "fetch-scope";

/// The stable machine-branchable error codes this feature contributes to the §7.4 error
/// taxonomy (add, never repurpose). A scope refusal is carried from `forklift-core` (which
/// never prints and cannot build the CLI's `ForkliftError`) to the CLI as a sentinel-framed
/// string; the CLI decodes it into a classified error + exit code. See [`refusal`].
pub const CODE_OUT_OF_SCOPE: &str = "out_of_scope";
pub const CODE_OUT_OF_SCOPE_CONFLICT: &str = "out_of_scope_conflict";
pub const CODE_SCOPE_PATH_TYPE_CHANGED: &str = "scope_path_type_changed";
pub const CODE_SPARSE_WORKSPACE: &str = "sparse_workspace";
pub const CODE_NON_ORIGIN_LIFT: &str = "non_origin_lift";
pub const CODE_NARROW_UNCLEAN: &str = "narrow_unclean";
pub const CODE_SCOPE_PRUNE_BLOCKED: &str = "scope_prune_blocked";

/// Not a scope/sparse-workspace code — large-file chunk transport (§9.4b) has no home of its
/// own for a `forklift-core` → CLI classified refusal, and this module's sentinel-framing
/// ([`refusal`]/[`decode_refusal`]) is the only such facility in the codebase (the same
/// piggy-backing precedent `CODE_NON_ORIGIN_LIFT` already set). Reused here rather than
/// duplicated.
pub const CODE_CHUNKED_TRANSPORT_UNSUPPORTED: &str = "chunked_transport_unsupported";

/// Not a scope/sparse-workspace code either — same piggy-backing precedent as
/// `CODE_CHUNKED_TRANSPORT_UNSUPPORTED` above. A **grandfathered** object above the whole-object
/// ceiling (authored, or imported via an old-version bundle, before the ceiling existed) is
/// readable and checkout-able locally forever — the ceiling gates writes and imports only — but
/// nothing accepts it on the wire, and there is no migration that would preserve its signed
/// identity (a blob's hash is pinned inside a signed tree; re-chunking it would mint a different
/// hash and so a different, unsigned tree). Refusing client-side, before anything is written into
/// a bundle or sent over a lift, is the honest failure at the source.
pub const CODE_OVERSIZED_TRANSPORT_UNSUPPORTED: &str = "oversized_transport_unsupported";

/// The framing that marks a scope refusal string so the CLI can classify it without
/// parsing prose. `\u{1f}` (ASCII Unit Separator) never appears in a message or a
/// warehouse path, so the framing is unambiguous; a plain error the CLI does not recognize
/// simply degrades to the generic classification.
pub const REFUSAL_PREFIX: &str = "\u{1f}scope\u{1f}";
pub const REFUSAL_FIELD_SEPARATOR: char = '\u{1f}';

/// Where a user-pallet content path sits relative to the materialization scope (design
/// §3.1). Every scope-aware walk branches on the three cases, never on a single predicate.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ScopeClass {
    /// At or under a configured in-scope prefix: fully materialized — descend, materialize,
    /// verify and merge exactly as a full workspace would. Nothing below needs re-classifying.
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

/// A bay's (or the warehouse's) materialization scope: a set of in-scope path prefixes.
///
/// An empty set means **full scope** — no restriction, so [`classify`](Self::classify)
/// returns [`ScopeClass::InScope`] for every path. This is what makes a scoped bay opt-in:
/// a plain bay (or the main tree) has no scope file, reads back as full, and every
/// scope-aware code path collapses to today's behavior unchanged.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializationScope {
    /// Normalized in-scope prefixes: warehouse path keys (no leading/trailing slash, `/`
    /// separators), sorted and de-duplicated, none empty. Empty vec = full scope.
    prefixes: Vec<String>,
}

impl MaterializationScope {
    /// The full (unrestricted) scope — every path is in scope.
    pub fn full() -> MaterializationScope {
        MaterializationScope { prefixes: Vec::new() }
    }

    /// Build a scope from raw in-scope prefixes, normalizing them: an empty (root) prefix
    /// means "materialize everything", so it collapses the whole scope to full; the rest are
    /// trimmed of `/`, de-duplicated and sorted for a canonical on-disk form.
    pub fn from_prefixes<I, S>(prefixes: I) -> MaterializationScope
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
                return MaterializationScope::full();
            }

            normalized.push(trimmed);
        }

        normalized.sort();
        normalized.dedup();

        MaterializationScope { prefixes: normalized }
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

    /// Classify a user-pallet content path against this scope (design §3.1). See
    /// [`ScopeClass`] for the three cases.
    ///
    /// # Arguments
    /// * `path` - A warehouse path key (the empty string is the root).
    pub fn classify(&self, path: &str) -> ScopeClass {
        if self.prefixes.is_empty() {
            return ScopeClass::InScope;
        }

        // In scope: at or under some in-scope prefix.
        for prefix in &self.prefixes {
            if path == prefix || is_strictly_under(path, prefix) {
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
    /// in scope for `other`. The bay materialization scope must always be ⊆ the warehouse
    /// fetch scope (design §3.1): a bay can never materialize what the warehouse never fetched.
    pub fn subset_of(&self, other: &MaterializationScope) -> bool {
        if other.is_full() {
            return true;
        }

        if self.is_full() {
            // A full bay is not a subset of a restricted fetch scope.
            return false;
        }

        self.prefixes.iter().all(|prefix| other.classify(prefix) == ScopeClass::InScope)
    }

    /// Serialize the scope to its on-disk form: one prefix per line, newline-terminated.
    /// A full scope serializes to the empty string.
    fn to_bytes(&self) -> Vec<u8> {
        let mut bytes = Vec::new();

        for prefix in &self.prefixes {
            bytes.extend_from_slice(prefix.as_bytes());
            bytes.push(b'\n');
        }

        bytes
    }

    /// Parse a scope from its on-disk form (blank lines and surrounding whitespace ignored).
    fn from_bytes(bytes: &[u8]) -> Result<MaterializationScope, String> {
        let text = std::str::from_utf8(bytes)
            .map_err(|_| "The scope file is not valid UTF-8.".to_string())?;

        let prefixes: Vec<&str> = text.lines()
            .map(|line| line.trim())
            .filter(|line| !line.is_empty())
            .collect();

        Ok(MaterializationScope::from_prefixes(prefixes))
    }
}

/// Whether `child` is a strict descendant of the `ancestor` directory (not equal to it).
/// The empty ancestor is the root, under which every non-root path lives.
fn is_strictly_under(child: &str, ancestor: &str) -> bool {
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

/// Read a scope file, returning [`MaterializationScope::full`] when the file is absent (the
/// unscoped default that keeps plain bays and the main tree behaving exactly as before).
fn read_scope_file(path: &Path) -> Result<MaterializationScope, String> {
    match std::fs::read(path) {
        Ok(bytes) => MaterializationScope::from_bytes(&bytes),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(MaterializationScope::full()),
        Err(e) => Err(format!("Error while reading the scope file \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// Write a scope file atomically (its parent folder must already exist).
fn write_scope_file(path: &Path, scope: &MaterializationScope) -> Result<(), String> {
    crate::util::file_utils::write_file_atomically(path, &scope.to_bytes())
}

/// The active bay's materialization scope (full when the bay has no scope file — i.e. it is
/// a plain, unscoped bay or the main tree).
pub fn current_scope() -> Result<MaterializationScope, String> {
    read_scope_file(&bay_scope_path())
}

/// Whether the active bay is scoped (has a non-full materialization scope). A quick gate for
/// the verbs that behave differently — or refuse — in a scoped workspace.
pub fn is_scoped() -> Result<bool, String> {
    Ok(!current_scope()?.is_full())
}

/// The warehouse fetch scope (full when unset — a full franchise; a sparse one records its
/// fetched prefixes here).
pub fn read_fetch_scope() -> Result<MaterializationScope, String> {
    read_scope_file(&fetch_scope_path())
}

/// The materialization scope of a named bay (full when the bay has no scope file). Read from
/// any checkout so a warehouse-level operation — scope-prune — can intersect every bay's scope
/// before freeing anything, not only the invoking bay's.
///
/// # Arguments
/// * `name` - The bay's name (as listed by `bay_utils::list_bays`).
pub fn read_bay_scope(name: &str) -> Result<MaterializationScope, String> {
    read_scope_file(&crate::util::bay_utils::bay_state_dir(name).join(FILE_NAME_BAY_SCOPE))
}

/// The materialization scope of the warehouse's main working tree (full when it has none).
/// The main tree keeps its bay-local state directly under the shared root, so its scope file
/// is at a fixed location every checkout can read — needed by scope-prune, which must intersect
/// the main tree's scope too, not only the invoking checkout's and the named bays'.
pub fn read_main_tree_scope() -> Result<MaterializationScope, String> {
    read_scope_file(&globals::forklift_root().join(FILE_NAME_BAY_SCOPE))
}

/// Set the active bay's materialization scope, refusing a scope that is not ⊆ the warehouse
/// fetch scope (a bay can never materialize what the warehouse never fetched, design §3.1).
///
/// # Arguments
/// * `scope` - The materialization scope to record for the active bay.
pub fn set_bay_scope(scope: &MaterializationScope) -> Result<(), String> {
    let fetch = read_fetch_scope()?;

    if !scope.subset_of(&fetch) {
        return Err(
            "The requested materialization scope is not within the warehouse's fetch scope; \
            a bay cannot materialize a path the warehouse has not fetched.".to_string()
        );
    }

    let path = bay_scope_path();

    if let Some(parent) = path.parent() {
        crate::util::file_utils::create_folder_if_not_exists(parent)?;
    }

    write_scope_file(&path, scope)
}

/// Set the warehouse fetch scope (shared across bays). A sparse franchise records the paths it
/// fetched here; `expand` unions new prefixes into it. A full (empty) scope means the store
/// holds everything, so every scope-aware fetch and walk collapses to today's behavior.
///
/// # Arguments
/// * `scope` - The fetch scope to record for the warehouse.
pub fn set_fetch_scope(scope: &MaterializationScope) -> Result<(), String> {
    let path = fetch_scope_path();

    if let Some(parent) = path.parent() {
        crate::util::file_utils::create_folder_if_not_exists(parent)?;
    }

    write_scope_file(&path, scope)
}

/// Whether the warehouse itself is sparse — its fetch scope is restricted, so some content is
/// sealed-but-unfetched. A quick gate for the operations that behave differently on a store
/// that was never fully fetched (the origin-only lift guard, franchise's bundle skip).
pub fn is_warehouse_sparse() -> Result<bool, String> {
    Ok(!read_fetch_scope()?.is_full())
}

/// Build a classified scope-refusal string (design §7.4 taxonomy). It carries the stable
/// `code`, the human `message` and a machine-actionable `next_step`, framed so the CLI can
/// decode it into a `ForkliftError` with the matching exit code. `forklift-core` never
/// prints and cannot depend on the CLI's error type, so this string is the seam.
///
/// # Arguments
/// * `code`      - One of the `CODE_*` constants in this module.
/// * `message`   - The human explanation.
/// * `next_step` - The machine-actionable recovery step.
pub fn refusal(code: &str, message: impl Into<String>, next_step: impl Into<String>) -> String {
    format!(
        "{}{}{}{}{}{}",
        REFUSAL_PREFIX,
        code,
        REFUSAL_FIELD_SEPARATOR,
        sanitize_for_framing(&message.into()),
        REFUSAL_FIELD_SEPARATOR,
        sanitize_for_framing(&next_step.into())
    )
}

/// Strip ASCII control characters (including `\u{1f}`, the field separator) out of text
/// before it is interpolated into a refusal frame. `message`/`next_step` are built by
/// formatting in caller-supplied text — often a path — and `WarehousePath::from_user_input`'s
/// control-character guard only covers paths a person typed; a path read back off disk (a
/// tree or inventory entry) never passes through that constructor and can carry `\u{1f}`
/// itself. Sanitizing here, at the one place every refusal is framed, keeps the frame
/// decodable regardless of where the text originated, rather than relying on every call site
/// to have scrubbed its input first.
fn sanitize_for_framing(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

/// Decode a scope-refusal string built by [`refusal`] into `(code, message, next_step)`.
/// Returns `None` for any string that is not a scope refusal (which the caller classifies
/// generically). Used by the CLI's error boundary.
pub fn decode_refusal(message: &str) -> Option<(&str, &str, &str)> {
    let rest = message.strip_prefix(REFUSAL_PREFIX)?;
    let mut parts = rest.splitn(3, REFUSAL_FIELD_SEPARATOR);

    let code = parts.next()?;
    let human = parts.next()?;
    let next_step = parts.next()?;

    Some((code, human, next_step))
}

/// A ready-made `scope_path_type_changed` refusal for a spine path whose entry flipped
/// between a directory and a file at the target revision (design §3.1). The spine's whole
/// job is to carry the hash forward assuming the path is still a directory; a type flip
/// breaks that assumption in a way a sparse workspace cannot safely reason about.
pub fn type_changed_refusal(path: &str) -> String {
    let next_step = "Re-scope the bay (or widen it to include the path), or resolve in a full workspace.";

    refusal(
        CODE_SCOPE_PATH_TYPE_CHANGED,
        format!(
            "The path \"{}\" is no longer a directory (or is now one) at this revision; the \
            sparse scope is no longer valid there. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `out_of_scope` refusal for a path argument outside the bay's scope.
pub fn out_of_scope_refusal(path: &str) -> String {
    let next_step = "Widen the bay's scope to include the path, or run the command in a full workspace.";

    refusal(
        CODE_OUT_OF_SCOPE,
        format!("The path \"{}\" is outside this bay's materialization scope. {}", path, next_step),
        next_step,
    )
}

/// A ready-made `out_of_scope_conflict` refusal for a merge where an out-of-scope entry (a
/// subtree, file or symlink this bay never materialized) changed on *both* sides against the
/// merge base. A one-sided out-of-scope change is resolved by hash without the
/// content; a genuine two-sided conflict cannot be — the bay has no content to reconcile — so
/// it refuses rather than guess.
pub fn out_of_scope_conflict_refusal(path: &str) -> String {
    let next_step = "Widen the bay's scope to include the path and retry, or resolve the merge in a full workspace.";

    refusal(
        CODE_OUT_OF_SCOPE_CONFLICT,
        format!(
            "The path \"{}\" changed on both sides outside this bay's materialization scope; a \
            scoped bay cannot merge it without the content. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `sparse_workspace` refusal for a whole-tree verb that does not yet support
/// running in a scoped bay.
pub fn sparse_workspace_refusal(verb: &str, next_step: &str) -> String {
    refusal(
        CODE_SPARSE_WORKSPACE,
        format!("\"{}\" is not supported in a scoped (sparse) bay yet. {}", verb, next_step),
        next_step,
    )
}

/// A ready-made `narrow_unclean` refusal: the subtree `narrow` was asked to drop still holds
/// uncommitted work. `narrow` de-materializes files unconditionally once it decides to act — it
/// has no working-directory-preserving path the way `shift`'s target-tree walk does — so it
/// refuses up front rather than silently discard staged, unstaged or untracked content. This
/// matches `shift`'s own precedent for untracked collisions: never delete something the
/// operator has not committed anywhere. No override in this round — stack, restore, park or
/// move the blocking paths out of the way, then narrow again.
///
/// # Arguments
/// * `path`      - The subtree that was asked to be narrowed away.
/// * `blocked_by` - What kind of uncommitted work is blocking it (for the message).
pub fn narrow_unclean_refusal(path: &str, blocked_by: &str) -> String {
    let next_step = format!(
        "Stack or restore the changes under \"{}\" (or move untracked files out of the way), \
        then narrow again.",
        path
    );

    refusal(
        CODE_NARROW_UNCLEAN,
        format!(
            "\"{}\" has {} that narrow would otherwise delete; narrow refuses to discard \
            uncommitted work. {}",
            path, blocked_by, next_step
        ),
        next_step,
    )
}

/// A ready-made `scope_prune_blocked` refusal: scope-prune would free content that a checkout
/// still materializes. A prune narrows the shared warehouse fetch scope and deletes the freed
/// objects, so it can never leave a checkout materializing a path the warehouse no longer
/// fetches — that would break the checkout the next time it read the now-absent content. It
/// refuses up front, naming the blocking checkout(s), rather than corrupt them.
///
/// # Arguments
/// * `path`      - The path the prune was asked to free.
/// * `blockers`  - The checkout(s) still materializing it (for the message).
pub fn scope_prune_blocked_refusal(path: &str, blockers: &str) -> String {
    let next_step = format!(
        "Narrow {} off \"{}\" first (that is bay-local and frees nothing), then prune.",
        blockers, path
    );

    refusal(
        CODE_SCOPE_PRUNE_BLOCKED,
        format!(
            "\"{}\" is still materialized by {}; pruning it would break that checkout. {}",
            path, blockers, next_step
        ),
        next_step,
    )
}

/// A ready-made `non_origin_lift` refusal for a lift from a sparse warehouse to a remote other
/// than the one it fetched against. A sparse warehouse only ever proved its out-of-scope
/// closure present on its origin; a different remote may lack objects it never verified there,
/// so the lift would fail late at the remote's closure check. Refusing up front is the clearer
/// failure.
///
/// # Arguments
/// * `origin` - The remote this warehouse was fetched (scoped) against.
/// * `other`  - The currently configured remote it is trying to lift to.
pub fn non_origin_lift_refusal(origin: &str, other: &str) -> String {
    let next_step = format!(
        "Point \"remote.url\" back at \"{}\", or run a full (unscoped) franchise against \"{}\".",
        origin, other
    );

    refusal(
        CODE_NON_ORIGIN_LIFT,
        format!(
            "This is a sparse workspace, fetched against \"{}\"; lifting to \"{}\" may fail \
            because that remote may lack out-of-scope objects this workspace never verified \
            there. {}",
            origin, other, next_step
        ),
        next_step,
    )
}

/// A ready-made `chunked_transport_unsupported` refusal for sending a large file stored in
/// chunks to a remote or into a bundle. Chunk transport (the wire-level upload/download of the
/// chunk objects themselves) has not shipped yet: a bundle or a lift can walk the tree closure
/// and carry a chunked file's *recipe* just like any other small object, but nothing today
/// negotiates or transfers the chunks a recipe references, so shipping one would silently
/// produce a signed ref (or a bundle) over content that can never be materialized elsewhere.
/// Refusing up front — client-side, before anything is sent — is the honest failure. This is a
/// transport gap, not a format one; the check is removed the moment chunk transport ships.
///
/// # Arguments
/// * `path` - The warehouse path of the chunked file that blocked the operation.
pub fn chunked_transport_refusal(path: &str) -> String {
    let next_step = "Keep this file under the chunking threshold, or wait for chunked \
        large-file transport support.".to_string();

    refusal(
        CODE_CHUNKED_TRANSPORT_UNSUPPORTED,
        format!(
            "\"{}\" is a large file stored in chunks, and sending chunked files to a remote \
            or into a bundle is not supported yet. {}",
            path, next_step
        ),
        next_step,
    )
}

/// A ready-made `oversized_transport_unsupported` refusal for sending an object above the
/// whole-object ceiling to a remote or into a bundle — the honest failure the maintainer settled
/// on for a **grandfathered** giant (an object authored, or imported via an old-version bundle,
/// before `MAX_OBJECT_BYTES` existed). It stays fully readable and checkout-able locally forever
/// (the ceiling gates writes and imports, never reads), but no migration exists that preserves its
/// signed identity, so it can never move to a remote or into a bundle: a version-3 bundle reader
/// refuses its declared length before reading a byte, and an older reader would only rediscover
/// the same problem on the far end. Refusing here — before anything is written into a bundle
/// stream or sent over the wire on a lift — is the honest failure at the source.
///
/// # Arguments
/// * `what` - What is being refused (a path, a hash, or both — whatever the caller has in hand).
/// * `len`  - The object's actual byte length.
pub fn oversized_transport_refusal(what: &str, len: u64) -> String {
    let next_step = "This object predates the whole-object size limit. It stays readable and \
        checkout-able locally, but no migration exists that would preserve its signed identity, \
        so it cannot be sent to a remote or into a bundle.".to_string();

    refusal(
        CODE_OVERSIZED_TRANSPORT_UNSUPPORTED,
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
/// * `Ok(())`      - If `len` is within the ceiling.
/// * `Err(String)` - The `oversized_transport_unsupported` refusal, otherwise.
pub fn refuse_if_over_object_ceiling(what: &str, len: usize) -> Result<(), String> {
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
        let scope = MaterializationScope::full();

        assert!(scope.is_full());
        for path in ["", "src", "src/api", "src/api/x.rs", "README.md"] {
            assert_eq!(scope.classify(path), ScopeClass::InScope, "path: {}", path);
        }
    }

    #[test]
    fn classify_is_three_valued_over_a_single_prefix() {
        let scope = MaterializationScope::from_prefixes(["src/api"]);

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
    fn classify_handles_multiple_prefixes() {
        let scope = MaterializationScope::from_prefixes(["src/api", "docs"]);

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
        let scope = MaterializationScope::from_prefixes(["/src/api/", "src/api", "docs/"]);
        assert_eq!(scope.prefixes(), &["docs".to_string(), "src/api".to_string()]);

        // A root prefix collapses to full scope.
        assert!(MaterializationScope::from_prefixes(["/"]).is_full());
        assert!(MaterializationScope::from_prefixes([""]).is_full());
    }

    #[test]
    fn scope_round_trips_through_its_on_disk_form() {
        let scope = MaterializationScope::from_prefixes(["src/api", "docs"]);
        let bytes = scope.to_bytes();

        assert_eq!(MaterializationScope::from_bytes(&bytes).unwrap(), scope);

        // The full scope round-trips as the empty file.
        assert!(MaterializationScope::full().to_bytes().is_empty());
        assert!(MaterializationScope::from_bytes(b"").unwrap().is_full());
        assert!(MaterializationScope::from_bytes(b"\n  \n").unwrap().is_full());
    }

    #[test]
    fn subset_invariant_holds_for_bay_within_fetch() {
        let fetch = MaterializationScope::from_prefixes(["src"]);

        // A bay ⊆ the fetch scope.
        assert!(MaterializationScope::from_prefixes(["src/api"]).subset_of(&fetch));
        assert!(MaterializationScope::from_prefixes(["src"]).subset_of(&fetch));

        // A bay reaching outside the fetch scope is not a subset.
        assert!(!MaterializationScope::from_prefixes(["src/api", "docs"]).subset_of(&fetch));
        assert!(!MaterializationScope::full().subset_of(&fetch));

        // Everything is a subset of the full fetch scope.
        assert!(MaterializationScope::from_prefixes(["anything"]).subset_of(&MaterializationScope::full()));
        assert!(MaterializationScope::full().subset_of(&MaterializationScope::full()));
    }

    #[test]
    fn refusal_round_trips_through_its_framing() {
        let refusal = refusal(CODE_OUT_OF_SCOPE, "message here", "do this next");
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_OUT_OF_SCOPE);
        assert_eq!(message, "message here");
        assert_eq!(next_step, "do this next");

        // A plain error is not a scope refusal.
        assert!(decode_refusal("something ordinary went wrong").is_none());
    }

    #[test]
    fn a_narrow_unclean_refusal_carries_the_stable_code_and_names_the_path() {
        let refusal = narrow_unclean_refusal("docs", "untracked file(s) (docs/draft.md)");
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_NARROW_UNCLEAN);
        assert!(message.contains("docs"), "the path is named: {}", message);
        assert!(message.contains("untracked file(s)"), "the blocker is named: {}", message);
        assert!(next_step.contains("docs"), "the recovery names the path: {}", next_step);
    }

    #[test]
    fn a_non_origin_lift_refusal_carries_the_stable_code_and_names_both_remotes() {
        let refusal = non_origin_lift_refusal("http://origin.example", "http://other.example");
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_NON_ORIGIN_LIFT);
        assert!(message.contains("http://origin.example"), "the origin is named: {}", message);
        assert!(message.contains("http://other.example"), "the target is named: {}", message);
        assert!(next_step.contains("http://origin.example"), "the recovery names the origin: {}", next_step);
    }

    #[test]
    fn a_chunked_transport_refusal_carries_the_stable_code_and_names_the_path() {
        let refusal = chunked_transport_refusal("big.bin");
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_CHUNKED_TRANSPORT_UNSUPPORTED);
        assert!(message.contains("big.bin"), "the path is named: {}", message);
        assert!(message.contains("chunks"), "the message explains why: {}", message);
        assert!(!next_step.is_empty());
    }

    #[test]
    fn an_oversized_transport_refusal_carries_the_stable_code_and_names_the_ceiling() {
        let refusal = oversized_transport_refusal(
            "\"big.bin\" (object aaaa)", crate::util::object_utils::MAX_OBJECT_BYTES as u64 + 1
        );
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
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

        let (code, _, _) = decode_refusal(&error).unwrap();
        assert_eq!(code, CODE_OVERSIZED_TRANSPORT_UNSUPPORTED);
    }

    #[test]
    fn refusal_survives_a_control_character_in_interpolated_text() {
        // A path sourced from disk (a tree/inventory entry), not `WarehousePath::from_user_input`,
        // can carry the framing separator itself. The frame must still decode cleanly, with the
        // control character stripped rather than corrupting the field boundaries.
        let hostile_path = "src/\u{1f}api";

        let refusal = out_of_scope_refusal(hostile_path);
        let (code, message, next_step) = decode_refusal(&refusal)
            .expect("a refusal built from a hostile path must still decode");

        assert_eq!(code, CODE_OUT_OF_SCOPE);
        assert!(!message.contains('\u{1f}'), "message still carries the control char: {:?}", message);
        assert!(!next_step.contains('\u{1f}'), "next_step still carries the control char: {:?}", next_step);
        assert!(message.contains("src/ api"), "control char should be replaced, not vanish: {:?}", message);
    }

    #[test]
    fn a_scope_prune_blocked_refusal_carries_the_stable_code_and_names_the_blocker() {
        let refusal = scope_prune_blocked_refusal("docs", "bay \"reviewer\"");
        let (code, message, next_step) = decode_refusal(&refusal).unwrap();

        assert_eq!(code, CODE_SCOPE_PRUNE_BLOCKED);
        assert!(message.contains("docs"), "the path is named: {}", message);
        assert!(message.contains("bay \"reviewer\""), "the blocker is named: {}", message);
        assert!(next_step.contains("Narrow"), "the recovery says how to unblock: {}", next_step);
    }
}
