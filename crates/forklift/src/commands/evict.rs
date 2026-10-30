use serde::Serialize;
use forklift_core::util::evict_utils;
use forklift_core::util::lock_utils::{ServeLock, StoreAccessLock, StoreLock};
use forklift_core::util::scope_utils;
use crate::events;
use crate::output::{self, CommandOutput};

/// Handle the evict command (§7.6.1): free the content of a cache warehouse that its origin holds
/// and nothing here needs, so a later read fetches it again.
///
/// It asks the origin which history it holds first, outside any hold, and refuses before anything
/// changes when the configured remote is not the origin, or the origin cannot be reached or is not
/// trusted. Then, under exclusive store access, it plans: the content of the origin's history,
/// minus every meta pallet, each checkout's base tree, the parcels a parked, consolidating or
/// cherry-picking bay names, the trust pins, and staged work. Every object it frees is recorded
/// first, so an interrupted eviction leaves a readable warehouse and running it again finishes.
/// Loose copies are deleted; packed objects go by a full repack that keeps everything else, so
/// like `compact --all` it refuses while the warehouse root is served. Last, the record is bounded:
/// the record of an evicted object nothing present names any more is dropped.
///
/// # Arguments
/// * `dry_run` - Report what would be freed and change nothing.
///
/// # Returns
/// * `Ok(())`      - If the plan was reported (dry run) or carried out.
/// * `Err(String)` - If the warehouse is not a cache, the configured remote is not its origin, the
///   origin cannot be reached or is not trusted, the root is being served, another process is using
///   the object store, or an object the origin's history reaches is missing here and unrecorded
///   (`heal` first).
pub async fn handle_command(dry_run: bool) -> Result<(), String> {
    if !scope_utils::is_cache_mode()? {
        return Err(
            "This warehouse is not a cache; there is nothing to evict. \"forklift evict\" frees content a \
            cache warehouse (one franchised with \"--cache\") can fetch again from its origin.".to_string()
        );
    }

    // Read outside the store hold, so a slow origin never keeps other bays' writers out. A ref can
    // move on the origin after this read; the hold does not reach the origin either way.
    let held_heads = crate::remote_heads::held_by_remote(
        "\"forklift evict\" asks the origin which history it holds, so that it only frees what the origin can \
        refill; nothing was changed.",
    ).await?;

    // The locks a full repack takes, in its order, held from the plan to the last delete: the store
    // lock keeps out other maintenance, the serve lock a server answering "present" for an object
    // about to go, and exclusive store access every writer that could dedup against a planned
    // object or move a checkout or stage work the keep set reads. A dry run takes them too, so it
    // reports the plan a real run would carry out. None waits. `main` takes no shared hold for
    // this command, which would refuse the last.
    let store_lock = StoreLock::acquire()?;
    let serve_lock = ServeLock::acquire().map_err(|e| format!(
        "\"forklift evict\" cannot run while this warehouse root is being served; stop the server first. If \
        no server is running, the lock below was left by a server, gc or full repack that was killed \
        and can be removed. {}", e
    ))?;
    let store_access = StoreAccessLock::try_exclusive()?;
    let plan = evict_utils::plan_evict(&held_heads, &store_access)?;

    let stats = if dry_run {
        None
    } else {
        Some(evict_utils::carry_out(&plan, &store_lock, &serve_lock, &store_access, &events::progress())?)
    };

    output::emit(&EvictReport {
        dry_run,
        evicted: stats.as_ref().map_or(0, |stats| stats.evicted),
        would_evict: plan.to_evict.len(),
        would_drop: plan.to_drop.len(),
        repack: plan.repack,
        bytes_freed: stats.as_ref().map_or(0, |stats| stats.bytes_freed),
        kept_for_checkouts: plan.kept_for_checkouts,
        kept_local: plan.kept_local,
        not_held: plan.not_held,
        still_packed: stats.as_ref().map_or(0, |stats| stats.still_packed),
        heads_not_lowered: plan.heads_not_lowered,
        unsealed: stats.as_ref().map_or(plan.to_unseal.len(), |stats| stats.unsealed),
        record_dropped: stats.as_ref().map_or(0, |stats| stats.record_dropped),
        record_bound_skipped: stats.and_then(|stats| stats.record_bound_skipped),
    });

    Ok(())
}

