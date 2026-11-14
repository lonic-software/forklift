use serde::Serialize;
use forklift_core::util::lock_utils::StoreAccessLock;
use forklift_core::util::scope_heal::{self, ScopeRebuilt};
use forklift_core::util::operation_utils::{self, RecoveryOutcome};
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::ref_heal::{self, Pass};
use forklift_core::util::{recovery_utils, sealed_utils, unavailable};
use crate::output::{self, CommandOutput};

/// Handle the `heal` command: rebuild a damaged sealed-object record from the part before the
/// damage (`forklift_core::util::sealed_utils::rebuild`), run the durability-taint recovery verb
/// (`forklift_core::util::recovery_utils::run`), then settle an operation interrupted in this
/// bay before it finished, if one is recorded (`forklift_core::util::operation_utils::recover`),
/// then clear the sealed-object record of every object the store holds again
/// (`forklift_core::util::sealed_utils::unseal`).
///
/// In a sparse warehouse whose fetch scope is lost or damaged, it first rebuilds the fetch scope
/// from the checkouts' scopes ([`scope_heal::repair`]), or, under `--fetch-scope`, sets it to the
/// paths given ([`scope_heal::set_chosen`]), holding the store exclusively meanwhile. A damaged
/// fetch scope it cannot settle makes the run a `fetch_scope_damaged` refusal (exit 54).
///
/// Around the taint recovery it rebuilds every lost or damaged user pallet ref it can
/// ([`ref_heal`]): once before, for what nothing still missing could change, and once after,
/// before the operation recovery, whose landed test reads the rebuilt ref. `--ref` and
/// `--remove-ref` settle damaged refs the user chose for, first. A damaged ref left standing makes
/// the run a `ref_damaged` refusal (exit 53) carrying the whole report.
///
/// See `recovery_utils`'s module doc comment for
/// the full per-verdict behavior (restage, the closure walk, partial clears); this handler's job
/// is only to turn the core result into a machine-first envelope.
///
/// A [`recovery_utils::HealOutcome`] means the taint (if any) is now fully resolved — exit 0. A
/// [`forklift_core::error::CoreError::Refusal`] (routed through the ordinary `?`/`String` bridge,
/// same as every other command) means at least one reference is still genuinely dangling, or the
/// taint is torn — exit 21, machine-coded in the refusal message.
///
/// # Arguments
/// * `store_access` - The command's shared store-access hold, which the record's push-down needs.
///   `None` under `--fetch-scope`, which takes the exclusive hold first and the shared one after.
/// * `fetch_scope`  - The paths `--fetch-scope` names, if any.
///
/// # Returns
/// * `Ok(())`      - Nothing was tainted, or the taint is now fully cleared.
/// * `Err(String)` - A `durability_taint` refusal (torn, or an unresolved dangling reference), or
///                   the sealed-object record is damaged and could not be rebuilt, which stops
///                   the run before anything else.
pub async fn handle_command(store_access: Option<&StoreAccessLock>,
                            set_ref: &[String],
                            remove_ref: &[String],
                            fetch_scope: &[String]) -> Result<(), String> {
    // Every spec is read before anything is written, so a malformed one changes nothing.
    let set_ref = set_ref.iter()
        .map(|spec| spec.split_once('=').ok_or_else(|| format!(
            "--ref takes PALLET=PARCEL, not \"{}\". Nothing was changed.", spec
        )))
        .collect::<Result<Vec<_>, _>>()?;

    // Taken before anything is read: the choice may narrow the fetch scope, so no other process may
    // use the store until it is written.
    let exclusive = match fetch_scope.is_empty() {
        true => None,
        false => Some(StoreAccessLock::try_exclusive()?),
    };

    let mut ref_notes: Vec<String> = Vec::new();
    let mut refs_rebuilt: Vec<ref_heal::Rebuilt> = Vec::new();
    let mut refs_removed: Vec<ref_heal::Removed> = Vec::new();

    // First, so the taint's rescan below tells what the record holds by hash from what is lost. The
    // rebuild writes only the record, which no recovery reads through the taint. A damaged record
    // it could not rebuild stops the run, since everything after it would read the damage. One
    // that cannot be read at all does not: the readers below degrade around it and say so.
    let record_rebuilt = sealed_utils::rebuild()
        .map_err(|error| format!(
            "{}{} Nothing else was healed: once that is fixed, run \"forklift heal\" again.",
            error, if error.ends_with('.') { "" } else { "." },
        ))?
        .map(RecordRebuilt::from);

    // After the record, which the check of the history leans on, and before everything else, which
    // reads the fetch scope.
    let scope_rebuilt = match &exclusive {
        Some(exclusive) => Some(scope_heal::set_chosen(fetch_scope, exclusive).await),
        None => scope_heal::repair().transpose(),
    }.transpose().map_err(|error| match &record_rebuilt {
        Some(record) => forklift_core::error::append_to_message(String::from(error), &record.sentence()),
        None => String::from(error),
    })?;
    drop(exclusive);

    let shared;
    let store_access = match store_access {
        Some(store_access) => store_access,
        None => {
            shared = StoreAccessLock::shared(crate::announce_store_access_wait)?;
            &shared
        }
    };

    // What the user chose goes after the record, which it reads, and before the passes, so they
    // judge only what is left. A refusal says what this run already changed: its own "Nothing was
    // changed" speaks only for the write it refused. Its notes go along: a check they say was
    // skipped would otherwise go unreported.
    let changed = |error: String, rebuilt: &[ref_heal::Rebuilt], removed: &[ref_heal::Removed], notes: &[String]| {
        let mut error = error;
        if let Some(record) = &record_rebuilt {
            error = forklift_core::error::append_to_message(error, &record.sentence());
        }
        if let Some(scope) = &scope_rebuilt {
            error = forklift_core::error::append_to_message(error, &scope_sentence(scope));
        }
        for rebuilt in rebuilt {
            error = forklift_core::error::append_to_message(error, &rebuilt_sentence(rebuilt));
        }
        for removed in removed {
            error = forklift_core::error::append_to_message(error, &removed_sentence(removed));
        }
        for note in notes {
            error = forklift_core::error::append_to_message(error, &format!("Note: {}", note));
        }
        error
    };

    for (pallet, parcel) in set_ref {
        match ref_heal::set_damaged_ref(pallet, parcel, &mut ref_notes) {
            Ok(rebuilt) => refs_rebuilt.push(rebuilt),
            Err(error) => return Err(changed(error, &refs_rebuilt, &refs_removed, &ref_notes)),
        }
    }

    for pallet in remove_ref {
        match ref_heal::remove_damaged_ref(pallet, &mut ref_notes) {
            Ok(removed) => refs_removed.push(removed),
            Err(error) => return Err(changed(error, &refs_rebuilt, &refs_removed, &ref_notes)),
        }
    }

    // Before the taint recovery, whose closure walk reads every ref: a ref rebuilt here roots
    // what it reaches for that walk.
    let first = ref_heal::repair(Pass::BeforeTaint).map_err(|error| changed(error, &refs_rebuilt, &refs_removed, &ref_notes))?;
    refs_rebuilt.extend(first.rebuilt);
    ref_notes.extend(first.notes);

    // A torn taint's rescan (§8.3) can take a while on a large or uncompacted store — it reports
    // per-phase/per-count progress through this callback rather than staying silent. `human!` is
    // itself a no-op under `--json` (progress is never part of the result document), and
    // `forklift-core` never prints on its own (DESIGN.html §3.4) — this closure is the one bridge.
    let outcome = match recovery_utils::run(Some(&|line: &str| crate::human!("{}", line))).await {
        Ok(outcome) => outcome,
        Err(error) => {
            // An interrupted operation waits for the taint (below), so the refusal says so: it
            // is not the only thing left standing.
            let mut error = changed(String::from(error), &refs_rebuilt, &refs_removed, &ref_notes);
            if ref_heal::detect(&mut Vec::new()).is_ok_and(|damaged| !damaged.is_empty()) {
                error = forklift_core::error::append_to_message(error,
                    "A damaged pallet ref also stands; heal rebuilds it once the taint is resolved.");
            }
            return Err(match operation_utils::read() {
                Ok(Some(operation)) => forklift_core::error::append_to_message(error, &format!(
                    "An interrupted {} also stands in this bay; heal finishes or undoes it once the \
                    taint is resolved.", operation.kind.as_str(),
                )),
                _ => error,
            });
        }
    };

    // After the taint, which may have refetched what a candidate lacked; before the operation
    // recovery, whose landed test reads the ref.
    let second = ref_heal::repair(Pass::AfterTaint).map_err(|error| changed(error, &refs_rebuilt, &refs_removed, &ref_notes))?;
    refs_rebuilt.extend(second.rebuilt);
    ref_notes.extend(second.notes);
    let refs_refused = second.refused;

    // An operation on a pallet whose ref is still damaged waits: its landed test would read the
    // damage as "not landed".
    let waits = match operation_utils::read() {
        Ok(Some(operation)) => refs_refused.iter()
            .any(|refused| refused.pallet == operation.source.pallet || refused.pallet == operation.target.pallet)
            .then_some(operation.kind),
        _ => None,
    };

    // After the taint: an operation's recovery reads and writes the store the taint vouches for.
    let recovered = match waits {
        Some(_) => Ok(None),
        None => operation_utils::recover(),
    }.map_err(|error| changed(error, &refs_rebuilt, &refs_removed, &ref_notes))?;
    let operation = recovered.map(|recovery| OperationRecovery {
        operation: recovery.operation.as_str().to_string(),
        outcome: match recovery.outcome {
            RecoveryOutcome::Finished => OperationOutcome::Finished,
            RecoveryOutcome::RolledForward => OperationOutcome::RolledForward,
            RecoveryOutcome::RolledBack => OperationOutcome::RolledBack,
            RecoveryOutcome::Cleared => OperationOutcome::Cleared,
        },
        pallet: recovery.position.pallet,
        base: recovery.position.base,
        unheld: recovery.unheld,
    });

    // A refill whose removal was lost leaves an object present and still recorded. That seals
    // nothing while it is present, but were it lost later it would read as held rather than lost.
    let mut notes = outcome.notes;
    for note in ref_notes {
        if !notes.contains(&note) {
            notes.push(note);
        }
    }
    if let Some(kind) = waits {
        notes.push(format!(
            "The interrupted {} in this bay waits for its pallet's ref; heal finishes or undoes it once that ref is set.",
            kind.as_str()
        ));
    }
    let (unsealed, unseal_failed) = unseal_present(store_access, &mut notes);
    drop_present_unavailable(&mut notes);

    // Last, once every repair before it has run: what a durable ref still reaches and this store
    // cannot give back. A walk that cannot finish claims nothing lost.
    let lost = recovery_utils::find_lost_objects();
    let objects_lost = match &lost {
        Ok(lost) if !lost.is_empty() => Some(ObjectsLost::of(lost)),
        _ => None,
    };

    let report = HealReport {
        record_rebuilt,
        fetch_scope_rebuilt: scope_rebuilt,
        was_tainted: outcome.was_tainted,
        restaged: outcome.restaged,
        resolved: outcome.resolved,
        notes,
        operation,
        unsealed: unsealed.removed,
        newly_held_by_hash: unsealed.pushed_down,
        unseal_failed,
        refs_rebuilt,
        refs_removed,
        refs_refused,
        objects_lost,
    };

    if let Err(error) = lost {
        if !output::is_json() {
            report.render_human();
        }
        let unfinished = format!(
            "heal could not finish its reachability check, so whether any object is lost is not known: {}",
            error
        );
        // A refused ref keeps its own code: its remedy does not wait on the walk.
        if !report.refs_refused.is_empty() {
            return Err(ref_refusal(&report, Some(&unfinished)).into());
        }
        return Err(unfinished);
    }

    if report.refs_refused.is_empty() && report.objects_lost.is_none() {
        output::emit(&report);
        return Ok(());
    }

    if !output::is_json() {
        report.render_human();
    }

    if let Some(lost) = &report.objects_lost {
        return Err(objects_lost_refusal(&report, lost, outcome.index_rebuild_skipped).into());
    }

    Err(ref_refusal(&report, None).into())
}

