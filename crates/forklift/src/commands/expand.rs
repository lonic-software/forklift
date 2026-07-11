use serde::Serialize;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::remote_utils::RemoteClient;
use forklift_core::util::scope_utils::{MaterializationScope, ScopeClass};
use forklift_core::util::{object_utils, pallet_utils, remote_utils, scope_utils};
use crate::output::{self, CommandOutput};

/// Handle the expand command (§7.6): widen a sparse warehouse's fetch scope and download the
/// newly in-scope subtree(s) across the current pallet's whole history.
///
/// The content is fetched from the configured remote and hash-verified against the seals the
/// history already commits, so widening from any remote is safe — only publishing (`lift`) is
/// origin-bound. A full (non-sparse) warehouse already holds everything, so there is nothing to
/// expand.
///
/// # Arguments
/// * `paths` - The subtree path(s) to add to the warehouse fetch scope.
///
/// # Returns
/// * `Ok(())`      - If the fetch scope was widened (and any newly in-scope content fetched).
/// * `Err(String)` - If the warehouse is not sparse, no remote is configured, a path is
///                   malformed or names nothing, or a transfer failed.
pub async fn handle_command(paths: Vec<String>) -> Result<(), String> {
    let current = scope_utils::read_fetch_scope()?;

    if current.is_full() {
        return Err(
            "This warehouse already holds the full tree; there is nothing to expand. \"expand\" \
            widens a sparse (\"franchise --only\") warehouse's fetch scope.".to_string()
        );
    }

    // Union the requested prefixes into the current fetch scope, skipping any already in scope.
    let mut prefixes: Vec<String> = current.prefixes().to_vec();
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

        if current.classify(&key) == ScopeClass::InScope {
            continue;
        }

        prefixes.push(key.clone());
        added.push(key);
    }

    if added.is_empty() {
        // Everything requested is already fetched — a clean no-op.
        output::emit("expand", &ExpandReport {
            added: Vec::new(),
            scope: current.prefixes().to_vec(),
            fetched_objects: 0,
        });

        return Ok(());
    }

    let widened = MaterializationScope::from_prefixes(prefixes);

    let client = RemoteClient::from_config()?;
    let pallet = pallet_utils::get_current_pallet_name()?;
    let head = pallet_utils::get_pallet_head(&pallet)?.ok_or_else(|| format!(
        "Pallet \"{}\" has nothing stacked yet; there is nothing to expand.", pallet
    ))?;

    // Fetch the newly in-scope content FIRST, using the widened scope directly — nothing is
    // written to disk yet. This mirrors the property `franchise`/`lower` already rely on (a ref
    // never moves until its whole closure is present): here, the fetch scope never claims a path
    // is in scope until it is actually, fully fetched. A transfer error, or a hard crash mid-
    // fetch, therefore leaves the on-disk scope exactly as it was — "not yet in scope" — so a
    // plain re-run (once the remote is reachable again) retries the fetch and self-heals, the
    // same wave-resumability every other fetch here has. There is nothing to roll back, because
    // nothing was written until this succeeded.
    let stats = remote_utils::fetch_expanded(&client, &head, &widened).await?;

    // Each added path must name a real directory at the head — the spine (and the subtree
    // itself) is fetched now, so the walk resolves without touching anything absent. A typo that
    // named nothing is rejected here, before the scope is ever written, so it leaves no trace.
    let tree = object_utils::load_parcel(&head)?.tree_hash;

    for prefix in &added {
        if !is_directory_in_tree(&tree, prefix)? {
            return Err(format!(
                "\"{}\" is not a directory in pallet \"{}\" at its head, so it cannot be a fetch \
                scope. Nothing was recorded.",
                prefix, pallet
            ));
        }
    }

    // Only now — after the fetch fully landed and every path validated — record the widened
    // scope.
    scope_utils::set_fetch_scope(&widened)?;

    output::emit("expand", &ExpandReport {
        added,
        scope: widened.prefixes().to_vec(),
        fetched_objects: stats.fetched_objects,
    });

    Ok(())
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
#[derive(Serialize)]
struct ExpandReport {
    /// The prefixes newly added to the fetch scope (empty when everything was already in scope).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,

    /// The warehouse fetch scope after the expand.
    scope: Vec<String>,

    /// Loose objects fetched for the newly in-scope subtree(s).
    fetched_objects: usize,
}

impl CommandOutput for ExpandReport {
    fn render_human(&self) {
        if self.added.is_empty() {
            println!("Nothing to expand: every requested path is already in the fetch scope.");
            return;
        }

        println!(
            "Expanded the fetch scope with {} ({} object(s) fetched). Fetch scope now: {}.",
            self.added.join(", "), self.fetched_objects, self.scope.join(", ")
        );
        println!("Scope a bay to a newly fetched path with \"bay add --scope\".");
    }
}
