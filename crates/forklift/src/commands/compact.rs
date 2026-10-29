use serde::Serialize;
use forklift_core::util::pack_utils;
use crate::events;
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

    let stats = pack_utils::compact(all, redelta, all, &events::progress())?;

    output::emit(&Compacted {
        all,
        objects_packed: stats.objects_packed,
        packs_written: stats.packs_written,
        loose_removed: stats.loose_removed,
        deltas: stats.deltas,
        bytes_packed: stats.bytes_packed,
        corrupt_skipped: stats.corrupt_skipped,
        over_ceiling_skipped: stats.over_ceiling_skipped,
        damaged_packs_dropped: stats.damaged_packs_dropped,
        damaged_packs_kept: stats.damaged_packs_kept.into_iter().map(|kept| KeptDamagedPack {
            index_path: kept.index_path,
            error: kept.error,
            unaccounted: kept.unaccounted,
        }).collect(),
        signature_tables_written: stats.signature_tables_written,
        signatures_moved: stats.signatures_moved,
        signature_leftovers_removed: stats.signature_leftovers_removed,
        signatures_dropped: stats.signatures_dropped,
        signature_conflicts: stats.signature_conflicts.into_iter().map(|conflict| SignatureConflict {
            parcel: conflict.parcel,
            sidecar_path: conflict.sidecar_path,
            reason: conflict.reason,
        }).collect(),
        signature_tables_unreadable: stats.signature_tables_unreadable,
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

    /// Loose objects skipped because their bytes did not decode or did not hash to their
    /// filename address; left in place rather than packed or removed.
    corrupt_skipped: usize,

    /// Loose objects skipped because decoding them would exceed the 64 MiB object ceiling —
    /// legitimate objects authored before that ceiling existed, not corruption; left in place
    /// rather than packed or removed.
    over_ceiling_skipped: usize,

    /// Damaged packs (an index that failed validation) a full repack salvaged and removed: every
    /// live object they might have held reads verified from another copy. Each entry is the
    /// index file's path. Always empty without `--all`.
    damaged_packs_dropped: Vec<String>,

    /// Damaged packs a full repack salvaged what it could from but kept, because some live
    /// object they might hold has no other verified copy. Always empty without `--all`.
    damaged_packs_kept: Vec<KeptDamagedPack>,

    /// Signature tables written: loose signatures moved into tables (a repack rewrites them all).
    signature_tables_written: usize,

    /// Loose signature files moved into a signature table this run wrote.
    signatures_moved: usize,

    /// Loose signature files removed because a table already held the same signature: leftover
    /// copies, not signatures this run moved.
    signature_leftovers_removed: usize,

    /// Signatures a full repack dropped because their parcel is no longer in the store. Always 0
    /// without `--all`.
    signatures_dropped: usize,

    /// Loose signature files left loose: one that differs from the signature already in a table
    /// for its parcel (the tabled one is what reads return), or one that is malformed.
    signature_conflicts: Vec<SignatureConflict>,

    /// Signature tables that could not be read, left as found. Each entry names the table and
    /// what to do about it.
    signature_tables_unreadable: Vec<String>,
}

/// A loose signature file a compaction left loose.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct SignatureConflict {
    /// The parcel the file signs.
    parcel: String,

    /// The file's path.
    sidecar_path: String,

    /// Why it was left loose.
    reason: String,
}

/// A damaged pack a full repack kept.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct KeptDamagedPack {
    /// The pack's index file.
    index_path: String,

    /// Why the index failed validation.
    error: String,

    /// The live objects the pack might hold that no other copy serves verified: they cannot be
    /// recovered from this store.
    unaccounted: Vec<String>,
}

impl Compacted {
    /// Warn about corrupt loose objects left in place, when there were any — shared by both
    /// render paths (a store can have only corrupt loose objects and nothing else to pack).
    fn print_corrupt_skipped_note(&self) {
        if self.corrupt_skipped > 0 {
            println!(
                "Skipped {} corrupt loose object{} — left in place. Run 'forklift audit' to check the store.",
                self.corrupt_skipped,
                if self.corrupt_skipped == 1 { "" } else { "s" },
            );
        }
    }

    /// Report the damaged packs a repack salvaged, removed or kept — shared by both render paths.
    fn print_damaged_packs_note(&self) {
        for index_path in &self.damaged_packs_dropped {
            println!(
                "Salvaged and removed the damaged pack \"{}\": every live object it held reads from \
                another copy.",
                index_path
            );
        }
        for kept in &self.damaged_packs_kept {
            println!(
                "Kept the damaged pack \"{}\" ({}). No other copy serves {} live object{} it may \
                hold, so {} cannot be recovered from this store: {}",
                kept.index_path,
                kept.error,
                kept.unaccounted.len(),
                if kept.unaccounted.len() == 1 { "" } else { "s" },
                if kept.unaccounted.len() == 1 { "it" } else { "they" },
                kept.unaccounted.join(", "),
            );
        }
    }

    /// Report what the signature sweep did and left behind — shared by both render paths.
    fn print_signatures_note(&self) {
        let plural = |count: usize| if count == 1 { "" } else { "s" };
        if self.signatures_moved > 0 {
            println!(
                "Moved {} signature{} into signature tables.",
                self.signatures_moved, plural(self.signatures_moved),
            );
        }
        if self.signature_tables_written > 0 {
            println!(
                "Wrote {} signature table{}.",
                self.signature_tables_written, plural(self.signature_tables_written),
            );
        }
        if self.signatures_dropped > 0 {
            println!(
                "Dropped {} signature{} of parcels no longer in the store.",
                self.signatures_dropped, plural(self.signatures_dropped),
            );
        }
        if self.signature_leftovers_removed > 0 {
            println!(
                "Removed {} signature file{} already held in signature tables.",
                self.signature_leftovers_removed, plural(self.signature_leftovers_removed),
            );
        }
        for conflict in &self.signature_conflicts {
            println!(
                "Left the signature file \"{}\" of parcel {} loose: {}.",
                conflict.sidecar_path, conflict.parcel, conflict.reason
            );
        }
        for message in &self.signature_tables_unreadable {
            println!("{}", message);
        }
    }

    /// Warn about over-ceiling loose objects left in place, when there were any — sibling to
    /// [`Self::print_corrupt_skipped_note`], shared by both render paths.
    fn print_over_ceiling_skipped_note(&self) {
        if self.over_ceiling_skipped > 0 {
            println!(
                "Skipped {} loose object{} larger than the 64 MiB object ceiling — left loose and \
                fully readable; this is expected for objects predating the ceiling, no action needed.",
                self.over_ceiling_skipped,
                if self.over_ceiling_skipped == 1 { "" } else { "s" },
            );
        }
    }
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
            self.print_corrupt_skipped_note();
            self.print_over_ceiling_skipped_note();
            self.print_damaged_packs_note();
            self.print_signatures_note();
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

        self.print_corrupt_skipped_note();
        self.print_over_ceiling_skipped_note();
        self.print_damaged_packs_note();
        self.print_signatures_note();
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Compacted>(),
    ]
}