/// The most hashes an `objects_lost` refusal lists.
const LOST_HASHES_LISTED: usize = 20;

/// What a `heal` run found lost: reachable objects the store cannot give back.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ObjectsLost {
    /// How many reachable objects the store does not hold at all.
    absent: usize,

    /// How many reachable objects the store could not read.
    unreadable: usize,

    /// Up to 20 of them, grouped by why each is lost.
    hashes: Vec<LostObject>,

    /// The pallets whose history reaches any of them.
    pallets: Vec<String>,

    /// Why `pallets` is missing or short, if the walk that finds them could not finish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pallets_unknown: Option<String>,

    /// Sources the check could not read this run, so the counts may be short.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    degraded_sources: Vec<String>,

    /// Whether the pack folder holds an index of another pack format.
    #[serde(skip)]
    foreign_index: bool,
}

/// One lost object and why it is lost.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LostObject {
    hash: String,

    /// "absent", or why the object could not be read.
    reason: String,
}

impl ObjectsLost {
    fn of(lost: &recovery_utils::LostObjects) -> ObjectsLost {
        let mut hashes: Vec<LostObject> = lost.absent.iter()
            .map(|hash| LostObject { hash: hash.clone(), reason: "absent".to_string() })
            .chain(lost.unreadable.iter().map(|(hash, reason)| LostObject { hash: hash.clone(), reason: reason.clone() }))
            .collect();
        hashes.sort_by(|a, b| (&a.reason, &a.hash).cmp(&(&b.reason, &b.hash)));
        hashes.truncate(LOST_HASHES_LISTED);

        let all: std::collections::BTreeSet<String> = lost.absent.iter().chain(lost.unreadable.keys()).cloned().collect();
        let (pallets, pallets_unknown) = match recovery_utils::pallets_reaching(&all) {
            Ok(pallets) => (pallets, None),
            Err(error) => (Vec::new(), Some(error)),
        };

        ObjectsLost {
            absent: lost.absent.len(),
            unreadable: lost.unreadable.len(),
            hashes,
            pallets,
            pallets_unknown,
            degraded_sources: lost.degraded_sources.clone(),
            foreign_index: lost.foreign_index,
        }
    }