/// The result of an eviction: how much was (or would be) freed, and why the rest stays.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct EvictReport {
    /// Whether this was a dry run (nothing changed).
    dry_run: bool,

    /// Objects freed, loose or packed (`0` on a dry run). A read of one fetches it again from the
    /// origin.
    evicted: usize,

    /// Objects the plan frees. After a real run, `evicted` is less only by `still_packed`.
    would_evict: usize,

    /// Of `would_evict`, the objects a pack holds, which a full repack drops.
    would_drop: usize,

    /// Whether the run rewrites every pack (on a dry run, would): when `would_drop` is not `0`, and
    /// also when a packed delta stands on a loose copy it frees, which the repack re-encodes first.
    repack: bool,

    /// Bytes freed on disk: loose files deleted, plus what the old packs held beyond the packs the
    /// repack wrote (`0` on a dry run).
    bytes_freed: u64,

    /// Objects the origin holds that stay because a checkout's base tree needs them.
    kept_for_checkouts: usize,

    /// Objects the origin holds that stay for a meta pallet, a parked, consolidating or
    /// cherry-picking bay, a trust pin, or staged work.
    kept_local: usize,

    /// Present objects only local history reaches (unlifted work, a haul, a deleted pallet), which
    /// the origin is not known to hold. Lifting that work lets a later eviction free them.
    not_held: usize,

    /// Objects the plan frees that are still inside a pack after the repack: one found damaged only
    /// during it, which a repack leaves as found (`0` on a dry run; a pack already damaged refuses
    /// the run). `compact --all` salvages it.
    still_packed: usize,

    /// Origin heads this warehouse does not hold yet; their history frees nothing until a `lower`
    /// brings it in.
    heads_not_lowered: usize,

    /// Kept objects an earlier eviction had recorded as held by hash, and a read had fetched again,
    /// whose record this run removed (on a dry run, would remove).
    unsealed: usize,

    /// Records of evicted objects this run dropped because nothing present names them any more: an
    /// object under an evicted tree is reached only through that tree, whose own record stays
    /// (`0` on a dry run).
    record_dropped: usize,

    /// Why no record was dropped: the run met an object missing here that the record does not hold,
    /// or could not finish, in which case the record may have shrunk though `record_dropped` is `0`
    /// (`null` otherwise, and on a dry run). A missing object keeps every later `evict` from
    /// shrinking the record until that content is back.
    record_bound_skipped: Option<String>,
}

impl CommandOutput for EvictReport {
    fn render_human(&self) {
        if self.dry_run {
            println!("Dry run: evicting would free {} object(s).", self.would_evict);
            if self.would_drop > 0 {
                println!("{} of them are inside packs, which would be rewritten without them.", self.would_drop);
            } else if self.repack {
                println!("Packed content stands on some of them, so the packs would be rewritten first.");
            }
        } else if self.evicted == 0 {
            println!("Nothing to evict.");
        } else {
            println!(
                "Evicted {} object(s), freeing {}; reading one fetches it again from the origin.",
                self.evicted,
                output::human_bytes(self.bytes_freed)
            );
        }

        if self.unsealed > 0 {
            println!(
                "{} kept object(s) {} recorded as held by hash.",
                self.unsealed,
                if self.dry_run { "would no longer be" } else { "are no longer" }
            );
        }

        if let Some(why) = &self.record_bound_skipped {
            println!(
                "The record of evicted objects was not shrunk, or not wholly: {}.",
                why.trim_end_matches('.')
            );
        }

        if self.kept_for_checkouts > 0 {
            println!("{} object(s) were kept because a checkout's files stand on them.", self.kept_for_checkouts);
        }

        if self.kept_local > 0 {
            println!(
                "{} object(s) were kept for metadata, trust records, staged work, or a parked or \
                in-progress operation.",
                self.kept_local
            );
        }

        if self.not_held > 0 {
            println!(
                "{} object(s) were kept because only history the origin does not hold yet reaches \
                them; lift that work, then run \"forklift evict\" again to free them.",
                self.not_held
            );
        }

        if self.still_packed > 0 {
            println!(
                "{} object(s) are inside a damaged pack and were not freed; run \"forklift compact --all\" to \
                salvage it, then \"forklift evict\" again.",
                self.still_packed
            );
        }

        if self.heads_not_lowered > 0 {
            println!(
                "{} of the origin's head(s) are not in this warehouse yet; run \"forklift lower\" to bring them \
                in, then \"forklift evict\" again to free what their history holds.",
                self.heads_not_lowered
            );
        }
    }
}

#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![crate::docgen::OutputSchema::of::<EvictReport>()]
}
