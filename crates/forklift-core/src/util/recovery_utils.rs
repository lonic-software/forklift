//! The `forklift heal` recovery verb's own machinery (DESIGN.html §3.1.1): the deeper analysis
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) refuses into when its
//! best-effort restage pass cannot resolve every recorded path on its own.
//!
//! Entry-heal is deliberately conservative: any recorded path that comes back
//! [`Vanished`](heal_utils::RestageOutcome), [`Unreadable`](heal_utils::RestageOutcome),
//! [`HashMismatch`](heal_utils::RestageOutcome), or fails to restage operationally leaves the
//! whole taint standing, because entry-heal has no way to tell "this vanished object was never
//! referenced by anything durable" (safe — the retry that follows just restages it fresh) apart
//! from "this vanished object is a durable ref's only copy" (real loss) without doing the one
//! thing entry-heal must never do: a repository-scale walk on every command's hot path. This
//! module is that walk, run only here, on demand, by the one command whose whole job is to pay
//! for it.
//!
//! ## The closure walk
//!
//! For every recorded path this attempt cannot restage, [`run`] classifies it (a loose object, a
//! pack data/index file, or an inventory shard — the three shapes the taint schema can ever
//! record) and, for the object-shaped ones, checks whether the object it names is genuinely absent
//! (present elsewhere — another pack, or loose — is not a loss) and, if so, whether *any* durable
//! ref source still reaches it: pallet heads (both namespaces), every bay's parked parcels, tag
//! subjects, every bay's in-progress consolidation (its `their_head`), the trust anchor's adopted
//! head (a re-genesis pin, §8.7), and every bay's staged inventory shards **enumerated from the
//! files on disk**, never the registration ledger (`inventory_utils::write_metadata_to_file` is a
//! non-atomic, unsynced `std::fs::write` that is blind to a published-but-unregistered shard — the
//! phase-B wart; a walk that trusted it could silently miss a real dangling reference). The taint
//! is over the *shared* object store, so every bay-local source is read across every bay
//! (`bay_utils::all_bay_state_dirs`), never just the active one — see
//! [`collect_walk_roots`]'s own doc comment for why. An object no live ref reaches is safe to drop
//! from the taint — the same "absent + unreachable" shape a plain crash already leaves unhealed
//! today, which the ordinary retry-and-restage path handles without this module's help. An object
//! a live ref *does* still reach is reported, machine-coded, with the remedies that actually exist
//! for it.
//!
//! The walk is **read-only by construction**: every object read goes through [`object_utils`]'s
//! plain loaders (`load_parcel`/`load_tree`/`recipe_chunk_hashes`), which never call
//! [`file_utils::does_object_exist`] (the taint-gated presence check — calling it here would trip
//! the very gate this recovery is trying to lift, on its first call, under the tainted root this
//! walk necessarily runs under) and never touch [`crate::util::graph_utils`]'s persisting entry
//! points (`node`/`ensure` durably self-heal a commit-graph shard via `write_file_atomically` —
//! exactly the kind of barrier-on-a-possibly-failing-device this recovery path must never risk).
//! Presence is checked with [`file_utils::raw_object_present`], the one sanctioned gate-free
//! presence check — see its own doc comment for why bypassing the gate is safe specifically here.
//! [`tests::closure_walk_never_touches_a_barrier_or_a_dir_sync`] is the actual enforcer, not this
//! paragraph.
//!
//! ## Presence-tolerant descent (invariant I3)
//!
//! In a sparse/narrowed or shallow warehouse, an object this walk would otherwise need to descend
//! into can be legitimately absent (sealed by a signed hash, never fetched) rather than lost. Every
//! *descent* point in [`walk_closure_for`]/`walk_tree`/`check_leaf` — a parent parcel, a subtree,
//! and a chunked leaf's recipe (the objects this walk would otherwise load and recurse through) —
//! therefore checks [`file_utils::raw_object_present`] **after** that node's own targets-check (a
//! vanished *target* must still be reported as referenced — see
//! [`tests::a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced`]),
//! **unconditionally in both modes below**, and on absence skips descending — it does **not**
//! error. This is sound to *clear* on, not merely tolerate: `collect_walk_roots`'s own doc comment
//! establishes that this walk's root set is a superset of
//! [`crate::util::gc_utils::collect_live_set`]'s, so with identical tolerance any target this walk
//! calls unreferenced is unreferenced under gc's smaller root set too — gc is already entitled to
//! collect it, so a heal that instead kept it tainted forever would brick every command over an
//! object the store's own collector calls garbage. A **present**-but-unloadable object still fails
//! loud (the ordinary `?` after the presence check returns `true`) — tolerance is for absence only,
//! never for corruption.
//!
//! A **terminal** leaf — a plain (blob) file entry, or one chunk hash out of a *present* recipe's
//! chunk list — has no further descent to skip: the walk never loads a blob's or a chunk's bytes
//! either way. Its presence check exists purely to *feed the sink* (see below), so it is gated on
//! the sink actually being present (`Option::is_some`) and skipped — not merely no-opped, the
//! syscall itself is skipped — when there is nothing to feed.
//!
//! The sink is `Option<&mut dyn FnMut(&str)>`: `None` for the common, targeted walk
//! ([`closure_references_any`]) — descent-guard absences are simply skipped, and the (gated)
//! terminal-leaf presence check never runs at all, so the targeted walk pays no per-leaf/per-chunk
//! stat it has no use for. `Some(collector)` for the targetless enumerator
//! ([`enumerate_absent_reachable`]) — every absence found, at every level including terminal
//! leaves and chunks, is recorded. Both share [`walk_closure_for`]'s exact descent (one code path,
//! two modes, so they can never drift apart); [`enumerate_absent_reachable`] additionally passes an
//! **empty target set**, so every node's targets-check is vacuous and every node's descent-guard is
//! actually reached.
//!
//! ## Partial clears
//!
//! A recovery attempt commonly resolves *some* recorded paths (restaged, or proven
//! vanished-and-unreferenced) while others remain genuinely dangling. The taint afterwards must
//! record exactly the unresolved remainder — [`taint_utils::resolve_taints`] is the crash-safe
//! primitive that makes that true without ever leaving a window where the remainder is unrecorded
//! on disk, and that also brings the in-memory gate into agreement with whatever is left standing
//! once that rewrite lands (never derived from `remainder` alone — see its own doc comment).
//!
//! ## Heal-driven refetch (§3.2) — closing the `forklift lower` wedge
//!
//! Before this existed, a vanished-still-referenced remainder was reported with a refusal that
//! named `forklift lower` as the remedy — but `lower` is not exempt from the entry-heal
//! chokepoint (only `Heal`/`Audit` are), so running the very command the message suggested hit
//! the same taint and refused before `lower` ever reached its own fetch. The remedy was
//! unreachable: a wedge.
//!
//! [`resolve_the_rest`] closes it by driving the fetch itself, from inside this locked verb,
//! reusing `lower`'s own fetch machinery ([`attempt_heal_driven_refetch`]) rather than shelling
//! out to `lower` or reimplementing a transport. That function runs **two** passes, not one — a
//! first draft that only reused [`remote_utils::fetch_history_scoped`]/[`remote_utils::
//! fetch_history`] (the exact functions `lower.rs` calls) was found, by actually reproducing the
//! wedge end to end, to leave the wedge's own motivating case unrecovered: those functions never
//! re-descend into a parcel's tree once that parcel is judged already-complete (reachable from a
//! local ref) — exactly the state of the *current*, already-published pallet head, the common
//! case. See [`attempt_heal_driven_refetch`]'s own doc comment for the full reasoning and the
//! second, targeted pass ([`remote_utils::fetch_missing_objects`]) that closes it.
//!
//! - **D1 (breadth).** Every pallet's remote head is walked (pass 1), not just the current
//!   pallet's — a vanished hash may be *referenced* only from a different pallet's not-yet-
//!   locally-known history, which the walk needs to bring in for the reference check below to be
//!   accurate; and every remainder candidate hash is also fetched directly (pass 2), regardless of
//!   which pallet(s) reference it. Cost is bounded by what is actually missing: every fetch
//!   primitive dedups against what is already present.
//! - **D2 (packed landing).** The post-fetch check reuses the exact same
//!   [`file_utils::raw_object_present`] (packed-or-loose) predicate the raw-presence filter above
//!   already uses — a remote that serves an object *packed* satisfies it even though the taint
//!   recorded a *loose* path; a packed object is itself content-addressed and index-verified, so
//!   accepting it never weakens what "recovered" means.
//! - **D3 (all-or-nothing, never a false clear).** Nothing here ever decides "recovered" from
//!   whether the fetch call itself returned `Ok` — [`attempt_heal_driven_refetch`]'s own outcome
//!   ([`RemoteConsultation`]) only ever feeds [`remedy_text`]'s wording, so a residual refusal
//!   never overclaims what this run established. Every remainder hash (ordinary or corrupt) is
//!   independently re-verified via `raw_object_present` after the fetch attempt, whether that
//!   attempt errored, hit a diverged pallet, or ran clean — a hash the fetch did not actually
//!   restore is never cleared, and one that a sibling pallet's history happened to also carry
//!   clears exactly the same way an ordinary present object would.
//! - **D4 (force-fetch a corrupt remainder entry).** An ordinary fetch dedups against *present*
//!   objects, so a corrupt-but-present recorded path (`attempt.hash_mismatch`) would never be
//!   re-fetched — `does_object_exist` already says "yes". Before the fetch runs,
//!   [`resolve_the_rest`] deletes that corrupt loose dentry (delete-then-fetch): the object is
//!   transiently absent for the rest of this call, which is fine — `heal` holds the warehouse
//!   lock throughout — and the ordinary dedup-aware fetch then pulls a good copy in exactly like
//!   any other vanished candidate. A genuinely vanished (not corrupt) candidate needs no deletion.
//!
//! **Scope boundary, stated honestly:** this recovers only objects a configured remote actually
//! has. An object that vanished *before* it was ever published (lifted) has no remote copy to
//! find — it stays in the remainder, and franchise / reproduce / accept-loss remain its only
//! exits (see [`HEAVYWEIGHT_EXITS`]). A torn taint is never routed through this refetch
//! *directly* — its unknown scope is resolved first by the store-wide rescan below, which
//! produces an ordinary (non-torn) remainder that the *next* `forklift heal` invocation then
//! carries through this exact refetch pipeline like any other dangling reference.
//!
//! ## Torn rescan (§8.3) — giving a torn taint an in-tool exit (invariant I5)
//!
//! Before this, a torn taint (`state.torn`, `taint_utils`'s module doc comment: a crash mid-write
//! left the recorded path list an unknown lower bound, never a full scope) was a permanent brick:
//! entry-heal refused it (`heal_utils::heal_if_tainted`, unchanged by this section — see below),
//! and so did this verb, immediately, with no remedy this tool could actually drive. [`run`]
//! now resolves it automatically, with no new flag, by converting the *unknown* scope into a
//! *known*, honest remainder — see [`rescan_torn_taint`]. Three steps, one atomic contract (the
//! taint record is replaced only at the very end, so a crash mid-rescan simply leaves torn
//! standing for an idempotent rerun — restaging a path twice is harmless, see
//! [`heal_utils::restage_object`]'s own soundness section):
//!
//! 1. **Restage every present recordable-shape path, enumerated *by directory*, never by
//!    reachability, and never the torn record's own (untrusted, lower-bound) recorded set:**
//!    every loose object under `objects/`'s fan-out, every `.pack`/`.idx` file under
//!    `objects/pack/`, and every staged inventory shard `data` file across every bay (see
//!    [`enumerate_store_wide_paths`]) — each driven through the exact same
//!    [`heal_utils::restage_object`] discipline (hash-verify a loose object, verbatim rewrite for
//!    anything else this schema can record) the ordinary restage pass uses. Directory-driven is
//!    load-bearing: an *un*reachable present-but-unproven object is still dedup bait for a future
//!    write once the taint clears, so it must be restaged too, exactly like §8.4 keeps
//!    `taint_recheck` root-wide rather than scoped to a smaller "obviously relevant" set. A
//!    hash-mismatched loose object is the verb's **existing** corrupt-present rule
//!    (`resolve_the_rest`'s own `attempt.hash_mismatch` handling above), just at store-wide scope
//!    — unconditionally into the remainder, whether or not anything currently references it (a
//!    corrupt object is dedup bait either way: `does_object_exist` would answer "yes" for it).
//!    **Honest cost:** hash-verifying every present loose object surfaces any pre-existing latent
//!    corruption — even in unreferenced garbage — as a standing remainder; resolvable later via
//!    §3.2 D4's force-fetch once the object is published. The loose-object pass (ordinarily the
//!    largest of the three sets by far) is parallelized above a threshold via
//!    [`heal_utils::attempt_restage_all_parallel`]; packs and shards stay serial for v1 (see that
//!    function's own doc comment on why `fanout_utils`, not `crate::model::task::TaskExecutor`,
//!    is the right tool, and why packs/shards are left serial). Progress is reported in
//!    per-phase, per-count lines via an optional callback (never printed from this crate — see
//!    [`run`]'s own doc comment) — this rescan is allowed to be slow, never silent.
//! 2. **Enumerate the vanished-referenced unknowns** via [`enumerate_absent_reachable`] — the
//!    *targetless* sibling of [`closure_references_any`] built in §8.1, rooted at the same
//!    [`collect_walk_roots`]. Its corrupt-boundary parameter is exactly step 1's
//!    hash-mismatched-hash set: a reachable node step 1 already proved corrupt must be treated as
//!    a recorded boundary (recorded, not descended, no abort) — otherwise one corrupt reachable
//!    tree would abort the whole rescan and re-brick torn, the opposite of I5. Genuine corruption
//!    step 1 could not see (it never hash-verifies pack *contents* — only pack dentries move
//!    verbatim) still fails this walk loud, exactly as it always has: an anomalous,
//!    deeper-than-torn integrity problem, not a silent brick.
//! 3. **[`taint_utils::resolve_taints`]** with the union of step 2's referenced-absent hashes and
//!    step 1's corrupt hashes, each encoded as its loose fan-out path
//!    ([`file_utils::get_path_for_object`]) — the same path shape [`classify_vanished`] round-trips
//!    back to `Loose(hash)` on the *next* (now non-torn) `forklift heal` run, so the existing
//!    dangling machinery and §3.2's refetch pick it up exactly like any other remainder, with no
//!    new schema or re-entry logic. Empty → the gate clears too (owned by the same call). Non-empty
//!    → refuse (see [`torn_rescan_dangling_refusal`]), naming exactly what remains.
//!
//! **Invariant I5:** `forklift heal` on a torn taint terminates in a well-formed remainder
//! (possibly empty) — or fails loud only on an anomalous corruption a hash-verify cannot
//! pre-classify (above all, corruption *inside* a pack) — never a silent brick. **Entry-heal's own
//! torn refusal is completely unchanged** (`heal_utils::heal_if_tainted`, `state.torn` still
//! refuses immediately, lock-free, directing to `forklift heal`) — only this locked verb rescans;
//! doing so lock-free would be exactly the unsound, non-content-addressed restage I1/I2 exist to
//! forbid, at store-wide scale.

