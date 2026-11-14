use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::globals;
use forklift_core::util::scope_utils::{self, ScopeClass};
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::{object_utils, pallet_utils};
use crate::output::{self, CommandOutput};

/// Refuse a path argument that is outside the active checkout's scope (§7.6),
/// with the stable `out_of_scope` code. A no-op in a full (unscoped) bay or the main tree.
///
/// # Arguments
/// * `key` - The warehouse path key of the argument.
pub fn ensure_path_in_scope(key: &str) -> Result<(), CoreError> {
    if scope_utils::current_scope()?.classify(key) == ScopeClass::OutOfScope {
        return Err(scope_utils::out_of_scope_refusal(key));
    }

    Ok(())
}

/// Tell a failed read of `path` in the tree `root_tree` what it is when the store lacks what this
/// sparse warehouse never fetched: a refusal with the stable `out_of_scope` code and how to fetch
/// it, in place of an `object_unreadable` that names `forklift heal`, which finds nothing lost.
///
/// Decided after the read, from what it failed on, so a read the store can serve (content shared
/// with a fetched path, a path that does not exist) is never refused. Only an `object_unreadable`
/// is told again, and only when the path leaves the fetch scope and every tree above that point
/// loads: a lost tree that should be here stays damage.
/// Every other error, and every error in a warehouse that is not sparse, is returned as it is; a
/// fetch scope that cannot be read is returned in its place.
///
/// # Arguments
/// * `root_tree` - The root tree of the revision the read walked.
/// * `path`      - The path as the user gave it.
/// * `error`     - The read's error.
pub fn explain_unfetched(root_tree: &str, path: &str, error: String) -> String {
    if forklift_core::error::deframe(&error).map(|(code, _, _)| code) != Some("object_unreadable") {
        return error;
    }

    // A fetch scope that cannot be read is the problem to report, never hidden behind the read's.
    let fetch = match scope_utils::read_fetch_scope() {
        Ok(fetch) => fetch,
        Err(scope_error) => return scope_error,
    };

    // Anything else that stops the refusal being decided leaves the read's own error standing.
    match unfetched_refusal(&fetch, root_tree, path) {
        Ok(Some(refusal)) => refusal,
        _ => error,
    }
}

/// The `out_of_scope` refusal [`explain_unfetched`] gives; `None` when the path stays inside the
/// fetch scope or an entry above the boundary is not a directory, and an error when a tree above
/// the boundary does not load. The caller keeps the read's own error in both of those cases.
fn unfetched_refusal(fetch: &scope_utils::PathScope, root_tree: &str, path: &str) -> Result<Option<String>, String> {
    let key = WarehousePath::from_user_input(path)?.as_key().to_string();
    let components: Vec<&str> = key.split('/').collect();
    let Some(depth) = (1..=components.len())
        .find(|&depth| fetch.classify(&components[..depth].join("/")) == ScopeClass::OutOfScope)
    else {
        return Ok(None);
    };

    // Every tree above the boundary is one this warehouse holds: one that does not load is damage.
    let mut tree = object_utils::load_tree(root_tree)?;

    for component in &components[..depth - 1] {
        let Some(hash) = subtree_hash(&tree, component) else {
            return Ok(None);
        };
        tree = object_utils::load_tree(&hash)?;
    }

    // The directory a fetch would bring in: the first one on the path outside the fetch scope, or,
    // for a file at a level the fetch scope only passes through, the directory holding it.
    let directory = match depth < components.len() {
        true => components[..depth].join("/"),
        false => components[..depth - 1].join("/"),
    };

    let next_step = match directory.is_empty() {
        true => "Clone the remote again without --only, into a new directory.".to_string(),
        false => match is_directory_at_base(&directory)? {
            true => format!("forklift scope fetch {}", directory),
            false => format!(
                "Read it in a full clone: \"forklift scope fetch\" takes a directory at this checkout's \
                base, and \"{}\" is not one there.",
                directory
            ),
        },
    };

    Ok(Some(CoreError::refusal(
        RefusalCode::OutOfScope,
        format!(
            "The path \"{}\" is outside this warehouse's fetch scope: reading it needs \"{}\", which was \
            never fetched.",
            key, components[..depth].join("/")
        ),
        next_step,
    ).into()))
}

/// The hash of the subtree `name` in `tree`, if it has one.
fn subtree_hash(tree: &forklift_core::model::tree_item::TreeItem, name: &str) -> Option<String> {
    tree.get_subtrees().find(|(entry, _)| entry.as_str() == name).map(|(_, item)| item.hash.clone())
}

