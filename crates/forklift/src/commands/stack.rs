use serde::Serialize;
use forklift_core::globals::bay_root;
use forklift_core::util::pallet_utils::{RefMoved, RefUpdateError};
use forklift_core::util::stack_utils::{self, StackError};
use forklift_core::util::stocktake_utils::{self, ChangeKind};
use forklift_core::util::{cherry_pick_utils, merge_utils, object_utils};
use crate::output::{self, CommandOutput};

/// Handle the stack command (git's "commit"):
/// build tree objects from the inventory, create a parcel pointing at them (with the
/// configured operator recorded as the stacking action), advance the current pallet's
/// head to the new parcel, and clean up the consumed staged state. A consolidation in
/// progress is completed by this command (the consolidated head becomes the second
/// parent of the new parcel).
///
/// A consolidation or cherry-pick in progress is stacked on the head it was built on. If another
/// bay or process moved the pallet since, the stack is refused and the merge is kept, because
/// rolling it back would destroy the user's resolutions; the error names the way out.
///
/// # Arguments
/// * `description` - The (optional) parcel description.
///
/// # Returns
/// * `Ok(())`      - If the parcel was stacked successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(description: Option<String>) -> Result<(), String> {
    let (parcel, pallet) = match stack_utils::stack_parcel(description, None).await {
        Ok(stacked) => stacked,
        Err(StackError::HeadWrite { error: RefUpdateError::Moved(moved), .. }) => {
            return Err(moved_under_merge(moved).await);
        }
        Err(error) => return Err(error.into()),
    };

    output::emit(&Stacked { parcel, pallet });

    Ok(())
}

/// The refusal for a stack whose pallet another bay or process moved. With a consolidation or
/// cherry-pick in progress, its staged output was built on the head it recorded, so stacking it on
/// the new head would revert the other bay's parcel: the message names the whole way out — abort,
/// discard the output, delete the files `restore` leaves behind as untracked (the staged paths
/// the pallet's new head does not hold) and merge again.
async fn moved_under_merge(moved: RefMoved) -> String {
    let consolidation = merge_utils::read_consolidation_state().ok().flatten();
    let pick = cherry_pick_utils::read_state().ok().flatten();

    let (what, base, abort, again) = match (consolidation, pick) {
        (Some(state), _) => (
            "consolidation",
            state.our_head,
            merge_utils::abort_hint_in(&bay_root()),
            "merge again (\"consolidate\", or the \"haul merge\" or \"lift\" that started it)".to_string(),
        ),
        (None, Some(state)) => (
            "cherry-pick",
            state.our_head,
            cherry_pick_utils::abort_hint(),
            format!("cherry-pick {} again", state.source),
        ),
        (None, None) => return moved.to_string(),
    };

    let left = match left_untracked(moved.found.as_deref()).await {
        Ok(left) if left.is_empty() => String::new(),
        Ok(left) => format!(
            " delete the files \"restore\" leaves behind as untracked: {};",
            left.iter().map(|path| format!("\"{}\"", path)).collect::<Vec<_>>().join(", ")
        ),
        Err(e) => format!(
            " delete the files \"restore\" leaves behind as untracked (they could not be \
            listed: {});",
            e
        ),
    };

    format!(
        "Another bay or process moved pallet \"{}\" since this {} started: it was built on {}, and \
        the pallet is now at {}. Nothing was stacked, and the {} is kept: stacking it on the new \
        head would revert what was stacked there. To redo it on the new head: abort it ({}); \
        discard its output with \"restore --staged .\" and then \"restore .\";{} then {}. Do not \
        stack the output after aborting: that too would revert the other parcel.",
        moved.reference.to_wire(),
        what,
        base,
        moved.found.as_deref().unwrap_or("no head"),
        what,
        abort,
        left,
        again
    )
}

