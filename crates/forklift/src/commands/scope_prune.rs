use serde::Serialize;
use forklift_core::error::{self, CoreError};
use forklift_core::util::bay_utils;
use forklift_core::util::remote_utils::{self, RemoteClient};
use forklift_core::util::lock_utils::StoreAccessLock;
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
/// on the origin, and nothing is freed that the origin does not hold: it asks the remote for its
/// pallet heads first, and keeps whatever history they do not reach needs under the path — work
/// stacked here and never lifted — so everything freed is re-fetchable with `expand` (FORK-251).
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
///   sub-path of what is still fetched, the last fetched path would be dropped, a checkout
///   still materializes the path, another process is using the object store, or the remote cannot
///   be reached or is not trusted.
pub async fn handle_command(paths: Vec<String>, dry_run: bool) -> Result<(), String> {
    // Judged once before the remote is asked, so a local refusal (a full warehouse, a path that is
    // not fetched, a checkout that still needs it) is reported as itself rather than masked by a
    // remote that cannot be reached, and again under the hold below, which is what the plan uses.
    judge_request(&paths)?;

    // The remote's heads are read next, outside the store hold, so a slow remote never keeps
    // other bays' writers out. They cannot go stale against the plan in a way that frees more: a
    // remote ref only moves forward, so what an earlier read reaches the remote still holds.
    let remote_heads = remote_pallet_heads().await?;

    // Exclusive store access before the judgement the plan uses (FORK-150). The keep-set depends
    // on the fetch scope `judge_request` reads and on every bay's materialization scope, not only
    // on the plan's walk, and the fetch scope is rewritten from that read; so no writer in another
    // bay — a load that dedups against a planned object, an expand widening the fetch scope — may
    // run anywhere between this read and the last deletion. A dry run takes it too, so it reports
    // exactly the plan a real run would carry out. Never waits: refused while any writer holds
    // the store. `main` takes no shared hold for this command, which would refuse this try.
    let store_access = StoreAccessLock::try_exclusive()?;
    let PruneRequest { post_prune, to_reclaim, all_resumed } = judge_request(&paths)?;

    let plan = prune_utils::plan_prune(&to_reclaim, &post_prune, &remote_heads, &store_access)?;

    if dry_run {
        output::emit(&PruneReport {
            dry_run: true,
            pruned: to_reclaim,
            scope: post_prune.prefixes().to_vec(),
            freed: 0,
            would_free: plan.to_free.len(),
            still_packed: plan.still_packed,
            retained_shared: plan.retained_shared,
            kept_unlifted: plan.kept_unlifted,
            unlifted_pallets: plan.unlifted_pallets.clone(),
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

    let stats = prune_utils::free_objects(&plan.to_free, &store_access)?;

    output::emit(&PruneReport {
        dry_run: false,
        pruned: to_reclaim,
        scope: post_prune.prefixes().to_vec(),
        freed: stats.freed,
        would_free: plan.to_free.len(),
        still_packed: plan.still_packed,
        retained_shared: plan.retained_shared,
        kept_unlifted: plan.kept_unlifted,
        unlifted_pallets: plan.unlifted_pallets,
        all_resumed,
    });

    Ok(())
}

/// A scope-prune request judged against the warehouse as it stands.
struct PruneRequest {
    /// The warehouse fetch scope after the prune.
    post_prune: MaterializationScope,
    /// The path keys to reclaim: the freshly pruned ones, then the resumed ones.
    to_reclaim: Vec<String>,
    /// Whether every requested path was already outside the fetch scope.
    all_resumed: bool,
}

/// Judge the requested paths against the fetch scope and every checkout's scope, refusing as the
/// handler documents: a full warehouse, a path that is not fetched, dropping the last fetched
/// path, or a path a checkout still materializes.
///
/// # Arguments
/// * `paths` - The paths as the user gave them.
///
/// # Returns
/// * `Ok(PruneRequest)` - What the prune would do.
/// * `Err(String)`      - The refusal.
fn judge_request(paths: &[String]) -> Result<PruneRequest, String> {
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

    for raw in paths {
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

    Ok(PruneRequest { post_prune, to_reclaim, all_resumed })
}

/// The remote's pallet heads, from a handshake whose trust is judged as a lower would judge it but
/// adopts nothing. The prune frees only what history these heads reach needs, so it never runs
/// without them: an unreachable or untrusted remote refuses the prune before anything changes.
///
/// # Returns
/// * `Ok(Vec<String>)` - The head of every pallet the remote holds.
/// * `Err(String)`     - If no remote is configured, it cannot be reached, or its trust state is
///   refused; the message says nothing was changed.
async fn remote_pallet_heads() -> Result<Vec<String>, String> {
    let note = "\"scope-prune\" asks the remote which history it holds, so that it never frees work \
        that exists only here; nothing was changed.";

    let client = RemoteClient::from_config().map_err(|e| error::append_to_message(e, note))?;
    let info = client.fetch_info().await.map_err(|e| error::append_to_message(e, note))?;
    remote_utils::check_remote_trust(&client, &info, None).map_err(|e| error::append_to_message(e, note))?;

    Ok(info.pallets.into_values().collect())
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

    /// Candidates kept because history the remote does not hold needs them: work stacked here
    /// and never lifted, or lifted to a remote head not lowered here since. Freeing them could
    /// lose the only copy; a later prune of the same path frees them once that history is lifted.
    kept_unlifted: usize,

    /// The pallets whose head the remote does not hold, when `kept_unlifted` is non-zero; lifting
    /// (or lowering) them is what lets a later prune free the rest.
    unlifted_pallets: Vec<String>,

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

        if self.kept_unlifted > 0 {
            let pallets = self.unlifted_pallets.iter()
                .map(|name| format!("\"{}\"", name)).collect::<Vec<_>>().join(", ");
            let paths = self.pruned.iter()
                .map(|path| if path.contains(char::is_whitespace) { format!("'{}'", path) } else { path.clone() })
                .collect::<Vec<_>>().join(" ");

            println!(
                "{} object(s) were kept because history the remote does not hold yet needs them \
                (pallets {}); \
                freeing them could lose the only copy. Lift that work (or lower, if the remote is \
                ahead), then run \"scope-prune {}\" again to free them.",
                self.kept_unlifted, pallets, paths
            );
        }

        if self.dry_run {
            println!("What it frees is re-fetchable from the origin with \"expand\".");
        } else {
            println!("What was freed is re-fetchable from the origin with \"expand\".");
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<PruneReport>(),
    ]
}