use std::collections::{BTreeMap, BTreeSet, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use crate::enums::dir_entry_type::DirEntryType;
use crate::error::{CoreError, RefusalCode};
use crate::globals::{forklift_root, FOLDER_NAME_INVENTORY_ROOT};
use crate::util::{
    bay_utils, file_utils, heal_utils, object_utils, office_utils, pack_utils,
    pallet_utils, remote_utils, scope_utils, sign_utils, tag_utils, taint_utils,
};

/// How many dangling references a refusal message names individually before summarizing the
/// rest as "(and N more)" — mirrors `audit_utils::MAX_NAMED_MISSING_CHUNKS`'s reasoning: a pack
/// whose every object turns out dangling could otherwise produce a message with thousands of
/// lines, and a handful is enough for an operator (or an agent) to act on.
const MAX_NAMED_DANGLING: usize = 16;

/// Below this many candidate loose objects, the §8.3 torn rescan's directory-driven restage pass
/// runs serially ([`heal_utils::attempt_restage_all`]) rather than paying `fanout_map`'s
/// thread-spawn cost for a batch small enough that it would not recoup it — mirrors
/// `audit_utils::PARALLEL_THRESHOLD`'s own reasoning (and its value: a loose-object restage's
/// per-item cost — read, decompress, hash-verify, write, fsync — is the same order of magnitude
/// as audit's per-parcel signature verify).
const PARALLEL_RESTAGE_THRESHOLD: usize = 256;

/// The heavyweight exits named in a dangling or every-remedy-exhausted refusal, stated once so
/// the wording never drifts between the refusal sites that need it (`dangling_refusal`,
/// `torn_rescan_dangling_refusal`). Deliberately never names "forklift lower" as a manual remedy:
/// for a non-torn taint, `run`/`resolve_the_rest` already drives that exact fetch itself, inside
/// this locked verb, *before* this text is ever reached (see [`attempt_heal_driven_refetch`]) —
/// naming it here would be false prose, since the very re-run this message would suggest is either
/// redundant (already tried) or, pre-fix, unreachable (the wedge this behavior exists to close).
/// A torn taint's own rescan (§8.3, [`rescan_torn_taint`]) does not drive this refetch either —
/// its remainder is left for the *next* `forklift heal` invocation to carry through it — so
/// "franchise a fresh clone" is named here too, for the same reason: it is the one remedy that is
/// never blocked by *this* warehouse's own taint, torn or not, since it never re-enters it.
const HEAVYWEIGHT_EXITS: &str = "franchise a fresh clone from a configured remote if one still \
    has what is missing (\"forklift franchise\"), reproduce it by re-running whatever operation \
    created it if its content still exists in your working tree (\"forklift load\" then \
    \"forklift stack\"), or accept the loss — Forklift has no in-tool way to drop a single \
    dangling reference yet, so an object neither remedy reaches stays lost until one of those \
    two applies.";

/// What running `forklift heal` accomplished, on a full clear (see [`run`]'s `Ok` case). An
/// unresolved remainder is reported as an [`Err(CoreError)`](CoreError) instead — see [`run`].
#[derive(Debug)]
pub struct HealOutcome {
    /// Whether anything was actually tainted at all — an untainted warehouse still reports
    /// success, with every list empty, so the command has something honest to say either way.
    pub was_tainted: bool,
    /// Recorded paths that were present, verified, and freshly rewritten.
    pub restaged: Vec<String>,
    /// Recorded paths (loose-object or pack-derived hashes, or a vanished shard's own path)
    /// resolved without a rewrite: proven absent *and* unreferenced by the closure walk, present
    /// in a pack despite a vanished loose dentry (I4, `heal_utils::RestageOutcome::RecoveredPacked`),
    /// or (for a shard) a staging concern that carries no object-trust risk at all.
    pub resolved: Vec<String>,
    /// Advisory notes that never block clearing: the "re-run the load" remedy note for each
    /// vanished inventory shard, and a note for each bay whose local state could not be read this
    /// run (see `collect_walk_roots`'s `Tolerate` policy) naming the bay and how to clean it up.
    pub notes: Vec<String>,
}

impl HealOutcome {
    fn nothing() -> HealOutcome {
        HealOutcome { was_tainted: false, restaged: Vec::new(), resolved: Vec::new(), notes: Vec::new() }
    }
}

/// Run the recovery verb once: read the standing taint (if any), attempt the same restage entry-
/// heal runs, and — for whatever it could not resolve — run the deeper, closure-walk-backed
/// analysis this module exists for. See the module doc comment for the full per-verdict behavior.
///
/// `progress`, when given, is called with per-phase/per-count human-readable status lines during
/// a §8.3 torn rescan (the one path here slow enough to need them — see [`rescan_torn_taint`]).
/// This crate never prints (`forklift-core`'s own architecture rule, DESIGN.html §3.4): the
/// caller (the `forklift` CLI head) supplies a closure that renders through its own `human!`
/// macro, which is itself silent under `--json`. `None` is a legitimate, silent caller (every
/// existing test, and any future caller that does not want status chatter).
///
/// # Returns
/// * `Ok(HealOutcome)` - Nothing was tainted, or the taint is now **fully** cleared (every
///                       recorded path reached restaged, or vanished-and-unreferenced/a resolved
///                       shard note). The in-memory gate is cleared too.
/// * `Err(CoreError)`  - A [`RefusalCode::DurabilityTaint`] refusal: at least one reference remains
///                       genuinely dangling after the walk (torn or not). The taint is rewritten to
///                       record exactly the unresolved remainder (never the original full set,
///                       never nothing) and the gate is left standing.
pub async fn run(progress: Option<&dyn Fn(&str)>) -> Result<HealOutcome, CoreError> {
    let root = forklift_root();
    let state = taint_utils::read_taints(&root).map_err(|e| read_failure_refusal(&root, &e))?;

    // §8.3 (I5): a torn taint's own recorded set is an untrusted lower bound, so it is never
    // driven through the ordinary restage pass below — the directory-driven rescan ignores it
    // entirely and derives an honest remainder from scratch. Entry-heal's own torn refusal
    // (`heal_utils::heal_if_tainted`) is completely unaffected by this — it still refuses torn
    // immediately, lock-free; only this locked verb rescans.
    if state.torn {
        return rescan_torn_taint(&root, &state.files, progress);
    }
    if state.recorded.is_empty() {
        // Companion fix (DESIGN.html §3.1.1): "nothing recorded" says nothing about the gate — a
        // durable write that failed *after* `record_taint`'s own `set_gate` succeeded can leave
        // the gate standing over an empty directory. Route through the same gate-owning primitive
        // rather than returning directly, so that stray gate is reconciled against disk (cleared,
        // since nothing is actually standing) instead of persisting for the rest of this process's
        // life. Empty remainder, empty snapshot: there is no file-level work to do here, only the
        // gate sync.
        taint_utils::resolve_taints(&root, &BTreeSet::new(), &[])
            .map_err(|e| sync_failure_refusal(&root, &e))?;
        return Ok(HealOutcome::nothing());
    }

    let attempt = heal_utils::attempt_restage_all(&root, &state.recorded);

    if attempt.all_clean() {
        heal_utils::finish_clean_heal(&root, &attempt.restaged, &state.files)
            .map_err(|e| sync_failure_refusal(&root, &e))?;
        return Ok(HealOutcome {
            was_tainted: true,
            restaged: display_paths(attempt.restaged.iter()),
            // I4: a pack-recovered path was never rewritten — it belongs in `resolved`, exactly
            // like a vanished-and-unreferenced hash, not in `restaged`.
            resolved: display_paths(attempt.recovered_packed.iter()),
            notes: Vec::new(),
        });
    }

    // Lock in whatever DID restage cleanly this attempt, independent of how the rest resolves —
    // a successfully-restaged path's durability must never wait on the deeper analysis below.
    if !attempt.restaged.is_empty() {
        let parents: BTreeSet<PathBuf> = attempt.restaged.iter()
            .filter_map(|relative| root.join(relative).parent().map(Path::to_path_buf))
            .collect();
        heal_utils::sync_restaged_parents(&parents).map_err(|e| sync_failure_refusal(&root, &e))?;
    }

    resolve_the_rest(&root, &state.recorded, &attempt, &state.files).await
}

/// The deeper analysis: classify every path [`heal_utils::attempt_restage_all`] could not
/// restage, run the closure walk over whatever turned out to be genuinely missing, and rewrite
/// the taint to record exactly what remains dangling. Split out of [`run`] only for readability —
/// still called exactly once per invocation.
///
/// `taint_files` is [`run`]'s own `state.files` — the snapshot [`taint_utils::read_taints`]
/// returned before this whole analysis (including the closure walk, which can run for minutes)
/// began. It is threaded straight through to [`taint_utils::resolve_taints`] so the eventual
/// rewrite deletes exactly the files that predate this call, never whatever the taint directory
/// holds by the time the walk finally finishes — see that function's doc comment.
async fn resolve_the_rest(
    root: &Path,
    recorded: &BTreeSet<PathBuf>,
    attempt: &heal_utils::RestageAttempt,
    taint_files: &[PathBuf],
) -> Result<HealOutcome, CoreError> {
    let mut remainder: BTreeSet<PathBuf> = BTreeSet::new();
    let mut dangling_lines: Vec<String> = Vec::new();

    // §3.2 D4: a corrupt-present recorded path is force-fetch-eligible when a remote is
    // configured — computed up front (cheap, local, no network) since the hash-mismatch
    // classification below needs it. With no remote, a corrupt entry behaves exactly as before
    // this slice: straight to the remainder, its bytes left untouched (deleting them would be
    // pure data loss with no possible recovery).
    let remote_configured = remote_utils::RemoteClient::from_config().is_ok();

    for (relative, error) in &attempt.unreadable {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("unreadable: \"{}\" ({})", relative.to_string_lossy(), error));
    }

    // D4: force-fetch bridge. `corrupt_candidates` collects exactly the entries this run will
    // attempt to recover via delete-then-fetch; anything not in it (no remote configured, or a
    // hash-mismatch path with no shape `hash_from_object_path` recognizes — unreachable in
    // practice, see `restage_object`'s `HashMismatch` verdict) commits straight to the remainder
    // below, unchanged from before this slice.
    let mut corrupt_candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
    for relative in &attempt.hash_mismatch {
        match (remote_configured, file_utils::hash_from_object_path(relative)) {
            (true, Some(hash)) => {
                // Delete-then-fetch (D4): force the corrupt object to genuinely vanish so the
                // ordinary fetch's dedup (`does_object_exist`) does not skip it as "already
                // present" — see the module doc comment. Safe under the lock `heal` holds
                // throughout: the object is transiently absent, never observed by anything else.
                // Load-bearing for correctness, not just for the fetch's own dedup: skipping it
                // was confirmed (by reverting this exact block) to make the post-check below a
                // false clear, not merely a missed recovery — `raw_object_present`'s loose branch
                // is a bare `fs::exists`, so a corrupt-but-still-present dentry left in place
                // would itself read back as "present" and get reported resolved with the corrupt
                // bytes never actually replaced.
                if let Err(e) = std::fs::remove_file(root.join(relative)) {
                    if e.kind() != std::io::ErrorKind::NotFound {
                        return Err(sync_failure_refusal(root, &format!(
                            "could not remove the corrupt copy of \"{}\" to make way for a \
                            re-fetched good one: {}", relative.to_string_lossy(), e
                        )));
                    }
                }
                corrupt_candidates.insert(hash, relative.clone());
            }
            _ => {
                remainder.insert(relative.clone());
                dangling_lines.push(format!(
                    "corrupt (content does not match its own hash): \"{}\"", relative.to_string_lossy()
                ));
            }
        }
    }

    for (relative, error) in &attempt.restage_failed {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("could not be restaged: \"{}\" ({})", relative.to_string_lossy(), error));
    }

    let mut shard_vanished: Vec<PathBuf> = Vec::new();
    let mut loose_candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
    let mut pack_stems: BTreeSet<PathBuf> = BTreeSet::new();

    for relative in &attempt.vanished {
        match classify_vanished(relative) {
            VanishedClass::Loose(hash) => { loose_candidates.insert(hash, relative.clone()); }
            VanishedClass::Shard => shard_vanished.push(relative.clone()),
            VanishedClass::PackData(stem) | VanishedClass::PackIndex(stem) => { pack_stems.insert(stem); }
            VanishedClass::Unrecognized => {
                remainder.insert(relative.clone());
                dangling_lines.push(format!(
                    "of a shape the recovery walk does not recognize and cannot auto-classify: \"{}\"",
                    relative.to_string_lossy()
                ));
            }
        }
    }

    // Resolve every vanished pack's stem: enumerate its surviving index, or escalate.
    let mut pack_candidates: BTreeMap<String, PathBuf> = BTreeMap::new();
    for stem in &pack_stems {
        let index_relative = stem.with_extension(pack_utils::PACK_INDEX_EXTENSION);
        let data_relative = stem.with_extension(pack_utils::PACK_DATA_EXTENSION);
        let index_absolute = root.join(&index_relative);

        let enumerated = if index_absolute.exists() {
            pack_utils::hashes_in_index_file(&index_absolute).ok()
        } else {
            None
        };

        match enumerated {
            Some(hashes) => {
                for hash in hashes {
                    pack_candidates.entry(hash).or_insert_with(|| data_relative.clone());
                }
            }
            None => {
                remainder.insert(data_relative.clone());
                if recorded.contains(&index_relative) {
                    remainder.insert(index_relative.clone());
                }
                dangling_lines.push(format!(
                    "pack \"{}\" is non-enumerable (its index is missing or unreadable), so its \
                    contents cannot be walked and it cannot be auto-healed",
                    data_relative.to_string_lossy()
                ));
            }
        }
    }

    // §3.2: heal drives the fetch itself, inside this locked verb, reusing `lower`'s own fetch
    // machinery — the wedge fix (see the module doc comment). Runs once, ahead of the raw-
    // presence filter below, so a hash recovered this way is indistinguishable from one that was
    // merely present elsewhere all along — one classification pass, not two. Every candidate this
    // run might still be able to close is named up front (ordinary vanished-and-possibly-
    // referenced hashes, and D4's force-vanished corrupt ones) — `attempt_heal_driven_refetch`
    // needs the exact set for its targeted fetch (D1-gap bridge, see its own doc comment).
    let candidate_hashes: Vec<String> = loose_candidates.keys()
        .chain(pack_candidates.keys())
        .chain(corrupt_candidates.keys())
        .cloned()
        .collect();

    let consultation = if !remote_configured {
        // `remote_configured` is `false` for two different reasons `RemoteClient::from_config`
        // conflates into one `Err`: genuinely nothing set, or something set but unusable (a
        // config read/parse error, or a URL `RemoteClient::new` rejects). Telling them apart
        // (`remote_utils::is_configured`) matters here specifically: reporting the second as the
        // first steers a user with a real, merely broken remote toward the heavyweight remedies
        // (franchise / reproduce / accept the loss) for an object that remote may still actually
        // have, once whatever is wrong with its config is fixed.
        if remote_utils::RemoteClient::is_configured() {
            RemoteConsultation::ConsultedWithErrors
        } else {
            RemoteConsultation::NotConfigured
        }
    } else if candidate_hashes.is_empty() {
        // Nothing a fetch could help with this round (e.g. every remaining recorded path is
        // `unreadable`/`restage_failed`, never object-shaped) — skip the network entirely.
        RemoteConsultation::ConsultedCleanly
    } else {
        attempt_heal_driven_refetch(&candidate_hashes).await
    };

    // Raw-presence filter: a candidate present elsewhere (another pack, or loose) was never
    // actually lost — only what remains absent everywhere needs the closure walk at all. This is
    // also §3.2 D3's actual safety net: whether a hash just got fetched, was already present, or
    // the fetch attempt above errored/skipped it entirely, this is the one check that decides
    // "recovered" — never the fetch call's own `Ok`/`Err`.
    let mut truly_missing: BTreeSet<String> = BTreeSet::new();
    // I4: a pack-recovered path was never rewritten and never entered `attempt.vanished` in the
    // first place (`restage_object` resolved it directly) — it belongs in `resolved` alongside
    // the vanished-and-unreferenced hashes this loop finds below, not in `remainder`.
    let mut resolved: Vec<String> = display_paths(attempt.recovered_packed.iter());

    for hash in loose_candidates.keys().chain(pack_candidates.keys()) {
        match file_utils::raw_object_present(hash) {
            Ok(true) => resolved.push(hash.clone()),
            Ok(false) => { truly_missing.insert(hash.clone()); }
            Err(e) => return Err(walk_failure_refusal(root, &e)),
        }
    }

    let walk = closure_references_any(&truly_missing).map_err(|e| walk_failure_refusal(root, &e))?;
    let referenced = &walk.hashes;

    for hash in &truly_missing {
        if referenced.contains(hash) {
            if let Some(path) = loose_candidates.get(hash) {
                remainder.insert(path.clone());
            }
            if let Some(pack_path) = pack_candidates.get(hash) {
                remainder.insert(pack_path.clone());
                let index_relative = pack_path.with_extension(pack_utils::PACK_INDEX_EXTENSION);
                if recorded.contains(&index_relative) {
                    remainder.insert(index_relative);
                }
            }
            dangling_lines.push(format!(
                "vanished and still referenced: \"{}\" ({})", hash, remedy_text(consultation)
            ));
        } else {
            resolved.push(hash.clone());
        }
    }

    // D4: re-verify each corrupt candidate the exact same way (`raw_object_present`, packed-or-
    // loose) — sound here specifically because its corrupt dentry was deleted above, before the
    // fetch ran: the loose path can only be occupied again by freshly fetched, hash-verified
    // bytes, or a pack hit, never by the original corrupt bytes. Unlike the ordinary vanished
    // set, a corrupt candidate is never exonerated by "unreferenced" — this mirrors the verb's
    // existing corrupt-is-always-remainder-until-recovered rule; only a genuine recovery (a good
    // copy landed) clears it, regardless of what the closure walk would say about reachability.
    for (hash, relative) in &corrupt_candidates {
        let recovered = match file_utils::raw_object_present(hash) {
            Ok(present) => present,
            Err(e) => return Err(walk_failure_refusal(root, &e)),
        };

        if recovered {
            resolved.push(relative.to_string_lossy().into_owned());
        } else {
            remainder.insert(relative.clone());
            dangling_lines.push(format!(
                "corrupt (content does not match its own hash): \"{}\" ({})",
                relative.to_string_lossy(), remedy_text(consultation)
            ));
        }
    }

    let mut notes: Vec<String> = Vec::new();
    for relative in &shard_vanished {
        resolved.push(relative.to_string_lossy().into_owned());
        notes.push(format!(
            "the staged inventory shard \"{}\" is gone; if that staging state mattered, re-run \
            the load that produced it — a vanished shard is staging state, not an object \
            reference, so it never blocks clearing this taint",
            relative.to_string_lossy()
        ));
    }
    // Any bay `collect_walk_roots` had to skip (see `closure_references_any`'s doc comment) — the
    // walk above still ran and still decided every hash's fate, but only as completely as every
    // *readable* bay could make it, so this is surfaced too rather than silently folded away.
    notes.extend(walk.degraded_bays.clone());

    // `resolve_taints` owns the gate too now (DESIGN.html §3.1.1): it derives the clear/set
    // decision from what is actually left standing in the taint directory once this rewrite
    // lands, not from `remainder` alone — a mid-run store failure elsewhere in this same call
    // (the heal-driven refetch just above, fully awaited) can leave a fresh taint file standing
    // even when `remainder` itself comes back empty, and that fresh file must keep the gate set.
    taint_utils::resolve_taints(root, &remainder, taint_files)
        .map_err(|e| sync_failure_refusal(root, &e))?;

    if remainder.is_empty() {
        Ok(HealOutcome {
            was_tainted: true,
            restaged: display_paths(attempt.restaged.iter()),
            resolved,
            notes,
        })
    } else {
        Err(dangling_refusal(root, &dangling_lines))
    }
}