    /// The report's lines, for the refusal's message.
    fn lines(&self) -> Vec<String> {
        let total = self.absent + self.unreadable;
        let mut lines = vec![format!(
            "{} object(s) that history still reaches cannot be read from this store: {} absent, {} unreadable.",
            total, self.absent, self.unreadable
        )];
        let mut reason: Option<&str> = None;
        for object in &self.hashes {
            if reason != Some(object.reason.as_str()) {
                lines.push(format!("  {}:", object.reason));
                reason = Some(object.reason.as_str());
            }
            lines.push(format!("    {}", object.hash));
        }
        if total > self.hashes.len() {
            lines.push(format!("  and {} more.", total - self.hashes.len()));
        }
        match (&self.pallets_unknown, self.pallets.is_empty()) {
            (Some(error), _) => lines.push(format!("Which pallets reach them could not be told: {}", error)),
            (None, false) => lines.push(format!("History that reaches them: {}.", self.pallets.join(", "))),
            (None, true) => lines.push("No pallet's history reaches them; other refs (parked work, a bay's operation, a tombstone) do.".to_string()),
        }
        for source in &self.degraded_sources {
            lines.push(format!("This check may be short: {}", source));
        }
        lines
    }
}

/// The `objects_lost` refusal of a run that found reachable objects the store cannot give back,
/// carrying the whole report. A damaged ref left standing is listed too.
fn objects_lost_refusal(report: &HealReport, lost: &ObjectsLost, rebuild_skipped: bool) -> CoreError {
    let mut lines = lost.lines();
    if !report.refs_refused.is_empty() {
        if let CoreError::Refusal { message, .. } = ref_refusal(report, None) {
            lines.push(message);
        }
    }

    let next_step = if lost.foreign_index {
        "re-create the warehouse with this build of Forklift: the pack folder holds an index another build wrote"
    } else if rebuild_skipped {
        "forklift heal"
    } else {
        "restore the warehouse from a backup, or clone it afresh from a remote that holds these objects"
    };

    CoreError::refusal_with_details(
        RefusalCode::ObjectsLost,
        lines.join("\n"),
        next_step,
        serde_json::json!(report),
    )
}

