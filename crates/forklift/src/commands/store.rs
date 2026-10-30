use serde::Serialize;
use forklift_core::util::pack_utils::{self, StoreStatus};
use crate::output::{self, human_bytes, CommandOutput};

/// Handle the `store` command: report the object store's health — loose vs packed object
/// counts, per-pack delta density, on-disk sizes, and whether an incremental compaction or a
/// consolidating repack is due. Read-only; the counterpart of `store compact` (which acts).
///
/// # Returns
/// * `Ok(())`      - The census was reported.
/// * `Err(String)` - If the object store could not be read.
pub fn handle_command() -> Result<(), String> {
    output::emit(&StoreReport::from(pack_utils::store_status()?));

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
    /// Whether this store was bulk-ingested (`git import`, or a clone's native bundle
    /// install) and would still benefit from a one-shot `store compact --all --redelta` pass.
    /// Additive field; never acted on automatically.
    densify_suggested: bool,
    /// Pack indexes that parsed but whose data file could not be loaded — their
    /// objects are quarantined, not lost: `forklift heal` can still refetch any of them under a
    /// durability taint, and restoring the data file recovers the rest. Empty on a healthy store.
    quarantined_packs: Vec<PackIndexIssue>,
    /// Packs that load but whose index is damaged in places: their sound parts are still read,
    /// and an object in a damaged part reads as unverifiable. `forklift store compact --all` salvages
    /// what the pack's data file still holds, and removes the pack when every live object it may
    /// hold reads from another copy. Empty on a healthy store.
    damaged_packs: Vec<PackIndexIssue>,
    /// Pack index files that could not even be parsed — move the file aside and
    /// re-run. Empty on a healthy store.
    unenumerable_indexes: Vec<PackIndexIssue>,
    /// Pack indexes written by a build with another pack format version: every read one might
    /// answer fails naming the version. Re-create the warehouse with this build; moving one aside
    /// would make its objects read as missing. Empty on a healthy store.
    foreign_version_indexes: Vec<PackIndexIssue>,
    /// Signature tables: parcel signatures packed many to a file.
    signature_tables: usize,
    /// Of `signature_tables`, how many are below the size bound, so a compaction would merge them.
    partial_signature_tables: usize,
    /// Loose signature files a compaction would move into a table.
    loose_signatures: usize,
    /// Signature tables that cannot be read: every signature lookup one might answer fails naming
    /// it. `error` says what to do, which depends on the cause. Empty on a healthy store.
    unreadable_signature_tables: Vec<PackIndexIssue>,
    /// Loose signature files a compaction leaves loose: each differs from the signature already in
    /// a table for its parcel (the tabled one is what reads return), or is malformed. Empty on a
    /// healthy store.
    signature_conflicts: Vec<SignatureConflictIssue>,
}

/// One entry in [`StoreReport::signature_conflicts`].
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct SignatureConflictIssue {
    /// The parcel the file signs.
    parcel: String,
    /// The file's path.
    sidecar_path: String,
    /// Why a compaction leaves it loose.
    reason: String,
}