/// §8.3 (I5): resolve a torn taint via a directory-driven, store-wide rescan — see the module doc
/// comment's "Torn rescan" section for the full design. One atomic contract: `old_files` (the
/// torn snapshot [`run`]'s own [`taint_utils::read_taints`] call captured) is replaced only at the
/// very end, via [`taint_utils::resolve_taints`] — so a crash anywhere in this
/// function leaves the original torn record standing exactly as it was, and a rerun simply redoes
/// whatever restaging already happened (idempotent, never lossy).
///
/// Deliberately does **not** drive [`attempt_heal_driven_refetch`] itself: by the time this
/// returns, the taint (if non-empty) is an ordinary, well-scoped remainder — no longer torn — so
/// the *next* `forklift heal` invocation resolves it through the completely ordinary
/// [`resolve_the_rest`] pipeline, refetch included, exactly like any other dangling remainder.
/// This function's entire job is turning "unknown scope" into "a known, honest one."
fn rescan_torn_taint(
    root: &Path,
    old_files: &[PathBuf],
    progress: Option<&dyn Fn(&str)>,
) -> Result<HealOutcome, CoreError> {
    let report = |line: String| { if let Some(f) = progress { f(&line); } };

    let candidates = enumerate_store_wide_paths(root).map_err(|e| rescan_failure_refusal(root, &e))?;
    let (loose_count, packs_count, shards_count) =
        (candidates.loose.len(), candidates.packs.len(), candidates.shards.len());

    report(format!(
        "torn durability taint: restaging {} loose object(s), {} pack file(s), and {} staged \
        inventory shard file(s) across the whole object store — every present loose object is \
        hash-verified, so this can take a while on a large or uncompacted store",
        loose_count, packs_count, shards_count,
    ));

    // The loose-object pass is the one that actually pays the hash-verify cost (see the module
    // doc comment's honest-cost note) and is ordinarily by far the largest of the three sets —
    // parallelized above a threshold since every path here is fully independent (own final name,
    // own temp file; see `heal_utils::attempt_restage_all_parallel`'s own doc comment for why
    // `fanout_utils`, not `TaskExecutor`, is the right tool). Packs and shards are ordinarily far
    // fewer and kept serial for v1 — a store with enough of either for that to matter on its own
    // is a real follow-up, not today's shape.
    let loose_set: BTreeSet<PathBuf> = candidates.loose.into_iter().collect();
    let loose_attempt = if loose_set.len() >= PARALLEL_RESTAGE_THRESHOLD {
        heal_utils::attempt_restage_all_parallel(root, &loose_set)
    } else {
        heal_utils::attempt_restage_all(root, &loose_set)
    };
    let pack_and_shard: BTreeSet<PathBuf> =
        candidates.packs.into_iter().chain(candidates.shards).collect();
    let ps_attempt = heal_utils::attempt_restage_all(root, &pack_and_shard);

    let restaged: BTreeSet<PathBuf> =
        loose_attempt.restaged.iter().chain(ps_attempt.restaged.iter()).cloned().collect();
    if !restaged.is_empty() {
        let parents: BTreeSet<PathBuf> = restaged.iter()
            .filter_map(|relative| root.join(relative).parent().map(Path::to_path_buf))
            .collect();
        heal_utils::sync_restaged_parents(&parents).map_err(|e| sync_failure_refusal(root, &e))?;
    }

    let mut remainder: BTreeSet<PathBuf> = BTreeSet::new();
    let mut dangling_lines: Vec<String> = Vec::new();

    // Operationally-unresolvable paths (an OS-level read failure, or the restage write itself
    // failing) go straight to the remainder, unconditionally — the same rule the ordinary
    // (non-torn) `resolve_the_rest` already applies to these same two categories above, just at
    // store-wide scope here rather than the recorded set's.
    for (relative, error) in loose_attempt.unreadable.iter().chain(ps_attempt.unreadable.iter()) {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("unreadable: \"{}\" ({})", relative.to_string_lossy(), error));
    }
    for (relative, error) in loose_attempt.restage_failed.iter().chain(ps_attempt.restage_failed.iter()) {
        remainder.insert(relative.clone());
        dangling_lines.push(format!("could not be restaged: \"{}\" ({})", relative.to_string_lossy(), error));
    }

    // Corrupt-present (loose only — a pack/shard restage is verbatim, never hash-verified, so it
    // can never classify `HashMismatch`): the verb's EXISTING rule (see `resolve_the_rest`'s own
    // `attempt.hash_mismatch` handling above), just at store-wide scope — unconditionally into the
    // remainder, regardless of whether anything currently references it (a corrupt object is
    // dedup bait either way). Also collected as hashes: step 2's corrupt-boundary carve-out.
    let mut corrupt_hashes: BTreeSet<String> = BTreeSet::new();
    for relative in &loose_attempt.hash_mismatch {
        remainder.insert(relative.clone());
        dangling_lines.push(format!(
            "corrupt (content does not match its own hash): \"{}\"", relative.to_string_lossy()
        ));
        if let Some(hash) = file_utils::hash_from_object_path(relative) {
            corrupt_hashes.insert(hash);
        }
    }

    report(format!(
        "torn durability taint: step 1 done ({} restaged, {} corrupt-present, {} unreadable or \
        failed to restage); walking every durable ref source for anything still referenced but \
        genuinely absent...",
        restaged.len(), loose_attempt.hash_mismatch.len(),
        loose_attempt.unreadable.len() + loose_attempt.restage_failed.len()
            + ps_attempt.unreadable.len() + ps_attempt.restage_failed.len(),
    ));

    // Step 2: the targetless enumerator (§8.1's shared walk core, collecting mode) — every hash
    // still reachable from a durable ref source that is genuinely absent. `corrupt_hashes` is the
    // carve-out (see `enumerate_absent_reachable`'s own doc comment): a reachable node step 1
    // already proved corrupt is a recorded boundary, never re-loaded — without it, one corrupt
    // reachable tree would abort the whole rescan and re-brick torn (I5).
    let walk = enumerate_absent_reachable(&corrupt_hashes)
        .map_err(|e| rescan_failure_refusal(root, &e))?;
    let referenced_absent = &walk.hashes;

    for hash in referenced_absent {
        let relative = loose_remainder_path(root, hash).map_err(|e| rescan_failure_refusal(root, &e))?;
        remainder.insert(relative);
        dangling_lines.push(format!("vanished and still referenced: \"{}\"", hash));
    }

    // Step 3: replace the torn record with exactly this remainder, and let the same call
    // reconcile the gate against whatever is left standing on disk (see `resolve_taints`'s own
    // doc comment) — no in-process refetch precedes this call (§1.2), so the gate would clear
    // exactly the same way an unconditional clear-on-empty-remainder did before this fix, but
    // going through the shared primitive means a future change here never has to re-derive that
    // reasoning on its own. Replaced only now, at the very end — see this function's own doc
    // comment on crash-safety.
    taint_utils::resolve_taints(root, &remainder, old_files)
        .map_err(|e| sync_failure_refusal(root, &e))?;

    if remainder.is_empty() {
        report("torn durability taint: rescan complete, nothing left dangling — cleared.".to_string());
        // A store-wide rescan can touch every object in the warehouse — listing each one
        // individually (as the ordinary, small-scale restage/resolved lists do) would make this
        // report itself unusably large. The counts are reported via `progress` above; this
        // outcome's own lists stay a summary note instead.
        let mut notes = vec![format!(
            "a torn taint was resolved by a full store-wide rescan: {} loose object(s), {} \
            pack file(s), and {} inventory shard file(s) were candidates, of which {} were \
            (re)restaged; individual paths are not listed here to keep this report readable",
            loose_count, packs_count, shards_count, restaged.len(),
        )];
        // Any bay `collect_walk_roots` had to skip this run — see `enumerate_absent_reachable`'s
        // doc comment; same reasoning as `resolve_the_rest`'s own degraded-bay note.
        notes.extend(walk.degraded_bays.clone());

        Ok(HealOutcome {
            was_tainted: true,
            restaged: Vec::new(),
            resolved: Vec::new(),
            notes,
        })
    } else {
        report(format!(
            "torn durability taint: rescan complete — {} reference(s) remain dangling.",
            dangling_lines.len()
        ));
        Err(torn_rescan_dangling_refusal(root, &dangling_lines))
    }
}

/// Encode an object hash as its loose fan-out path, root-relative — the shape
/// [`taint_utils::resolve_taints`] records and [`classify_vanished`] round-trips
/// back to `VanishedClass::Loose` on the next (now non-torn) heal run. `Err` only if `hash` is not
/// a well-formed object hash (never actually the case for a hash [`enumerate_absent_reachable`]
/// enumerated — every one came from a real parsed reference — but propagated rather than
/// panicking, matching this module's fail-loud-not-fail-hard discipline elsewhere).
fn loose_remainder_path(root: &Path, hash: &str) -> Result<PathBuf, String> {
    let (folder, file_name) = file_utils::get_path_for_object(hash)?;
    let absolute = PathBuf::from(folder).join(file_name);
    absolute.strip_prefix(root)
        .map(Path::to_path_buf)
        .map_err(|_| format!("The object path for \"{}\" is not under the storage root.", hash))
}

/// The result of directory-driving every present recordable-shape path under a storage root — the
/// §8.3 torn rescan's step 1 enumeration. Every path is root-relative, exactly the shape
/// `taint_utils`/`heal_utils::restage_object` already use. Never the working tree.
struct StoreWidePaths {
    /// Every loose object's on-disk path (the `objects/<2-hex>/<rest>` fan-out) — every one
    /// currently *present*, not just whatever a torn taint's own (untrusted, lower-bound) record
    /// happened to name — see the module doc comment's torn-rescan section.
    loose: Vec<PathBuf>,
    /// Every `.pack`/`.idx` file under `objects/pack/` (skips in-progress `.tmp` staging debris a
    /// killed `compact` may have left behind — see `pack_utils`'s own temp-file naming).
    packs: Vec<PathBuf>,
    /// Every staged inventory shard `data` file, across every bay.
    shards: Vec<PathBuf>,
}

/// Build [`StoreWidePaths`] by walking the object store's own on-disk layout directly — reusing
/// the exact same fan-out/pack-folder/shard shapes `gc_utils`'s sweep, `pack_utils::compact`'s
/// loose-object enumeration, and this module's own [`walk_shard_files`] already walk, rather than
/// hand-rolling a fourth variant of any of them.
fn enumerate_store_wide_paths(root: &Path) -> Result<StoreWidePaths, String> {
    let mut loose = Vec::new();
    walk_loose_object_paths(root, &mut loose)?;

    let mut packs = Vec::new();
    walk_pack_paths(root, &mut packs)?;

    let mut shards = Vec::new();
    for dir in bay_utils::all_bay_state_dirs()? {
        let mut data_files = Vec::new();
        walk_shard_data_files(&dir.join(FOLDER_NAME_INVENTORY_ROOT), &mut data_files)?;
        for path in data_files {
            if let Ok(relative) = path.strip_prefix(root) {
                shards.push(relative.to_path_buf());
            }
        }
    }

    Ok(StoreWidePaths { loose, packs, shards })
}

/// Walk every loose object currently present under `objects/`'s hash fan-out, root-relative —
/// mirrors `gc_utils::collect_garbage`'s own sweep (same fan-out-folder filter) and
/// `pack_utils`'s loose-object enumeration for `compact` (same `.sig`/`.tmp` skip rule), applied
/// here for restaging instead of collection or packing.
fn walk_loose_object_paths(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let objects_root = PathBuf::from(file_utils::get_path_objects_root());
    if !objects_root.exists() {
        return Ok(());
    }

    for folder in file_utils::read_directory(&objects_root)? {
        let folder = folder.map_err(|e| format!("Error while listing the objects folder: {}", e))?;
        if !folder.path().is_dir() {
            continue;
        }

        let prefix = folder.file_name().to_string_lossy().to_string();
        // The pack folder (and anything else that is not a 2-hex fan-out folder) is skipped —
        // it holds packed objects, not loose ones.
        if prefix.len() != file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS
            || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        for file in file_utils::read_directory(&folder.path())? {
            let file = file.map_err(|e| format!("Error while listing an objects folder: {}", e))?;
            let name = file.file_name().to_string_lossy().to_string();

            // Signature sidecars are not restaged on their own (they ride along with their
            // object); a `.tmp` name is in-progress staging debris a killed writer left behind,
            // never a recordable-shape object.
            if name.ends_with(sign_utils::FILE_SUFFIX_SIGNATURE) || name.contains(".tmp") {
                continue;
            }

            if let Ok(relative) = file.path().strip_prefix(root) {
                out.push(relative.to_path_buf());
            }
        }
    }

    Ok(())
}

/// Walk every `.pack`/`.idx` file currently present under `objects/pack/`, root-relative. Skips
/// `.tmp`-named staging debris a killed `compact` may have left behind (`pack_utils`'s own temp
/// files are named `.compact-<pid>-<seq>.<ext>.tmp`).
fn walk_pack_paths(root: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let pack_dir = pack_utils::pack_folder();
    if !pack_dir.exists() {
        return Ok(());
    }

    for file in file_utils::read_directory(&pack_dir)? {
        let file = file.map_err(|e| format!("Error while listing the pack folder: {}", e))?;
        if !file.path().is_file() {
            continue;
        }

        let name = file.file_name().to_string_lossy().to_string();
        if name.contains(".tmp") {
            continue;
        }

        let is_pack_shaped = matches!(
            file.path().extension().and_then(|e| e.to_str()),
            Some(ext) if ext == pack_utils::PACK_DATA_EXTENSION || ext == pack_utils::PACK_INDEX_EXTENSION
        );
        if !is_pack_shaped {
            continue;
        }

        if let Ok(relative) = file.path().strip_prefix(root) {
            out.push(relative.to_path_buf());
        }
    }

    Ok(())
}

/// The three shapes a taint's recorded final path can ever take — see
/// [`file_utils::hash_from_object_path`]'s doc comment — plus an escape hatch for a shape none of
/// them recognize (never reached by any current write path, but a refusal beats a silent drop).
enum VanishedClass {
    /// A loose object; the hash its path encodes.
    Loose(String),
    /// A staged inventory shard file — not an object reference at all.
    Shard,
    /// A pack **data** file; its stem (the shared path prefix its `.idx` sibling shares).
    PackData(PathBuf),
    /// A pack **index** file; its stem.
    PackIndex(PathBuf),
    /// A shape none of the above recognizes.
    Unrecognized,
}

fn classify_vanished(relative: &Path) -> VanishedClass {
    if let Some(hash) = file_utils::hash_from_object_path(relative) {
        return VanishedClass::Loose(hash);
    }

    if relative.file_name().and_then(|name| name.to_str()) == Some(file_utils::FILE_NAME_INVENTORY_DATA) {
        return VanishedClass::Shard;
    }

    match relative.extension().and_then(|ext| ext.to_str()) {
        Some(ext) if ext == pack_utils::PACK_DATA_EXTENSION => VanishedClass::PackData(relative.with_extension("")),
        Some(ext) if ext == pack_utils::PACK_INDEX_EXTENSION => VanishedClass::PackIndex(relative.with_extension("")),
        _ => VanishedClass::Unrecognized,
    }
}

/// Every durable ref source's roots for the closure walk (§3.2: "every durable ref source", never
/// the registration ledger) — see the module doc comment.
struct WalkRoots {
    /// Parcel hashes to walk ancestry-and-tree-closure from: every pallet head (both namespaces),
    /// every bay's parked parcels, every tag's subject parcel, every bay's in-progress
    /// consolidation `their_head`, and the trust anchor's adopted head (if any).
    parcels: Vec<String>,
    /// Object hashes a staged inventory shard (of any bay) references directly (never through a
    /// parcel), with the entry's type (so a chunked file's recipe is still descended into for its
    /// chunks).
    shard_referenced: Vec<(String, DirEntryType)>,
    /// Plain-language notes about any bay whose `parked`/`consolidation` state could not be read
    /// this run (`bay_utils::BayReadPolicy::Tolerate` — see [`collect_walk_roots`]'s own doc
    /// comment). Empty unless at least one bay had to be skipped; that bay simply contributes no
    /// roots to `parcels` above.
    degraded_bays: Vec<String>,
}

