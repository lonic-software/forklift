use serde::Serialize;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::scope_utils::{MaterializationScope, ScopeClass};
use forklift_core::util::stocktake_utils::ChangeKind;
use forklift_core::util::{inventory_utils, object_utils, pallet_utils, scope_utils, stocktake_utils};
use crate::output::{self, CommandOutput};

/// Handle the narrow command (§7.6): shrink this checkout's materialization scope, dropping
/// subtree path(s) and de-materializing their files. This frees nothing in the shared object
/// store — the dropped content is ordinary reachable history, not garbage — it only shrinks what
/// this checkout shows. Bay-local and reversible: widen the bay again with `bay add --scope` on a
/// fresh checkout, or `expand` the warehouse and re-scope.
///
/// # Arguments
/// * `paths` - The in-scope subtree path(s) to drop.
///
/// # Returns
/// * `Ok(())`      - If the scope was narrowed and the dropped subtree(s) de-materialized.
/// * `Err(String)` - If this checkout is not scoped, a path is not an in-scope prefix, the
///                   last in-scope path would be dropped, or a dropped subtree still holds
///                   uncommitted work.
pub async fn handle_command(paths: Vec<String>) -> Result<(), String> {
    let current = scope_utils::current_scope()?;

    if current.is_full() {
        return Err(
            "This checkout materializes the full tree; there is nothing to narrow. \"narrow\" \
            shrinks a scoped (sparse) checkout's materialization scope.".to_string()
        );
    }

    let mut keep: Vec<String> = current.prefixes().to_vec();
    let mut dropped: Vec<String> = Vec::new();

    for raw in &paths {
        let path = WarehousePath::from_user_input(raw)?;
        let key = path.as_key().to_string();

        match keep.iter().position(|prefix| *prefix == key) {
            Some(pos) => {
                keep.remove(pos);
                dropped.push(key);
            }
            None => return Err(format!(
                "\"{}\" is not one of this checkout's in-scope paths, so it cannot be narrowed \
                away. In scope: {}.",
                raw, current.prefixes().join(", ")
            )),
        }
    }

    if keep.is_empty() {
        return Err(
            "A scoped checkout must keep at least one in-scope path. To stop scoping entirely, \
            open a fresh full checkout (a plain bay, or a franchise without --only).".to_string()
        );
    }

    // A dropped prefix that still contains a kept (more specific) prefix cannot be cleanly
    // de-materialized — part of it is still needed. Refuse rather than remove kept files.
    for prefix in &dropped {
        if let Some(inner) = keep.iter().find(|kept| is_strictly_under(kept, prefix)) {
            return Err(format!(
                "Cannot narrow away \"{}\": the kept path \"{}\" is inside it. Narrow the inner \
                path first, or drop both.",
                prefix, inner
            ));
        }
    }

    // Refuse before touching anything if a dropped subtree holds uncommitted work: narrow has
    // no working-directory-preserving path (unlike `shift`, which only ever writes what a target
    // tree names), so a delete here is unconditional and would otherwise silently destroy staged,
    // unstaged or untracked content. Every prefix is checked before any is de-materialized, so a
    // clean prefix earlier in the list is never dropped ahead of a later one that turns out dirty.
    for prefix in &dropped {
        ensure_narrow_target_is_clean(prefix).await?;
    }

    let narrowed = MaterializationScope::from_prefixes(keep);
    scope_utils::set_bay_scope(&narrowed)?;

    // De-materialize each dropped subtree: drop its staged shards (so it stops being reported),
    // then remove its working files. No object-store deletion — narrow frees nothing.
    for prefix in &dropped {
        inventory_utils::remove_inventories_under(prefix)?;
        remove_working_subtree(prefix)?;
    }

    output::emit("narrow", &NarrowReport {
        dropped,
        scope: narrowed.prefixes().to_vec(),
    });

    Ok(())
}

/// Refuse to narrow away `prefix` while it holds uncommitted work: staged changes, unstaged
/// changes to tracked files, or untracked files anywhere under it. Matches the codebase's own
/// precedent (`shift` refuses rather than overwrite untracked content) — narrow's delete is
/// unconditional once it decides to act, so the decision itself must be conservative.
async fn ensure_narrow_target_is_clean(prefix: &str) -> Result<(), String> {
    // Reuses the classifier to test "at or under `prefix`" — a scope of exactly this one
    // prefix classifies everything at or under it as `InScope`, everything else as
    // `OutOfScope` (there is nothing else to be `Spine` for, since it is the whole scope).
    let under = MaterializationScope::from_prefixes([prefix.to_string()]);

    let pallet = pallet_utils::get_current_pallet_name()?;
    let head_tree = match pallet_utils::get_pallet_head(&pallet)? {
        Some(head) => Some(object_utils::load_parcel(&head)?.tree_hash),
        None => None,
    };

    let staged_dirty = stocktake_utils::collect_staged_changes(head_tree.as_deref()).await?
        .into_iter()
        .any(|change| under.classify(&change.path) == ScopeClass::InScope);

    let unstaged = stocktake_utils::collect_unstaged_changes().await?;

    let unstaged_tracked_dirty = unstaged.iter()
        .any(|change| change.kind != ChangeKind::Untracked
            && under.classify(&change.path) == ScopeClass::InScope);

    if staged_dirty || unstaged_tracked_dirty {
        return Err(scope_utils::narrow_unclean_refusal(prefix, "uncommitted tracked changes"));
    }

    let untracked: Vec<&str> = unstaged.iter()
        .filter(|change| change.kind == ChangeKind::Untracked
            && under.classify(&change.path) == ScopeClass::InScope)
        .map(|change| change.path.as_str())
        .collect();

    if !untracked.is_empty() {
        return Err(scope_utils::narrow_unclean_refusal(
            prefix,
            &format!("untracked file(s) ({})", untracked.join(", ")),
        ));
    }

    Ok(())
}

/// Remove a subtree's working files (relative to the checkout root the process entered). A
/// subtree already gone is the desired outcome.
fn remove_working_subtree(prefix: &str) -> Result<(), String> {
    match std::fs::remove_dir_all(prefix) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Error while de-materializing \"{}\": {}", prefix, e)),
    }
}

/// Whether `child` is a strict descendant of the `ancestor` directory (both non-root keys).
fn is_strictly_under(child: &str, ancestor: &str) -> bool {
    child.len() > ancestor.len()
        && child.as_bytes()[ancestor.len()] == b'/'
        && child.starts_with(ancestor)
}

/// The result of a narrow: the paths dropped and the scope that remains.
#[derive(Serialize)]
struct NarrowReport {
    /// The in-scope prefixes dropped from this checkout.
    dropped: Vec<String>,

    /// This checkout's materialization scope after the narrow.
    scope: Vec<String>,
}

impl CommandOutput for NarrowReport {
    fn render_human(&self) {
        println!(
            "Narrowed away {}; those files are de-materialized. Materialization scope now: {}.",
            self.dropped.join(", "), self.scope.join(", ")
        );
        println!("Nothing was freed in the object store — the content is still reachable history.");
    }
}