/// The `ref_damaged` refusal of a run that left a damaged ref standing, carrying the whole report.
fn ref_refusal(report: &HealReport, unfinished: Option<&str>) -> CoreError {
    let mut lines = Vec::new();

    for refused in &report.refs_refused {
        lines.push(format!("The ref of pallet \"{}\" was not rebuilt: {}.", refused.pallet, refused.reason));

        if refused.candidates.is_empty() {
            lines.push("  Nothing names a head for it.".to_string());
        }
        for candidate in &refused.candidates {
            lines.push(format!("  {} ({})", candidate.head, candidate.source));
        }
    }

    if let Some(unfinished) = unfinished {
        lines.push(format!("{}.", unfinished.trim_end_matches('.')));
    }

    let first = &report.refs_refused[0].pallet;

    CoreError::refusal_with_details(
        RefusalCode::RefDamaged,
        lines.join("\n"),
        format!("forklift heal --ref {}=<parcel>", first),
        serde_json::json!(report),
    )
}

fn scope_sentence(rebuilt: &ScopeRebuilt) -> String {
    let prefixes = scope_heal::listed(&rebuilt.scope);

    let mut sentence = match rebuilt.chosen {
        true => format!(
            "The fetch scope {} and is set to {}, as chosen; {} object(s) were fetched for it.",
            rebuilt.was.trim_end_matches('.'), prefixes, rebuilt.fetched_objects
        ),
        false => format!(
            "The fetch scope {} and is rebuilt as {}, the paths {} check out. Paths fetched beyond those \
            are no longer checked for loss; \"forklift scope fetch\" adds one back.",
            rebuilt.was.trim_end_matches('.'), prefixes, rebuilt.from.join(" and ")
        ),
    };

    for unread in &rebuilt.left_out {
        sentence.push_str(&format!(" The scope of {} was left out: {}.", unread.checkout, unread.reason));
    }

    sentence
}

