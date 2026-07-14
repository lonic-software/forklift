use serde::Serialize;
use forklift_core::error::CoreError;
use forklift_core::globals;
use forklift_core::util::scope_utils::{self, ScopeClass};
use crate::cli::ScopeAction;
use crate::output::{self, CommandOutput};

/// Refuse a path argument that is outside the active bay's materialization scope (§7.6),
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

/// Handle the scope command (§7.6): report the sparse-workspace scope — this bay's
/// materialization scope and the warehouse fetch scope. Read-only.
///
/// # Arguments
/// * `action` - The subcommand (`None` shows the status, same as `status`).
///
/// # Returns
/// * `Ok(())`      - If the status was reported.
/// * `Err(String)` - If a scope file could not be read.
pub fn handle_command(action: Option<ScopeAction>) -> Result<(), String> {
    match action {
        Some(ScopeAction::Status) | None => status(),
    }
}

/// Report the active bay's materialization scope and the warehouse fetch scope.
fn status() -> Result<(), String> {
    let bay_scope = scope_utils::current_scope()?;
    let fetch_scope = scope_utils::read_fetch_scope()?;

    output::emit("scope", &ScopeStatus {
        bay: globals::active_bay(),
        scoped: !bay_scope.is_full(),
        materialization_scope: bay_scope.prefixes().to_vec(),
        fetch_scope: fetch_scope.prefixes().to_vec(),
    });

    Ok(())
}

/// The sparse-workspace scope of the current bay (§7.6). An empty prefix list means the full
/// tree (an unscoped bay or the main tree).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ScopeStatus {
    /// The active bay's name (`null` in the main tree).
    #[serde(skip_serializing_if = "Option::is_none")]
    bay: Option<String>,

    /// Whether this bay is scoped (has a non-full materialization scope).
    scoped: bool,

    /// The bay's in-scope prefixes (empty = the full tree).
    materialization_scope: Vec<String>,

    /// The warehouse's fetch-scope prefixes (empty = fully fetched; a sparse franchise records
    /// its fetched prefixes here).
    fetch_scope: Vec<String>,
}

impl CommandOutput for ScopeStatus {
    fn render_human(&self) {
        let where_ = match &self.bay {
            Some(bay) => format!("bay \"{}\"", bay),
            None => "the main tree".to_string(),
        };

        if !self.scoped {
            println!("{} materializes the full tree (not a scoped bay).", where_);
        } else {
            println!("{} is a scoped (sparse) bay. Materialization scope:", where_);
            for prefix in &self.materialization_scope {
                println!("  {}", prefix);
            }
        }

        if self.fetch_scope.is_empty() {
            println!("Warehouse fetch scope: full (the store holds everything).");
        } else {
            println!("Warehouse fetch scope:");
            for prefix in &self.fetch_scope {
                println!("  {}", prefix);
            }
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ScopeStatus", schemars::schema_for!(ScopeStatus)),
    ]
}