/// Collect every durable ref source's roots — see [`WalkRoots`].
///
/// **Warehouse-scale, not bay-scale.** The taint this walk exists to resolve is over the
/// *shared* object store (`forklift_root()`, invariant across every bay) — objects and pallet
/// refs are shared, but a bay's parked parcels, staged inventory shards and in-progress
/// consolidation are bay-*local* (`.forklift/bays/<name>/…`; the main tree keeps them directly
/// under `.forklift/`). Answering "is this shared object still referenced by *anything*
/// durable" from only the active bay's local state would under-count references and could clear
/// the taint on (and later let `gc` delete) an object a *different* bay still needs — silent
/// data loss. So every bay-local source below is read across every bay
/// (`bay_utils::all_bay_state_dirs`), never just the active one.
///
/// **Invariant with [`gc_utils::collect_live_set`](crate::util::gc_utils::collect_live_set):**
/// this walk's roots must stay a *superset* of gc's live-set roots (every pallet head, every
/// bay's parked parcels, every bay's in-progress consolidation `their_head`, and the shared
/// trust-anchor `adopts`) — plus every tag's subject and every bay's staged inventory shards,
/// sources gc deliberately does not root (tags are not a gc root today; an unstacked staged
/// shard is a pre-existing, accepted gc design choice, not a bug this walk needs to match). If
/// gc ever treats an object as live, this walk must never call the same object safe to drop —
/// otherwise `forklift heal` could clear a taint over an object `forklift gc` would refuse to
/// delete. The parked-parcels/consolidation portion of that shared root list is not duplicated
/// here — both this function and `collect_live_set` call
/// [`bay_utils::collect_bay_scoped_parcel_roots`] for it, so the two can never drift apart on
/// that portion by construction. That helper takes the bay dirs as a parameter rather than
/// enumerating them itself specifically so a caller that also needs those dirs for something
/// else — this function, for staged inventory shards — can enumerate them once
/// ([`bay_utils::all_bay_state_dirs`]) and feed the same `Vec` to both the helper and its own
/// extra loop, instead of `bay_utils::list_bays` running twice per heal. **A future edit adding a
/// new bay-local ref source should still re-check both callers of the shared helper, and re-check
/// the other function's root list for anything not routed through it (tags, shards, the trust
/// anchor).**
///
/// **Tolerant, unlike `collect_live_set`.** This call passes [`bay_utils::BayReadPolicy::
/// Tolerate`], not `FailClosed`: `forklift heal` never deletes anything (it restages and
/// reports), and is the very command a standing taint tells users to run to recover, so a single
/// unreadable bay must not brick it the way it must still brick a *deleting* sweep. An
/// unreadable bay is skipped — it contributes no roots — and named in [`WalkRoots::
/// degraded_bays`] instead of aborting the walk. This can only make the walk's answer more
/// conservative (a hash only that bay referenced now reads as unreferenced and clears, rather
/// than the walk refusing outright), never less — see `BayReadPolicy`'s own doc comment for why
/// that is sound specifically because heal never deletes. `gc_utils::collect_live_set` keeps
/// `FailClosed` unconditionally; do not change that call site to match this one.
///
/// The undo journal is deliberately excluded from **both** walks (parity, not an oversight): an
/// entry there records history for `undo`/`redo`, never a live reference a future write would
/// resurrect from it.
///
/// Staged inventory shards are enumerated by walking the shard **files on disk**, never the
/// registration ledger (`inventory_utils::write_metadata_to_file`'s plain, unsynced
/// `std::fs::write`, which is blind to a shard published but never registered — the phase-B
/// wart §3.2 calls out by name; a walk that trusted the ledger could silently miss a real
/// dangling reference).
fn collect_walk_roots() -> Result<WalkRoots, String> {
    let mut parcels: Vec<String> = Vec::new();
    let mut shard_referenced: Vec<(String, DirEntryType)> = Vec::new();

    for (_, head) in pallet_utils::all_pallet_refs()? {
        parcels.push(head);
    }

    for tag in tag_utils::read_tags()? {
        parcels.push(tag.tag.subject);
    }

    // Bay-local sources, read across every bay — see this function's doc comment. One
    // `all_bay_state_dirs` enumeration, fed to both the shared parked/consolidation helper and
    // this function's own staged-shard loop, so `list_bays` runs exactly once per heal. Parked
    // parcels and in-progress-consolidation `their_head` are the portion shared with gc's live
    // set — see `bay_utils::collect_bay_scoped_parcel_roots`'s doc comment for the shared-helper
    // rationale. `Tolerate`, not `FailClosed` — see this function's own doc comment. Staged
    // inventory shards are recovery-only (gc deliberately does not root them) and stay a separate
    // per-bay loop here.
    let bay_dirs = bay_utils::all_bay_state_dirs()?;
    let bay_scope = bay_utils::collect_bay_scoped_parcel_roots(&bay_dirs, bay_utils::BayReadPolicy::Tolerate)?;
    parcels.extend(bay_scope.roots);

    for dir in &bay_dirs {
        walk_shard_files(&dir.join(FOLDER_NAME_INVENTORY_ROOT), &mut shard_referenced)?;
    }

    // The trust anchor is shared (warehouse-global): read once, not per bay.
    if let Some(anchor) = office_utils::read_trust_anchor()? {
        if let Some(adopts) = anchor.adopts {
            parcels.push(adopts);
        }
    }

    Ok(WalkRoots { parcels, shard_referenced, degraded_bays: bay_scope.degraded })
}

fn walk_shard_files(folder: &Path, hashes: &mut Vec<(String, DirEntryType)>) -> Result<(), String> {
    let mut data_files = Vec::new();
    walk_shard_data_files(folder, &mut data_files)?;

    for path in data_files {
        let bytes = std::fs::read(&path)
            .map_err(|e| format!("Error while reading inventory shard \"{}\": {}", path.to_string_lossy(), e))?;
        let inventory = crate::parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing inventory shard \"{}\": {}", path.to_string_lossy(), e))?;

        for (_, item) in inventory.get_items() {
            hashes.push((item.hash.clone(), item.item_type));
        }
    }

    Ok(())
}

/// Recursively enumerate every staged-inventory shard `data` file's absolute path under `folder`
/// (a bay's inventory root) — the on-disk traversal both [`walk_shard_files`] (parses each one for
/// its referenced hashes, step 2's roots) and the §8.3 torn rescan's step 1 (restages each one's
/// own bytes, regardless of content) need identically, so this is the one place that walk is
/// written; the two callers differ only in what they do with the paths this returns.
fn walk_shard_data_files(folder: &Path, found: &mut Vec<PathBuf>) -> Result<(), String> {
    if !folder.exists() {
        return Ok(());
    }

    for entry_result in file_utils::read_directory(&folder.to_path_buf())? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let path = entry.path();

        if path.is_dir() {
            walk_shard_data_files(&path, found)?;
            continue;
        }

        if path.file_name().and_then(|name| name.to_str()) == Some(file_utils::FILE_NAME_INVENTORY_DATA) {
            found.push(path);
        }
    }

    Ok(())
}

/// A closure walk's result, paired with any bay [`collect_walk_roots`] had to skip this run
/// rather than abort on — see that function's own doc comment on its `Tolerate` policy.
/// `degraded_bays` is empty unless at least one bay's state was unreadable; when it is not empty,
/// `hashes` is only as complete as what every *readable* bay could contribute — never a reason to
/// treat `hashes` itself as wrong, only as conservative.
pub(crate) struct ClosureWalkResult {
    /// [`closure_references_any`]: the subset of its `targets` actually found referenced.
    /// [`enumerate_absent_reachable`]: every raw-absent (or corrupt-boundary) hash the walk
    /// reached.
    pub hashes: BTreeSet<String>,
    /// Plain-language notes, one per degraded bay — see [`collect_walk_roots`]'s doc comment.
    pub degraded_bays: Vec<String>,
}

/// Walk every durable ref source's closure looking for `targets`, returning the subset actually
/// found referenced. Read-only: see the module doc comment and
/// [`tests::closure_walk_never_touches_a_barrier_or_a_dir_sync`].
///
/// Presence-tolerant (I3) — see the module doc comment. Passes `None` for the sink: this call only
/// ever cares whether a target is referenced, never which hashes the walk found absent along the
/// way, and the `None` also gates off the terminal-leaf/chunk presence stats this call has no use
/// for (the descent guards — parcel, subtree, chunked-leaf's recipe — still run regardless).
pub(crate) fn closure_references_any(targets: &BTreeSet<String>) -> Result<ClosureWalkResult, String> {
    if targets.is_empty() {
        return Ok(ClosureWalkResult { hashes: BTreeSet::new(), degraded_bays: Vec::new() });
    }

    let roots = collect_walk_roots()?;
    // No corrupt-boundary carve-out for the targeted walk: nothing calls it with one (the §8.3
    // rescan is the one caller that has a corrupt set to carve out, and it drives
    // `enumerate_absent_reachable` instead — see that function's doc comment), so a present-but-
    // corrupt reachable object must keep failing this walk loud, exactly as before the carve-out
    // existed — see `tests::a_present_but_corrupt_pallet_head_fails_the_walk_loudly`.
    let no_corrupt: BTreeSet<String> = BTreeSet::new();
    let hashes = walk_closure_for(targets, &roots, &no_corrupt, None)?;
    Ok(ClosureWalkResult { hashes, degraded_bays: roots.degraded_bays })
}

/// The targetless sibling of [`closure_references_any`]: enumerate every hash the walk finds
/// raw-absent (a root ref itself, a parent parcel, a subtree boundary, an absent recipe, a raw-
/// absent chunk under a present recipe, or a plain leaf), without ever descending past one. Built
/// for the §8.3 torn-taint rescan (a torn taint has no `targets` set to drive
/// [`closure_references_any`] with), which calls this as its step 2 — see [`rescan_torn_taint`].
///
/// Shares [`walk_closure_for`]'s exact descent, with an **empty** target set — every node then
/// falls through past its (vacuous) targets-check to its presence guard — and `Some` sink, which
/// both collects every absence found and (per the module doc comment) turns on the terminal-
/// leaf/chunk presence checks that [`closure_references_any`] skips entirely. Same descent, so this
/// can never tolerate (or fail to tolerate) anything [`closure_references_any`] doesn't.
///
/// `corrupt` is the §8.3 corrupt-boundary carve-out: a hash in this set that the walk reaches is
/// treated exactly like a raw-absent one — recorded via the sink, not descended into — even
/// though [`file_utils::raw_object_present`] would say it *is* present. Without this, a reachable
/// node the caller already proved corrupt (step 1's hash-verify) would hit the ordinary
/// present-but-unloadable `?` and abort the whole rescan — re-bricking a torn taint on exactly the
/// crash-induced corruption this rescan exists to get past. Empty for every other caller (there is
/// no other caller yet, but the parameter is real, not vestigial — see `rescan_torn_taint`).
///
/// # Returns
/// * `Ok(ClosureWalkResult)` - Every raw-absent (or corrupt-boundary) hash the walk reached,
///                            recorded once each, plus any degraded-bay notes.
/// * `Err(String)`          - A ref source could not be read, or a *present*, non-corrupt-boundary
///                            object could not be loaded (corrupt/unreadable) — the walk still
///                            fails loud on that, exactly as before the carve-out existed (I5: an
///                            anomalous corruption the caller could not pre-classify, above all
///                            one discovered only inside a pack).
pub(crate) fn enumerate_absent_reachable(corrupt: &BTreeSet<String>) -> Result<ClosureWalkResult, String> {
    let roots = collect_walk_roots()?;
    let empty_targets: BTreeSet<String> = BTreeSet::new();
    let mut absent: BTreeSet<String> = BTreeSet::new();
    {
        let mut collecting_sink = |hash: &str| { absent.insert(hash.to_string()); };
        walk_closure_for(&empty_targets, &roots, corrupt, Some(&mut collecting_sink))?;
    }
    Ok(ClosureWalkResult { hashes: absent, degraded_bays: roots.degraded_bays })
}

/// Re-borrow an `Option<&mut dyn FnMut(&str)>` for one call, without moving the original out of
/// its owning local variable — needed because `Option<&mut dyn FnMut(&str)>` is used repeatedly
/// across loop iterations and nested calls in [`walk_closure_for`]/`walk_tree`/`check_leaf`, and
/// the generic `Option::as_deref_mut` does not shrink a `&mut &mut dyn Trait`'s lifetime down to
/// just the one call the way this concrete, non-generic match does (a known reborrowing gap for
/// generic `DerefMut` blanket impls over nested mutable references).
fn reborrow_sink<'a>(sink: &'a mut Option<&mut dyn FnMut(&str)>) -> Option<&'a mut dyn FnMut(&str)> {
    match sink {
        Some(s) => Some(&mut **s),
        None => None,
    }
}

/// The shared descent core both [`closure_references_any`] and [`enumerate_absent_reachable`] call
/// (I3, module doc comment). `sink`:
/// * `None` — the targeted walk: a descent-guard absence (parcel/subtree/recipe) is simply skipped,
///   and the terminal-leaf/chunk presence checks are skipped entirely (not merely no-opped) since
///   nothing would consume their result.
/// * `Some(collector)` — the enumerating walk: every absence found, at every level, is recorded via
///   `collector`.
///
/// `corrupt` (§8.3's carve-out — see [`enumerate_absent_reachable`]'s doc comment): a hash in this
/// set is treated as absent at every descent guard, however [`file_utils::raw_object_present`]
/// would answer for it — recorded (if a sink is listening) and never loaded. Empty for the ordinary
/// targeted walk ([`closure_references_any`]).
///
/// `targets` empty (as [`enumerate_absent_reachable`] passes) means every node's targets-check is
/// vacuous, so the walk falls through to (and records at) every presence guard it meets.
fn walk_closure_for(
    targets: &BTreeSet<String>,
    roots: &WalkRoots,
    corrupt: &BTreeSet<String>,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<BTreeSet<String>, String> {
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    let mut visited_trees: HashSet<String> = HashSet::new();

    for (hash, item_type) in &roots.shard_referenced {
        check_leaf(hash, *item_type, targets, corrupt, &mut referenced, reborrow_sink(&mut sink))?;
    }

    let mut visited_parcels: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = roots.parcels.iter().cloned().collect();

    while let Some(hash) = queue.pop_front() {
        // Targets-check FIRST: a vanished hash that is itself a root ref (a pallet head, a parked
        // parcel, a tag subject, …) is still a genuine reference to it — the presence guard below
        // must never pre-empt this, or a root reference to a raw-absent target would be silently
        // dropped instead of reported. See
        // [`tests::a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced`].
        if targets.contains(&hash) {
            referenced.insert(hash);
            continue;
        }
        if !visited_parcels.insert(hash.clone()) {
            continue;
        }
        // Presence-tolerant (I3), plus the §8.3 corrupt-boundary carve-out: a parent parcel
        // reached by ancestry can be legitimately absent (sparse/shallow/narrowed), or already
        // known corrupt by the caller (`corrupt`) — either way, record it via `sink` (if any) and
        // stop; do not enqueue its parents, since neither an absent nor a corrupt parcel's own
        // parent list can be trusted. Unconditional in both modes — this is a descent guard, not a
        // sink-feeding-only stat.
        if corrupt.contains(&hash) || !file_utils::raw_object_present(&hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(&hash); }
            continue;
        }

        let parcel = object_utils::load_parcel(&hash)?;
        walk_tree(&parcel.tree_hash, targets, corrupt, &mut referenced, &mut visited_trees, reborrow_sink(&mut sink))?;

        for parent in parcel.parents {
            queue.push_back(parent);
        }
    }

    Ok(referenced)
}