fn rebuilt_sentence(rebuilt: &ref_heal::Rebuilt) -> String {
    format!("Rebuilt the ref of pallet \"{}\" at {}, from {}.", rebuilt.pallet, rebuilt.head, rebuilt.source)
}

fn removed_sentence(removed: &ref_heal::Removed) -> String {
    format!(
        "Removed the unreadable ref of pallet \"{}\", keeping its bytes in \"{}\"; what only it reached is no longer held by any pallet.",
        removed.pallet, removed.kept
    )
}

/// Drop the unavailable record's entry of every object the store holds, whose content came back,
/// noting how many went, or why none could. Heal never drops an absent entry: it runs under the
/// shared store hold, beside a fetch that may have recorded one and not yet written its head.
fn drop_present_unavailable(notes: &mut Vec<String>) {
    match unavailable::drop_present() {
        Ok(0) => {}
        Ok(dropped) => notes.push(format!(
            "{} object(s) recorded unavailable are in the store again, so their record was removed.",
            dropped
        )),
        Err(error) => notes.push(format!(
            "The record of content this warehouse cannot get was not checked against the store. {}",
            error
        )),
    }
}

/// Remove the sealed-object record of every object the store holds, pushing each one's coverage
/// down first. A record that cannot be read is noted rather than failing the run: audit fails over
/// it, and a torn rescan leaves its taint standing over it.
///
/// # Returns
/// What was removed and newly recorded, and why the removal failed if it did: a failure to check
/// the store or write the record, after which part of what it would have written may have landed.
/// Each object left recorded gets a note.
fn unseal_present(
    store_access: &StoreAccessLock,
    notes: &mut Vec<String>,
) -> (sealed_utils::Unsealed, Option<String>) {
    let record = match sealed_utils::read_sealed() {
        Ok(record) => record,
        Err(error) => {
            notes.push(format!("{} No object it names was checked against the store.", error));
            return (sealed_utils::Unsealed::default(), None);
        }
    };

    // The recoveries above have landed by now, so a failure here is reported rather than returned:
    // an error would drop the report of what they did, and a record left in place seals nothing.
    let unsealed = match sealed_utils::present_recorded(&record)
        .and_then(|present| sealed_utils::unseal(store_access, &present))
    {
        Ok(unsealed) => unsealed,
        Err(error) => return (sealed_utils::Unsealed::default(), Some(error)),
    };

    for (hash, reason) in &unsealed.kept {
        notes.push(format!(
            "Object {} is on disk but stays in the sealed-object record: {}.",
            hash, reason
        ));
    }

    (unsealed, None)
}

