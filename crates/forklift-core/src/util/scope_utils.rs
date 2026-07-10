//! Task-scoped sparse workspaces (§7.6), stage 1 — the local scope record and the
//! three-valued classifier every scope-aware walk branches on.
//!
//! A scoped bay materializes and operates on only chosen path subtrees of the user
//! pallet's working tree. Scope is **local only, never tracked** (it is a property of
//! *this* checkout, not the project — tracking it would push it onto collaborators, git
//! sparse-checkout's leaky middle ground). It lives in two path-prefix files:
//!
//! * the **bay materialization scope** (`<bay_root>/scope`, bay-local) — what this bay
//!   materializes and stacks; always a subset (⊆) of the fetch scope, and
//! * the **warehouse fetch scope** (`config/fetch-scope`, shared) — what the warehouse
//!   has fetched at all. In stage 1 the store is full, so the fetch scope is unset
//!   (= full) and only the bay scope restricts behavior.
//!
//! The classifier is **three-valued, not boolean** (design finding 5): a boolean
//! `in_scope` conflates two situations a walk must treat differently — a *spine* directory
//! (an ancestor of an in-scope path, walked but with its out-of-scope siblings copied by
//! hash) and a *fully in-scope* directory (descended normally). See [`ScopeClass`].
//!
//! **Meta pallets (`@office` and `.forklift/meta/*`) are never scoped** (design D8): this
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
pub const CODE_SCOPE_PATH_TYPE_CHANGED: &str = "scope_path_type_changed";
pub const CODE_SPARSE_WORKSPACE: &str = "sparse_workspace";

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

/// The warehouse fetch scope (full when unset — the stage-1 full-store default).
pub fn read_fetch_scope() -> Result<MaterializationScope, String> {
    read_scope_file(&fetch_scope_path())
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

/// A ready-made `sparse_workspace` refusal for a whole-tree verb that stage 1 does not yet
/// support in a scoped bay.
pub fn sparse_workspace_refusal(verb: &str, next_step: &str) -> String {
    refusal(
        CODE_SPARSE_WORKSPACE,
        format!("\"{}\" is not supported in a scoped (sparse) bay yet. {}", verb, next_step),
        next_step,
    )
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
}
