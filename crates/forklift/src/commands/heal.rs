use serde::Serialize;
use forklift_core::util::recovery_utils;
use crate::output::{self, CommandOutput};

/// Handle the `heal` command: run the durability-taint recovery verb
/// (`forklift_core::util::recovery_utils::run`).
///
/// No arguments — v1 keeps the surface minimal. See `recovery_utils`'s module doc comment for
/// the full per-verdict behavior (restage, the closure walk, partial clears); this handler's job
/// is only to turn the core result into a machine-first envelope.
///
/// A [`recovery_utils::HealOutcome`] means the taint (if any) is now fully resolved — exit 0. A
/// [`forklift_core::error::CoreError::Refusal`] (routed through the ordinary `?`/`String` bridge,
/// same as every other command) means at least one reference is still genuinely dangling, or the
/// taint is torn — exit 21, machine-coded in the refusal message.
///
/// # Returns
/// * `Ok(())`      - Nothing was tainted, or the taint is now fully cleared.
/// * `Err(String)` - A `durability_taint` refusal (torn, or an unresolved dangling reference).
pub fn handle_command() -> Result<(), String> {
    let outcome = recovery_utils::run().map_err(String::from)?;

    output::emit("heal", &HealReport {
        was_tainted: outcome.was_tainted,
        restaged: outcome.restaged,
        resolved: outcome.resolved,
        notes: outcome.notes,
    });

    Ok(())
}

/// The result of a `heal` run that fully resolved (or found nothing to resolve).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HealReport {
    /// Whether a durability taint was actually standing (an already-healthy warehouse reports
    /// `false`, with every other list empty).
    was_tainted: bool,

    /// Recorded paths that were present, verified, and freshly rewritten.
    restaged: Vec<String>,

    /// Recorded paths (or hashes) resolved without a rewrite: proven absent and unreferenced by
    /// the closure walk, or a vanished inventory shard (a staging concern, never an object-trust
    /// one — see `notes`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resolved: Vec<String>,

    /// Advisory notes that never blocked clearing — currently, the "re-run the load" remedy for
    /// each vanished inventory shard in `resolved`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,
}

impl CommandOutput for HealReport {
    fn render_human(&self) {
        if !self.was_tainted {
            println!("Nothing to heal — no durability taint is standing.");
            return;
        }

        if self.restaged.is_empty() && self.resolved.is_empty() {
            println!("Durability taint cleared.");
        } else {
            println!("Durability taint cleared:");
            for path in &self.restaged {
                println!("  restaged: {}", path);
            }
            for path in &self.resolved {
                println!("  resolved (absent and unreferenced): {}", path);
            }
        }

        for note in &self.notes {
            println!("Note: {}", note);
        }
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("HealReport", schemars::schema_for!(HealReport)),
    ]
}
