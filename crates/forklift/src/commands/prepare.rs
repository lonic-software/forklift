use serde::Serialize;
use forklift_core::util::warehouse_utils;
use crate::output::{self, CommandOutput};

/// Handle the prepare command: create the missing pieces of a warehouse in the current
/// directory. With `--verbose`, every created piece is reported.
///
/// # Arguments
/// * `verbose` - Whether to print each created piece.
///
/// # Returns
/// * `Ok(())`      - If the warehouse is prepared.
/// * `Err(String)` - If the directory holds an unfinished clone, a bay, or a `.forklift` with
///   something in it but no warehouse marker, or a folder or file could not be created.
pub fn handle_command(verbose: bool) -> Result<(), String> {
    // Preparing over an unfinished clone would hand it to every other command half-built (only
    // `clone` finishes it), and over an unmarked folder would vouch for whatever it holds.
    let cwd = std::env::current_dir()
        .map_err(|e| format!("Error while getting the current directory: {}", e))?;
    warehouse_utils::check_preparable(&cwd, warehouse_utils::Head::Client)?;

    let created = warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full)?;

    output::emit(&Prepared { created, verbose });

    Ok(())
}

/// The pieces `prepare` created (empty when the warehouse already existed).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Prepared {
    created: Vec<String>,

    /// Whether to list each created piece in human output (`--verbose`); not part of
    /// the data.
    #[serde(skip)]
    verbose: bool,
}

impl CommandOutput for Prepared {
    fn render_human(&self) {
        if self.verbose {
            for note in &self.created {
                println!("{}", note);
            }
        }

        if self.created.is_empty() {
            println!("Nothing to do.");
        } else {
            println!("Prepared warehouse.");
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Prepared>(),
    ]
}
