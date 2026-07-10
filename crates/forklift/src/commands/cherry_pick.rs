use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::cherry_pick_utils::{self, CherryPickState};
use forklift_core::util::{merge_utils, object_utils, office_utils, pallet_utils, stack_utils};
use crate::commands::consolidate;
use crate::output::{self, CommandOutput};

/// Handle the cherry-pick command (§9.1 #8): apply a parcel's diff onto the current pallet
/// as a new, author-preserving, freshly-signed parcel.
///
/// The source parcel's change — its diff against its first parent — is three-way merged
/// onto the current head (the same machinery `consolidate` uses). A clean pick is stacked
/// immediately as a single-parent parcel that preserves the source's authors and records
/// this operator as the stacker; a conflicting pick leaves markers and a cherry-pick state,
/// and the next `stack` completes it. Cherry-pick only *adds* — no rewrite, no force-push.
///
/// # Arguments
/// * `revision` - The parcel to cherry-pick (a pallet name, or a parcel hash / prefix).
/// * `message`  - An optional message for the new parcel (default: the source's message).
///
/// # Returns
/// * `Ok(())`      - If the pick applied, or left conflicts to resolve.
/// * `Err(String)` - If there is nothing to pick, the warehouse is dirty, or an operation
///                   failed.
pub async fn handle_command(revision: &str, message: Option<String>) -> Result<(), String> {
    // A pick applies a diff (the merge machinery) into the working directory, which could
    // touch out-of-scope paths a scoped bay never materialized, so it refuses here.
    crate::commands::scope::refuse_in_scoped_bay(
        "cherry-pick",
        "Run it from a full bay.",
    )?;

    let current = pallet_utils::get_current_pallet_name()?;

    // A pick materializes into the working directory, so it cannot start on top of an
    // in-progress consolidation or another in-progress pick.
    if merge_utils::read_consolidation_state()?.is_some() {
        return Err(
            "A consolidation is in progress. Complete it (resolve, \"load\", \"stack\") or abort \
            it before cherry-picking.".to_string()
        );
    }

    if cherry_pick_utils::read_state()?.is_some() {
        return Err(
            "A cherry-pick is already in progress. Resolve its conflicts and \"stack\", or remove \
            \".forklift/cherry-pick\" to abort it.".to_string()
        );
    }

    let our_head = pallet_utils::get_pallet_head(&current)?.ok_or(format!(
        "Pallet \"{}\" has nothing stacked yet; cherry-pick applies a change onto existing history.",
        current
    ))?;

    let source = pallet_utils::resolve_revision(revision)?;
    let source_parcel = object_utils::load_parcel(&source)?;
    let short = source[..source.len().min(12)].to_string();

    // A hand-made ref could point at an office or other meta parcel; its tracked-metadata
    // namespace must never be cherry-picked onto a working pallet.
    office_utils::ensure_not_metadata_tree(&source_parcel.tree_hash)?;

    if source == our_head || merge_utils::is_ancestor(&source, &our_head)? {
        return Err(format!(
            "Parcel {} is already in the history of \"{}\"; there is nothing to cherry-pick.",
            short, current
        ));
    }

    let our_tree = object_utils::load_parcel(&our_head)?.tree_hash;

    // The pick materializes the merged result, so the warehouse must be clean first.
    if !consolidate::is_warehouse_clean(&our_tree).await? {
        return Err(
            "There are local changes that a cherry-pick would overwrite. Stack them, restore \
            them, or park them first (see \"stocktake\" for the details).".to_string()
        );
    }

    // The pick's base is the source's first-parent tree (its "before"); an empty tree for a
    // root parcel, so every file it introduces is new relative to nothing.
    let base_tree = match source_parcel.parents.first() {
        Some(parent) => object_utils::load_parcel(parent)?.tree_hash,
        None => empty_tree_hash()?,
    };
    let theirs_tree = source_parcel.tree_hash.clone();

    if base_tree == theirs_tree {
        return Err(format!("Parcel {} makes no changes; there is nothing to cherry-pick.", short));
    }

    let source_label = format!("cherry-pick {}", &source[..source.len().min(10)]);

    let actions = merge_utils::compute_merge_actions(
        &base_tree, &our_tree, &theirs_tree, &current, &source_label
    )?;

    // Every change the source made is already present here: an empty pick.
    if actions.is_empty() {
        return Err(format!(
            "The changes in {} are already present in \"{}\"; there is nothing to cherry-pick.",
            short, current
        ));
    }

    consolidate::ensure_no_untracked_collisions(&actions, &source_label)?;

    let mut conflicts: Vec<String> = Vec::new();

    for action in &actions {
        consolidate::apply_merge_action(action, &mut conflicts)?;
    }

    // The completing parcel's message: the -m override, else the source's own message.
    let description = message.or_else(|| source_parcel.description.clone());

    // Record the pick so `stack` completes it single-parent, preserving the source's
    // authors — whether it completes now (clean) or after the user resolves conflicts.
    cherry_pick_utils::write_state(&CherryPickState {
        source: source.clone(),
        description: description.clone(),
    })?;

    if conflicts.is_empty() {
        let (parcel, _) = stack_utils::stack_parcel(description).await?;

        output::emit("cherry-pick", &CherryPicked::applied(&source, &current, &parcel));
    } else {
        conflicts.sort();

        output::emit("cherry-pick", &CherryPicked::conflicts(&source, &current, conflicts));
    }

    Ok(())
}

