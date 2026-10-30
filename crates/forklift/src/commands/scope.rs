use serde::Serialize;
use forklift_core::error::CoreError;
use forklift_core::globals;
use forklift_core::util::scope_utils::{self, ScopeClass};
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
        implicit_scope: bay_scope.implicit_members().iter().map(|path| path.to_string()).collect(),
        fetch_scope: fetch_scope.prefixes().to_vec(),
        implicit_fetch_scope: fetch_scope.implicit_members().iter().map(|path| path.to_string()).collect(),
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
    /// file, so its rules apply to the checkout. Empty when the bay is not scoped.
    implicit_scope: Vec<String>,

    /// The warehouse's fetch-scope prefixes (empty = fully fetched; a sparse clone records
    /// its fetched prefixes here).
    fetch_scope: Vec<String>,

    /// The paths a sparse fetch scope holds without listing them as prefixes: the root ignore
    /// file, which the store fetches along with the prefixes. Empty when fully fetched.
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
