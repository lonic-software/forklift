use serde::Serialize;
use forklift_core::error::CoreError;
use forklift_core::util::bay_utils;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::scope_utils::{self, MaterializationScope, ScopeClass};
use forklift_core::util::prune_utils;
use crate::output::{self, CommandOutput};

/// Handle the scope-prune command (§7.6): the deliberate, destructive counterpart of `narrow`.
/// It narrows the shared warehouse fetch scope by dropping a fetched path, then frees the
/// content under that path from the object store — reclaiming disk a `narrow` never could,
/// because a narrowed-away object is still reachable history that reachability-`gc` correctly
/// keeps.
///
/// It is multi-bay-aware: a warehouse can have several checkouts sharing one object store, so
/// it refuses to free a path any checkout still materializes (freeing it would break that
/// checkout the next time it read the now-absent content). It is durable before destructive:
/// the fetch scope is narrowed first, so a partly-finished deletion always leaves the store
/// correct — freed objects read as sealed-but-unfetched, never as missing. Nothing is destroyed
/// on the origin — the pruned content is re-fetchable with `expand`.
///
/// It is also **resumable**: a path already outside the fetch scope — because an earlier prune
/// narrowed it there but was killed before it finished freeing everything — is not refused as
/// "not a fetched path." It is detected and re-derived against the scope as it stands today, so
/// a second call finishes the job (or reports there is nothing left to free) instead of leaving
/// the leftovers stuck forever. A path that was simply never fetched at all lands on the exact
/// same code path and gets the exact same honest answer: nothing to free.
///
/// # Arguments
/// * `paths`   - The fetched path(s) to prune. Each is either a current warehouse fetch-scope
///   prefix (a fresh prune) or already outside it (a resumed one).
/// * `dry_run` - Report what would be freed and change nothing.
///
/// # Returns
/// * `Ok(())`      - If the plan was reported (dry run) or carried out.
/// * `Err(String)` - If the warehouse is not sparse, a path is a live spine ancestor or a
///   sub-path of what is still fetched, the last fetched path would be dropped, or a checkout
///   still materializes the path.
pub fn handle_command(paths: Vec<String>, dry_run: bool) -> Result<(), String> {
    let fetch = scope_utils::read_fetch_scope()?;

    if fetch.is_full() {
        return Err(
            "This warehouse holds the full tree; there is nothing to prune. \"scope-prune\" \
            reclaims disk from a sparse warehouse — one franchised with \"--only\", or widened \
            with \"expand\" — by forgetting a fetched path.".to_string()
        );
    }

    // Each path is either one of the warehouse's current fetch-scope prefixes (a fresh prune:
    // it is dropped from `keep` and the fetch scope narrows this call) or already outside the
    // fetch scope entirely (a resume: nothing to narrow, but there may be leftovers to free from
    // an earlier, interrupted run — see the function doc). A path that is merely a sub-path or a
    // still-needed spine ancestor of a live prefix classifies InScope/Spine, not OutOfScope, so
    // it is refused rather than silently reinterpreted as either case.
    let mut keep: Vec<String> = fetch.prefixes().to_vec();
    let mut freshly_pruned: Vec<String> = Vec::new();
    let mut resumed: Vec<String> = Vec::new();

    for raw in &paths {
        let key = WarehousePath::from_user_input(raw)?.as_key().to_string();

        match keep.iter().position(|prefix| *prefix == key) {
            Some(pos) => {
                keep.remove(pos);
                freshly_pruned.push(key);
            }
            None if fetch.classify(&key) == ScopeClass::OutOfScope => resumed.push(key),
            None => return Err(format!(
                "\"{}\" is not one of the warehouse's fetched paths, so it cannot be pruned. \
                Fetched: {}.",
                raw, fetch.prefixes().join(", ")
            )),
        }
    }

    // Whether every requested path was already outside the fetch scope (a pure resume of an
    // earlier, interrupted prune) rather than newly dropped here — the human report needs this
    // to tell "already pruned" apart from a fresh prune that simply found nothing loose to free.
    let all_resumed = freshly_pruned.is_empty() && !resumed.is_empty();

    if keep.is_empty() {
        return Err(
            "Pruning every fetched path would leave the warehouse with no content at all; keep \
            at least one. To reclaim everything, remove the warehouse.".to_string()
        );
    }

    let post_prune = MaterializationScope::from_prefixes(keep);
    let to_reclaim: Vec<String> = freshly_pruned.iter().chain(resumed.iter()).cloned().collect();

    // Multi-bay hazard, checked for every requested path, resumed ones included: a resumed path
    // can never actually be in a checkout's scope today (a bay can only ever be scoped inside
    // the CURRENT fetch scope, and this path is already outside it — `bay add --scope`/`narrow`
    // enforce that at write time), so this is provably a no-op for `resumed`. It costs nothing
    // to check anyway, and a destructive verb is exactly where redundant defense earns its keep.
    guard_materialization_scopes(&to_reclaim, &post_prune)?;

    let plan = prune_utils::plan_prune(&to_reclaim, &post_prune)?;

    if dry_run {
        output::emit("scope-prune", &PruneReport {
            dry_run: true,
            pruned: to_reclaim,
            scope: post_prune.prefixes().to_vec(),
            freed: 0,
            would_free: plan.to_free.len(),
            still_packed: plan.still_packed,
            retained_shared: plan.retained_shared,
            all_resumed,
        });

        return Ok(());
    }

    // Durable before destructive: narrow the shared fetch scope FIRST (a no-op write when every
    // requested path was already a resume). Once it is narrowed, every scope-aware walk reads
    // the pruned path as out-of-scope, so a crash mid-deletion leaves the store correct — the
    // freed-so-far objects read as sealed-but-unfetched (never as unexpectedly missing), and
    // anything not yet freed is a harmless present-but-out-of-scope extra a later call resumes.
    // The reverse order could leave an in-scope object missing, which is unsafe.
    scope_utils::set_fetch_scope(&post_prune)?;

    let stats = prune_utils::free_objects(&plan.to_free)?;

    output::emit("scope-prune", &PruneReport {
        dry_run: false,
        pruned: to_reclaim,
        scope: post_prune.prefixes().to_vec(),
        freed: stats.freed,
        would_free: plan.to_free.len(),
        still_packed: plan.still_packed,
        retained_shared: plan.retained_shared,
        all_resumed,
    });

    Ok(())
}