/// One entry in any of [`StoreReport`]'s pack problem lists.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PackIndexIssue {
    /// The index file's path.
    index_path: String,
    /// Why it could not be fully trusted.
    error: String,
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
    /// Count of signature tables below the size bound above which an incremental compaction is
    /// due, to merge them (`maintenance.signatureTables`).
    signature_table_threshold: usize,
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
                signature_table_threshold: status.signature_table_threshold,
                compaction_due: status.incremental_due,
                repack_due: status.repack_due,
            },
            densify_suggested: status.densify_pending,
            quarantined_packs: status.quarantined_packs.into_iter().map(|p| PackIndexIssue {
                index_path: p.index_path,
                error: p.error,
            }).collect(),
            damaged_packs: status.damaged_packs.into_iter().map(|p| PackIndexIssue {
                index_path: p.index_path,
                error: p.error,
            }).collect(),
            unenumerable_indexes: status.unenumerable_indexes.into_iter().map(|p| PackIndexIssue {
                index_path: p.index_path,
                error: p.error,
            }).collect(),
            foreign_version_indexes: status.foreign_version_indexes.into_iter().map(|p| PackIndexIssue {
                index_path: p.index_path,
                error: p.error,
            }).collect(),
            signature_tables: status.signature_tables,
            partial_signature_tables: status.partial_signature_tables,
            loose_signatures: status.loose_signatures,
            unreadable_signature_tables: status.unreadable_signature_tables.into_iter().map(|p| PackIndexIssue {
                index_path: p.index_path,
                error: p.error,
            }).collect(),
            signature_conflicts: status.signature_conflicts.into_iter().map(|c| SignatureConflictIssue {
                parcel: c.parcel,
                sidecar_path: c.sidecar_path,
                reason: c.reason,
            }).collect(),
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
        if self.signature_tables > 0 || self.loose_signatures > 0 {
            println!(
                "  signatures: {} table{} ({} below the size bound), {} loose",
                self.signature_tables,
                plural(self.signature_tables),
                self.partial_signature_tables,
                self.loose_signatures,
            );
        }
        println!();

        let m = &self.maintenance;
        println!("  maintenance: auto {}", if m.auto { "on" } else { "off" });
        let (loose, noun) = if self.loose_signatures > 0 {
            (self.loose_objects + self.loose_signatures, "loose objects and signatures")
        } else {
            (self.loose_objects, "loose objects")
        };
        if pack_utils::signature_tables_due(self.partial_signature_tables, m.signature_table_threshold) {
            println!(
                "    compaction  {}",
                verdict(true, self.partial_signature_tables, m.signature_table_threshold, "small signature tables"),
            );
        } else {
            println!("    compaction  {}", verdict(m.compaction_due, loose, m.loose_threshold, noun));
        }
        println!(
            "    repack      {}",
            verdict(m.repack_due, self.pack_files, m.pack_threshold, "packs"),
        );

        if self.densify_suggested {
            println!();
            println!("{}", output::DENSIFY_TIP);
        }

        // FORK-47: surfaced unconditionally whenever any list is non-empty — never silent.
        let problems = [&self.quarantined_packs, &self.damaged_packs, &self.unenumerable_indexes, &self.foreign_version_indexes];
        if problems.iter().any(|list| !list.is_empty()) {
            println!();
            println!("  pack problems:");
            for issue in &self.quarantined_packs {
                println!("    quarantined:   \"{}\" — {}", issue.index_path, issue.error);
            }
            for issue in &self.damaged_packs {
                println!("    damaged:       \"{}\" — {}", issue.index_path, issue.error);
            }
            for issue in &self.unenumerable_indexes {
                println!("    unenumerable:  \"{}\" — {}", issue.index_path, issue.error);
            }
            for issue in &self.foreign_version_indexes {
                println!("    other format:  \"{}\" — {}", issue.index_path, issue.error);
            }
            println!(
                "    quarantined objects are recoverable via \"forklift heal\"; a damaged pack's \
                sound parts are still read, and \"forklift store compact --all\" salvages it, recovering \
                what its data file still holds (if it keeps the pack, or refuses because the pack \
                could not be completely salvaged, the objects it reports cannot be recovered from \
                this store); an unenumerable index should be moved aside, then re-run; \
                an index of another format means re-creating the warehouse with this build (moving \
                it aside would make its objects read as missing)."
            );
        }

        if !self.unreadable_signature_tables.is_empty() || !self.signature_conflicts.is_empty() {
            println!();
            println!("  signature problems:");
            for issue in &self.unreadable_signature_tables {
                println!("    unreadable table:  {}", issue.error);
            }
            for conflict in &self.signature_conflicts {
                println!(
                    "    left loose:        \"{}\" (parcel {}) — {}",
                    conflict.sidecar_path, conflict.parcel, conflict.reason
                );
            }
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
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<StoreReport>(),
    ]
}
