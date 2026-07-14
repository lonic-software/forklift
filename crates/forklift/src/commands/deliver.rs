use serde::Serialize;
use forklift_core::util::deliver_utils;
use crate::output::{self, CommandOutput};

/// Handle the deliver command (§7.3): squash the current (draft) pallet's checkpoint
/// trail into one clean signed parcel on the target pallet, and keep the trail as a
/// signed delivery manifest entry on that parcel.
///
/// The draft pallet is left intact (its checkpoints stay browsable via `history`), and
/// the current pallet becomes the target. Because the delivered parcel carries the draft
/// head's tree, the working directory does not change.
///
/// # Arguments
/// * `target`  - The pallet to deliver onto.
/// * `message` - The delivered parcel's message (`None` uses a default).
///
/// # Returns
/// * `Ok(())`      - If the delivery completed.
/// * `Err(String)` - If there is nothing to deliver, trust is not established, or an
///                   operation failed.
pub fn handle_command(target: &str, message: Option<String>) -> Result<(), String> {
    let outcome = deliver_utils::deliver(target, message)?;

    output::emit("deliver", &DeliverReport {
        delivered: outcome.delivered,
        target: outcome.target,
        source: outcome.source,
        trail_head: outcome.trail_head,
        checkpoints: outcome.checkpoints,
        manifest_head: outcome.manifest_head,
    });

    Ok(())
}

/// The result of a delivery.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct DeliverReport {
    /// The clean squashed parcel now on the target pallet.
    delivered: String,

    /// The target pallet (now the current pallet).
    target: String,

    /// The draft pallet the trail came from (kept).
    source: String,

    /// The trail tip that was squashed.
    trail_head: String,

    /// How many checkpoints were squashed.
    checkpoints: usize,

    /// The manifest parcel that recorded the delivery.
    manifest_head: String,
}

impl CommandOutput for DeliverReport {
    fn render_human(&self) {
        println!(
            "Delivered {} checkpoint(s) from \"{}\" onto \"{}\" as parcel {}.",
            self.checkpoints, self.source, self.target, self.delivered
        );
        println!(
            "The trail is kept on \"{}\" (tip {}) and recorded on the parcel's manifest \
            (\"manifest show {}\").",
            self.source, self.trail_head, self.delivered
        );
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("DeliverReport", schemars::schema_for!(DeliverReport)),
    ]
}
