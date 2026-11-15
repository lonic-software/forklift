use serde::Serialize;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::remote_utils::{BatchLaneClosure, ExpandCommit, RemoteClient};
use forklift_core::util::scope_utils::ScopeClass;
use forklift_core::util::{object_utils, pallet_utils, remote_utils, scope_utils};
use crate::output::{self, CommandOutput};

/// Handle `scope fetch` (§7.6): widen a sparse warehouse's fetch scope and download the
/// newly in-scope subtree(s) across every closure the warehouse holds.
///
/// A ref's closure is complete only at the scope it was fetched at, so widening the scope
/// re-walks the bay's base, every local ref head and every head a fetch recorded without a ref
/// yet ([`remote_utils::Expansion`]); the shared history is walked once. The content is fetched
/// from the origin (a partial warehouse dials no other remote, [`forklift_core::util::origin_utils::checked_remote`])
/// and hash-verified against the seals the history already commits. A full
/// (non-sparse) warehouse already holds everything, so there is nothing to fetch.
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
            "This warehouse was cloned with --cache: its scope is already the whole tree, so \
            there is nothing to fetch. \"forklift scope fetch\" widens a sparse (\"forklift clone --only\") \
            warehouse's fetch scope.".to_string()
        );
    }

    if current.is_full() {
        return Err(
            "This warehouse already holds the full tree; there is nothing to fetch. \"forklift scope fetch\" \
            widens a sparse (\"forklift clone --only\") warehouse's fetch scope.".to_string()
        );
    }

    // The requested prefixes not already in scope.
    let mut added: Vec<String> = Vec::new();

    for raw in &paths {
        let path = WarehousePath::from_user_input(raw)?;

        if path.is_root() {
            return Err(
                "Fetching the warehouse root would fetch the whole tree — re-clone \
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
        output::emit(&ScopeFetchReport {
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
        "Pallet \"{}\" has nothing stacked yet; there is nothing to fetch.", pallet
    ))?;

    // The handshake sets the client to leave out of the widened walk the content the remote's host
    // withholds, recorded unavailable, as a lower leaves it out, so a path brought into scope that
    // names it widens without it rather than being refused.
    client.fetch_info().await?;

    // Each added path must name a directory at the base, checked before anything is fetched. A
    // tree the fetch scope seals is read from the remote into memory, verified and not stored; the
    // spine above the scope is read from the store. A path refused here leaves the store and the
    // fetch scope as they were. A retry only ever narrows the added paths, so this check covers
    // every attempt.
    let tree = object_utils::load_parcel(&head)?.tree_hash;
    let sealed = |path: &str| current.classify(path) == ScopeClass::OutOfScope;
    let checked = remote_utils::first_non_directory(&client, &tree, &added, sealed).await
        .map_err(|e| forklift_core::error::append_to_message(
            forklift_core::error::prefix_message(
                &format!("Could not check the requested path(s) against pallet \"{}\" at this checkout's base", pallet),
                e,
            ),
            "Nothing was recorded.",
        ))?;

    if let Some(prefix) = checked {
        return Err(format!(
            "\"{}\" is not a directory in pallet \"{}\" at this checkout's base, so it cannot be a \
            fetch scope. Nothing was recorded.",
            prefix, pallet
        ));
    }

    // Fetch the newly in-scope content FIRST, at the widened scope, and only then record it: the
    // fetch scope never claims a path is in scope until it is fully fetched. A transfer error, or
    // a hard crash mid-fetch, leaves the on-disk scope as it was, so a plain re-run retries the
    // fetch and self-heals; the fetch primitives skip what already landed.
    let mut expansion = remote_utils::Expansion::new(&pallet, &head, current, added);

    for _ in 0..remote_utils::EXPAND_ATTEMPTS {
        expansion.walk(&client).await?;
        expansion.walk_new_ref_heads_locally()?;

        match expansion.commit()? {
            ExpandCommit::Written(scope) => {
                output::emit(&ScopeFetchReport {
                    added: expansion.added().to_vec(),
                    scope: scope.prefixes().to_vec(),
                    fetched_objects: expansion.fetched_objects(),
                    batch_lane: client.batch_lane_closure(),
                });

                return Ok(());
            }
            ExpandCommit::Covered(scope) => {
                output::emit(&ScopeFetchReport {
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
        "The warehouse kept changing while \"forklift scope fetch\" ran, so the fetch scope was not widened; run it \
        again.".to_string()
    )
}

/// The result of a `scope fetch`: what was added to the fetch scope and how much was fetched.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ScopeFetchReport {
    /// The prefixes newly added to the fetch scope (empty when everything was already in scope).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    added: Vec<String>,

    /// The warehouse fetch scope after the fetch.
    scope: Vec<String>,

    /// Loose objects fetched for the newly in-scope subtree(s).
    fetched_objects: usize,

    /// Why the remote's batch download stopped being used, when it did. Absent when it stayed open.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_lane: Option<BatchLaneClosure>,
}

impl CommandOutput for ScopeFetchReport {
    fn render_human(&self) {
        if self.added.is_empty() {
            println!("Nothing to fetch: every requested path is already in the fetch scope.");
        } else {
            println!(
                "Widened the fetch scope with {} ({} object(s) fetched). Fetch scope now: {}.",
                self.added.join(", "), self.fetched_objects, self.scope.join(", ")
            );
            println!("Scope a bay to a newly fetched path with \"forklift bay add --scope\".");
        }

        output::print_batch_lane(self.batch_lane);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ScopeFetchReport>(),
    ]
}
