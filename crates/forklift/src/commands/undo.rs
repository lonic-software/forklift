use serde::Serialize;
use forklift_core::error::sanitize;
use forklift_core::util::journal_utils;
use forklift_core::util::{object_utils, pallet_utils};
use crate::commands::shift;
use crate::output::{self, CommandOutput};

/// Handle the undo command (§7.8): reverse the last state-changing operation, using the
/// undo journal. `undo` spans `stack`, `consolidate`, `cherry-pick`, `shift` and a pallet delete —
/// not just the last stack — because each of those journals the state on both sides of it.
///
/// * Reversing a `stack`, `consolidate` or `cherry-pick` is a **soft reset**: the pallet head
///   moves back while the working directory and inventory are kept, so the undone changes end
///   up staged again (this is how a merge is reversed too).
/// * Reversing a `shift` moves back to the previous pallet, re-materializing its tree
///   (it refuses if the working directory is dirty, exactly like a forward `shift`).
/// * Reversing a pallet delete brings the pallet back at the head it was deleted at, without
///   touching the bay (see `journal_utils::restore_deleted`); with `as_name`, under that name.
///
/// An empty journal means there is nothing to undo. An entry is reversed only while the bay is
/// still where its operation left it (`journal_utils::check_undoable`); otherwise an operation
/// outside the journal ran since, and reversing the entry would undo an older operation than
/// the user's last. Nor is one reversed that would put back a cherry-pick whose source is gone. A
/// delete changed no bay state, so nothing the bay did since stops its undo.
///
/// The journal entry is only ever consumed (`journal_utils::pop`) once its undo has fully
/// succeeded — `peek` looks at it first. A refused or failed undo must leave the journal exactly
/// as it found it, so a retried `undo` sees the same entry rather than skipping straight past it.
///
/// # Arguments
/// * `as_name` - For `undo --as`: the name to bring a deleted pallet back under. Refused unless the
///               last entry is a delete.
///
/// # Returns
/// * `Ok(())`      - If an operation was undone.
/// * `Err(String)` - If there is nothing to undo, the journal cannot be read, the bay has moved
///                   past the last entry, or the reversal failed.
pub async fn handle_command(as_name: Option<&str>) -> Result<(), String> {
    let Some(entry) = journal_utils::peek()? else {
        return Err(
            "There is nothing to undo: the undo journal is empty. Only stack, consolidate, \
            pick, shift and a pallet delete are journaled, and a pallet's first parcel \
            cannot be undone."
                .to_string()
        );
    };

    if as_name.is_some() && entry.op != journal_utils::DELETE_OP {
        return Err(format!(
            "\"forklift undo --as\" brings a deleted pallet back under another name, but the last journaled \
            operation is a \"{}\", not a pallet delete. Nothing was undone.",
            entry.op
        ));
    }

    // Gate before mutating anything, for every kind of entry: refuse, leaving the journal
    // untouched, unless the bay is still exactly where the entry's operation left it.
    journal_utils::check_undoable(&entry)?;

    #[cfg(test)]
    tests::AFTER_GATE.with(|seam| seam.take().map(|seam| seam()));

    if entry.op == journal_utils::DELETE_OP {
        // The bay is left alone: a delete changed none of its state. It is read first, so nothing
        // after the pop can fail.
        let position = pallet_utils::read_position()?;
        let restored = journal_utils::restore_deleted(&entry, as_name)?;
        journal_utils::pop()?;

        output::emit(&Undone {
            op: entry.op,
            pallet: position.pallet,
            left: String::new(),
            undone: String::new(),
            head: position.base.unwrap_or_default(),
            description: String::new(),
            restored: restored.deleted,
            restored_as: restored.name,
            restored_head: restored.head,
        });

        return Ok(());
    }

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
            restored: String::new(),
            restored_as: String::new(),
            restored_head: String::new(),
        });

        return Ok(());
    }

    // A soft reset: restore the refs (and any consolidation or cherry-pick), keep the working
    // directory. The gate established that the current pallet is the entry's, at the head the
    // entry left. An operation that moved no head (a consolidate or cherry-pick that stopped at
    // conflicts) takes no parcel off the pallet; undoing it only abandons what it left in progress.
    let undone = match entry.after.current_head() {
        Some(after) if entry.before.head(&entry.after.current_pallet) != Some(after) => after.clone(),
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
        restored: String::new(),
        restored_as: String::new(),
        restored_head: String::new(),
    });

    Ok(())
}

/// The result of an undo.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Undone {
    /// The operation that was reversed (`stack`, `consolidate`, `cherry-pick`, `shift`, `delete`).
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

    /// For a reversed `delete`, the pallet that was deleted.
    #[serde(skip_serializing_if = "String::is_empty")]
    restored: String,

    /// For a reversed `delete`, the name the pallet is back under: its own, or the one `--as` gave.
    #[serde(skip_serializing_if = "String::is_empty")]
    restored_as: String,

    /// For a reversed `delete`, the head the pallet is back at (the one it was deleted at).
    #[serde(skip_serializing_if = "String::is_empty")]
    restored_head: String,
}