/// Whether `directory` is a directory at this checkout's base, the tree `scope fetch` widens
/// against. Only the trees holding its entry are read, never its own, which a sparse warehouse
/// may not have fetched.
pub(crate) fn is_directory_at_base(directory: &str) -> Result<bool, String> {
    let Some(base) = pallet_utils::read_position()?.base else {
        return Ok(false);
    };

    names_directory(&object_utils::load_parcel(&base)?.tree_hash, directory)
}

/// Whether the warehouse path `key` names a directory in the root tree `root_tree`. Only the trees
/// holding its entry are read, never its own, which a sparse warehouse may not have fetched.
pub(crate) fn names_directory(root_tree: &str, key: &str) -> Result<bool, String> {
    let mut tree = object_utils::load_tree(root_tree)?;
    let (parents, name) = match key.rsplit_once('/') {
        Some((parents, name)) => (Some(parents), name),
        None => (None, key),
    };

    for component in parents.into_iter().flat_map(|parents| parents.split('/')) {
        match subtree_hash(&tree, component) {
            Some(hash) => tree = object_utils::load_tree(&hash)?,
            None => return Ok(false),
        }
    }

    Ok(subtree_hash(&tree, name).is_some())
}

/// Refuse a whole-tree verb that does not yet support running in a scoped (sparse) bay
/// (§7.6), with the stable `sparse_workspace` code and a recovery step. A no-op in a full
/// (unscoped) bay or the main tree.
///
/// # Arguments
/// * `verb`      - The command name, for the message.
/// * `next_step` - The machine-actionable recovery step.
pub fn refuse_in_scoped_bay(verb: &str, next_step: &str) -> Result<(), CoreError> {
    if scope_utils::is_scoped()? {
        return Err(scope_utils::sparse_workspace_refusal(verb, next_step));
    }

    Ok(())
}

/// Handle `scope status` and bare `scope` (§7.6): report this checkout's scope and the
/// warehouse fetch scope. Read-only.
///
/// # Returns
/// * `Ok(())`      - If the status was reported.
/// * `Err(String)` - If a scope file could not be read.
pub fn handle_command() -> Result<(), String> {
    let bay_scope = scope_utils::current_scope()?;
    let fetch_scope = scope_utils::read_fetch_scope()?;

    output::emit(&ScopeStatus {
        bay: globals::active_bay(),
        scoped: !bay_scope.is_full(),
        checkout_scope: bay_scope.prefixes().to_vec(),
        implicit_scope: bay_scope.implicit_members().to_vec(),
        fetch_scope: fetch_scope.prefixes().to_vec(),
        implicit_fetch_scope: fetch_scope.implicit_members().to_vec(),
    });

    Ok(())
}

/// The sparse-workspace scope of the current bay. An empty prefix list means the full
/// tree (an unscoped bay or the main tree).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ScopeStatus {
    /// The active bay's name (`null` in the main tree).
    #[serde(skip_serializing_if = "Option::is_none")]
    bay: Option<String>,

    /// Whether this bay is scoped (has a non-full checkout scope).
    scoped: bool,

    /// The bay's in-scope prefixes (empty = the full tree).
    checkout_scope: Vec<String>,

    /// The paths a scoped bay holds in scope without listing them as prefixes: the root ignore
    /// files and the `.gitignore` of each folder above a prefix, so their rules apply to the
    /// checkout. Empty when the bay is not scoped.
    implicit_scope: Vec<String>,

    /// The warehouse's fetch-scope prefixes (empty = fully fetched; a sparse clone records
    /// its fetched prefixes here).
    fetch_scope: Vec<String>,

    /// The paths a sparse fetch scope holds without listing them as prefixes: the root ignore
    /// files and the `.gitignore` of each folder above a prefix, which the store fetches along
    /// with the prefixes. Empty when fully fetched.
    implicit_fetch_scope: Vec<String>,
}

impl CommandOutput for ScopeStatus {
    fn render_human(&self) {
        let where_ = match &self.bay {
            Some(bay) => format!("bay \"{}\"", bay),
            None => "the main tree".to_string(),
        };

        if !self.scoped {
            println!("{} checks out the full tree (not scoped).", where_);
        } else {
            println!("{} is scoped (sparse). Checkout scope:", where_);
            for prefix in &self.checkout_scope {
                println!("  {}", prefix);
            }
            for path in &self.implicit_scope {
                println!("  {} (always in scope)", path);
            }
        }

        if self.fetch_scope.is_empty() {
            println!("Warehouse fetch scope: full (the store holds everything).");
        } else {
            println!("Warehouse fetch scope:");
            for prefix in &self.fetch_scope {
                println!("  {}", prefix);
            }
            for path in &self.implicit_fetch_scope {
                println!("  {} (always in scope)", path);
            }
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ScopeStatus>(),
    ]
}