/// The result of a `heal` run that fully resolved (or found nothing to resolve).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HealReport {
    /// What this run dropped from a damaged sealed-object record, which it rebuilt as the part
    /// before the damage. An object whose record was dropped reads as missing if it is absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    record_rebuilt: Option<RecordRebuilt>,

    /// The fetch scope of a sparse warehouse, which was lost or damaged and this run wrote: rebuilt
    /// from the checkouts' scopes, or set to the paths `--fetch-scope` gave.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope_rebuilt: Option<ScopeRebuilt>,

    /// Whether a durability taint was actually standing (an already-healthy warehouse reports
    /// `false`, with `restaged` and `resolved` empty).
    was_tainted: bool,

    /// Recorded paths that were present, verified, and freshly rewritten.
    restaged: Vec<String>,

    /// Recorded paths (or hashes) resolved without a rewrite: proven absent and unreferenced by
    /// the closure walk, or a vanished inventory shard (a staging concern, never an object-trust
    /// one — see `notes`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resolved: Vec<String>,

    /// Advisory notes that never blocked clearing: a note for each vanished inventory shard in
    /// `resolved`, a note for each bay whose local state could not be
    /// read this run (skipped rather than blocking recovery) naming the bay and how to clean it
    /// up, and — only when this run's own closure walk actually confirmed one — a note for each
    /// trust pin (a trust or distrust boundary) that is not held locally. That last kind is never
    /// unconditional: a run with nothing left to check after restaging skips the closure walk
    /// entirely (there is nothing for it to resolve), so it reports no pin note even when one
    /// genuinely applies; a run that still has something dangling reports no pin note either, since
    /// this field only ever appears on a fully-cleared run. The sealed-object record adds a note
    /// for each object on disk that keeps its record, and one when the record cannot be read.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    notes: Vec<String>,

    /// The operation this run settled: one interrupted in this bay before it finished, which every
    /// command that changes the bay or commits staged content refused over until now.
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<OperationRecovery>,

    /// Objects on disk that the sealed-object record still named as held by hash alone (a refill
    /// whose last write was lost leaves one), whose records this run removed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unsealed: Vec<String>,

    /// Absent objects beneath those in `unsealed` that the record now holds by hash alone, so that
    /// removing those records left nothing beneath them unaccounted for.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    newly_held_by_hash: Vec<String>,

    /// Why removing those records failed, if it did. A failed write may have landed in part, so
    /// `unsealed` and `newly_held_by_hash` are then empty whatever the record holds.
    /// Running `heal` again retries.
    #[serde(skip_serializing_if = "Option::is_none")]
    unseal_failed: Option<String>,

    /// Pallet refs that were lost or damaged and this run set: rebuilt at the head the evidence
    /// agreed on, or set to the parcel `--ref` gave.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    refs_rebuilt: Vec<ref_heal::Rebuilt>,

    /// Unreadable pallet refs `--remove-ref` removed, and where their bytes were kept.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    refs_removed: Vec<ref_heal::Removed>,

    /// Damaged pallet refs this run left alone, with every head the evidence named. Present only
    /// on a `ref_damaged` refusal's details.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    refs_refused: Vec<ref_heal::Refused>,

    /// The reachable objects this store cannot give back. Present only on an `objects_lost`
    /// refusal's details.
    #[serde(skip_serializing_if = "Option::is_none")]
    objects_lost: Option<ObjectsLost>,
}

/// What `heal` dropped from a damaged sealed-object record.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RecordRebuilt {
    /// The byte the damage started at. Everything before it was kept.
    at: usize,

    /// How many bytes from there on were dropped: the damage and any records after it.
    dropped_bytes: usize,
}

impl From<sealed_utils::Rebuilt> for RecordRebuilt {
    fn from(rebuilt: sealed_utils::Rebuilt) -> Self {
        RecordRebuilt { at: rebuilt.at, dropped_bytes: rebuilt.dropped_bytes }
    }
}

impl RecordRebuilt {
    fn sentence(&self) -> String {
        format!(
            "The sealed-object record was damaged at byte {} and is rebuilt from the part before it: \
            the {} byte(s) from there on are dropped. An absent object whose record was among them now \
            reads as missing.",
            self.at, self.dropped_bytes
        )
    }
}

