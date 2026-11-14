use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::globals;
use forklift_core::util::scope_utils::{self, ScopeClass};
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

/// What a read of a path at a revision loads below the root tree, which decides what a sparse
/// warehouse must have fetched for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PathRead {
    /// The file's content, as `show` reads it: every directory on the path, and the file.
    Content,

    /// The file's entry in its directory, the hash naming its content, as `redact record` reads
    /// it: the directories on the path only.
    Entry,
}

/// Refuse a read of `path` at a revision that would load what this sparse warehouse never
/// fetched, with the stable `out_of_scope` code and how to fetch it. Unrefused, the read fails as a
/// lost object and names `forklift heal`, which finds nothing lost. Decided from the fetch scope
/// before anything is read, as [`ensure_path_in_scope`] decides from the checkout scope. A no-op in
/// a warehouse that is not sparse.
///
/// # Arguments
/// * `path` - The path as the read walks it, split on "/".
/// * `read` - What the read loads.
pub fn ensure_fetched(path: &str, read: PathRead) -> Result<(), String> {
    let fetch = scope_utils::read_fetch_scope()?;
    let components: Vec<&str> = path.split('/').collect();
    let loaded = match read {
        PathRead::Content => components.len(),
        PathRead::Entry => components.len() - 1,
    };

    let Some(depth) = (1..=loaded)
        .find(|&depth| fetch.classify(&components[..depth].join("/")) == ScopeClass::OutOfScope)
    else {
        return Ok(());
    };

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

    Err(CoreError::refusal(
        RefusalCode::OutOfScope,
        format!(
            "The path \"{}\" is outside this warehouse's fetch scope, so its content was never fetched.",
            path
        ),
        next_step,
    ).into())
}

/// Whether `directory` is a directory at this checkout's base, the tree `scope fetch` widens
/// against. Only the trees holding its entry are read, never its own, which a sparse warehouse
/// may not have fetched.
fn is_directory_at_base(directory: &str) -> Result<bool, String> {
    let Some(base) = pallet_utils::read_position()?.base else {
        return Ok(false);
    };
    let mut tree = object_utils::load_tree(&object_utils::load_parcel(&base)?.tree_hash)?;
    let (parents, name) = match directory.rsplit_once('/') {
        Some((parents, name)) => (Some(parents), name),
        None => (None, directory),
    };

    for component in parents.into_iter().flat_map(|parents| parents.split('/')) {
        let subtree = tree.get_subtrees()
            .find(|(name, _)| name.as_str() == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(hash) => tree = object_utils::load_tree(&hash)?,
            None => return Ok(false),
        }
    }

    Ok(tree.get_subtrees().any(|(entry, _)| entry.as_str() == name))
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
