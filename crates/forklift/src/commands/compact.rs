use serde::Serialize;
use forklift_core::util::pack_utils;
use crate::output::{self, CommandOutput};

/// Handle the compact command: pack the warehouse's objects into a few dense pack files
/// (see `docs/OBJECT_STORE_SCALING.md`). Deltas similar objects, and never removes an
/// original until the pack that holds it is durably written, so it is safe to interrupt.
///
/// # Arguments
/// * `all` - Repack existing packs too: drop unreachable (garbage) objects and consolidate,
///   rather than only sweeping the loose set into a new pack.
/// * `redelta` - Re-encode every live object (packed or loose) to delta-compress across the
///   whole store, instead of a repack's usual verbatim copy of already-packed records. Only
///   meaningful with `all`; refused otherwise (a plain incremental compact never touches
///   packed records, so there would be nothing for it to redo).
///
/// # Returns
/// * `Ok(())`      - If the store was compacted (a no-op when there is nothing to do).
/// * `Err(String)` - If the store could not be compacted (no object is lost on failure), or if
///   `redelta` was passed without `all`.
pub fn handle_command(all: bool, redelta: bool) -> Result<(), String> {
    if redelta && !all {
        return Err(
            "--redelta re-encodes everything and only makes sense with --all.".to_string()
        );
    }

    let stats = pack_utils::compact(all, redelta)?;

    output::emit("compact", &Compacted {
        all,
        objects_packed: stats.objects_packed,
        packs_written: stats.packs_written,
        loose_removed: stats.loose_removed,
        deltas: stats.deltas,
        bytes_packed: stats.bytes_packed,
    });

    Ok(())
}

/// The result of a compaction.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Compacted {
    /// Whether this was a full repack (existing packs rewritten, garbage dropped).
    all: bool,

    /// Objects packed.
    objects_packed: usize,

    /// Packs written (more than one when the set crossed a rollover threshold).
    packs_written: usize,

    /// Original files removed after their pack was durably written.
    loose_removed: usize,

    /// Of the packed objects, how many were stored as deltas against a similar base.
    deltas: usize,

    /// Total bytes written into the packs (delta-compressed where deltas were used).
    bytes_packed: u64,
}

impl CommandOutput for Compacted {
    fn render_human(&self) {
        if self.objects_packed == 0 {
            let nothing = if self.all {
                "Nothing to repack — the object store is empty."
            } else {
                "Nothing to compact — the object store has no loose objects."
            };
            println!("{}", nothing);
            return;
        }

        let deltas = if self.deltas > 0 {
            format!(", {} as delta{}", self.deltas, if self.deltas == 1 { "" } else { "s" })
        } else {
            String::new()
        };

        let verb = if self.all { "Repacked" } else { "Compacted" };

        println!(
            "{} {} object{} into {} pack{}{} ({}).",
            verb,
            self.objects_packed,
            if self.objects_packed == 1 { "" } else { "s" },
            self.packs_written,
            if self.packs_written == 1 { "" } else { "s" },
            deltas,
            output::human_bytes(self.bytes_packed),
        );
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Compacted", schemars::schema_for!(Compacted)),
    ]
}
