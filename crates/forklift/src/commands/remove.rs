use serde::Serialize;
use forklift_core::util::inventory_utils;
use forklift_core::util::path_utils::WarehousePath;
use crate::output::{self, CommandOutput};

/// Handle the remove command.
/// This command stages a file or directory for removal: the corresponding inventory entries
/// are marked as `Deleted` (they will not be part of the next parcel). The entries are kept
/// in the inventory so the staged removal is remembered; the working directory is untouched.
///
/// # Arguments
/// * `subject` - The path of the file or directory to stage for removal.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(subject: &str) -> Result<(), String> {
    let path = WarehousePath::from_user_input(subject)?;

    // An out-of-scope path is not materialized in a scoped bay, so there is nothing to stage
    // for removal — refuse clearly rather than silently no-op (§7.6).
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    inventory_utils::stage_removal(&path)?;

    output::emit("remove", &Removed { path: path.as_key().to_string() });

    Ok(())
}

/// The path a `remove` staged for removal. Human output stays silent; `--json` gets
/// a confirmation envelope.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Removed {
    path: String,
}

impl CommandOutput for Removed {
    fn render_human(&self) {}
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Removed", schemars::schema_for!(Removed)),
    ]
}