/// The staged paths `head`'s tree does not hold: once `restore --staged .` resets the inventory
/// to the pallet's head, these are the files left behind as untracked.
async fn left_untracked(head: Option<&str>) -> Result<Vec<String>, String> {
    let tree = match head {
        Some(head) => Some(object_utils::load_parcel(head)?.tree_hash),
        None => None,
    };

    Ok(stocktake_utils::collect_staged_changes(tree.as_deref()).await?
        .into_iter()
        .filter(|change| matches!(change.kind, ChangeKind::Added | ChangeKind::Moved))
        .map(|change| change.path)
        .collect())
}

/// The parcel a `stack` created and the pallet it advanced.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Stacked {
    parcel: String,
    pallet: String,
}

impl CommandOutput for Stacked {
    fn render_human(&self) {
        println!("Stacked parcel {} on pallet \"{}\".", self.parcel, self.pallet);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<Stacked>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use forklift_core::util::{file_utils, pallet_utils, shift_utils};
    use forklift_core::util::progress_utils::Progress;
    use crate::commands::{cherry_pick, consolidate, load};
    use crate::test_support::{self, Warehouse};

    #[test]
    fn a_stack_completing_a_merge_whose_pallet_moved_is_refused_and_keeps_the_merge() {
        let warehouse = Warehouse::with_working_tree("stack-merge-moved");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_tree("a.txt", b"base\n"), &[]);
        let our_tree = test_support::store_tree("a.txt", b"ours\n");
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"theirs\n"), ("new.txt", b"new\n"), ("both.txt", b"both\n")]),
            &[&base],
        );
        // The other bay's parcel holds one of the files the merge adds: `restore` tracks it again.
        let elsewhere = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"elsewhere\n"), ("both.txt", b"both\n")]),
            &[&our_head],
        );

        pallet_utils::set_current_pallet_name("main").unwrap();
        pallet_utils::update_pallet_head("main", None, &our_head).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head).unwrap();
        shift_utils::materialize_tree(None, &our_tree, "Test", file_utils::take_stat_anchor().unwrap(), &Progress::NONE).unwrap();

        // A conflicted merge, resolved and loaded by hand.
        test_support::block_on(consolidate::handle_command("feature")).unwrap();
        assert!(merge_utils::read_consolidation_state().unwrap().is_some());
        std::fs::write(warehouse.root.join("a.txt"), b"resolved\n").unwrap();
        test_support::block_on(load::handle_command("a.txt")).unwrap();

        // Another bay stacks on "main" before the merge is completed.
        pallet_utils::update_pallet_head("main", Some(&our_head), &elsewhere).unwrap();

        let error = test_support::block_on(handle_command(None))
            .expect_err("completing a merge over a moved pallet must be refused");
        assert!(error.contains("moved") && error.contains("consolidation is kept"), "{}", error);
        assert!(error.contains("\"new.txt\""), "the files restore leaves untracked must be listed: {}", error);
        assert!(!error.contains("both.txt"), "a file the new head holds is tracked again, never left: {}", error);
        assert!(error.contains("restore --staged"), "{}", error);

        // Completing it from another pallet is refused before it starts.
        let shifted = test_support::block_on(crate::commands::shift::handle_command("feature"))
            .expect_err("shifting during a consolidation must be refused");
        assert!(shifted.contains("consolidation is in progress"), "{}", shifted);

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere),
            "the other bay's parcel must stay the head");
        assert_eq!(merge_utils::read_consolidation_state().unwrap().map(|state| state.our_head), Some(our_head),
            "the consolidation must stay in progress");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"resolved\n"), "the resolution must be kept");
    }

    #[test]
    fn a_merge_is_only_stacked_on_the_pallet_it_started_on() {
        let warehouse = Warehouse::with_working_tree("stack-merge-other-pallet");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_tree("a.txt", b"base\n"), &[]);
        let our_tree = test_support::store_tree("a.txt", b"ours\n");
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let their_head = test_support::store_parcel(&test_support::store_tree("a.txt", b"theirs\n"), &[&base]);

        pallet_utils::set_current_pallet_name("main").unwrap();
        pallet_utils::update_pallet_head("main", None, &our_head).unwrap();
        pallet_utils::update_pallet_head("feature", None, &their_head).unwrap();
        shift_utils::materialize_tree(None, &our_tree, "Test", file_utils::take_stat_anchor().unwrap(), &Progress::NONE).unwrap();

        test_support::block_on(consolidate::handle_command("feature")).unwrap();
        std::fs::write(warehouse.root.join("a.txt"), b"resolved\n").unwrap();
        test_support::block_on(load::handle_command("a.txt")).unwrap();

        // Creating a pallet mid-merge is refused before the pallet exists.
        let refused = test_support::block_on(crate::commands::palletize::handle_command(Some("side".to_string()), None, false))
            .expect_err("switching to a new pallet during a consolidation must be refused");
        assert!(refused.contains("consolidation is in progress"), "{}", refused);
        assert!(!pallet_utils::does_pallet_exist("side"), "a refused palletize must leave no pallet behind");

        // Any other switch to a pallet at the very same head: the merge must not land there.
        pallet_utils::update_pallet_head("side", None, &our_head).unwrap();
        pallet_utils::set_current_pallet_name("side").unwrap();

        let error = test_support::block_on(handle_command(None))
            .expect_err("completing a merge from another pallet must be refused");
        assert!(error.contains("started on pallet \"main\"") && error.contains("Nothing was stacked"), "{}", error);
        assert_eq!(pallet_utils::get_pallet_head("side").unwrap(), Some(our_head.clone()), "nothing may land on \"side\"");
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(our_head), "nothing may land on \"main\"");
        assert!(merge_utils::read_consolidation_state().unwrap().is_some(), "the consolidation must stay in progress");
    }

    #[test]
    fn a_stack_completing_a_pick_whose_pallet_moved_is_refused_and_keeps_the_pick() {
        let warehouse = Warehouse::with_working_tree("stack-pick-moved");
        test_support::configure_operator();

        let base = test_support::store_parcel(&test_support::store_tree("a.txt", b"base\n"), &[]);
        let our_tree = test_support::store_tree("a.txt", b"ours\n");
        let our_head = test_support::store_parcel(&our_tree, &[&base]);
        let source = test_support::store_parcel(
            &test_support::store_files(&[("a.txt", b"theirs\n"), ("new.txt", b"new\n")]),
            &[&base],
        );
        let elsewhere = test_support::store_parcel(&test_support::store_tree("a.txt", b"elsewhere\n"), &[&our_head]);

        pallet_utils::set_current_pallet_name("main").unwrap();
        pallet_utils::update_pallet_head("main", None, &our_head).unwrap();
        shift_utils::materialize_tree(None, &our_tree, "Test", file_utils::take_stat_anchor().unwrap(), &Progress::NONE).unwrap();

        // A conflicted pick, resolved and loaded by hand.
        test_support::block_on(cherry_pick::handle_command(&source, None)).unwrap();
        assert!(cherry_pick_utils::read_state().unwrap().is_some());
        std::fs::write(warehouse.root.join("a.txt"), b"resolved\n").unwrap();
        test_support::block_on(load::handle_command("a.txt")).unwrap();

        // Another bay stacks on "main" before the pick is completed.
        pallet_utils::update_pallet_head("main", Some(&our_head), &elsewhere).unwrap();

        let error = test_support::block_on(handle_command(None))
            .expect_err("completing a pick over a moved pallet must be refused");
        assert!(error.contains("moved") && error.contains("cherry-pick is kept"), "{}", error);
        assert!(error.contains("\"new.txt\""), "the files the pick added must be listed: {}", error);

        assert_eq!(pallet_utils::get_pallet_head("main").unwrap(), Some(elsewhere),
            "the other bay's parcel must stay the head");
        assert_eq!(cherry_pick_utils::read_state().unwrap().map(|state| state.our_head), Some(our_head),
            "the pick must stay in progress");
        assert!(test_support::file_holds(&warehouse.root, "a.txt", b"resolved\n"), "the resolution must be kept");
    }
}
