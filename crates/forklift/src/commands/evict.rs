use serde::Serialize;
use forklift_core::util::evict_utils;
use forklift_core::util::lock_utils::StoreAccessLock;
use forklift_core::util::scope_utils;
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
///
/// # Arguments
/// * `dry_run` - Report what would be freed and change nothing.
///
/// # Returns
/// * `Ok(())`      - If the plan was reported (dry run) or carried out.
/// * `Err(String)` - If the warehouse is not a cache, the configured remote is not its origin, the
///   origin cannot be reached or is not trusted, another process is using the object store, or an
///   object the origin's history reaches is missing here and unrecorded (`heal` first).
pub async fn handle_command(dry_run: bool) -> Result<(), String> {
    if !scope_utils::is_cache_mode()? {
        return Err(
            "This warehouse is not a cache; there is nothing to evict. \"evict\" frees content a \
            cache warehouse (one franchised with \"--cache\") can fetch again from its origin.".to_string()
        );
    }

    ensure_origin_remote()?;

    // Read outside the store hold, so a slow origin never keeps other bays' writers out. A ref can
    // move on the origin after this read; the hold does not reach the origin either way.
    let held_heads = crate::remote_heads::held_by_remote(
        "\"evict\" asks the origin which history it holds, so that it only frees what the origin can \
        refill; nothing was changed.",
    ).await?;

    // Exclusive store access from the plan to the last delete: no writer in any bay may dedup
    // against a planned object, or move a checkout or stage work the keep set reads, in between. A
    // dry run takes it too, so it reports the plan a real run would carry out. Never waits. `main`
    // takes no shared hold for this command, which would refuse this try.
    let store_access = StoreAccessLock::try_exclusive()?;
    let plan = evict_utils::plan_evict(&held_heads, &store_access)?;

    let stats = if dry_run {
        None
    } else {
        Some(evict_utils::carry_out(&plan, &store_access)?)
    };

    output::emit(&EvictReport {
        dry_run,
        evicted: stats.as_ref().map_or(0, |stats| stats.evicted),
        would_evict: plan.to_free.len(),
        kept_for_checkouts: plan.kept_for_checkouts,
        kept_local: plan.kept_local,
        not_held: plan.not_held,
        still_packed: plan.still_packed,
        heads_not_lowered: plan.heads_not_lowered,
        unsealed: stats.as_ref().map_or(0, |stats| stats.unsealed),
    });

    Ok(())
}

/// Refuse unless the configured remote is the cache's recorded origin: its heads decide what is
/// freed, and only the origin was proved to hold what this warehouse lacks. No remote configured
/// at all is left to the remote client, which says so plainly.
fn ensure_origin_remote() -> Result<(), String> {
    scope_utils::ensure_remote_is_origin(
        scope_utils::unrecorded_origin_evict_refusal,
        scope_utils::non_origin_evict_refusal,
    ).map_err(Into::into)
}

/// The result of an eviction: how much was (or would be) freed, and why the rest stays.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct EvictReport {
    /// Whether this was a dry run (nothing changed).
    dry_run: bool,

    /// Loose objects freed (`0` on a dry run). A read of one fetches it again from the origin.
    evicted: usize,

    /// Loose objects the plan frees. After a real run, `evicted` is less only by objects already
    /// gone when it came to delete them.
    would_evict: usize,

    /// Objects the origin holds that stay because a checkout's base tree needs them.
    kept_for_checkouts: usize,

    /// Objects the origin holds that stay for a meta pallet, a parked, consolidating or
    /// cherry-picking bay, a trust pin, or staged work.
    kept_local: usize,

    /// Present objects only local history reaches (unlifted work, a haul, a deleted pallet), which
    /// the origin is not known to hold. Lifting that work lets a later eviction free them.
    not_held: usize,

    /// Evictable objects held only inside a pack, which this eviction does not free.
    still_packed: usize,

    /// Origin heads this warehouse does not hold yet; their history frees nothing until a `lower`
    /// brings it in.
    heads_not_lowered: usize,

    /// Kept objects an earlier eviction had recorded as held by hash, and a read had fetched again,
    /// whose record this run removed (`0` on a dry run).
    unsealed: usize,
}

impl CommandOutput for EvictReport {
    fn render_human(&self) {
        if self.dry_run {
            println!("Dry run: evicting would free {} object(s).", self.would_evict);
        } else if self.evicted == 0 && self.unsealed == 0 {
            println!("Nothing to evict.");
        } else if self.evicted == 0 {
            println!("Nothing to evict; {} kept object(s) are no longer recorded as held by hash.", self.unsealed);
        } else {
            println!("Evicted {} object(s); reading one fetches it again from the origin.", self.evicted);
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
                them; lift that work, then run \"evict\" again to free them.",
                self.not_held
            );
        }

        if self.still_packed > 0 {
            println!(
                "{} object(s) are inside packs and were not freed; evicting packed objects is not \
                built yet.",
                self.still_packed
            );
        }

        if self.heads_not_lowered > 0 {
            println!(
                "{} of the origin's head(s) are not in this warehouse yet; run \"lower\" to bring them \
                in, then \"evict\" again to free what their history holds.",
                self.heads_not_lowered
            );
        }
    }
}

#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![crate::docgen::OutputSchema::of::<EvictReport>()]
}