/// Iterative, work-queue-driven — a parcel's spine tree can nest arbitrarily deep, and the
/// obvious recursive shape (one stack frame per level) risks a stack overflow on a
/// pathologically deep tree; see [`tests::walk_tree_survives_a_pathologically_deep_tree`]. Mirrors
/// `gc_utils::collect_live_set`'s own `tree_queue: VecDeque` loop — same shape, same semantics,
/// just applied here instead of there. `check_leaf`'s own descent (recipe → chunks) is one level
/// deep, never further, so it stays recursion-free without needing this treatment.
fn walk_tree(
    tree_hash: &str,
    targets: &BTreeSet<String>,
    corrupt: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    visited_trees: &mut HashSet<String>,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<(), String> {
    let mut tree_queue: VecDeque<String> = VecDeque::new();
    tree_queue.push_back(tree_hash.to_string());

    while let Some(tree_hash) = tree_queue.pop_front() {
        // Targets-check FIRST — same reasoning as `walk_closure_for`'s parcel queue: a vanished
        // hash that is itself a hunted target is a reference, and must be reported as one even
        // though it is raw-absent.
        if targets.contains(&tree_hash) {
            referenced.insert(tree_hash);
            continue;
        }
        if !visited_trees.insert(tree_hash.clone()) {
            continue;
        }
        // Presence-tolerant (I3), plus the §8.3 corrupt-boundary carve-out: a sealed subtree
        // boundary in a sparse/shallow warehouse is legitimately absent, or already known corrupt
        // by the caller (`corrupt` — step 1's hash-verify found this exact hash present but
        // wrong) — either way, record it (if a sink is listening) and stop; nothing beneath it is
        // trustworthy either way (the store invariant a warehouse never holds a child without its
        // parent tree, mirrored from `gc_utils::collect_live_set`'s own tree descent, extends to
        // "nor a child whose parent's own bytes cannot be trusted"). Unconditional in both modes —
        // a descent guard, not a sink-feeding-only stat. Without the corrupt half of this check, a
        // corrupt reachable tree would instead hit `load_tree`'s bare `?` below and abort the
        // whole rescan (I5) — see
        // [`tests::a_corrupt_reachable_tree_is_a_recorded_boundary_not_a_walk_abort`].
        if corrupt.contains(&tree_hash) || !file_utils::raw_object_present(&tree_hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(&tree_hash); }
            continue;
        }

        let tree = object_utils::load_tree(&tree_hash)?;

        for (_, file) in tree.get_files() {
            check_leaf(&file.hash, file.item_type, targets, corrupt, referenced, reborrow_sink(&mut sink))?;
        }

        for (_, subtree) in tree.get_subtrees() {
            tree_queue.push_back(subtree.hash.clone());
        }
    }

    Ok(())
}

/// Check one leaf entry (a tree's file entry, or a staged shard's item): if its own hash is a
/// target, record it; otherwise, for a chunked file, descend into its recipe's chunk list looking
/// for a target chunk hash (a chunk is reachable only *through* its recipe, never directly).
///
/// Presence-tolerant (I3): a **chunked** leaf's recipe can itself be raw-absent (sparse/shallow) —
/// its chunks are then locally absent too (mirrors `gc_utils::mark_recipe_chunks_live`), so an
/// absent recipe is recorded (if a sink is listening) and its chunk list is never fetched. This
/// recipe-presence guard is a real descent guard (skips the `recipe_chunk_hashes` load) and runs
/// **unconditionally in both modes**, exactly like the parcel/subtree guards.
///
/// The two *terminal* checks below it are different in kind: a **present** recipe's individual
/// chunk hashes, and a **plain** (blob) leaf's own hash, are never loaded by this walk either way
/// (only ever compared against `targets`) — there is no descent for a presence check to gate. So
/// each is gated on `sink.is_some()` and skipped entirely (not merely no-opped — the
/// `raw_object_present` syscall itself is skipped) for the targeted walk
/// ([`closure_references_any`], `sink: None`), and only actually runs for the enumerator
/// ([`enumerate_absent_reachable`], `sink: Some(_)`) — see the module doc comment.
fn check_leaf(
    hash: &str,
    item_type: DirEntryType,
    targets: &BTreeSet<String>,
    corrupt: &BTreeSet<String>,
    referenced: &mut BTreeSet<String>,
    mut sink: Option<&mut dyn FnMut(&str)>,
) -> Result<(), String> {
    if targets.contains(hash) {
        referenced.insert(hash.to_string());
        return Ok(());
    }

    if item_type.is_chunked() {
        // Presence-tolerant (I3) plus the §8.3 corrupt-boundary carve-out (same reasoning as the
        // parcel/subtree guards above): a recipe already known corrupt is never loaded either,
        // since `recipe_chunk_hashes` below would otherwise fail loud on it exactly like
        // `load_tree`/`load_parcel` would.
        if corrupt.contains(hash) || !file_utils::raw_object_present(hash)? {
            if let Some(s) = reborrow_sink(&mut sink) { s(hash); }
            return Ok(());
        }
        for chunk in object_utils::recipe_chunk_hashes(hash)? {
            // Targets-check first (same reasoning as every other node): a target chunk is
            // recorded as referenced regardless of mode.
            if targets.contains(&chunk) {
                referenced.insert(chunk.clone());
            }
            // Enumerating-mode only (gated on the sink, not merely no-opped): the targeted walk
            // never needs a chunk's raw presence, only whether it is a target (handled above).
            if let Some(s) = reborrow_sink(&mut sink) {
                if !file_utils::raw_object_present(&chunk)? {
                    s(&chunk);
                }
            }
        }
    } else if let Some(s) = reborrow_sink(&mut sink) {
        // Enumerating-mode only, same reasoning: a plain leaf's bytes are never loaded by this
        // walk either way.
        if !file_utils::raw_object_present(hash)? {
            s(hash);
        }
    }

    Ok(())
}

/// Whether, and how cleanly, [`attempt_heal_driven_refetch`] actually consulted a configured
/// remote this run — feeds [`remedy_text`] so a residual refusal never overclaims what this
/// specific attempt established (the no-false-prose rule). Never gates *whether* a hash is
/// treated as recovered — see the module doc comment's D3 note — only the wording of what to do
/// about one that stayed missing.
#[derive(Clone, Copy)]
enum RemoteConsultation {
    /// No remote is configured for this warehouse at all.
    NotConfigured,
    /// A remote is configured, the handshake succeeded, and every pallet's fetch (best-effort)
    /// completed without error — whatever is still missing afterward, the remote genuinely lacks
    /// too, at least as of this run.
    ConsultedCleanly,
    /// A remote is configured, but the handshake or at least one pallet's fetch failed this run —
    /// a hash still missing below was not necessarily *confirmed* absent upstream, only not yet
    /// recovered; retrying once connectivity is restored may still help.
    ConsultedWithErrors,
}

/// The remedies that actually exist for a vanished-and-referenced (or still-corrupt) object —
/// see [`HEAVYWEIGHT_EXITS`]'s doc comment for why "abandon" is never named here: no ref class
/// this walk covers (a pallet head, a parked parcel, a tag subject) has an in-tool command to
/// drop it. Unlike before §3.2, this never tells the operator to run a fetch by hand — `heal`
/// already tried one itself (see [`attempt_heal_driven_refetch`]) before this text is ever
/// reached; `consultation` says only how much weight that attempt's silence carries.
fn remedy_text(consultation: RemoteConsultation) -> &'static str {
    match consultation {
        RemoteConsultation::NotConfigured =>
            "no remote is configured for this warehouse; reproduce it by re-running the \
            operation that created it if its content still exists in your working tree \
            (\"forklift load\" then \"forklift stack\") — there is no in-tool way to abandon a \
            single dangling reference yet",
        RemoteConsultation::ConsultedCleanly =>
            "this heal already checked the configured remote automatically and it does not have \
            this object either; reproduce it by re-running the operation that created it if its \
            content still exists in your working tree (\"forklift load\" then \"forklift \
            stack\") — there is no in-tool way to abandon a single dangling reference yet",
        RemoteConsultation::ConsultedWithErrors =>
            "this heal tried to check the configured remote automatically but could not complete \
            that check this run; re-run \"forklift heal\" once connectivity is restored, or \
            reproduce it by re-running the operation that created it if its content still exists \
            in your working tree (\"forklift load\" then \"forklift stack\") — there is no \
            in-tool way to abandon a single dangling reference yet",
    }
}

/// §3.2 D1/D3/D4: the heal-driven refetch. Two passes, both needed — see the "empirically found"
/// note below, which is why this is *not* a bare call to `fetch_history_scoped` as first drafted.
///
/// **Pass 1 — every pallet's remote head** ([`remote_utils::fetch_history_scoped`]/
/// [`remote_utils::fetch_history`], the exact functions `lower.rs:78`/`adopt_meta_pallets` call;
/// meta pallets unscoped, mirroring `adopt_meta_pallets` — their own audit reads full content, so
/// a sparse fetch scope must never prune them). Its job is **not primarily to land
/// `candidate_hashes`' own bytes** — it is to bring each pallet's *actual, current* history
/// locally up to date before the caller's `closure_references_any` runs, so that check sees every
/// pallet's real history rather than a stale local ref: without this, a hash referenced only from
/// a pallet's not-yet-locally-known newer history could be misclassified "unreferenced" (safe to
/// drop) purely because this warehouse never looked far enough. A pallet whose remote head has
/// diverged from its local counterpart (`lower.rs:89-97`'s own check) needs no special-case skip
/// here: unlike `lower`, this function never calls `set_pallet_head` or merges anything — it only
/// ever stores content-addressed objects, so there is no ref-move decision for divergence to gate.
///
/// **Pass 2 — a direct, targeted fetch of `candidate_hashes`** ([`remote_utils::
/// fetch_missing_objects`], `pub(crate)` specifically for this). **Empirically found to be
/// required, not belt-and-suspenders:** `fetch_history`/`fetch_history_scoped` bound their walk at
/// any parcel already reachable from a local ref (`is_known_complete`) and, once a parcel is
/// judged "complete," never re-descend into *its own* tree to re-verify or re-fetch an
/// individually-vanished blob/tree/recipe/chunk — confirmed by reproducing the wedge exactly as
/// §1.1 describes it (delete a blob referenced by the *current*, already-lifted pallet head,
/// taint it, run `heal`): pass 1 alone left it in the remainder, because the current pallet's own
/// remote head was already "known complete" and its tree was never re-walked. A direct
/// hash-addressed `GET /v1/objects/{hash}` is deliberately path-blind server-side
/// (`forklift-server`'s own doc comment on `get_object`), so it finds and verifies an object
/// regardless of which pallet(s) reference it or whether their closure is already considered
/// complete — closing exactly this gap. This is also how a D4 force-vanished corrupt candidate is
/// actually recovered (its containing parcel is typically already "complete" too).
///
/// Best-effort and read/store-only throughout: a failure in either pass for one pallet or the
/// targeted batch never aborts the rest, and nothing here ever calls `set_pallet_head` or merges a
/// pallet. Whether anything is actually recovered is decided afterward, uniformly, by the caller's
/// own `raw_object_present` recheck — never by this function's return value (§3.2 D3, see the
/// module doc comment); `RemoteConsultation` only ever feeds [`remedy_text`]'s wording.
async fn attempt_heal_driven_refetch(candidate_hashes: &[String]) -> RemoteConsultation {
    // In practice this function's own caller already filters out the "nothing configured" case
    // before ever calling it (`resolve_the_rest`'s own `remote_configured` gate, fixed the same
    // way — see its doc comment), so this `Err` branch mainly guards a config that broke *between*
    // that check and this `.await` actually running. Same distinction either way: never conflate
    // "configured but unusable" with "nothing configured" — see `remote_utils::is_configured`'s
    // own doc comment.
    let client = match remote_utils::RemoteClient::from_config() {
        Ok(client) => client,
        Err(_) => return if remote_utils::RemoteClient::is_configured() {
            RemoteConsultation::ConsultedWithErrors
        } else {
            RemoteConsultation::NotConfigured
        },
    };

    // Armed for the rest of this function — both passes below, and every `.await` inside them —
    // so every store this refetch makes is exempt from `taint_recheck`'s standing-taint success
    // re-check against the very taint this run is in the middle of healing (see
    // `SelfTripExemptionGuard`'s own doc comment for why this must be process-global and the
    // precondition it depends on). A zero-sized RAII guard held across `.await` points is fine
    // here — it is not a lock, so it does not threaten the future's `Send`-ness.
    let _self_trip_exemption = file_utils::SelfTripExemptionGuard::new();

    let mut clean = true;

    match client.fetch_info().await {
        Ok(info) => {
            let fetch_scope = match scope_utils::read_fetch_scope() {
                Ok(scope) => scope,
                Err(_) => {
                    clean = false;
                    scope_utils::MaterializationScope::full()
                }
            };

            for (wire, remote_head) in &info.pallets {
                let fetched = match wire.strip_prefix(pallet_utils::META_QUALIFIER) {
                    Some(_) => remote_utils::fetch_history(&client, remote_head).await,
                    None => remote_utils::fetch_history_scoped(&client, remote_head, &fetch_scope).await,
                };

                if fetched.is_err() {
                    clean = false;
                }
            }
        }
        // Unreachable remote, protocol mismatch, etc. — pass 2 below still gets a chance (it may
        // fail too, or may not, depending on what actually broke); either way the caller's own
        // presence recheck is what decides "recovered" (D3), never this function's outcome.
        Err(_) => clean = false,
    }

    if !candidate_hashes.is_empty()
        && remote_utils::fetch_missing_objects(&client, candidate_hashes).await.is_err()
    {
        clean = false;
    }

    if clean { RemoteConsultation::ConsultedCleanly } else { RemoteConsultation::ConsultedWithErrors }
}

fn display_paths<'a>(paths: impl Iterator<Item = &'a PathBuf>) -> Vec<String> {
    paths.map(|p| p.to_string_lossy().into_owned()).collect()
}

/// §8.3: the store-wide rescan itself (enumeration, or the closure walk over it) could not
/// complete — an operational failure of the rescan's own machinery, distinct from the rescan
/// completing and finding a genuine dangling remainder (see [`torn_rescan_dangling_refusal`]).
/// The torn record is left standing untouched (the rescan never reached
/// [`taint_utils::resolve_taints`]), so a rerun starts the rescan over from
/// scratch — safe, since step 1's restaging is idempotent.
fn rescan_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": its record is torn, and the store-wide rescan that would resolve \
            it into a known remainder could not complete ({}). The torn record is left standing \
            so nothing is trusted prematurely.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check that the object store and every bay's inventory folder are readable, then run \
        \"forklift heal\" again — the rescan is idempotent and simply starts over.",
    )
}

/// §8.3 (I5): the store-wide rescan completed and turned the torn record's unknown scope into a
/// known one, but at least one reference in that known scope is still genuinely dangling. The
/// taint by this point already records exactly this remainder (no longer torn) — see
/// [`rescan_torn_taint`] — so the *next* `forklift heal` run resolves it through the entirely
/// ordinary [`resolve_the_rest`] pipeline (refetch included), not a special torn-only path.
fn torn_rescan_dangling_refusal(root: &Path, lines: &[String]) -> CoreError {
    let named: Vec<&String> = lines.iter().take(MAX_NAMED_DANGLING).collect();
    let overflow = lines.len().saturating_sub(named.len());

    let mut message = format!(
        "{} under \"{}\": its record was torn (a crash interrupted the write that would have \
        named every affected path); a full store-wide rescan has resolved that unknown scope, \
        and {} reference(s) turned out to be genuinely dangling: {}",
        taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), lines.len(),
        named.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; "),
    );
    if overflow > 0 {
        message.push_str(&format!(" (and {} more)", overflow));
    }

    let next_step = format!(
        "The taint now records exactly this remainder — it is no longer torn. Re-run \"forklift \
        heal\" — it will attempt the ordinary recovery (including a configured remote fetch) \
        against this known scope; whatever it still cannot resolve needs a heavyweight \
        resolution: {}",
        HEAVYWEIGHT_EXITS
    );

    CoreError::refusal(RefusalCode::DurabilityTaint, message, next_step)
}

fn dangling_refusal(root: &Path, lines: &[String]) -> CoreError {
    let named: Vec<&String> = lines.iter().take(MAX_NAMED_DANGLING).collect();
    let overflow = lines.len().saturating_sub(named.len());

    let mut message = format!(
        "{} under \"{}\": {} reference(s) remain dangling after recovery: {}",
        taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), lines.len(),
        named.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; "),
    );
    if overflow > 0 {
        message.push_str(&format!(" (and {} more)", overflow));
    }

    let next_step = format!(
        "Every dangling reference needs a heavyweight resolution: {} Re-run \"forklift heal\" \
        once you have resolved what you can; it reports exactly what is left.",
        HEAVYWEIGHT_EXITS
    );

    CoreError::refusal(RefusalCode::DurabilityTaint, message, next_step)
}

fn read_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\", but its record could not be read ({}); treating this warehouse as \
            unhealed rather than risk trusting unproven state.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check the taint directory's permissions and disk health, then run \"forklift heal\" again.",
    )
}

fn sync_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": recovery made progress, but making it durable (or recording the \
            remainder) failed ({}). The taint is left standing so nothing is trusted \
            prematurely.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check disk health and run \"forklift heal\" again.",
    )
}

