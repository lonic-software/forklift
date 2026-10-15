use serde::Serialize;
use crate::output::{self, CommandOutput};

const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Handle the "version" command.
///
/// # Returns
/// * `Ok(())` - The command always succeeds.
pub fn handle_command() -> Result<(), String> {
    output::emit(&Version { version: VERSION.to_string() });

    Ok(())
}

/// The forklift version.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Version {
    version: String,
}

impl CommandOutput for Version {
    fn render_human(&self) {
        println!("Forklift version {}", self.version);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Version>(),
    ]
}