/// Refuse the prune if the main tree or any bay still materializes a pruned path. Every
/// checkout's materialization scope is always a subset of the fetch scope; after the prune the
/// fetch scope shrinks, so a checkout whose scope is no longer a subset is one that still needs
/// a path the prune would free. All blockers are named at once.
fn guard_materialization_scopes(pruned: &[String], post_prune: &MaterializationScope) -> Result<(), CoreError> {
    let mut blockers: Vec<String> = Vec::new();

    if !scope_utils::read_main_tree_scope()?.subset_of(post_prune) {
        blockers.push("the main tree".to_string());
    }

    for bay in bay_utils::list_bays()? {
        if !scope_utils::read_bay_scope(&bay)?.subset_of(post_prune) {
            blockers.push(format!("bay \"{}\"", bay));
        }
    }

    if !blockers.is_empty() {
        return Err(scope_utils::scope_prune_blocked_refusal(&pruned.join(", "), &blockers.join(", ")));
    }

    Ok(())
}

/// The result of a scope-prune: what was pruned, the fetch scope that remains, and how much was
/// (or would be) freed.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PruneReport {
    /// Whether this was a dry run (nothing changed).
    dry_run: bool,

    /// The fetched path(s) pruned (forgotten) from the warehouse fetch scope.
    pruned: Vec<String>,

    /// The warehouse fetch scope after the prune.
    scope: Vec<String>,

    /// Loose objects actually freed (`0` on a dry run).
    freed: usize,

    /// Loose objects a prune would free (equals `freed` after a real run).
    would_free: usize,

    /// Candidate objects present only inside a pack: a loose delete cannot reclaim them and a
    /// reachability repack keeps them (they are still reachable history), so a scope-aware
    /// repack is future work. Reported so the count is never silently lost.
    still_packed: usize,

    /// Candidates kept because they are shared (by content hash) with a scope that is still
    /// fetched, or with a meta pallet. Distinct from `still_packed`: this content stays by
    /// design, not pending a future repack.
    retained_shared: usize,

    /// Whether every requested path was already outside the fetch scope before this call — a
    /// pure resume of an earlier, interrupted prune, rather than a path pruned for the first
    /// time here.
    all_resumed: bool,
}

impl CommandOutput for PruneReport {
    fn render_human(&self) {
        if self.dry_run {
            println!(
                "Dry run: pruning {} would free {} loose object(s) and leave the fetch scope at {}.",
                self.pruned.join(", "), self.would_free, self.scope.join(", ")
            );
        } else if self.freed == 0 && self.still_packed > 0 {
            // Nothing loose to reclaim, but the pruned content still exists — packed, not
            // freeable by a loose delete. Saying "already pruned" here would be wrong: this can
            // happen on a fresh prune whose whole target was packed from the start. The footer
            // below names the count and explains the repack is future work.
            println!(
                "{} pruned; nothing was loose to free — its content is packed, awaiting a repack. \
                Fetch scope now: {}.",
                self.pruned.join(", "), self.scope.join(", ")
            );
        } else if self.freed == 0 && self.retained_shared > 0 {
            // Nothing freed because the pruned content is shared (by hash) with a scope that is
            // still fetched, or with a meta pallet — retained by design, not pending anything.
            println!(
                "{} pruned, but nothing was freed: its content is still retained by other scopes. \
                Fetch scope now: {}.",
                self.pruned.join(", "), self.scope.join(", ")
            );
        } else if self.freed == 0 && self.all_resumed {
            // Every requested path was already outside the fetch scope: an earlier, interrupted
            // prune already finished the job, and this call found nothing left to do.
            println!(
                "{} already pruned; nothing left to free. Fetch scope now: {}.",
                self.pruned.join(", "), self.scope.join(", ")
            );
        } else if self.freed == 0 {
            // A fresh prune of a path with no reclaimable, packed, or shared content at all
            // (e.g. it was always empty) — an honest no-op, not an error.
            println!(
                "{} pruned; nothing to free. Fetch scope now: {}.",
                self.pruned.join(", "), self.scope.join(", ")
            );
        } else {
            println!(
                "Pruned {}; freed {} loose object(s). Fetch scope now: {}.",
                self.pruned.join(", "), self.freed, self.scope.join(", ")
            );
        }

        if self.still_packed > 0 {
            println!(
                "{} object(s) are inside packs and are not reclaimed by this prune; a scope-aware \
                repack is not yet built, so they remain on disk (harmless, sealed by hash).",
                self.still_packed
            );
        }

        println!("The pruned content is re-fetchable from the origin with \"expand\".");
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("PruneReport", schemars::schema_for!(PruneReport)),
    ]
}
