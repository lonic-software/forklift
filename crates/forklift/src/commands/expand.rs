use serde::Serialize;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::remote_utils::{BatchLaneClosure, ExpandCommit, RemoteClient};
use forklift_core::util::scope_utils::ScopeClass;
use forklift_core::util::{object_utils, pallet_utils, remote_utils, scope_utils};
use crate::output::{self, CommandOutput};

/// Handle the expand command (§7.6): widen a sparse warehouse's fetch scope and download the
/// newly in-scope subtree(s) across every closure the warehouse holds.
///
/// A ref's closure is complete only at the scope it was fetched at, so widening the scope
/// re-walks the bay's base, every local ref head and every head a fetch recorded without a ref
/// yet ([`remote_utils::Expansion`]); the shared history is walked once. The content is fetched
/// from the configured remote and hash-verified against the seals the history already commits, so
/// widening from any remote is safe — only publishing (`lift`) is origin-bound. A full
/// (non-sparse) warehouse already holds everything, so there is nothing to expand.
///
/// # Arguments
/// * `paths` - The subtree path(s) to add to the warehouse fetch scope.
///
/// # Returns
/// * `Ok(())`      - If the fetch scope was widened (and any newly in-scope content fetched).
/// * `Err(String)` - If the warehouse is not sparse, no remote is configured, a path is
///                   malformed or names nothing, a closure cannot be fetched, the warehouse kept
///                   changing, or a transfer failed.
pub async fn handle_command(paths: Vec<String>) -> Result<(), String> {
    let current = scope_utils::read_fetch_scope()?;

    // Checked first: a cache's scope is the whole tree, but it does not hold the whole tree.
    if scope_utils::is_cache_mode()? {
        return Err(
            "This warehouse was franchised with --cache: its scope is already the whole tree, so \
            there is nothing to expand. \"expand\" widens a sparse (\"franchise --only\") \
            warehouse's fetch scope.".to_string()
        );
    }

    if current.is_full() {
        return Err(
            "This warehouse already holds the full tree; there is nothing to expand. \"expand\" \
            widens a sparse (\"franchise --only\") warehouse's fetch scope.".to_string()
        );
    }

    // The requested prefixes not already in scope.
    let mut added: Vec<String> = Vec::new();

    for raw in &paths {
        let path = WarehousePath::from_user_input(raw)?;

        if path.is_root() {
            return Err(
                "Expanding to the warehouse root would fetch the whole tree — re-franchise \
                without --only for a full warehouse.".to_string()
            );
        }

        let key = path.as_key().to_string();

        if current.classify(&key) == ScopeClass::InScope || added.contains(&key) {
            continue;
        }

        added.push(key);
    }

    if added.is_empty() {
        // Everything requested is already fetched — a clean no-op.
        output::emit(&ExpandReport {
            added: Vec::new(),
            scope: current.prefixes().to_vec(),
            fetched_objects: 0,
            batch_lane: None,
        });

        return Ok(());
    }

    let client = RemoteClient::for_warehouse()?;
    // The tree this bay holds: its base, not the pallet's ref, which another bay can move.
    let position = pallet_utils::read_position()?;
    let pallet = position.pallet.clone();
    let head = position.base.clone().ok_or_else(|| format!(
        "Pallet \"{}\" has nothing stacked yet; there is nothing to expand.", pallet
    ))?;

    // Fetch the newly in-scope content FIRST, at the widened scope, and only then record it: the
    // fetch scope never claims a path is in scope until it is fully fetched. A transfer error, or
    // a hard crash mid-fetch, leaves the on-disk scope as it was, so a plain re-run retries the
    // fetch and self-heals; the fetch primitives skip what already landed.
    let mut expansion = remote_utils::Expansion::new(&pallet, &head, current, added);

    expansion.walk_base(&client).await?;

    // Each added path must name a real directory at the head — the spine (and the subtree itself)
    // is fetched now, so the walk resolves without touching anything absent. A typo that named
    // nothing is rejected before any other history is fetched or the scope written, so it leaves
    // no trace. A retry only ever narrows the added paths, so this check covers every attempt.
    let tree = object_utils::load_parcel(&head)?.tree_hash;

    for prefix in expansion.added() {
        if !is_directory_in_tree(&tree, prefix)? {
            return Err(format!(
                "\"{}\" is not a directory in pallet \"{}\" at its head, so it cannot be a fetch \
                scope. Nothing was recorded.",
                prefix, pallet
            ));
        }
    }

    for _ in 0..remote_utils::EXPAND_ATTEMPTS {
        expansion.walk(&client).await?;
        expansion.walk_new_ref_heads_locally()?;

        match expansion.commit()? {
            ExpandCommit::Written(scope) => {
                output::emit(&ExpandReport {
                    added: expansion.added().to_vec(),
                    scope: scope.prefixes().to_vec(),
                    fetched_objects: expansion.fetched_objects(),
                    batch_lane: client.batch_lane_closure(),
                });

                return Ok(());
            }
            ExpandCommit::Covered(scope) => {
                output::emit(&ExpandReport {
                    added: Vec::new(),
                    scope: scope.prefixes().to_vec(),
                    fetched_objects: expansion.fetched_objects(),
                    batch_lane: client.batch_lane_closure(),
                });

                return Ok(());
            }
            ExpandCommit::Retry => {}
        }
    }

    Err(
        "The warehouse kept changing while expand ran, so the fetch scope was not widened; run it \
        again.".to_string()
    )
}

/// Whether a warehouse path resolves to a subtree (directory) in the given root tree.
fn is_directory_in_tree(root_tree_hash: &str, key: &str) -> Result<bool, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for component in key.split('/') {
        let subtree = current.get_subtrees()
            .find(|(name, _)| name.as_str() == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(hash) => current = object_utils::load_tree(&hash)?,
            None => return Ok(false),
        }
    }

    Ok(true)
}

/// The result of an expand: what was added to the fetch scope and how much was fetched.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ExpandReport {
    /// The prefixes newly added to the fetch scope (empty when everything was already in scope).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,

    /// The warehouse fetch scope after the expand.
    scope: Vec<String>,

    /// Loose objects fetched for the newly in-scope subtree(s).
    fetched_objects: usize,

    /// Why the remote's batch download stopped being used, when it did. Absent when it stayed open.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_lane: Option<BatchLaneClosure>,
}

impl CommandOutput for ExpandReport {
    fn render_human(&self) {
        if self.added.is_empty() {
            println!("Nothing to expand: every requested path is already in the fetch scope.");
        } else {
            println!(
                "Expanded the fetch scope with {} ({} object(s) fetched). Fetch scope now: {}.",
                self.added.join(", "), self.fetched_objects, self.scope.join(", ")
            );
            println!("Scope a bay to a newly fetched path with \"bay add --scope\".");
        }

        output::print_batch_lane(self.batch_lane);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ExpandReport>(),
    ]
}
