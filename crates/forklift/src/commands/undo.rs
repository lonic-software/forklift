use serde::Serialize;
use forklift_core::error::sanitize;
use forklift_core::util::journal_utils;
use forklift_core::util::{object_utils, pallet_utils};
use crate::commands::shift;
use crate::output::{self, CommandOutput};

/// Handle the undo command (§7.8): reverse the last state-changing operation, using the
/// undo journal. `undo` spans `stack`, `consolidate`, `cherry-pick` and `shift` — not just the
/// last stack — because each of those journals the state on both sides of it.
///
/// * Reversing a `stack`, `consolidate` or `cherry-pick` is a **soft reset**: the pallet head
///   moves back while the working directory and inventory are kept, so the undone changes end
///   up staged again (this is how a merge is reversed too).
/// * Reversing a `shift` moves back to the previous pallet, re-materializing its tree
///   (it refuses if the working directory is dirty, exactly like a forward `shift`).
///
/// An empty journal means there is nothing to undo. An entry is reversed only while the bay is
/// still where its operation left it (`journal_utils::check_undoable`); otherwise an operation
/// outside the journal ran since, and reversing the entry would undo an older operation than
/// the user's last. Nor is one reversed that would put back a cherry-pick whose source is gone.
///
/// The journal entry is only ever consumed (`journal_utils::pop`) once its undo has fully
/// succeeded — `peek` looks at it first. A refused or failed undo must leave the journal exactly
/// as it found it, so a retried `undo` sees the same entry rather than skipping straight past it.
///
/// # Returns
/// * `Ok(())`      - If an operation was undone.
/// * `Err(String)` - If there is nothing to undo, the journal cannot be read, the bay has moved
///                   past the last entry, or the reversal failed.
pub async fn handle_command() -> Result<(), String> {
    let Some(entry) = journal_utils::peek()? else {
        return Err(
            "There is nothing to undo: the undo journal is empty. Only stack, consolidate, \
            cherry-pick and shift are journaled, and a pallet's first parcel cannot be undone."
                .to_string()
        );
    };

    // Gate before mutating anything, for every kind of entry: refuse, leaving the journal
    // untouched, unless the bay is still exactly where the entry's operation left it.
    journal_utils::check_undoable(&entry)?;

    #[cfg(test)]
    tests::AFTER_GATE.with(|seam| seam.take().map(|seam| seam()));

    if entry.op == "shift" {
        // Move back to the pallet that was current before the shift, re-materializing it.
        // `shift_to` refuses up front (a dirty workdir, a missing target pallet, …) without
        // touching anything, so the entry is only popped once it has actually succeeded.
        let left = entry.after.current_pallet.clone();
        let head = shift::shift_to(&entry.before.current_pallet).await?;
        journal_utils::pop()?;

        output::emit(&Undone {
            op: entry.op,
            pallet: entry.before.current_pallet,
            left,
            undone: String::new(),
            head,
            description: String::new(),
        });

        return Ok(());
    }

    // A soft reset: restore the refs (and any consolidation or cherry-pick), keep the working
    // directory. The gate established that the current pallet is the entry's, at the head the
    // entry left. An operation that moved no head (a consolidate or cherry-pick that stopped at
    // conflicts) takes no parcel off the pallet; undoing it only abandons what it left in progress.
    let undone = match entry.after.current_head() {
        Some(after) if entry.before.heads.get(&entry.after.current_pallet) != Some(after) => after.clone(),
        _ => String::new(),
    };

    journal_utils::restore_refs(&entry)?;
    journal_utils::pop()?;

    let head = pallet_utils::read_position()?.base.unwrap_or_default();
    let description = object_utils::load_parcel(&undone).ok()
        .and_then(|parcel| parcel.description)
        .unwrap_or_default();

    output::emit(&Undone {
        op: entry.op,
        pallet: entry.before.current_pallet,
        left: String::new(),
        undone,
        head,
        description,
    });

    Ok(())
}

/// The result of an undo.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Undone {
    /// The operation that was reversed (`stack`, `consolidate`, `cherry-pick`, `shift`).
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

        if self.undone.is_empty() {
            println!("The pallet head is unchanged ({}); the {} is no longer in progress.", self.head, self.op);
            println!("The working directory is left as it is — resolve or restore it as you see fit.");
            return;
        }

        if let Some(first_line) = self.description.lines().next() {
            if !first_line.is_empty() {
                println!("  ({})", sanitize(first_line));
            }
        }

        println!("The pallet head is now {}.", self.head);
        println!("Its changes are staged again — \"stack\" to redo, or adjust them first.");
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Undone>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Warehouse};

    thread_local! {
        /// Runs once just after undo's gate passed, before anything is restored.
        pub(super) static AFTER_GATE: std::cell::Cell<Option<Box<dyn FnOnce()>>> = std::cell::Cell::new(None);
    }

    fn hash(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn an_undo_whose_current_pallet_moved_after_the_gate_is_refused_and_touches_nothing() {
        let _warehouse = Warehouse::storage_only("undo-moved-after-gate");

        // "zeta" is current and sorts after "alpha", so a restore that did not take the current
        // pallet first would reach "alpha" before finding "zeta" moved. Zeta's parcels are real,
        // one on the next, because the undo reads their ancestry when it stops.
        let tree = test_support::store_tree("a.txt", b"a\n");
        let one = test_support::store_parcel(&tree, &[]);
        let two = test_support::store_parcel(&tree, &[&one]);
        let three = test_support::store_parcel(&tree, &[&two]);
        pallet_utils::update_pallet_head("zeta", None, &one).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("zeta", Some(one.clone()))).unwrap();
        pallet_utils::update_pallet_head("alpha", None, &hash('a')).unwrap();

        let before = journal_utils::capture().unwrap();
        pallet_utils::update_pallet_head("zeta", Some(&one), &two).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("zeta", Some(two.clone()))).unwrap();
        pallet_utils::update_pallet_head("alpha", Some(&hash('a')), &hash('b')).unwrap();
        journal_utils::push_if_changed("stack", before).unwrap();

        // Another bay stacks on "zeta" between the gate and the restore.
        let (on, moved_to) = (two.clone(), three.clone());
        AFTER_GATE.with(|seam| seam.set(Some(Box::new(move || {
            pallet_utils::update_pallet_head("zeta", Some(&on), &moved_to).unwrap();
        }))));

        let error = test_support::block_on(handle_command()).expect_err("the undo must be refused");
        assert!(error.contains("has moved since"), "{}", error);

        assert_eq!(pallet_utils::get_pallet_head("zeta").unwrap(), Some(three), "the other bay's head must stand");
        assert_eq!(pallet_utils::get_pallet_head("alpha").unwrap(), Some(hash('b')), "no other ref may be restored");
        assert_eq!(pallet_utils::read_position().unwrap(), pallet_utils::BayPosition::new("zeta", Some(two)),
            "the bay still stands where the operation left it");
        assert!(journal_utils::peek().unwrap().is_some(), "a refused undo must keep its entry");
    }
}
