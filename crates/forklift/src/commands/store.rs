use serde::Serialize;
use forklift_core::util::pack_utils::{self, StoreStatus};
use crate::output::{self, human_bytes, CommandOutput};

/// Handle the `store` command: report the object store's health — loose vs packed object
/// counts, per-pack delta density, on-disk sizes, and whether an incremental compaction or a
/// consolidating repack is due. Read-only; the counterpart of `compact` (which acts).
///
/// # Returns
/// * `Ok(())`      - The census was reported.
/// * `Err(String)` - If the object store could not be read.
pub fn handle_command() -> Result<(), String> {
    output::emit("store", &StoreReport::from(pack_utils::store_status()?));

    Ok(())
}

/// The object-store census. The `Serialize` shape is the public `--json` schema; byte counts
/// are exact integers there (the human view renders them in binary units).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct StoreReport {
    /// Loose (unpacked) object files.
    loose_objects: usize,
    /// Total on-disk bytes of the loose objects.
    loose_bytes: u64,
    /// Objects held across all packs.
    packed_objects: usize,
    /// Number of pack files.
    pack_files: usize,
    /// Objects stored as deltas across all packs.
    deltas: usize,
    /// Total on-disk bytes of the packs.
    pack_bytes: u64,
    /// Loose + packed bytes — the object store's on-disk footprint.
    total_bytes: u64,
    /// One entry per pack file.
    packs: Vec<PackReport>,
    /// The maintenance thresholds and the current verdict.
    maintenance: Maintenance,
    /// Whether this store was bulk-ingested (`import-git`, or a franchise's native bundle
    /// install) and would still benefit from a one-shot `compact --all --redelta` pass.
    /// Additive field; never acted on automatically.
    densify_suggested: bool,
}

/// One pack's line in the census.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PackReport {
    /// The pack's id (its file stem).
    id: String,
    /// Objects the pack holds.
    objects: usize,
    /// Of `objects`, how many are stored as deltas.
    deltas: usize,
    /// On-disk bytes of the pack (data file + index file).
    bytes: u64,
}

/// The maintenance picture: whether auto-maintenance is on, the effective thresholds, and
/// whether either action is due now.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Maintenance {
    /// Whether background maintenance (`maintenance.auto`) is enabled.
    auto: bool,
    /// Loose-object count above which an incremental compaction is due (`maintenance.loose`).
    loose_threshold: usize,
    /// Pack-count above which a consolidating repack is due (`maintenance.packs`).
    pack_threshold: usize,
    /// Whether an incremental compaction is due now.
    compaction_due: bool,
    /// Whether a consolidating repack is due now.
    repack_due: bool,
}

impl From<StoreStatus> for StoreReport {
    fn from(status: StoreStatus) -> StoreReport {
        StoreReport {
            loose_objects: status.loose_objects,
            loose_bytes: status.loose_bytes,
            packed_objects: status.packed_objects,
            pack_files: status.packs.len(),
            deltas: status.deltas,
            pack_bytes: status.pack_bytes,
            total_bytes: status.loose_bytes + status.pack_bytes,
            packs: status.packs.into_iter().map(|pack| PackReport {
                id: pack.id,
                objects: pack.objects,
                deltas: pack.deltas,
                bytes: pack.bytes,
            }).collect(),
            maintenance: Maintenance {
                auto: status.auto_enabled,
                loose_threshold: status.loose_threshold,
                pack_threshold: status.pack_threshold,
                compaction_due: status.incremental_due,
                repack_due: status.repack_due,
            },
            densify_suggested: status.densify_pending,
        }
    }
}

impl CommandOutput for StoreReport {
    fn render_human(&self) {
        let total_objects = self.loose_objects + self.packed_objects;
        let percent_packed = if total_objects == 0 {
            100
        } else {
            self.packed_objects * 100 / total_objects
        };

        println!("Object store");
        println!(
            "  loose:   {} object{}   ({})",
            self.loose_objects,
            plural(self.loose_objects),
            human_bytes(self.loose_bytes),
        );
        println!(
            "  packed:  {} object{} in {} pack{}   ({}{})   {}% packed",
            self.packed_objects,
            plural(self.packed_objects),
            self.pack_files,
            plural(self.pack_files),
            human_bytes(self.pack_bytes),
            if self.deltas > 0 { format!(", {} delta{}", self.deltas, plural(self.deltas)) } else { String::new() },
            percent_packed,
        );
        println!("  total:   {} on disk", human_bytes(self.total_bytes));
        println!();

        let m = &self.maintenance;
        println!("  maintenance: auto {}", if m.auto { "on" } else { "off" });
        println!(
            "    compaction  {}",
            verdict(m.compaction_due, self.loose_objects, m.loose_threshold, "loose objects"),
        );
        println!(
            "    repack      {}",
            verdict(m.repack_due, self.pack_files, m.pack_threshold, "packs"),
        );

        if self.densify_suggested {
            println!();
            println!("{}", output::DENSIFY_TIP);
        }
    }
}

/// `""` for one, `"s"` for any other count.
fn plural(count: usize) -> &'static str {
    if count == 1 { "" } else { "s" }
}

/// Render a maintenance verdict line: whether the action is due, and the current value against
/// its threshold (`due — 7000 / 6700 loose objects (over threshold)` /
/// `not due — 12 / 6700 loose objects`). Count-then-threshold-then-noun reads cleanly for any
/// count, including one.
fn verdict(due: bool, current: usize, threshold: usize, noun: &str) -> String {
    if due {
        format!("due — {} / {} {} (over threshold)", current, threshold, noun)
    } else {
        format!("not due — {} / {} {}", current, threshold, noun)
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("StoreReport", schemars::schema_for!(StoreReport)),
    ]
}