fn walk_failure_refusal(root: &Path, error: &str) -> CoreError {
    CoreError::refusal(
        RefusalCode::DurabilityTaint,
        format!(
            "{} under \"{}\": the recovery walk over this warehouse's durable references could \
            not complete ({}), so whether the remaining recorded paths are safe to drop could \
            not be determined.",
            taint_utils::GATE_TAINT_MARKER, root.to_string_lossy(), error
        ),
        "Check that every pallet head, parked parcel, tag and staged inventory shard is itself \
        readable, then run \"forklift heal\" again.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::file_utils;

    #[test]
    fn closure_walk_never_touches_a_barrier_or_a_dir_sync() {
        // Pins the read-only-by-construction claim in this module's doc comment: the walk must
        // never persist anything, however many (or few) targets it is asked about, and however
        // many ref roots exist. Mutation: route the walk through a persisting `graph_utils` entry
        // point (or any write path) → red.
        //
        // Enters a real scope with a real ref source (a pallet head over a real, stored tree) so
        // the walk actually runs against something — with no scope entered at all (the previous
        // shape of this test), the walk read nothing and the assertion below was vacuous: it
        // would still pass with the walk routed through a barrier, as long as that code path was
        // never reached because there was nothing to walk.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-walk-readonly-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        let mut tree_object = LooseObjectBuilder::build_tree(&tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("base".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        // Thread-local sync recorders, NOT the process-wide `barrier_count()`/`dir_sync_count()`:
        // those are global atomics a test on another thread bumps mid-walk, which made this test
        // flaky under parallel `cargo test`. A durability barrier always ends by fsyncing its
        // touched directories, and an immediate / graph-self-heal write fsyncs via `sync_dir`, so
        // "no directory sync attempted on THIS thread during the walk" is equivalent to "no barrier
        // ran" — with zero cross-test pollution. Armed here, after the setup writes above, so only
        // the walk's own (non-)syncs are recorded; both guards RAII-reset on drop.
        let _sync_dir_guard = file_utils::SyncDirFaultGuard::recording();
        let _barrier_dir_guard = file_utils::DirSyncFaultGuard::recording();

        let targets: BTreeSet<String> = ["a".repeat(64), "b".repeat(64)].into_iter().collect();
        let referenced = closure_references_any(&targets)
            .expect("the walk must succeed against a real, readable ref source")
            .hashes;
        assert!(referenced.is_empty(), "neither target hash is actually referenced");

        assert!(file_utils::sync_dir_attempts().is_empty(),
            "the closure walk must never fsync a directory (immediate / graph-self-heal path): {:?}",
            file_utils::sync_dir_attempts());
        assert!(file_utils::dir_sync_attempts().is_empty(),
            "the closure walk must never run a durability barrier (caught via its trailing dir sync): {:?}",
            file_utils::dir_sync_attempts());

        // The enumerating mode (`enumerate_absent_reachable`, the §8.3 hook) shares the exact same
        // descent — pin that it is equally read-only, not just the no-op-sink mode above. Same
        // recording guards, still armed; any sync attempt from either call would show up here.
        let absent = enumerate_absent_reachable(&BTreeSet::new())
            .expect("the enumerating walk must succeed against a real, readable ref source")
            .hashes;
        assert!(absent.is_empty(), "nothing in this fixture is raw-absent");

        assert!(file_utils::sync_dir_attempts().is_empty(),
            "the enumerating walk must never fsync a directory either: {:?}",
            file_utils::sync_dir_attempts());
        assert!(file_utils::dir_sync_attempts().is_empty(),
            "the enumerating walk must never run a durability barrier either: {:?}",
            file_utils::dir_sync_attempts());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Shared fixture for the I3 (presence-tolerant descent) tests: a pallet head "main" whose
    /// parcel's spine tree has exactly one subtree child pointing at a hash that is never actually
    /// stored anywhere (`absent_subtree_hash`) — the present-parent / absent-child boundary shape
    /// §8.1 exists to tolerate (a sealed hash committed to a signed tree, never fetched into this
    /// warehouse). Must be called after a `StorageRootScope` is entered. Returns the absent
    /// subtree's hash.
    fn plant_present_parent_with_absent_subtree_boundary() -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let absent_subtree_hash = "a".repeat(64);

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "sub".to_string(), absent_subtree_hash.clone(), DirEntryType::Tree,
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("boundary".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        absent_subtree_hash
    }

    /// (I3, test 1) A target only theoretically reachable *through* an absent subtree boundary
    /// must be reported unreferenced, and the walk must complete without erroring on that boundary
    /// — the tolerant-descent behavior this slice adds. `target_hash` can never actually be linked
    /// from the absent subtree (there are no bytes to link from), which is exactly the point: the
    /// walk cannot know, and must not assume, anything about what an absent object might have
    /// pointed to — it just stops at the boundary. Mutation: revert `walk_tree`'s presence guard
    /// (recovery_utils.rs, before `object_utils::load_tree`) back to a bare `?` on
    /// `load_tree(&tree_hash)` → `load_tree` errors on the absent subtree (object does not exist)
    /// → `closure_references_any` returns `Err` → `.expect(...)` panics → red.
    #[test]
    fn tolerant_walk_clears_a_target_only_reachable_through_an_absent_subtree_boundary() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-boundary-clears-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        plant_present_parent_with_absent_subtree_boundary();

        let target_hash = "b".repeat(64);
        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();

        let referenced = closure_references_any(&targets)
            .expect("a sealed/absent subtree boundary must be skipped, not fail the whole walk")
            .hashes;
        assert!(!referenced.contains(&target_hash),
            "T is not actually reachable — its only theoretical path runs through the absent subtree");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 2 — gc-consistency) Pins the superset invariant `collect_walk_roots`'s doc
    /// comment establishes: on the exact same boundary fixture as the test above, `gc_utils`'s
    /// independently-implemented live-set walk must also treat the unreachable target as excluded.
    /// If heal's tolerance (this module) ever diverges from gc's own presence tolerance
    /// (`gc_utils::collect_live_set`), this reddens — heal must never clear (or keep tainted) an
    /// object gc disagrees about.
    #[test]
    fn gc_consistency_pins_the_same_absent_subtree_boundary() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-gc-consistency-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        plant_present_parent_with_absent_subtree_boundary();

        let target_hash = "b".repeat(64);
        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;
        assert!(!referenced.contains(&target_hash));

        let live = crate::util::gc_utils::collect_live_set().unwrap();
        assert!(!live.contains(&target_hash),
            "gc's own live set must also exclude a target only reachable through an absent \
            subtree boundary — heal's root set is a superset of gc's, so anything heal's \
            tolerant walk calls unreferenced must be unreferenced under gc's smaller root set too");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 3 — targets-before-presence) A vanished object that is itself a hunted target
    /// (here, a pallet head whose parcel was never actually stored) must still be reported
    /// referenced: a root reference to a raw-absent object is a genuine reference, and the
    /// presence guard must never pre-empt the targets-check that catches it. Mutation: swap the
    /// order in `walk_closure_for`'s parcel-queue loop so the `raw_object_present` guard runs
    /// *before* `targets.contains(&hash)` → the vanished target is skipped via `sink` and `continue`
    /// before ever being checked against `targets` → `referenced` does not contain it → red.
    #[test]
    fn a_vanished_pallet_head_that_is_itself_the_target_is_still_reported_referenced() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-targets-before-presence-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let vanished_head_hash = "e".repeat(64);
        pallet_utils::set_pallet_head("main", &vanished_head_hash).unwrap();

        let targets: BTreeSet<String> = [vanished_head_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&vanished_head_hash),
            "a vanished pallet head that is itself a hunted target must still be reported referenced");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 4 — present-but-corrupt still fails loud) A pallet head naming a hash that IS
    /// present on disk (`raw_object_present` → true) but whose stored bytes do not actually hash to
    /// it — present-but-unloadable, the opposite case from raw-absent. The presence guard must
    /// never turn this into a silent skip: once presence says "yes," the walk still runs the
    /// ordinary `?` load, and a corrupt object still fails the whole walk loudly, exactly as it did
    /// before this slice (and exactly as `gc_utils` still fails loud on a present-but-corrupt
    /// object after its own presence check).
    #[test]
    fn a_present_but_corrupt_pallet_head_fails_the_walk_loudly() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-corrupt-fails-loud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let corrupt_hash = "f".repeat(64);
        let mismatched_bytes = zstd::encode_all(
            b"these bytes do not correspond to corrupt_hash".as_slice(), 0,
        ).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(&corrupt_hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        std::fs::write(PathBuf::from(&folder).join(&file_name), &mismatched_bytes).unwrap();

        pallet_utils::set_pallet_head("main", &corrupt_hash).unwrap();

        // Unrelated, non-empty (an empty `targets` set short-circuits before the walk ever runs).
        let targets: BTreeSet<String> = ["1".repeat(64)].into_iter().collect();
        let result = closure_references_any(&targets);

        assert!(result.is_err(),
            "a present-but-corrupt object must fail the walk loudly, never be silently skipped");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3, test 9 — enumerating mode) On the same present-parent/absent-subtree boundary fixture
    /// as tests 1–2, `enumerate_absent_reachable` (the targetless, collecting-sink sibling) must
    /// record exactly the boundary subtree's own hash, and nothing beneath it — it must never
    /// descend past an absent node no matter which mode drives the shared walk core. Mutation:
    /// remove the `continue` after the presence-guard `sink` call in `walk_tree` (i.e. keep
    /// recording but still fall through to `load_tree`) → either an `Err` (load fails on the
    /// absent hash) or, if the mutation instead skipped the guard, spurious hashes from
    /// "descending" into content that was never there → this test reddens either way.
    #[test]
    fn enumerate_absent_reachable_records_the_boundary_and_nothing_beneath_it() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-enumerate-boundary-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let absent_subtree_hash = plant_present_parent_with_absent_subtree_boundary();

        let absent = enumerate_absent_reachable(&BTreeSet::new())
            .expect("the enumerator must not error on a sealed/absent subtree boundary")
            .hashes;

        assert!(absent.contains(&absent_subtree_hash),
            "the absent subtree boundary itself must be recorded by the collecting sink");
        assert_eq!(absent.len(), 1,
            "nothing beneath the absent boundary may be recorded — it must never be descended \
            into: {:?}", absent);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// Shared fixture for the chunked-leaf tests below: a pallet head "main" whose parcel's spine
    /// tree has one `NormalChunked` file entry naming a recipe that IS stored (present), with one
    /// chunk in its list. The chunk object itself is never stored — its hash is only ever compared
    /// (as a target) or presence-checked (in enumerating mode), never loaded, so this is a valid,
    /// realistic fixture either way. Must be called after a `StorageRootScope` is entered. Returns
    /// the chunk's hash.
    fn plant_present_recipe_with_one_chunk() -> String {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::parcel::Parcel;
        use crate::model::recipe::{Recipe, RecipeChunk};
        use crate::model::tree_item::TreeItem;

        let chunk_hash = "9".repeat(64);
        let chunk_size = 10u64;

        let recipe = Recipe {
            // Never verified by this walk (or by gc) — any 64-hex value is fine here.
            content_hash: "0".repeat(64),
            total_size: chunk_size,
            chunks: vec![RecipeChunk { hash: chunk_hash.clone(), size: chunk_size }],
        };
        let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);
        recipe_object.store().unwrap();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "big.bin".to_string(), recipe_object.hash.clone(), DirEntryType::NormalChunked,
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("chunked".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        chunk_hash
    }

    /// (I3 refinement — per-chunk enumeration) `enumerate_absent_reachable` must record a raw-
    /// absent chunk hash out of a *present* recipe's chunk list, not just an absent recipe itself.
    /// Mutation: remove the per-chunk `raw_object_present`+sink check inside `check_leaf`'s chunked
    /// branch (the `if let Some(s) = reborrow_sink(&mut sink) { ... }` block after the recipe is
    /// confirmed present) → the chunk is compared against `targets` (empty, so never matches) and
    /// otherwise ignored → `absent` does not contain it → red. This is exactly the data-loss shape
    /// the coordinator flagged: a future §8.3 torn rescan would then lose this chunk from the
    /// remainder.
    #[test]
    fn enumerate_absent_reachable_records_an_absent_chunk_under_a_present_recipe() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-enumerate-absent-chunk-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let chunk_hash = plant_present_recipe_with_one_chunk();

        let absent = enumerate_absent_reachable(&BTreeSet::new())
            .expect("the enumerator must not error on a present recipe with an absent chunk")
            .hashes;

        assert!(absent.contains(&chunk_hash),
            "a raw-absent chunk under a present recipe must be recorded by the collecting sink: {:?}",
            absent);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (I3 refinement — gating non-regression) `closure_references_any` must still find a target
    /// chunk hash under a present recipe after the leaf/chunk presence checks were gated off for
    /// the targeted walk (`sink: None`) — the gating must only skip the *presence* stat, never the
    /// *targets* comparison the walk exists to answer. Mutation: gate the targets-check itself
    /// (instead of just the presence check) on `sink.is_some()` → the target chunk is never
    /// compared and `referenced` comes back empty → red.
    #[test]
    fn closure_references_any_still_finds_a_target_chunk_under_a_present_recipe() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-i3-target-chunk-noregress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let chunk_hash = plant_present_recipe_with_one_chunk();

        let targets: BTreeSet<String> = [chunk_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&chunk_hash),
            "a target chunk hash under a present recipe must still be found by the targeted walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A pathologically deep parcel spine tree (a chain of `DEPTH` nested subtrees, one child
    /// each) must not crash the closure walk. Before `walk_tree` was made iterative (an explicit
    /// `VecDeque` work queue, mirroring `gc_utils::collect_live_set`'s own `tree_queue`), it
    /// recursed once per subtree level — confirmed to overflow the default test-thread stack at
    /// this depth: with `walk_tree`'s body temporarily reverted to its old recursive shape (one
    /// `walk_tree` call inside the `for (_, subtree) in tree.get_subtrees()` loop instead of a
    /// queue push) and this same test run in isolation, the process aborted outright — "thread
    /// '...' has overflowed its stack", `SIGABRT`, `cargo test` reporting "process didn't exit
    /// successfully" — not a normal `Err` a `#[should_panic]` could catch, an abnormal process
    /// crash, which is the actual falsifier a stack-overflow bug produces (empirically confirmed
    /// the recursive shape already dies somewhere between 1,000 and 2,000 levels on the machine
    /// this was verified on). Re-applying the iterative fix makes the same test pass cleanly.
    /// `DEPTH` here is ~25x that empirically-observed threshold — comfortably past it on any
    /// plausible stack size — while the iterative version still finishes in low single-digit
    /// seconds.
    ///
    /// Tree objects are written directly to their final on-disk path (`write_tree_object_fast`)
    /// rather than through `LooseObject::store` (which fsyncs, and renames through a barrier,
    /// per object): at `DEPTH` writes that per-object durability cost dominates real wall time
    /// for no benefit this test needs — it is exercising `walk_tree`'s traversal, not any
    /// write path's own durability, and toggling `FORKLIFT_FSYNC` would be a process-wide,
    /// cross-test change this shared unit-test binary cannot risk. Mirrors the same fast,
    /// direct-write shape `heal_utils::tests::write_loose_object` already uses for the same
    /// reason.
    #[test]
    fn walk_tree_survives_a_pathologically_deep_tree() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        fn write_tree_object_fast(tree: &TreeItem) -> String {
            let mut object = LooseObjectBuilder::build_tree(tree);
            let compressed = object.compress().unwrap();
            let (folder, file_name) = file_utils::get_path_for_object(&object.hash).unwrap();
            std::fs::create_dir_all(&folder).unwrap();
            std::fs::write(PathBuf::from(&folder).join(&file_name), &compressed).unwrap();
            object.hash.clone()
        }

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-deep-tree-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        const DEPTH: usize = 50_000;
        let target_hash = "9".repeat(64);

        // Innermost tree: a single file entry naming the (absent) target hash — never actually
        // stored as an object, since `check_leaf` only needs to check membership in `targets`
        // against an entry's own hash, never read the entry's bytes.
        let mut innermost = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        innermost.add_child(TreeItem::new(
            "file.txt".to_string(), target_hash.clone(), DirEntryType::Normal,
        ));
        let mut current_hash = write_tree_object_fast(&innermost);

        // Wrap it in `DEPTH` further levels, each with exactly one subtree child pointing at the
        // previous level — the deep, narrow spine shape that blows a recursive walk's stack.
        for i in 0..DEPTH {
            let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
            tree.add_child(TreeItem::new(
                format!("child-{}", i), current_hash.clone(), DirEntryType::Tree,
            ));
            current_hash = write_tree_object_fast(&tree);
        }

        let parcel = Parcel {
            tree_hash: current_hash,
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("deep".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets)
            .expect("the closure walk must complete without crashing on a deeply nested tree")
            .hashes;

        assert!(referenced.contains(&target_hash),
            "the deeply nested leaf's target hash must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (i) A staged shard planted in bay "b" (never the active bay — no bay context is entered;
    /// every read here is path-based) references a target hash the closure walk must still find.
    /// Pins `collect_walk_roots`'s per-bay staged-shard enumeration. Red without it: the
    /// pre-fix walk only ever read the active bay's inventory (`bay_root()`, which with no
    /// active bay is `forklift_root()` itself — never bay "b"'s), so the shard planted here was
    /// invisible and `referenced` would come back empty.
    #[test]
    fn walk_finds_a_staged_shard_in_a_non_active_bay() {
        use crate::builder::inventory::InventoryBuilder;
        use crate::enums::inventory_item_state::InventoryItemState;
        use crate::globals::StorageRootScope;
        use crate::model::inventory::{Inventory, InventoryItem};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-shard-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "d".repeat(64);

        let mut inventory = Inventory::new();
        inventory.add_item(InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 0,
            inode: 0,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: target_hash.clone(),
            file_name_length: "file.txt".len() as u64,
            state: InventoryItemState::Normal,
            name: "file.txt".to_string(),
        });
        let bytes = InventoryBuilder::build(&inventory);

        // Bay "b"'s staged inventory, planted directly by path — mirrors the real on-disk shape
        // (`<bay-state-dir>/inventory/inv_/data`) without ever entering a bay context.
        let shard_path = bay_utils::bay_state_dir("b")
            .join(FOLDER_NAME_INVENTORY_ROOT)
            .join(file_utils::PREFIX_INVENTORY_FOLDER)
            .join(file_utils::FILE_NAME_INVENTORY_DATA);
        std::fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        std::fs::write(&shard_path, bytes).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&target_hash),
            "a shard staged in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (ii) A parked hash planted in bay "b"'s `parked` file must still be found. Pins
    /// `collect_walk_roots`'s per-bay `read_parked_in` enumeration. Red without it: the pre-fix
    /// walk called `park_utils::read_parked()` once, scoped to the active bay only, so bay "b"'s
    /// parked hash was never a root.
    #[test]
    fn walk_finds_a_parked_hash_in_a_non_active_bay() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-parked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "e".repeat(64);

        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), format!("{}\n", target_hash)).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&target_hash),
            "a parked hash in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (fail-closed/tolerant split — design call) `closure_references_any` is `forklift heal`'s
    /// own walk, and heal never deletes — so, unlike `gc_utils::collect_live_set` (still pinned
    /// fail-closed by `gc_utils::tests::
    /// gc_fails_closed_and_deletes_nothing_on_an_unreadable_bay_parked_file`), a malformed
    /// `parked` file in a *non-active* bay must no longer abort this walk: it is skipped
    /// (contributing no roots) and named in the returned degraded-bay notes instead. This
    /// used to pin the opposite (fail-closed) behavior for this exact fixture, before the
    /// tolerant/fail-closed split — see `bay_utils::BayReadPolicy`'s own doc comment for why the
    /// two callers are allowed to differ. Red if `collect_walk_roots` ever went back to a single,
    /// unconditionally-propagating `?` on `collect_bay_scoped_parcel_roots`'s result.
    #[test]
    fn walk_tolerates_an_unreadable_bay_parked_file_and_names_it_degraded() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-unreadable-parked-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        // Bay "b"'s `parked` file is malformed (not 64 hex chars) — `read_parked_in` errors on it.
        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), b"not-a-valid-hash\n").unwrap();

        let targets: BTreeSet<String> = ["a".repeat(64)].into_iter().collect();
        let walk = closure_references_any(&targets)
            .expect("an unreadable bay must be skipped, never abort heal's closure walk");

        assert!(walk.hashes.is_empty(), "the unresolvable target is not (falsely) reported referenced");
        assert_eq!(walk.degraded_bays.len(), 1, "exactly the one corrupt bay must be reported degraded");
        assert!(walk.degraded_bays[0].contains("\"b\""), "the note must name the bay: {}", walk.degraded_bays[0]);
        assert!(walk.degraded_bays[0].contains("forklift bay remove"),
            "the note must name the in-tool cleanup route: {}", walk.degraded_bays[0]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (iii) A consolidation `their_head` planted in bay "b"'s `consolidation` file must still be
    /// found. Pins `collect_walk_roots`'s addition of the consolidation source (per-bay). Red
    /// without it: pre-fix, `collect_walk_roots` never read consolidation state at all — this
    /// source did not exist in the walk yet.
    #[test]
    fn walk_finds_a_consolidation_their_head_in_a_non_active_bay() {
        use crate::globals::StorageRootScope;

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-bay-consolidation-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "f".repeat(64);

        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("consolidation"), format!("{}\ntheir-pallet\n", target_hash)).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&target_hash),
            "a consolidation their_head in a non-active bay must still be found by the closure walk");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// (iv) A trust-anchor `adopts` hash (shared, warehouse-global — no bay involved) must still
    /// be found. Pins `collect_walk_roots`'s addition of the trust-anchor source. Red without it:
    /// pre-fix, `collect_walk_roots` never read the trust anchor at all.
    #[test]
    fn walk_finds_a_trust_anchor_adopts_hash() {
        use crate::globals::StorageRootScope;
        use crate::util::office_utils::{self, TrustAnchor};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-trust-adopts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "1".repeat(64);

        office_utils::write_trust_anchor(&TrustAnchor {
            genesis: "0".repeat(64),
            enabled_at: 0,
            boundary: Vec::new(),
            prior_genesis: None,
            adopts: Some(target_hash.clone()),
        }).unwrap();

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&target_hash),
            "a trust-anchor adopts hash must be found by the closure walk (a re-genesis GC root)");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn classify_vanished_recognizes_every_shape_the_taint_schema_can_record() {
        assert!(matches!(
            classify_vanished(Path::new("objects/ab/cdef1234567890")),
            VanishedClass::Loose(hash) if hash == "abcdef1234567890"
        ));
        assert!(matches!(
            classify_vanished(Path::new("inventory/inv_/inv_src/data")),
            VanishedClass::Shard
        ));
        assert!(matches!(
            classify_vanished(Path::new("objects/pack/abc123.pack")),
            VanishedClass::PackData(stem) if stem == Path::new("objects/pack/abc123")
        ));
        assert!(matches!(
            classify_vanished(Path::new("objects/pack/abc123.idx")),
            VanishedClass::PackIndex(stem) if stem == Path::new("objects/pack/abc123")
        ));
        assert!(matches!(
            classify_vanished(Path::new("something/unexpected.txt")),
            VanishedClass::Unrecognized
        ));
    }

    #[test]
    fn shard_referenced_hash_is_found_even_when_never_registered_in_the_ledger() {
        // The phase-B "published but unregistered shard" shape (§3.2): a shard file exists on
        // disk referencing an object, but the (non-atomic, unsynced) registration ledger
        // (`inventory_utils::write_metadata_to_file`) was never updated to mention it. The
        // closure walk must still find the reference — it enumerates shard files on disk, never
        // the ledger. Mutation: enumerate shards from the ledger instead of disk → red (the
        // ledger here is deliberately left absent).
        use crate::builder::inventory::InventoryBuilder;
        use crate::enums::inventory_item_state::InventoryItemState;
        use crate::globals::StorageRootScope;
        use crate::model::inventory::{Inventory, InventoryItem};

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-shard-only-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);

        let target_hash = "c".repeat(64);

        let mut inventory = Inventory::new();
        inventory.add_item(InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 0,
            inode: 0,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: target_hash.clone(),
            file_name_length: "file.txt".len() as u64,
            state: InventoryItemState::Normal,
            name: "file.txt".to_string(),
        });

        let bytes = InventoryBuilder::build(&inventory);
        let shard_path = file_utils::get_inventory_data_path_for_key("src");
        std::fs::create_dir_all(shard_path.parent().unwrap()).unwrap();
        std::fs::write(&shard_path, bytes).unwrap();

        // The registration ledger was never written at all — proving there is nothing there
        // for a ledger-based enumeration to have read.
        let (_, metadata) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        assert!(metadata.is_none(), "the registration ledger must stay untouched by this test");

        let targets: BTreeSet<String> = [target_hash.clone()].into_iter().collect();
        let referenced = closure_references_any(&targets).unwrap().hashes;

        assert!(referenced.contains(&target_hash),
            "a shard-only reference (absent from the ledger) must still be found by the walk");
    }

    // ---- §8.3 torn rescan (I5) ----

    /// Serializes every test (in this module or `taint_utils`'s own) that touches the process-
    /// global taint activation switch — see `taint_utils::ACTIVATION_TEST_LOCK`'s own doc comment.
    fn lock_activation() -> std::sync::MutexGuard<'static, ()> {
        taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Drive [`run`] (the async recovery verb) from a plain `#[test]` fn — mirrors
    /// `heal_utils::tests`' own `runtime.block_on(recovery_utils::run(...))` pattern. `None`
    /// progress: these tests assert on the returned [`HealOutcome`]/[`CoreError`], not on status
    /// chatter.
    fn run_heal() -> Result<HealOutcome, CoreError> {
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(run(None))
    }

    fn assert_durability_taint(error: &CoreError, contains: &[&str]) {
        match error {
            CoreError::Refusal { code, message, .. } => {
                assert_eq!(*code, RefusalCode::DurabilityTaint, "wrong code, message: {}", message);
                for &needle in contains {
                    assert!(message.contains(needle), "expected {:?} in message: {}", needle, message);
                }
            }
            other => panic!("expected a DurabilityTaint refusal, got {:?}", other),
        }
    }

    /// Hand-write a torn taint file (no terminator line) recording `surviving_lines` — mirrors
    /// `heal_utils::tests::plant_taint`'s complete-file counterpart, but deliberately omits the
    /// `END` terminator so `taint_utils::read_taints` reports `torn: true`.
    fn plant_torn_taint(forklift_root: &Path, surviving_lines: &[&str]) {
        let taint_dir = forklift_root.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        let mut content = String::new();
        for line in surviving_lines {
            content.push_str(line);
            content.push('\n');
        }
        std::fs::write(taint_dir.join("taint-88888-0"), content).unwrap();
    }

    /// Write a genuinely corrupt loose object at `hash`'s own fan-out path: valid zstd bytes whose
    /// decompressed content does not hash to `hash` — the exact shape
    /// `tests::a_present_but_corrupt_pallet_head_fails_the_walk_loudly` already uses, reused here
    /// for the §8.3 corrupt-present fixtures. Returns the root-relative path.
    fn write_corrupt_loose_object(forklift_root: &Path, hash: &str) -> PathBuf {
        let mismatched_bytes = zstd::encode_all(
            b"these bytes do not correspond to this hash at all".as_slice(), 0,
        ).unwrap();
        let (folder, file_name) = file_utils::get_path_for_object(hash).unwrap();
        std::fs::create_dir_all(&folder).unwrap();
        let absolute = PathBuf::from(&folder).join(&file_name);
        std::fs::write(&absolute, &mismatched_bytes).unwrap();
        absolute.strip_prefix(forklift_root).unwrap().to_path_buf()
    }

    /// (memo §8.5 test 7, I5) A torn taint over a fully intact store must be resolved
    /// automatically — no exit 21. Mutation: keep the torn early-return (`if state.torn { return
    /// Err(torn_refusal(&root)); }`, the pre-§8.3 shape of `run`) → `run_heal()` returns `Err`
    /// instead of clearing → the `.expect` below panics → red.
    #[test]
    fn torn_taint_over_a_fully_intact_store_rescans_and_clears() {
        use crate::globals::StorageRootScope;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-torn-intact-clears-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        // A real, intact, otherwise-unreferenced object — directory-driven step 1 must restage it
        // regardless of reachability (an unreachable present-but-unproven object is still dedup
        // bait for a future write), and it must not, by itself, cause any dangling report.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;
        let mut blob_object = LooseObjectBuilder::build_blob(&Blob { content: b"an intact, unreferenced object".to_vec() });
        blob_object.store().unwrap();

        // Torn: no terminator at all (an empty file, or any content missing "END\n" as its exact
        // suffix, reads as torn — see `taint_utils`'s own format tests).
        plant_torn_taint(&forklift, &[]);
        assert!(taint_utils::read_taints(&forklift).unwrap().torn, "sanity: the fixture is torn");

        let outcome = run_heal().expect("a torn taint over a fully intact store must clear, not refuse");
        assert!(outcome.was_tainted);

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.torn, "the taint must no longer be torn after a clean rescan");
        assert!(state.recorded.is_empty(), "nothing may remain recorded after a clean rescan");
        assert!(taint_utils::gate_check(&forklift).is_ok(), "the in-memory gate must clear too");

        std::fs::remove_dir_all(&root).ok();
    }

    /// (memo §8.5 test 8, I5) A torn taint whose surviving recorded prefix does **not** name blob
    /// B; a real pallet head's tree references B; B's loose file is then deleted. `heal` must
    /// produce a remainder naming **exactly** B's loose fan-out path — not empty (torn must not
    /// clear over a genuinely dangling reference) and not the whole store (everything else is
    /// intact). Mutation: drive step 2 with the target-driven `closure_references_any` over the
    /// torn record's surviving prefix instead of the targetless `enumerate_absent_reachable` (or
    /// skip step 2 outright) → B, absent from that prefix, is never discovered as referenced → the
    /// remainder comes back empty → torn clears over a dangling reference → both the `Err`
    /// assertion and the exact-remainder assertion below go red.
    #[test]
    fn torn_taint_dangling_remainder_names_exactly_the_referenced_absent_object() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-torn-dangling-remainder-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let mut blob_object = LooseObjectBuilder::build_blob(&Blob { content: b"blob B's content".to_vec() });
        blob_object.store().unwrap();
        let b_hash = blob_object.hash.clone();
        let (folder, file_name) = file_utils::get_path_for_object(&b_hash).unwrap();
        let b_absolute = PathBuf::from(&folder).join(&file_name);
        let b_relative = b_absolute.strip_prefix(&forklift).unwrap().to_path_buf();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new("b.txt".to_string(), b_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("references B".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        // B vanishes — the crash-lost dentry this whole mechanism exists for.
        std::fs::remove_file(&b_absolute).unwrap();

        // Torn, and its surviving prefix deliberately does NOT mention B.
        plant_torn_taint(&forklift, &["objects/aa/an-unrelated-surviving-line"]);
        assert!(taint_utils::read_taints(&forklift).unwrap().torn, "sanity: the fixture is torn");

        let error = run_heal()
            .expect_err("a torn taint over a store with a genuinely dangling reference must not clear");
        // The refusal message names the dangling *hash* (matching `resolve_the_rest`'s own
        // "vanished and still referenced" wording); the exact-path assertion below (on the
        // on-disk remainder itself, which the taint schema records as paths) is the stronger,
        // more load-bearing check that this names exactly B and nothing else.
        assert_durability_taint(&error, &[b_hash.as_str()]);

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.torn, "the taint must no longer be torn — the rescan resolved its scope");
        let expected: BTreeSet<PathBuf> = [b_relative].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "the remainder must name exactly B's loose path — not empty, and not the whole store");

        std::fs::remove_dir_all(&root).ok();
    }

    /// (memo §8.5 test 12, I5) Two corrupt-present loose objects under an otherwise intact,
    /// torn-tainted store: one an orphan (referenced by nothing), one reachable (a real pallet
    /// head's tree references it as a subtree). `heal` must (a) put the orphan's path in the
    /// remainder **unconditionally** — even though nothing references it, a corrupt object is
    /// dedup bait either way — and (b) **not abort** the rescan over the corrupt reachable tree
    /// (the step-2 corrupt-boundary carve-out). Mutations:
    ///  - skip step 1's hash-verify (treat a present, wrong-content object as clean), or otherwise
    ///    exonerate an unreferenced corrupt object → the orphan never lands in the remainder → the
    ///    exact-remainder assertion below goes red;
    ///  - revert the corrupt-boundary carve-out in `walk_tree` (`corrupt.contains(&tree_hash) ||`
    ///    back to a bare presence check) → loading the corrupt reachable tree fails loud →
    ///    `run_heal()` still returns `Err`, but from `rescan_failure_refusal`, not
    ///    `torn_rescan_dangling_refusal` — the `"genuinely dangling"` substring assertion below
    ///    (unique to the latter) goes red.
    #[test]
    fn torn_taint_corrupt_present_is_unconditional_remainder_and_never_aborts_on_a_corrupt_reachable_tree() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-torn-corrupt-present-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        // An orphan corrupt object — referenced by nothing at all.
        let orphan_hash = "f".repeat(64);
        let orphan_relative = write_corrupt_loose_object(&forklift, &orphan_hash);

        // A second corrupt object, used as a REACHABLE subtree from a real pallet head's spine
        // tree — reachable, but its own bytes are garbage, so loading it without the
        // corrupt-boundary carve-out would fail the whole walk loudly.
        let corrupt_tree_hash = "1".repeat(64);
        let corrupt_tree_relative = write_corrupt_loose_object(&forklift, &corrupt_tree_hash);

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new(
            "corrupt-sub".to_string(), corrupt_tree_hash.clone(), DirEntryType::Tree,
        ));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("corrupt reachable subtree".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        // Torn; the surviving prefix is irrelevant here (empty) — directory-driven step 1 finds
        // both corrupt objects regardless of what the torn record itself happens to name.
        plant_torn_taint(&forklift, &[]);
        assert!(taint_utils::read_taints(&forklift).unwrap().torn, "sanity: the fixture is torn");

        let error = run_heal()
            .expect_err("a corrupt-present object must never let a torn taint clear");
        assert_durability_taint(&error, &[
            "genuinely dangling",
            orphan_relative.to_string_lossy().as_ref(),
            corrupt_tree_relative.to_string_lossy().as_ref(),
        ]);

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(!state.torn);
        let expected: BTreeSet<PathBuf> = [orphan_relative, corrupt_tree_relative].into_iter().collect();
        assert_eq!(state.recorded, expected,
            "the remainder must name exactly the two corrupt objects — nothing more (never the \
            whole store — the walk must not have aborted), nothing less (never empty)");

        std::fs::remove_dir_all(&root).ok();
    }

    /// (memo §8.5 test 10) Entry-heal's own torn refusal is completely unaffected by §8.3 — it
    /// still refuses immediately, lock-free, never attempting a rescan itself. This is
    /// `heal_utils::tests::a_torn_taint_refuses_immediately_and_survives`, unchanged; re-asserted
    /// here, alongside the verb's own torn tests, so the invariant "entry-heal still refuses torn"
    /// has a witness in the same module that made the verb itself auto-heal it.
    #[test]
    fn entry_heal_still_refuses_torn_even_though_the_verb_now_rescans_it() {
        use crate::globals::StorageRootScope;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-entry-heal-still-refuses-torn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        plant_torn_taint(&forklift, &["objects/ab/cdef"]);

        let error = heal_utils::heal_if_tainted()
            .expect_err("entry-heal must still refuse a torn taint immediately, lock-free");
        assert_durability_taint(&error, &["record is itself incomplete"]);
        assert!(taint_utils::read_taints(&forklift).unwrap().torn,
            "entry-heal must never attempt the store-wide rescan itself — the taint stays torn");

        std::fs::remove_dir_all(&root).ok();
    }

    // ---- Multi-bay ref-pointer resurrection audit (design memo §10.3) ----
    //
    // §10.3's claim: `forklift heal` verbatim-restaging a tainted pallet ref pointer could
    // silently revert a concurrent, legitimate head move in another bay (the ref file has no
    // compare-and-swap — `pallet_utils::set_pallet_head_in` is a plain `write_file_atomically`).
    // The two tests below settle what `restage_object` actually does with a recorded ref path,
    // constructed deterministically (no real concurrency needed — see each test's own comment
    // for why the sequencing alone decides the outcome, with no timing dependence at all).

    /// Hand-write a complete (non-torn) taint file recording `paths` — the terminated counterpart
    /// of [`plant_torn_taint`] above, mirroring `heal_utils::tests::plant_taint`'s on-disk format
    /// exactly (this module already has the torn variant; this is the "clean, well-formed record"
    /// shape the §10.3 tests below need).
    fn plant_complete_taint(forklift_root: &Path, paths: &[&str]) {
        let taint_dir = forklift_root.join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        let mut content = String::new();
        for path in paths {
            content.push_str(path);
            content.push('\n');
        }
        content.push_str("END\n");
        std::fs::write(taint_dir.join("taint-77777-0"), content).unwrap();
    }

    /// (§10.3, mechanism check 1 — the literal claim) Taint a pallet ref pointer while it holds
    /// V0; advance the head to V1 via an ordinary, successful `set_pallet_head_in` call (standing
    /// in for a legitimate `stack`/`lift` in another bay, sharing the same ref file); then run
    /// `heal`. If §10.3's mechanism is right, `heal`'s verbatim restage of the taint's recorded
    /// path resurrects V0 and the head silently reverts.
    ///
    /// No timing is needed to settle this, because `restage_object` (`heal_utils.rs`) never
    /// stores or replays a byte *snapshot* — it re-reads whatever is CURRENTLY at the recorded
    /// path when `heal` actually runs and rewrites exactly those bytes fresh (see its own doc
    /// comment: "reads them back... then writes them fresh"). The taint schema itself records
    /// only the *path*, never a payload (`taint_utils`'s module doc comment, "Format" section: one
    /// line per path, no bytes). So whichever value is *durably current* at the path when `heal`
    /// runs is what survives — the sequencing below (V0, taint, THEN V1, THEN heal) already forces
    /// the interleaving the claim needs (V1 written after the taint, before heal) with no race at
    /// all: if this reverts to V0 it will revert to V0 every single run, deterministically.
    ///
    /// Mutation: make `restage_object` special-case a ref-shaped path by restoring some stored
    /// snapshot of V0 instead of re-reading the live file → `head_after` comes back V0 → red.
    ///
    /// **Surprise found while building this fixture, worth flagging on its own:**
    /// `pallet_utils::set_pallet_head`'s own write (`write_file_atomically`) already refuses to
    /// report success while ANY taint stands anywhere under the root — `file_utils::
    /// taint_recheck` runs after this write's own rename *and* directory sync both succeed, and
    /// unconditionally errors if `taint_utils::read_taints` finds anything recorded at all,
    /// regardless of path. So the "advance to V1" call below returns `Err`, even though — because
    /// that check runs strictly after the rename already landed — the ref file's bytes on disk
    /// are already durably V1 by the time the error is returned. A real `stack`/`lift` driving
    /// this same call would see (and presumably report) failure despite the head having actually
    /// already moved; this test does not chase that quirk further, but does not paper over it by
    /// pretending the call succeeds either.
    #[test]
    fn heal_restaging_a_tainted_pallet_ref_never_reverts_a_later_legitimate_head_move() {
        use crate::globals::StorageRootScope;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-ref-resurrection-overwrite-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let v0 = "0".repeat(64);
        let v1 = "1".repeat(64);

        // V0: the value standing at the moment of the (simulated) failed post-rename directory
        // sync for this ref's own write.
        pallet_utils::set_pallet_head("main", &v0).unwrap();

        // The taint records the ref's *path* only ("pallets/main") — see this test's own doc
        // comment on why that already rules out a stored-snapshot mechanism.
        plant_complete_taint(&forklift, &["pallets/main"]);

        // Bay B's forward progress: an ordinary head move to V1, through the exact same
        // `write_file_atomically`-backed primitive any real pallet-advancing command uses —
        // simulating a legitimate `stack`/`lift` elsewhere while this (unrelated-content) taint
        // still stands. This call itself returns `Err` (see the "surprise" note above) — the
        // rename to V1 lands first, and only the trailing, blanket taint-recheck then refuses —
        // so the call is deliberately not `.unwrap()`-ed here; what matters for this test is the
        // durable on-disk value it leaves behind, confirmed next.
        let _ = pallet_utils::set_pallet_head("main", &v1);
        assert_eq!(pallet_utils::get_pallet_head("main").unwrap().unwrap(), v1,
            "sanity: the rename to V1 must have physically landed despite the call's own Err");

        let outcome = run_heal().expect("a present, readable, non-content-addressed path restages cleanly");
        assert!(outcome.was_tainted);
        assert!(outcome.restaged.iter().any(|p| p == "pallets/main"),
            "the ref path must actually go through the restage path this test is auditing: {:?}", outcome.restaged);

        let head_after = pallet_utils::get_pallet_head("main").unwrap().unwrap();
        assert_eq!(head_after, v1,
            "heal must never revert a ref pointer to a stale recorded value: restage_object \
            rewrites whatever bytes it reads back from the CURRENT file at heal time, and the \
            taint schema records only the path, never a byte snapshot — so V1 (written after the \
            taint, before heal ran) survives untouched, exactly like restaging any other file \
            that was legitimately overwritten in place after it was tainted");

        std::fs::remove_dir_all(&root).ok();
    }

    /// (§10.3, mechanism check 2 — the delete variant) The complementary case to the test above:
    /// if, instead of being overwritten in place, the tainted ref path is *removed* before heal
    /// runs (the shape that actually matters for a delete-vs-restage race, symmetric to §10.2's
    /// pack scenario), `restage_object`'s `ENOENT` branch has no hash to fall back on for a
    /// non-loose-shaped path (`file_utils::hash_from_object_path` returns `None` for "pallets/
    /// main") — it reports `Vanished` unconditionally, never recreating the file. One level up,
    /// `recovery_utils::resolve_the_rest` cannot even classify a bare ref path as an object hash
    /// to run its absent-and-unreferenced closure walk over (`classify_vanished` falls through to
    /// `VanishedClass::Unrecognized` — not `Loose`, not `Shard`, not pack-shaped) — the vanished
    /// ref path goes straight into the dangling remainder, unconditionally.
    ///
    /// So a deleted ref pointer is never silently resurrected either — the failure mode this
    /// variant actually has is the opposite of §10.3's feared silent data loss: a permanently
    /// dangling, unresolvable-by-heal remainder (worse UX, but not a silent revert).
    #[test]
    fn heal_never_recreates_a_tainted_pallet_ref_whose_path_was_since_deleted() {
        use crate::globals::StorageRootScope;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-ref-resurrection-delete-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        pallet_utils::set_pallet_head("main", &"0".repeat(64)).unwrap();
        plant_complete_taint(&forklift, &["pallets/main"]);

        // The ref file vanishes entirely (e.g. a pallet removed from under the taint) before heal
        // ever runs.
        std::fs::remove_file(forklift.join("pallets").join("main")).unwrap();

        let error = run_heal()
            .expect_err("a vanished, unrecognized-shape recorded path must not silently clear");
        assert_durability_taint(&error, &["pallets/main"]);

        // The critical negative: the file was never recreated by heal.
        assert!(!forklift.join("pallets").join("main").exists(),
            "heal must never recreate a vanished, non-object-shaped recorded path — it has no \
            bytes to restage and no hash to fall back on, unlike a vanished loose object");

        let state = taint_utils::read_taints(&forklift).unwrap();
        assert!(state.recorded.contains(&PathBuf::from("pallets/main")),
            "the vanished ref path must survive as an unresolved (unrecognized-shape) remainder");

        std::fs::remove_dir_all(&root).ok();
    }

    // ---- Fail-closed/tolerant split on an unreadable bay (design call) ----

    /// A single fixture pinning BOTH halves of the split this fix makes, on the exact same
    /// corrupt bay: `forklift heal` (this crate's `run`, exercised end to end here — not just its
    /// inner `closure_references_any`) must complete and report the degraded bay, because heal
    /// never deletes anything and is the very command a standing taint tells users to run to
    /// recover; `gc`/`compact`, which DO delete, must keep refusing outright on that same file,
    /// because sweeping with an incompletely known live set risks real data loss.
    ///
    /// Revert the heal-side tolerance (`bay_utils::collect_walk_roots`'s call passing
    /// `BayReadPolicy::Tolerate`) and this test's first half reddens (`run_heal()` returns `Err`
    /// instead of a clean `Ok` naming bay "b"). Weaken either `gc_utils::collect_live_set`'s or
    /// `pack_utils::compact`'s fail-closed policy and this test's second half reddens (`Ok` where
    /// an `Err` is required) — so this test cannot pass by drifting either direction.
    #[test]
    fn heal_tolerates_a_corrupt_bay_while_gc_and_compact_still_refuse() {
        use crate::globals::StorageRootScope;

        let _serial = lock_activation();
        taint_utils::activate();

        let dir = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-heal-tolerates-gc-refuses-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let _scope = StorageRootScope::enter(&dir);
        let forklift = forklift_root();

        // Bay "b"'s `parked` file is malformed — the exact same corrupt-file shape both halves of
        // this fix are about.
        let bay_b_dir = bay_utils::bay_state_dir("b");
        std::fs::create_dir_all(&bay_b_dir).unwrap();
        std::fs::write(bay_b_dir.join("parked"), b"not-a-valid-hash\n").unwrap();

        // A genuinely vanished, genuinely unreferenced object recorded as tainted — the ordinary
        // "resolved, taint clears" shape, so this run actually drives the closure walk (and so
        // `collect_walk_roots`'s tolerance) rather than taking an early-return shortcut.
        let vanished_hash = "9".repeat(64);
        let vanished_path = loose_remainder_path(&forklift, &vanished_hash).unwrap();
        plant_complete_taint(&forklift, &[vanished_path.to_str().unwrap()]);

        let outcome = run_heal().expect(
            "an unreadable bay must never abort forklift heal — it is the escape hatch users are \
            told to run for a standing taint, and it never deletes anything"
        );
        assert!(outcome.was_tainted);
        assert!(outcome.resolved.iter().any(|p| p.contains(&vanished_hash)),
            "the vanished, unreferenced object must still resolve cleanly despite bay b being \
            unreadable: {:?}", outcome.resolved);
        assert!(outcome.notes.iter().any(|n| n.contains("\"b\"") && n.contains("forklift bay remove")),
            "heal must name the degraded bay and its cleanup route in its notes: {:?}", outcome.notes);

        // The other half: gc and compact must still refuse outright on the exact same corrupt
        // file — never weakened by this fix. (The taint above is already cleared by the `run_heal`
        // call, so this is testing the bay-read policy alone, not a leftover taint gate.)
        let gc_result = crate::util::gc_utils::collect_garbage(0);
        assert!(gc_result.is_err(), "gc must still fail closed on an unreadable bay's parked file");

        // `compact --all` (`all: true`) is the variant that actually computes the live set
        // (`collect_targets` only calls `collect_live_set` under `all` — an incremental compact
        // never repacks against the live set at all, so it would not exercise this fail-closed
        // path). This is `compact --all`'s own fail-closed check, unweakened by this fix.
        let compact_all_result = pack_utils::compact(true, false);
        assert!(compact_all_result.is_err(),
            "compact --all must still fail closed on an unreadable bay's parked file");

        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- "not configured" vs "configured but unusable" (design call) ----

    /// `RemoteClient::from_config()` returns the same `Err` shape for two different situations —
    /// nothing is set, or something is set but unreadable/unparseable/unusable. Before this fix,
    /// both `resolve_the_rest`'s `remote_configured` gate and `attempt_heal_driven_refetch`'s own
    /// early return treated the two identically as "not configured," which steers a user whose
    /// remote is merely *broken* toward the heavyweight remedies (franchise / reproduce / accept
    /// the loss) — wording that overclaims when a fixable, real remote might still have the
    /// object. This fixture makes the warehouse config file itself unparseable (not merely unset)
    /// and asserts the refusal reports the remote as configured-but-unconsultable — reusing
    /// [`RemoteConsultation::ConsultedWithErrors`]'s existing wording, never `NotConfigured`'s.
    ///
    /// Revert either fixed call site (`resolve_the_rest`'s `remote_configured` branch, or
    /// `attempt_heal_driven_refetch`'s own `Err` branch) back to a bare `NotConfigured` return and
    /// this reddens.
    #[test]
    fn heal_reports_a_broken_remote_config_as_unconsultable_not_unconfigured() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;
        use crate::util::config_utils;

        let _serial = lock_activation();
        taint_utils::activate();

        let root = std::env::temp_dir()
            .join(format!("forklift-recovery-utils-broken-remote-config-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        // A real, still-referenced object that vanishes — the ordinary "dangling, remote-refetch-
        // eligible" shape needed to actually reach the `consultation`/`remedy_text` code path
        // this fix touches (mirrors `torn_taint_dangling_remainder_names_exactly_the_
        // referenced_absent_object`'s fixture, complete rather than torn).
        let mut blob_object = LooseObjectBuilder::build_blob(&Blob { content: b"referenced content".to_vec() });
        blob_object.store().unwrap();
        let b_hash = blob_object.hash.clone();
        let (folder, file_name) = file_utils::get_path_for_object(&b_hash).unwrap();
        let b_absolute = PathBuf::from(&folder).join(&file_name);
        let b_relative = b_absolute.strip_prefix(&forklift).unwrap().to_path_buf();

        let mut root_tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        root_tree.add_child(TreeItem::new("b.txt".to_string(), b_hash.clone(), DirEntryType::Normal));
        let mut tree_object = LooseObjectBuilder::build_tree(&root_tree);
        tree_object.store().unwrap();

        let parcel = Parcel {
            tree_hash: tree_object.hash.clone(),
            parents: Vec::new(),
            actions: Vec::new(),
            description: Some("references B".to_string()),
        };
        let mut parcel_object = LooseObjectBuilder::build_parcel(&parcel);
        parcel_object.store().unwrap();
        pallet_utils::set_pallet_head("main", &parcel_object.hash).unwrap();

        std::fs::remove_file(&b_absolute).unwrap();
        plant_complete_taint(&forklift, &[b_relative.to_str().unwrap()]);

        // A remote IS configured, in the sense that something real is on disk at `remote.url`'s
        // scope — it just cannot be read: the warehouse config file itself is unparseable.
        let config_folder = config_utils::get_warehouse_config_folder();
        std::fs::create_dir_all(&config_folder).unwrap();
        std::fs::write(config_folder.join("warehouse.toml"), "not valid toml {{{\n").unwrap();

        let error = run_heal().expect_err(
            "a genuinely dangling reference must not clear just because the remote config is broken"
        );
        assert_durability_taint(&error, &[b_hash.as_str()]);

        let message = match &error {
            CoreError::Refusal { message, .. } => message.clone(),
            other => panic!("expected a DurabilityTaint refusal, got {:?}", other),
        };
        assert!(!message.contains("no remote is configured"),
            "a broken (but real) remote config must never be reported as though nothing were \
            configured: {}", message);
        assert!(message.contains(
            "tried to check the configured remote automatically but could not complete that check"
        ), "a broken remote config must be reported as consulted-but-failed, not silently \
            misclassified as unconfigured: {}", message);

        std::fs::remove_dir_all(&root).ok();
    }
}