/// How `heal` settled an interrupted operation.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum OperationOutcome {
    /// It had already finished; only its record was left.
    Finished,

    /// It was completed.
    RolledForward,

    /// It was undone: the bay is back where it started.
    RolledBack,

    /// It had changed nothing that needed undoing; its record was cleared.
    Cleared,
}

/// An interrupted operation `heal` settled.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct OperationRecovery {
    /// The operation: shift, park, fast-forward, lower, bay creation, git import, merge or
    /// cherry-pick.
    operation: String,

    outcome: OperationOutcome,

    /// The pallet the bay stands on afterwards.
    pallet: String,

    /// The parcel the bay stands on afterwards (absent for an unborn pallet).
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<String>,

    /// Tracked files the working directory does not hold at their recorded content. They read as
    /// local changes, never staged ones; `restore` brings them back.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unheld: Vec<String>,
}

impl CommandOutput for HealReport {
    fn render_human(&self) {
        if let Some(rebuilt) = &self.record_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(rebuilt) = &self.fetch_scope_rebuilt {
            println!("{}", scope_sentence(rebuilt));
        }

        for rebuilt in &self.refs_rebuilt {
            println!("{}", rebuilt_sentence(rebuilt));
        }

        for removed in &self.refs_removed {
            println!("{}", removed_sentence(removed));
        }

        if let Some(operation) = &self.operation {
            operation.render_human();
        }

        if self.was_tainted {
            if self.restaged.is_empty() && self.resolved.is_empty() {
                println!("Durability taint cleared.");
            } else {
                println!("Durability taint cleared:");
                for path in &self.restaged {
                    println!("  restaged: {}", path);
                }
                for path in &self.resolved {
                    println!("  resolved (absent and unreferenced): {}", path);
                }
            }
        } else if self.operation.is_none() && self.unsealed.is_empty() && self.newly_held_by_hash.is_empty()
            && self.notes.is_empty() && self.unseal_failed.is_none() && self.record_rebuilt.is_none()
            && self.fetch_scope_rebuilt.is_none()
            && self.refs_rebuilt.is_empty() && self.refs_removed.is_empty() && self.refs_refused.is_empty()
            && self.objects_lost.is_none()
        {
            println!("Nothing to heal — no durability taint is standing.");
        }

        if !self.unsealed.is_empty() {
            println!(
                "{} object(s) are on disk again and no longer held by hash alone; their sealed-object \
                records are removed:",
                self.unsealed.len()
            );
            for hash in &self.unsealed {
                println!("  {}", hash);
            }
        }

        if !self.newly_held_by_hash.is_empty() {
            println!(
                "{} absent object(s) beneath recorded objects on disk are now held by hash alone, as \
                they were beneath their records:",
                self.newly_held_by_hash.len()
            );
            for hash in &self.newly_held_by_hash {
                println!("  {}", hash);
            }
        }

        if let Some(error) = &self.unseal_failed {
            println!(
                "The sealed-object records of objects on disk again were not all removed: {}",
                error
            );
            println!("Run \"forklift heal\" again to retry.");
        }

        for note in &self.notes {
            println!("Note: {}", note);
        }
    }
}

impl OperationRecovery {
    fn render_human(&self) {
        let at = match &self.base {
            Some(base) => format!("\"{}\" at {}", self.pallet, base),
            None => format!("\"{}\" (unborn)", self.pallet),
        };

        match self.outcome {
            OperationOutcome::Finished => println!("The interrupted {} had already finished; the bay stands on {}.", self.operation, at),
            OperationOutcome::RolledForward => println!("Finished the interrupted {}; the bay stands on {}.", self.operation, at),
            OperationOutcome::RolledBack => println!("Undid the interrupted {}; the bay is back on {}.", self.operation, at),
            OperationOutcome::Cleared => println!("The interrupted {} had changed nothing to undo; its record is cleared.", self.operation),
        }

        if !self.unheld.is_empty() {
            println!("These tracked files do not hold their recorded content and show as local changes (\"forklift restore --confirm <path>\" brings one back):");
            for path in &self.unheld {
                println!("  {}", path);
            }
        }
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<HealReport>(),
    ]
}
