use serde::Serialize;
use forklift_core::util::stocktake_utils::Change;
use forklift_core::util::{merge_utils, object_utils, pallet_utils, stocktake_utils};
use crate::output::{self, CommandOutput};

/// Handle the stocktake command (git's "status"): report the current pallet, the staged
/// changes (inventory vs pallet head — what the next `stack` records) and the unstaged
/// changes (working directory vs inventory — what a `load` would stage).
///
/// # Arguments
/// * `summary` - Report only the counts, not the per-path changes (token-cheap).
///
/// # Returns
/// * `Ok(())`      - If the stocktake completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub async fn handle_command(summary: bool) -> Result<(), String> {
    let pallet = pallet_utils::get_current_pallet_name()?;
    let head = pallet_utils::get_pallet_head(&pallet)?;

    let consolidation = merge_utils::read_consolidation_state()?
        .map(|state| state.their_pallet);

    let head_tree_hash = match &head {
        Some(hash) => Some(object_utils::load_parcel(hash)?.tree_hash),
        None => None,
    };

    let staged = stocktake_utils::collect_staged_changes(head_tree_hash.as_deref()).await?;
    let unstaged = stocktake_utils::collect_unstaged_changes().await?;

    let report = StocktakeReport {
        pallet,
        head,
        consolidation_in_progress: consolidation,
        staged_count: staged.len(),
        unstaged_count: unstaged.len(),
        // `--summary` keeps the counts and drops the per-path lists — the token-cheap
        // overview an agent asks for before deciding whether to pull the full report.
        staged: if summary { Vec::new() } else { staged },
        unstaged: if summary { Vec::new() } else { unstaged },
        summary,
    };

    output::emit("stocktake", &report);

    Ok(())
}

/// The stocktake report: the current pallet's state plus the staged and unstaged
/// changes (counts always; per-path lists unless `--summary`).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct StocktakeReport {
    /// The current pallet.
    pallet: String,

    /// The pallet's head parcel, or `null` when it is unborn.
    head: Option<String>,

    /// The pallet being consolidated in, when a merge is in progress.
    #[serde(skip_serializing_if = "Option::is_none")]
    consolidation_in_progress: Option<String>,

    /// How many changes are staged (inventory vs pallet head).
    staged_count: usize,

    /// How many changes are unstaged (working directory vs inventory).
    unstaged_count: usize,

    /// The staged changes (empty under `--summary`).
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    staged: Vec<Change>,

    /// The unstaged changes (empty under `--summary`).
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    unstaged: Vec<Change>,

    /// Whether this is a counts-only report.
    summary: bool,
}

impl CommandOutput for StocktakeReport {
    fn render_human(&self) {
        match &self.head {
            Some(hash) => println!("On pallet \"{}\" (head {})", self.pallet, hash),
            None => println!("On pallet \"{}\" (unborn — nothing stacked yet)", self.pallet),
        }

        if let Some(their_pallet) = &self.consolidation_in_progress {
            println!(
                "A consolidation with pallet \"{}\" is in progress; \"stack\" completes it.",
                their_pallet
            );
        }

        if self.summary {
            println!();
            println!("{} staged, {} not loaded.", self.staged_count, self.unstaged_count);

            return;
        }

        println!();

        if self.staged_count == 0 {
            println!("The inventory matches the pallet head; nothing is staged.");
        } else {
            println!("Staged changes (will be recorded by \"stack\"):");
            print_changes(&self.staged);
        }

        println!();

        if self.unstaged_count == 0 {
            println!("The working directory matches the inventory.");
        } else {
            println!("Changes not in the inventory (use \"load\" to stage them):");
            print_changes(&self.unstaged);
        }
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![("StocktakeReport", schemars::schema_for!(StocktakeReport))]
}

/// Print a list of changes, one per line, with aligned change kinds.
///
/// # Arguments
/// * `changes` - The changes to print.
fn print_changes(changes: &[Change]) {
    for change in changes {
        match &change.moved_from {
            Some(from) => println!("  {:<10} {} -> {}", format!("{}:", change.kind), from, change.path),
            None => println!("  {:<10} {}", format!("{}:", change.kind), change.path),
        }
    }
}
