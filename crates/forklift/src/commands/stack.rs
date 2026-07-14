use serde::Serialize;
use forklift_core::util::stack_utils;
use crate::output::{self, CommandOutput};

/// Handle the stack command (git's "commit"):
/// build tree objects from the inventory, create a parcel pointing at them (with the
/// configured operator recorded as the stacking action), advance the current pallet's
/// head to the new parcel, and clean up the consumed staged state. A consolidation in
/// progress is completed by this command (the consolidated head becomes the second
/// parent of the new parcel).
///
/// # Arguments
/// * `description` - The (optional) parcel description.
///
/// # Returns
/// * `Ok(())`      - If the parcel was stacked successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(description: Option<String>) -> Result<(), String> {
    let (parcel, pallet) = stack_utils::stack_parcel(description).await?;

    output::emit("stack", &Stacked { parcel, pallet });

    Ok(())
}

/// The parcel a `stack` created and the pallet it advanced.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Stacked {
    parcel: String,
    pallet: String,
}

impl CommandOutput for Stacked {
    fn render_human(&self) {
        println!("Stacked parcel {} on pallet \"{}\".", self.parcel, self.pallet);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Stacked", schemars::schema_for!(Stacked)),
    ]
}
