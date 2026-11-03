use serde::Serialize;
use forklift_core::util::{upgrade_utils, warehouse_utils};
use crate::output::{self, CommandOutput};

/// Handle the upgrade command: upgrade the warehouse the current directory is in to the format
/// this build reads, or resume an upgrade that did not finish.
///
/// # Returns
/// * `Ok(())`      - If the warehouse is in the format this build reads.
/// * `Err(String)` - If there is no warehouse here, the current directory is a bay's, the warehouse
///   holds an unfinished clone, is in use, is in a format this build cannot upgrade, or a step of
///   the upgrade failed (it resumes on the next run).
pub fn handle_command() -> Result<(), String> {
    let root = warehouse_utils::find_warehouse_root()?;

    // Like every command that works in a warehouse, run from its root, so storage resolves there on
    // every thread a migration step uses, not only inside the storage-root scope.
    std::env::set_current_dir(&root)
        .map_err(|e| format!("Error while changing into the warehouse at \"{}\": {}", root.to_string_lossy(), e))?;

    let report = upgrade_utils::upgrade_warehouse(&root, warehouse_utils::Head::Client, &upgrade_utils::CHAIN)?;

    output::emit(&Upgraded {
        summary: report.summary(),
        from: report.from,
        to: report.to,
        resumed: report.resumed.map(|resumed| Resumed { target: resumed.target, steps_done: resumed.step }),
        steps_run: report.steps_run,
    });

    Ok(())
}

/// What `upgrade` did: the format the warehouse was in and the one it is in now (the same when
/// there was nothing to do).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Upgraded {
    from: u32,
    to: u32,

    /// The unfinished upgrade found on record and finished, if any.
    resumed: Option<Resumed>,

    /// The migration steps this run carried out.
    steps_run: u32,

    /// What it did, for a person; not part of the data.
    #[serde(skip)]
    summary: Vec<String>,
}

/// An unfinished upgrade `upgrade` found on record.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Resumed {
    /// The format it was moving to.
    target: u32,

    /// The steps of it already done when this run started.
    steps_done: u32,
}

impl CommandOutput for Upgraded {
    fn render_human(&self) {
        for line in &self.summary {
            println!("{}", line);
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Upgraded>(),
    ]
}