impl CommandOutput for Undone {
    fn render_human(&self) {
        if self.op == journal_utils::DELETE_OP && self.restored_as == self.restored {
            println!("Undid delete — pallet \"{}\" is back at {}.", self.restored, self.restored_head);
            return;
        }

        if self.op == journal_utils::DELETE_OP {
            println!(
                "Undid delete — deleted pallet \"{}\" is back as \"{}\", at {}.",
                self.restored, self.restored_as, self.restored_head
            );
            println!(
                "Older undo entries still refer to \"{}\": undoing them acts on whatever that name holds then.",
                self.restored
            );
            return;
        }

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
        println!("Its changes are staged again — \"forklift stack\" to redo, or adjust them first.");
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
        pallet_utils::update_pallet_head("zeta", None, &one, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("zeta", Some(one.clone()))).unwrap();
        let (alpha_a, alpha_b) = (test_support::store_parcel(&tree, &[&one]), test_support::store_parcel(&tree, &[&two]));
        pallet_utils::update_pallet_head("alpha", None, &alpha_a, &pallet_utils::RefPreconditions::NONE).unwrap();

        let before = journal_utils::capture().unwrap();
        pallet_utils::update_pallet_head("zeta", Some(&one), &two, &pallet_utils::RefPreconditions::NONE).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("zeta", Some(two.clone()))).unwrap();
        pallet_utils::update_pallet_head("alpha", Some(&alpha_a), &alpha_b, &pallet_utils::RefPreconditions::NONE).unwrap();
        journal_utils::push_if_changed("stack", before).unwrap();

        // Another bay stacks on "zeta" between the gate and the restore.
        let (on, moved_to) = (two.clone(), three.clone());
        AFTER_GATE.with(|seam| seam.set(Some(Box::new(move || {
            pallet_utils::update_pallet_head("zeta", Some(&on), &moved_to, &pallet_utils::RefPreconditions::NONE).unwrap();
        }))));

        let error = test_support::block_on(handle_command(None)).expect_err("the undo must be refused");
        assert!(error.contains("has moved since"), "{}", error);

        assert_eq!(pallet_utils::get_pallet_head("zeta").unwrap(), Some(three), "the other bay's head must stand");
        assert_eq!(pallet_utils::get_pallet_head("alpha").unwrap(), Some(alpha_b), "no other ref may be restored");
        assert_eq!(pallet_utils::read_position().unwrap(), pallet_utils::BayPosition::new("zeta", Some(two)),
            "the bay still stands where the operation left it");
        assert!(journal_utils::peek().unwrap().is_some(), "a refused undo must keep its entry");
    }
    #[test]
    fn undo_of_a_delete_whose_history_is_gone_refuses_and_keeps_its_entry() {
        let _warehouse = Warehouse::storage_only("undo-delete-freed");
        let none = pallet_utils::RefPreconditions::NONE;

        let main = test_support::store_parcel(&test_support::store_tree("a.txt", b"a\n"), &[]);
        pallet_utils::update_pallet_head("main", None, &main, &none).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(main))).unwrap();

        // A parcel whose tree this store no longer holds.
        let freed = test_support::store_parcel(&hash('f'), &[]);
        test_support::write_ref_unchecked("side", &freed);
        let entry = journal_utils::delete_entry("side", &freed).unwrap();
        pallet_utils::remove_pallet_head_in(pallet_utils::PalletNamespace::User, "side", &freed, &none, Some(&entry)).unwrap();

        let error = test_support::block_on(handle_command(None)).expect_err("the undo must refuse");
        assert!(error.contains("is no longer complete in this warehouse"), "{}", error);
        assert!(error.contains("forklift lower side"), "{}", error);
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), None, "nothing was created");
        assert_eq!(journal_utils::peek().unwrap(), Some(entry), "a refused undo keeps its entry");
    }

    /// In a served warehouse the delete leaves a tombstone, and undo's restore acknowledges it:
    /// the pallet comes back and the tombstone goes.
    ///
    /// Revert: restore through `update_pallet_head_in` (no acknowledgement) → the restore is
    /// refused as a re-creation of a deleted pallet → red.
    #[test]
    fn undo_of_a_delete_in_a_served_warehouse_clears_its_tombstone() {
        let _warehouse = Warehouse::storage_only("undo-delete-served");
        let none = pallet_utils::RefPreconditions::NONE;
        assert!(forklift_core::util::tombstone_utils::mark_served(false).unwrap());

        let tree = test_support::store_tree("a.txt", b"a\n");
        let main = test_support::store_parcel(&tree, &[]);
        let side = test_support::store_parcel(&tree, &[&main]);
        pallet_utils::update_pallet_head("main", None, &main, &none).unwrap();
        pallet_utils::update_pallet_head("side", None, &side, &none).unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", Some(main))).unwrap();
        let entry = journal_utils::delete_entry("side", &side).unwrap();
        pallet_utils::remove_pallet_head_in(pallet_utils::PalletNamespace::User, "side", &side, &none, Some(&entry)).unwrap();
        assert_eq!(forklift_core::util::tombstone_utils::tombstone_heads().unwrap(), vec![side.clone()]);

        test_support::block_on(handle_command(None)).expect("the undo restores the pallet");

        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(side));
        assert!(forklift_core::util::tombstone_utils::read_tombstones().unwrap().is_empty(), "the tombstone is cleared");
    }
}