/// The hash of an empty (root) tree — the base for cherry-picking a parcel that has no
/// parent. Building and storing it is cheap and content-addressed (identical every time).
fn empty_tree_hash() -> Result<String, String> {
    let empty = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut object = LooseObjectBuilder::build_tree(&empty);
    object.store()?;

    Ok(object.hash)
}

/// What a cherry-pick did. `Conflicts` is the only outcome that leaves work for the operator
/// (resolve, load, stack); `Applied` is complete.
#[derive(Serialize, PartialEq)]
#[serde(rename_all = "snake_case")]
enum CherryPickOutcome {
    Applied,
    Conflicts,
}

/// The result of a cherry-pick.
#[derive(Serialize)]
struct CherryPicked {
    outcome: CherryPickOutcome,

    /// The parcel that was picked.
    source: String,

    /// The pallet the pick applied to (the current one).
    pallet: String,

    /// The new parcel, when the pick completed cleanly.
    #[serde(skip_serializing_if = "Option::is_none")]
    parcel: Option<String>,

    /// The conflicting paths, when the pick did not complete cleanly.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    conflicts: Vec<String>,
}

impl CherryPicked {
    fn applied(source: &str, pallet: &str, parcel: &str) -> CherryPicked {
        CherryPicked {
            outcome: CherryPickOutcome::Applied,
            source: source.to_string(),
            pallet: pallet.to_string(),
            parcel: Some(parcel.to_string()),
            conflicts: Vec::new(),
        }
    }

    fn conflicts(source: &str, pallet: &str, conflicts: Vec<String>) -> CherryPicked {
        CherryPicked {
            outcome: CherryPickOutcome::Conflicts,
            source: source.to_string(),
            pallet: pallet.to_string(),
            parcel: None,
            conflicts,
        }
    }
}

impl CommandOutput for CherryPicked {
    fn render_human(&self) {
        let short = &self.source[..self.source.len().min(12)];

        match self.outcome {
            CherryPickOutcome::Applied => {
                println!(
                    "Cherry-picked {} onto \"{}\": stacked parcel {} (authors preserved).",
                    short,
                    self.pallet,
                    self.parcel.as_deref().unwrap_or(""),
                );
            }
            CherryPickOutcome::Conflicts => {
                println!(
                    "Cherry-picking {} onto \"{}\" produced {} conflict(s):",
                    short, self.pallet, self.conflicts.len()
                );

                for path in &self.conflicts {
                    println!("  conflict: {}", path);
                }

                println!(
                    "\nResolve the conflicts, \"load\" the resolved files, then \"stack\" to \
                    complete the cherry-pick (or remove \".forklift/cherry-pick\" to abort it)."
                );
            }
        }
    }
}
