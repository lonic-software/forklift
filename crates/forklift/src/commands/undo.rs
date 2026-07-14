use serde::Serialize;
use forklift_core::util::journal_utils::{self, JournalEntry};
use forklift_core::util::{object_utils, pallet_utils};
use crate::commands::shift;
use crate::output::{self, CommandOutput};

/// Handle the undo command (§7.8): reverse the last state-changing operation, using the
/// undo journal. `undo` now spans `stack`, `consolidate` and `shift` — not just the last
/// stack — because each of those snapshots its pre-operation state before it runs.
///
/// * Reversing a `stack` or `consolidate` is a **soft reset**: the pallet head moves back
///   while the working directory and inventory are kept, so the undone changes end up
///   staged again (this is how a merge is reversed too — no longer refused).
/// * Reversing a `shift` moves back to the previous pallet, re-materializing its tree
///   (it refuses if the working directory is dirty, exactly like a forward `shift`).
///
/// When the journal is empty (e.g. a stack made before this feature), it falls back to the
/// classic behavior: soft-reset the current pallet's head to its first parent.
///
/// # Returns
/// * `Ok(())`      - If an operation was undone.
/// * `Err(String)` - If there is nothing to undo, or the reversal failed.
pub async fn handle_command() -> Result<(), String> {
    match journal_utils::pop()? {
        Some(entry) => undo_from_journal(entry).await,
        None => undo_last_stack(),
    }
}

/// Reverse the operation described by a journal entry.
async fn undo_from_journal(entry: JournalEntry) -> Result<(), String> {
    if entry.op == "shift" {
        // Move back to the pallet that was current before the shift, re-materializing it.
        let left = pallet_utils::get_current_pallet_name().unwrap_or_default();
        let head = shift::shift_to(&entry.current_pallet).await?;

        output::emit("undo", &Undone {
            op: entry.op,
            pallet: entry.current_pallet,
            left,
            undone: String::new(),
            head,
            description: String::new(),
        });

        return Ok(());
    }

    // A soft reset: restore the refs (and any consolidation), keep the working directory.
    let pallet = pallet_utils::get_current_pallet_name()?;
    let undone = pallet_utils::get_pallet_head(&pallet)?.unwrap_or_default();

    journal_utils::restore_refs(&entry)?;

    let head = pallet_utils::get_pallet_head(&entry.current_pallet)?.unwrap_or_default();
    let description = object_utils::load_parcel(&undone).ok()
        .and_then(|parcel| parcel.description)
        .unwrap_or_default();

    output::emit("undo", &Undone {
        op: entry.op,
        pallet: entry.current_pallet,
        left: String::new(),
        undone,
        head,
        description,
    });

    Ok(())
}

/// The classic behavior, used when the journal is empty: soft-reset the current pallet's
/// head to its first parent (a merge is reversed the same way).
fn undo_last_stack() -> Result<(), String> {
    let pallet = pallet_utils::get_current_pallet_name()?;

    let Some(head) = pallet_utils::get_pallet_head(&pallet)? else {
        return Err(format!(
            "Pallet \"{}\" has nothing stacked yet; there is nothing to undo.",
            pallet
        ));
    };

    let parcel = object_utils::load_parcel(&head)?;

    let Some(parent) = parcel.parents.first() else {
        return Err(format!(
            "The head of \"{}\" is its first parcel (no parent); there is nothing to undo.",
            pallet
        ));
    };

    pallet_utils::set_pallet_head(&pallet, parent)?;

    output::emit("undo", &Undone {
        op: "stack".to_string(),
        pallet,
        left: String::new(),
        undone: head,
        head: parent.clone(),
        description: parcel.description.unwrap_or_default(),
    });

    Ok(())
}

/// The result of an undo.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Undone {
    /// The operation that was reversed (`stack`, `consolidate`, `shift`).
    op: String,

    /// The pallet that is current after the undo.
    pallet: String,

    /// For a reversed `shift`, the pallet left behind.
    #[serde(skip_serializing_if = "String::is_empty")]
    left: String,

    /// For a soft reset, the parcel that came off the pallet (its changes are staged again).
    #[serde(skip_serializing_if = "String::is_empty")]
    undone: String,

    /// The pallet's head after the undo.
    head: String,

    /// The undone parcel's description (for orientation), when there is one.
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
}

impl CommandOutput for Undone {
    fn render_human(&self) {
        if self.op == "shift" {
            println!("Undid shift — back on pallet \"{}\" (head {}).", self.pallet, self.head);
            return;
        }

        println!("Undid {} on pallet \"{}\".", self.op, self.pallet);

        if let Some(first_line) = self.description.lines().next() {
            if !first_line.is_empty() {
                println!("  ({})", first_line);
            }
        }

        println!("The pallet head is now {}.", self.head);
        println!("Its changes are staged again — \"stack\" to redo, or adjust them first.");
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("Undone", schemars::schema_for!(Undone)),
    ]
}
