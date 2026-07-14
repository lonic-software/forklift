use serde::Serialize;
use forklift_core::util::inventory_utils;
use forklift_core::util::path_utils::WarehousePath;
use crate::output::{self, CommandOutput};

/// Handle the load command.
/// This command adds the given file or directory to its corresponding inventory.
///
/// # Arguments
/// * `target` - The path of the file or directory to load.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(target: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // In a scoped bay an out-of-scope path is not materialized, so staging it would be a
    // confusing "not in the inventory" (or a silent no-op) — refuse it clearly instead (§7.6).
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    inventory_utils::add_changes_to_inventory(&path).await?;

    output::emit("load", &Loaded { path: path.as_key().to_string() });

    Ok(())
}

/// The path a `load` staged. Human output stays silent (as it always has); `--json`
/// still gets a confirmation envelope so a program sees a result.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Loaded {
    path: String,
}

impl CommandOutput for Loaded {
    fn render_human(&self) {}
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Loaded", schemars::schema_for!(Loaded)),
    ]
}
