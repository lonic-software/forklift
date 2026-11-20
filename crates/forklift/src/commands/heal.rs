use serde::Serialize;
use forklift_core::util::lock_utils::StoreAccessLock;
use forklift_core::util::checkout_scope_heal::{self, Base, ScopeRestored};
use forklift_core::util::merge_state_heal::{self, Agreement, MergeStateRestored};
use forklift_core::util::position_heal::{self, PositionRebuilt};
use forklift_core::util::scope_heal::{self, ScopeRebuilt};
use forklift_core::util::inventory_heal::{self, TreeUnread};
use forklift_core::util::operation_utils::{self, Operation, OperationKind, RecoveryOutcome};
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::ref_heal::{self, Pass};
use forklift_core::util::reseal_utils::{self, ResealError};
use forklift_core::util::warehouse_utils::{self, WarehouseMode};
use forklift_core::util::applied_redactions::{self, RedactionState};
use forklift_core::util::unavailable_heal::{self, RemoteError};
use forklift_core::util::{config_utils, office_utils, origin_utils, pallet_utils, park_utils, recovery_utils, redaction_utils, sealed_utils, tree_utils, unavailable};
use crate::output::{self, CommandOutput};

/// The note of a re-apply of redactions that could not run, followed by why.
const UNREAPPLIED: &str = "No redaction whose content is lost was applied again";

/// The note of a re-record a check that could not read every source stopped.
const UNRECORDED_SHORT: &str = "No absent object was recorded unavailable again: the check for lost objects could not read every source.";

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
/// A damaged record of content this warehouse cannot get, and a damaged record of applied
/// redactions, are rebuilt first, from the entries that still read whole, a copy kept
/// ([`unavailable::rebuild`], [`applied_redactions::rebuild`]). Last, over a check that read
/// every source, each redaction naming lost absent user file content that the applied record does
/// not hold is applied again where its scan and the configured remote's report allow
/// ([`applied_redactions::reapply`]); one that is not is named in a note. Then each lost absent
/// user file content the remote's host withholds is recorded unavailable again
/// ([`unavailable_heal::record_withheld_lost`]); not when the warehouse's mode does not read, nor
/// in a partial warehouse whose origin record does not read. The check runs again after each step
/// that changed something. Both read one handshake ([`unavailable_heal::handshake`]). A remote
/// that sends no response, or refuses, is handled as the origin is below when the re-record asked
/// it; when only the re-apply did, it is a note.
///
/// Then, in a partial warehouse, it reads the origin record. In a cache whose check for lost
/// objects read every source, it re-seals each lost absent object the origin's held history meets
/// ([`reseal_utils::reseal_held`]), then checks again. An origin that sends no response leaves the
/// lost objects lost and names running heal again. A coded refusal, the origin's trust among them,
/// is the run's own, carrying the report unless it has details of its own; any other failure is a
/// note on the `objects_lost` refusal. A missing or damaged origin record, with nothing else standing,
/// makes the run a `non_origin_remote` refusal (exit 11) carrying the whole report. With objects
/// lost, it becomes a note naming the move that records the origin again.
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
/// * `set_ref`, `remove_ref` - The `--ref PALLET=PARCEL` and `--remove-ref PALLET` specs.
/// * `fetch_scope`  - The paths `--fetch-scope` names, if any.
/// * `scope`        - The paths `--scope` names for the checkout heal runs in, if any.
/// * `position`     - The pallet `--position` names, if any.
/// * `accept_config_loss`    - `--config`: rewrite a damaged configuration file even when the
///   damage costs entries.
/// * `accept_inventory_loss` - `--inventory`: write the rebuilt inventory even when it leaves
///   staging unmatched or a tree unread.
///
/// # Returns
/// * `Ok(())`      - Nothing was tainted, or the taint is now fully cleared.
/// * `Err(String)` - A `durability_taint` refusal (torn, or an unresolved dangling reference), or
///                   the sealed-object record is damaged and could not be rebuilt, which stops
///                   the run before anything else.
pub async fn handle_command(store_access: Option<&StoreAccessLock>,
                            set_ref: &[String],
                            remove_ref: &[String],
                            fetch_scope: &[String],
                            scope: &[String],
                            position: Option<&str>,
                            accept_config_loss: bool,
                            accept_inventory_loss: bool) -> Result<(), String> {
    // Every spec is read before anything is written, so a malformed one changes nothing.
    let set_ref = set_ref.iter()
        .map(|spec| spec.split_once('=').ok_or_else(|| format!(
            "--ref takes PALLET=PARCEL, not \"{}\". Nothing was changed.", spec
        )))
        .collect::<Result<Vec<_>, _>>()?;
    let named = set_ref.iter().map(|(pallet, _)| *pallet).chain(remove_ref.iter().map(String::as_str));
    if let Some(twice) = named.clone().enumerate().find(|(at, pallet)| named.clone().skip(at + 1).any(|other| other == *pallet)) {
        return Err(format!(
            "Pallet \"{}\" is named more than once by --ref and --remove-ref: name each pallet once. Nothing was changed.",
            twice.1
        ));
    }

    // Taken before anything is read: the choice may narrow the fetch scope, so no other process may
    // use the store until it is written.
    let exclusive = match fetch_scope.is_empty() {
        true => None,
        false => Some(StoreAccessLock::try_exclusive()?),
    };

    // Before everything else: every later step reads the configuration, and a damaged file would
    // stop it or, worse, be read around. Nothing here is written yet, so a refusal stops the run
    // with nothing changed. A file that is not damaged is left to those readers.
    let config_repaired = config_utils::repair_warehouse_config(accept_config_loss)?;

    let mut ref_notes: Vec<String> = Vec::new();
    let mut refs_rebuilt: Vec<ref_heal::Rebuilt> = Vec::new();
    let mut refs_removed: Vec<ref_heal::Removed> = Vec::new();

    // First, so the taint's rescan below tells what the record holds by hash from what is lost. The
    // rebuild writes only the record, which no recovery reads through the taint. A damaged record
    // it could not rebuild stops the run, since everything after it would read the damage. One
    // that cannot be read at all does not: the readers below degrade around it and say so.
    let config_said = |error: String| match &config_repaired {
        Some(repaired) => repaired.sentences().iter()
            .fold(error, |error, sentence| forklift_core::error::append_to_message(error, sentence.trim_start())),
        None => error,
    };

    // Before anything reads the trust anchor or a meta ref, whether or not a taint stands: the
    // office first, then the trust file and its copy, then the other meta refs, which are verified
    // against both. A lost or damaged trust file stops every reader of the anchor, and its copy
    // can put it back; a meta ref is read by any later step. Only the trust file repair's refusal
    // stops the run, naming the office when its rebuild landed first; what this pass cannot
    // settle, or fails to write, the pass after the taint recovery judges again.
    let meta_first = ref_heal::repair_meta(Pass::BeforeTaint).map_err(|stopped| {
        let error = stopped.rebuilt.iter().fold(config_said(stopped.error), |error, rebuilt| forklift_core::error::append_to_message(error, &rebuilt_sentence(rebuilt)));
        let error = deferred_sentences(&stopped.deferred, stopped.held.as_ref()).iter().chain(&unjudged_sentences(&stopped.unjudged))
            .fold(error, |error, sentence| forklift_core::error::append_to_message(error, sentence));
        stopped.notes.iter().fold(error, |error, note| forklift_core::error::append_to_message(error, &format!("Note: {}", note)))
    })?;
    let trust_repair = meta_first.trust;
    ref_notes.extend(trust_repair.notes);
    ref_notes.extend(meta_first.repair.notes);
    if let Some(error) = meta_first.repair.stopped {
        ref_notes.push(format!("Not every meta pallet ref was judged before the taint recovery: {}", error.trim_end_matches('.')));
    }
    let meta_rebuilt = meta_first.repair.rebuilt;
    let meta_undurable = meta_first.repair.undurable;
    // The damaged refs a refusal from here on names beside what was written: those the pass before
    // the taint left for the pass after it, until that pass has judged them; then those it refused.
    let meta_deferred = meta_first.repair.deferred;
    let refs_left = std::cell::RefCell::new(RefsLeft {
        deferred: meta_deferred.clone(), refused: Vec::new(), unjudged: meta_first.repair.unjudged, held: meta_first.repair.held,
    });

    // Every refusal from here on says what this run already wrote, the trust file and the meta
    // refs among it, and the refs it left damaged.
    let config_said = |error: String| {
        let error = match &trust_repair.restored {
            Some(restored) => forklift_core::error::append_to_message(config_said(error), &restored.sentence()),
            None => config_said(error),
        };
        let error = meta_rebuilt.iter().fold(error, |error, rebuilt| forklift_core::error::append_to_message(error, &rebuilt_sentence(rebuilt)));
        let left = refs_left.borrow();
        let error = refused_too(error, &left.refused);
        deferred_sentences(&left.deferred, left.held.as_ref()).iter().chain(&unjudged_sentences(&left.unjudged))
            .fold(error, |error, sentence| forklift_core::error::append_to_message(error, sentence))
    };

    // A rebuild left not durable is said by the pass that wrote it alone: every later pass reads
    // its ref as undamaged. So it ends the run, in each pass below as here.
    if let Some(error) = undurable_error(&meta_undurable) {
        return Err(config_said(ref_notes.iter().fold(error, |error, note| forklift_core::error::append_to_message(error, &format!("Note: {}", note)))));
    }

    let record_rebuilt = sealed_utils::rebuild()
        .map_err(|error| config_said(ref_notes.iter().fold(format!(
            "{}{} Heal went no further: once that is fixed, run \"forklift heal\" again.",
            error, if error.ends_with('.') { "" } else { "." },
        ), |error, note| forklift_core::error::append_to_message(error, &format!("Note: {}", note)))))?
        .map(RecordRebuilt::from);

    // Before the taint recovery too, whose walks read the record of content this warehouse cannot
    // get: one that does not read makes them hold nothing absent. What it drops, heal records again
    // below when the remote's host still withholds it. A rebuild that fails is a note, which every
    // refusal below carries too: the readers degrade around the damage and say so.
    let unavailable_rebuilt = match unavailable::rebuild() {
        Ok(rebuilt) => rebuilt.map(UnavailableRebuilt::from),
        Err(error) => {
            ref_notes.push(format!(
                "The damaged record of content this warehouse cannot get was not rebuilt: {}",
                error.trim_end_matches('.')
            ));
            None
        }
    };

    // Before the taint recovery too, whose walks read the record of applied redactions: one that
    // does not read is a degraded source of every walk, and fails every read of redacted content.
    // What it drops, heal applies again below when the content is gone. A failure is a note, as
    // the record's above.
    let applied_rebuilt = match applied_redactions::rebuild() {
        Ok(rebuilt) => rebuilt.map(AppliedRebuilt::from),
        Err(error) => {
            ref_notes.push(format!(
                "The damaged record of applied redactions was not rebuilt: {}",
                error.trim_end_matches('.')
            ));
            None
        }
    };

    let records_said = |error: CoreError, notes: &[String], position: Option<&PositionRebuilt>, restored: Option<&ScopeRestored>| {
        let mut error = config_said(String::from(error));
        let sentences = record_rebuilt.iter().map(RecordRebuilt::sentence)
            .chain(unavailable_rebuilt.iter().map(UnavailableRebuilt::sentence))
            .chain(applied_rebuilt.iter().map(AppliedRebuilt::sentence))
            .chain(position.map(PositionRebuilt::sentence))
            .chain(restored.map(ScopeRestored::sentence));
        for sentence in sentences {
            error = forklift_core::error::append_to_message(error, &sentence);
        }
        for note in notes {
            error = forklift_core::error::append_to_message(error, &format!("Note: {}", note));
        }
        error
    };

    // The bay's position, before the checkout's scope, whose stranding guard reads the tree the
    // checkout stands on, and so before the fetch-scope heal too: a rebuild from the checkouts'
    // scopes would leave out a scope not yet restored. It reads neither scope. A --position it
    // sets, on a pallet whose ref this run sets or removes, stands on that write, so it waits for
    // it below; one it refuses is refused here, before the steps below write anything.
    let early = !position.is_some_and(|pallet| {
        set_ref.iter().any(|(named, _)| *named == pallet) || remove_ref.iter().any(|named| named == pallet)
    }) || !position_heal::sets_chosen().map_err(|error| records_said(CoreError::from(error), &ref_notes, None, None))?;
    let early_position = match early {
        true => position_heal::repair(position, &mut ref_notes)
            .map_err(|error| records_said(CoreError::from(error), &ref_notes, None, None))?,
        false => None,
    };

    // The checkout's own scope, just before the fetch-scope heal, whose rebuild from the checkouts'
    // scopes then sees it restored. Nothing before it reads either scope. A position that waits is
    // set to the parcel its --ref names, so that is the tree the guard judges against, or unborn
    // when its --remove-ref removes the last pallet. The write it waits for is judged first, as
    // that write judges it, so no scope is judged against a tree the write then refuses.
    // Some(None): the position will be unborn.
    let mut check_notes = Vec::new();
    let waits_on: Result<Option<Option<String>>, String> = match (early, position) {
        (false, Some(pallet)) => match set_ref.iter().find(|(named, _)| *named == pallet) {
            Some((_, parcel)) => ref_heal::check_set_damaged_ref(pallet, parcel, &mut check_notes).map(|head| Some(Some(head))),
            None => pallet_utils::list_pallets().and_then(|pallets| match pallets.iter().filter(|named| !remove_ref.contains(named)).map(|named| format!("\"{}\"", named)).collect::<Vec<_>>() {
                left if !left.is_empty() => Err(format!(
                    "--position names pallet \"{}\", whose ref --remove-ref removes, so the bay cannot be put on it: name one of {} with --position instead. Nothing was changed.",
                    pallet, left.join(", ")
                )),
                _ => ref_heal::check_remove_damaged_ref(pallet, &mut check_notes).map(|()| Some(None)),
            }),
        },
        _ => Ok(None),
    };
    let waits_on = match waits_on {
        Ok(waits_on) => waits_on,
        Err(error) => {
            ref_notes.append(&mut check_notes);
            return Err(records_said(CoreError::from(error), &ref_notes, None, None));
        }
    };
    let base = match &waits_on {
        Some(Some(parcel)) => Base::Parcel(parcel),
        Some(None) => Base::Unborn,
        None => Base::Position,
    };
    let early_scope = heal_checkout_scope(scope, base, &mut ref_notes)
        .map_err(|error| records_said(error, &ref_notes, early_position.as_ref(), None))?;

    // The records of the merge in progress, after the position and the scope, so a copy is judged
    // against the pallet and parcel heal settled for the bay, and before the passes and the
    // operation's recovery, which read them.
    let agreement = match base {
        Base::Position => merge_state_heal::agreement_in(&forklift_core::globals::bay_root()),
        Base::Parcel(head) => position.map(|pallet| Agreement { pallet: pallet.to_string(), head: head.to_string() }),
        Base::Unborn => None,
    };
    let mut merge_repair = merge_state_heal::repair(agreement.as_ref(), &mut ref_notes)
        .map_err(|error| records_said(error, &ref_notes, early_position.as_ref(), early_scope.as_ref()))?;
    let early_restored = merge_repair.restored.clone();
    // A refusal heal ends with once every repair has run: a note until then, so a refusal that
    // ends the run earlier still names it. Rendered, since a note is plain text in a sentence.
    let merge_note_of = |refusal: &Option<CoreError>| refusal.clone().map(|refusal| forklift_core::error::rendered_frames(&String::from(refusal)));
    let mut merge_note = merge_note_of(&merge_repair.refusal);
    ref_notes.extend(merge_note.clone());
    let with_merge = |mut error: String| {
        for sentence in early_restored.iter().flat_map(MergeStateRestored::sentences) {
            error = forklift_core::error::append_to_message(error, &sentence);
        }
        error
    };

    // After the records, which the check of the history leans on, and before everything else,
    // which reads the fetch scope.
    let scope_rebuilt = match &exclusive {
        Some(exclusive) => Some(scope_heal::set_chosen(fetch_scope, exclusive).await),
        None => scope_heal::repair().transpose(),
    }.transpose().map_err(|error| with_merge(records_said(error, &ref_notes, early_position.as_ref(), early_scope.as_ref())))?;
    drop(exclusive);

    let shared;
    let store_access = match store_access {
        Some(store_access) => store_access,
        None => {
            shared = StoreAccessLock::shared(crate::announce_store_access_wait)
                .map_err(|error| with_merge(records_said(CoreError::from(error), &ref_notes, early_position.as_ref(), early_scope.as_ref())))?;
            &shared
        }
    };

    // Under store access, since it scans the store for parked parcels, and before the operation's
    // recovery, which reads the parked-work record for a park, and the taint recovery, whose walks
    // root every parked parcel. A failure is a note, as the records' above: the walks degrade
    // around the damage and say so.
    let parked_rebuilt = match park_utils::rebuild() {
        Ok(rebuilt) => rebuilt.map(ParkedRebuilt::from),
        Err(error) => {
            ref_notes.push(format!("The parked-work record was not rebuilt: {}", error.trim_end_matches('.')));
            None
        }
    };

    // What the user chose goes after the record, which it reads, and before the passes, so they
    // judge only what is left. A refusal says what this run already changed: its own "Nothing was
    // changed" speaks only for the write it refused. Its notes go along: a check they say was
    // skipped would otherwise go unreported.
    let changed = |error: String, rebuilt: &[ref_heal::Rebuilt], removed: &[ref_heal::Removed], notes: &[String]| {
        let mut error = config_said(error);
        if let Some(record) = &record_rebuilt {
            error = forklift_core::error::append_to_message(error, &record.sentence());
        }
        if let Some(record) = &unavailable_rebuilt {
            error = forklift_core::error::append_to_message(error, &record.sentence());
        }
        if let Some(record) = &applied_rebuilt {
            error = forklift_core::error::append_to_message(error, &record.sentence());
        }
        if let Some(position) = &early_position {
            error = forklift_core::error::append_to_message(error, &position.sentence());
        }
        if let Some(restored) = &early_scope {
            error = forklift_core::error::append_to_message(error, &restored.sentence());
        }
        error = with_merge(error);
        if let Some(scope) = &scope_rebuilt {
            error = forklift_core::error::append_to_message(error, &scope_sentence(scope));
        }
        if let Some(record) = &parked_rebuilt {
            error = forklift_core::error::append_to_message(error, &record.sentence());
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

    // A meta ref is removed on the judgement the meta pass above made of it in this run, which
    // its removal pins the ref and the copy to.
    for pallet in remove_ref {
        let removed = match pallet.starts_with('@') {
            true => ref_heal::remove_damaged_meta_ref(pallet, &meta_deferred, &refs_left.borrow().unjudged, &mut ref_notes),
            false => ref_heal::remove_damaged_ref(pallet, &mut ref_notes).map_err(|error| (error, None)),
        };
        match removed {
            Ok(removed) => refs_removed.push(removed),
            // What a removal that stopped had removed is said with its error.
            Err((error, partial)) => {
                refs_removed.extend(partial);
                return Err(changed(error, &refs_rebuilt, &refs_removed, &ref_notes));
            }
        }
    }

    // A chosen position beside --ref or --remove-ref: after the refs the user set, which it stands
    // on. Either way before the ref heal and the operation's recovery, which both read the
    // position.
    let late_position = match early {
        true => None,
        false => position_heal::repair(position, &mut ref_notes).map_err(|error| changed(error, &refs_rebuilt, &refs_removed, &ref_notes))?,
    };

    let changed_after = |error: String, rebuilt: &[ref_heal::Rebuilt], removed: &[ref_heal::Removed], notes: &[String]| {
        let mut error = changed(error, rebuilt, removed, notes);
        if let Some(position) = &late_position {
            error = forklift_core::error::append_to_message(error, &position.sentence());
        }
        error
    };

    // Before the taint recovery, whose closure walk reads every ref: a ref rebuilt here roots
    // what it reaches for that walk.
    let first = ref_heal::repair(Pass::BeforeTaint).map_err(|error| changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes))?;
    refs_rebuilt.extend(first.rebuilt);
    ref_notes.extend(first.notes);
    if let Some(error) = stopped_with(first.stopped, &first.undurable) {
        return Err(changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes));
    }

    // A torn taint's rescan (§8.3) can take a while on a large or uncompacted store — it reports
    // per-phase/per-count progress through this callback rather than staying silent. `human!` is
    // itself a no-op under `--json` (progress is never part of the result document), and
    // `forklift-core` never prints on its own (DESIGN.html §3.4) — this closure is the one bridge.
    let outcome = match recovery_utils::run(Some(&|line: &str| crate::human!("{}", line))).await {
        Ok(outcome) => outcome,
        Err(error) => {
            // An interrupted operation waits for the taint (below), so the refusal says so: it
            // is not the only thing left standing.
            let mut error = changed_after(String::from(error), &refs_rebuilt, &refs_removed, &ref_notes);
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
    // The meta refs first, as before the taint: a user ref's rebuild reads the office.
    let meta_second = match ref_heal::repair_meta(Pass::AfterTaint) {
        Ok(meta) => {
            refs_left.replace(RefsLeft::after_second(&meta_deferred, meta.repair.refused.clone(), meta.repair.unjudged.clone()));
            meta
        }
        Err(stopped) => {
            refs_left.replace(RefsLeft::after_second(&meta_deferred, stopped.refused, stopped.unjudged));
            refs_rebuilt.extend(stopped.rebuilt);
            ref_notes.extend(stopped.notes);
            return Err(changed_after(stopped.error, &refs_rebuilt, &refs_removed, &ref_notes));
        }
    };
    // The trust repair runs in both passes; what it noted in the first is said once. After a
    // first write the two files agree, so a second one means they moved between the passes: the
    // report names the first, and the second is noted.
    let trust_restored_after = meta_second.trust.restored;
    if let (Some(_), Some(after)) = (&trust_repair.restored, &trust_restored_after) {
        ref_notes.push(format!("{} (after the taint recovery).", after.sentence().trim_end_matches('.')));
    }
    for note in meta_second.trust.notes {
        if !ref_notes.contains(&note) {
            ref_notes.push(note);
        }
    }
    let meta_second = meta_second.repair;
    refs_rebuilt.extend(meta_second.rebuilt);
    ref_notes.extend(meta_second.notes);
    if let Some(error) = stopped_with(meta_second.stopped, &meta_second.undurable) {
        return Err(changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes));
    }

    let second = ref_heal::repair(Pass::AfterTaint).map_err(|error| changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes))?;
    refs_rebuilt.extend(second.rebuilt);
    ref_notes.extend(second.notes);
    let mut refs_refused = second.refused;
    refs_refused.extend(meta_second.refused);
    refs_left.replace(RefsLeft { deferred: Vec::new(), refused: refs_refused.clone(), unjudged: Vec::new(), held: None });
    if let Some(error) = stopped_with(second.stopped, &second.undurable) {
        return Err(changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes));
    }

    // An operation on a pallet whose ref is still damaged waits: its landed test would read the
    // damage as "not landed".
    let waits = match operation_utils::read() {
        Ok(Some(operation)) => refs_refused.iter()
            .any(|refused| refused.pallet == operation.source.pallet || refused.pallet == operation.target.pallet)
            .then_some(operation.kind),
        _ => None,
    };

    // After the position heal and both ref heals: the rebuild reads the bay's base from the
    // position, and no ref. A partial park's recovery reads the set before it rewrites part of it,
    // so for that marker the rebuild comes first, even while the park waits; a rebuild held there
    // holds the park's recovery too. Every other recovery rewrites staging wholesale or writes
    // none, so the rebuild follows it, and waits with it while it waits on a ref.
    let mut inventory_rebuilt: Option<InventoryRebuilt> = None;
    let mut inventory_held: Option<InventoryRebuilt> = None;
    let marker = operation_utils::read().ok().flatten();
    let partial_park = marker.as_ref()
        .filter(|marker| marker.kind == OperationKind::Park && !marker.pathspec.is_empty());
    // A metadata path that is not a file, or a rebuild that fails, ends the run like a held
    // rebuild: every other repair runs first, and the report says what they did. Either leaves the
    // set unreadable, so the recovery, which reads it, does not run.
    let mut inventory_error: Option<String> = None;
    match inventory_heal::damaged() {
        Ok(true) if partial_park.is_some() => match rebuild_inventory(partial_park, accept_inventory_loss) {
            Ok((view, written)) => *(if written { &mut inventory_rebuilt } else { &mut inventory_held }) = Some(view),
            Err(error) => inventory_error = Some(error),
        },
        Ok(_) => {}
        Err(error) => inventory_error = Some(error),
    }
    let unreadable = inventory_error.is_some();
    // A step that fails after a written rebuild says so beside every other repair.
    let with_rebuilt = |error: String, rebuilt: &Option<InventoryRebuilt>| match rebuilt {
        Some(rebuilt) => forklift_core::error::append_to_message(error, &rebuilt.sentence()),
        None => error,
    };
    let held_park = partial_park.is_some() && inventory_held.is_some();
    if held_park {
        ref_notes.push(
            "The interrupted park in this bay waits for the inventory's rebuild; heal finishes or undoes it once that is written."
                .to_string(),
        );
    }
    // Every recovery reads the set, which neither leaves readable.
    if unreadable {
        if let Some(standing) = &marker {
            ref_notes.push(format!(
                "The interrupted {} in this bay waits for the inventory's record of staged folders to read; heal finishes or undoes it once it does.",
                standing.kind.as_str()
            ));
        }
    }

    // After the taint: an operation's recovery reads and writes the store the taint vouches for.
    let recovered = match waits.is_some() || held_park || unreadable {
        true => Ok(None),
        false => operation_utils::recover(),
    }.map_err(|error| with_rebuilt(changed_after(error, &refs_rebuilt, &refs_removed, &ref_notes), &inventory_rebuilt))?;

    // A merge record left damaged under another kind of operation is repaired again once that
    // operation's recovery has lowered its marker, so a refusal it ends with names a way out that
    // can now run.
    if merge_repair.deferred {
        match operation_utils::read() {
            Ok(None) => {
                // Judged against the position that recovery left, which it may have moved.
                let settled = merge_state_heal::agreement_in(&forklift_core::globals::bay_root());
                let later = merge_state_heal::repair(settled.as_ref(), &mut ref_notes)
                    .map_err(|error| with_rebuilt(changed_after(String::from(error), &refs_rebuilt, &refs_removed, &ref_notes), &inventory_rebuilt))?;
                merge_repair = merge_repair.then(later);
                if merge_note.is_none() {
                    merge_note = merge_note_of(&merge_repair.refusal);
                    ref_notes.extend(merge_note.clone());
                }
            }
            _ => ref_notes.push(
                "A damaged record of the merge in progress in this bay waits for the interrupted operation there to be \
                 finished or undone; heal says what to run for it once that is done."
                    .to_string(),
            ),
        }
    }

    if waits.is_none() && !held_park && !unreadable {
        let rebuilt = inventory_heal::damaged().and_then(|damaged| match damaged {
            true => rebuild_inventory(operation_utils::read().ok().flatten().as_ref(), accept_inventory_loss).map(Some),
            false => Ok(None),
        });
        match rebuilt {
            Ok(Some((view, written))) => *(if written { &mut inventory_rebuilt } else { &mut inventory_held }) = Some(view),
            Ok(None) => {}
            Err(error) => inventory_error = Some(error),
        }
    }
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
    // A damaged merge record no copy could restore is refused last, once every repair has run: its
    // way out reads what they mend. While another refusal leads, it stays a note.
    let merge_refusal = merge_repair.refusal;
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
    let mut lost = recovery_utils::find_lost_objects();

    // Absent content a file of this checkout's inventory still holds in the working tree is stored
    // again from there: the file hashes to it, so what is written is the lost object itself. Never
    // content a redaction names, which a lost record of applied redactions leaves reading as
    // absent, so nothing is stored while the redactions cannot all be read.
    let mut restored_from_working_tree: Vec<String> = Vec::new();
    let redacted = match &lost {
        Ok(found) if !found.absent.is_empty() => match redaction_utils::read_redactions() {
            Ok(read) if read.unreadable.is_empty() => {
                Ok(read.redactions.into_iter().map(|attributed| attributed.redaction.hash).collect())
            }
            Ok(read) => Err(format!("{} redaction(s) cannot be read", read.unreadable.len())),
            Err(why) => Err(why),
        },
        _ => Ok(std::collections::BTreeSet::new()),
    };
    if let (Ok(_), Err(why)) = (&lost, &redacted) {
        notes.push(format!(
            "Lost content was not looked for in the working tree, since what the redactions name is not known, so none was stored again from there: {}",
            why.trim_end_matches('.')
        ));
    }
    if let (Ok(found), Ok(redacted)) = (&lost, &redacted) {
        match recovery_utils::restore_from_working_tree(&found.absent, redacted) {
            Ok(restore) => {
                for (path, why) in &restore.failed {
                    notes.push(format!(
                        "\"{}\" in the working tree may hold content the store lost, and was not stored again from there: {}",
                        path, why.trim_end_matches('.')
                    ));
                }
                if !restore.restored.is_empty() {
                    restored_from_working_tree = restore.restored;
                    lost = recovery_utils::find_lost_objects();
                }
            }
            Err(why) => notes.push(format!(
                "Lost content was not looked for in the working tree, so none was stored again from there: {}",
                why.trim_end_matches('.')
            )),
        }
    }

    // A partial warehouse leans on its origin for what it lacks, so a missing or damaged record of
    // it is damage heal names, even with nothing lost.
    let mode = match warehouse_utils::warehouse_mode() {
        Ok(mode) => Some(mode),
        Err(why) => {
            notes.push(format!(
                "Whether this is a partial warehouse could not be read, so its origin record was not checked, and nothing was re-sealed from the origin or recorded unavailable again: {}",
                why.trim_end_matches('.')
            ));
            None
        }
    };
    // A damaged warehouse file is mended by now, so a failure here is one heal does not mend: a
    // mistake made by hand, an unreadable file, or the global file's damage. Heal goes on as
    // though no remote were asked for, saying why.
    let configured = match config_utils::get_effective_value(config_utils::KEY_REMOTE_URL) {
        Ok(configured) => configured.map(|(url, _)| url),
        Err(why) => {
            notes.push(format!(
                "The remote's address could not be read, so heal went on as though none were configured: {}",
                why.trim_end_matches('.')
            ));
            None
        }
    };
    let origin = match mode {
        Some(WarehouseMode::Full) | None => None,
        Some(_) => Some(origin_utils::recorded_origin(configured.as_deref())),
    };

    // Absent file content whose unavailable entry was lost, or dropped from a damaged record, is
    // content the remote's host may still withhold: recorded again, a checkout or an export leaves
    // it out, as after the fetch. Only file content, as a fetch records. Not when the warehouse's
    // mode does not read, nor in a partial warehouse whose origin record does not read: the remote
    // a client would ask is not known to be its origin, and the notes name that. Only over a check
    // that read every source, as the re-seal below.
    let mut recorded_unavailable: Vec<String> = Vec::new();
    let mut redactions_reapplied: Vec<String> = Vec::new();
    let mut origin_unanswered = false;
    let mut remote_refused: Option<CoreError> = None;
    let rerecord_from = match (&lost, mode, &origin) {
        (Ok(found), Some(_), None | Some(Ok(_))) if !found.absent_files.is_empty() => {
            if found.degraded_sources.is_empty() {
                Some(found.absent_files.clone())
            } else {
                notes.push(UNRECORDED_SHORT.to_string());
                None
            }
        }
        _ => None,
    };

    // Absent file content that a redaction names whose hash a lost or damaged record of applied
    // redactions dropped: applied again, a read of it is refused as redacted and an export leaves
    // it out, as before. Over a check that read every source, as the re-record; with no remote,
    // and whatever the origin record says, too: the scan and the remote configured decide, as
    // they do for `redact apply`. Before the re-record, which skips applied content.
    let redactions = match &lost {
        Ok(found) if !found.absent_files.is_empty() && found.degraded_sources.is_empty() => {
            match redaction_utils::read_redactions() {
                Ok(read) => Some((read, found.absent_files.clone())),
                Err(why) => {
                    notes.push(format!("{}: {}", UNREAPPLIED, why.trim_end_matches('.')));
                    None
                }
            }
        }
        _ => None,
    };
    let reapply_of = match &redactions {
        Some((read, absent)) => applied_redactions::unapplied_among(read, absent).unwrap_or_else(|why| {
            notes.push(format!("{}: {}", UNREAPPLIED, why.trim_end_matches('.')));
            Vec::new()
        }),
        None => Vec::new(),
    };

    // One handshake for both, so the remote's report and what its host withholds are one answer.
    let mut waits_for_remote: Vec<&str> = Vec::new();
    if rerecord_from.is_some() {
        waits_for_remote.push("no absent object its host may withhold was recorded unavailable again");
    }
    if !reapply_of.is_empty() {
        waits_for_remote.push("no redaction whose content is lost was applied again");
    }
    let mut remote_failed = false;
    let answered = match waits_for_remote.is_empty() {
        true => None,
        false => match unavailable_heal::handshake().await {
            Ok(answered) => answered,
            // A remote only the re-apply asked never stands in for the origin's answer: the
            // re-seal and the run's refusal are not this step's to settle.
            Err(RemoteError::NoResponse(why)) => {
                origin_unanswered |= rerecord_from.is_some();
                remote_failed = true;
                notes.push(format!(
                    "The remote did not answer, so {}: {}", waits_for_remote.join(", and "), why.trim_end_matches('.')
                ));
                None
            }
            // As the re-seal's below: a coded refusal is the run's own, anything else a note.
            Err(RemoteError::Refused(why)) => {
                remote_failed = true;
                match CoreError::from(why) {
                    refused @ CoreError::Refusal { .. } if rerecord_from.is_some() => remote_refused = Some(refused),
                    // A note is prose: a refusal gives its message, never its machine frame.
                    other => notes.push(format!(
                        "The handshake with the remote failed, so {}: {}",
                        waits_for_remote.join(", and "),
                        match other {
                            CoreError::Refusal { message, .. } => message,
                            other => String::from(other),
                        }.trim_end_matches('.')
                    )),
                }
                None
            }
        },
    };

    if !reapply_of.is_empty() && !remote_failed {
        let report = answered.as_ref().map(unavailable_heal::Answered::report);
        match applied_redactions::reapply(&reapply_of, report.as_ref()) {
            Ok(statuses) => {
                for status in statuses {
                    match status.state {
                        RedactionState::Applied => redactions_reapplied.push(status.hash),
                        state => {
                            notes.push(format!(
                                "The redaction of lost object {} is not applied again: it is {}.",
                                status.hash, crate::commands::redact::StateView::from(&state).describe()
                            ));
                        }
                    }
                }
                if !redactions_reapplied.is_empty() {
                    lost = recovery_utils::find_lost_objects();
                }
            }
            Err(why) => notes.push(format!("{}: {}", UNREAPPLIED, why.trim_end_matches('.'))),
        }
    }

    if let (Some(absent), Some(answered)) = (rerecord_from, &answered) {
        match unavailable_heal::record_withheld_lost(&absent, answered) {
            Ok(recorded) => {
                if !recorded.is_empty() {
                    recorded_unavailable = recorded;
                    lost = recovery_utils::find_lost_objects();
                }
            }
            Err(RemoteError::NoResponse(why) | RemoteError::Refused(why)) => notes.push(format!(
                "No absent object was recorded unavailable again: {}",
                why.trim_end_matches('.')
            )),
        }
    }

    // In a cache, an absent object the sealed record lost may be one the origin still holds:
    // recorded again, a read refills it on demand. Only once the origin's record reads, and only
    // over a check that read every source, since a short one may call held objects absent. Not
    // once the same remote went unanswered or refused above.
    let mut resealed: Vec<String> = Vec::new();
    let reseal_from = match (&lost, mode, &origin) {
        _ if origin_unanswered || remote_refused.is_some() => None,
        (Ok(found), Some(WarehouseMode::Cache), Some(Ok(_))) if !found.absent.is_empty() => {
            if found.degraded_sources.is_empty() {
                Some(found.absent.clone())
            } else {
                // One note for both steps the short check stopped.
                match notes.iter().position(|note| note == UNRECORDED_SHORT) {
                    Some(at) => notes[at] = "No absent object was recorded unavailable again or re-sealed from the origin: the check for lost objects could not read every source.".to_string(),
                    None => notes.push("No absent object was re-sealed from the origin: the check for lost objects could not read every source.".to_string()),
                }
                None
            }
        }
        _ => None,
    };
    if let Some(absent) = reseal_from {
        match reseal_utils::reseal_held(&absent, store_access).await {
            Ok(done) => {
                notes.extend(done.notes);
                if !done.recorded.is_empty() {
                    resealed = done.recorded;
                    lost = recovery_utils::find_lost_objects();
                }
            }
            Err(ResealError::NoResponse(why)) => {
                origin_unanswered = true;
                notes.push(format!(
                    "The origin did not answer, so no absent object it may still hold was re-sealed: {}",
                    why.trim_end_matches('.')
                ));
            }
            // A coded refusal is the run's own, with its next step, once the report is built; its
            // step clears it, and heal can run again after. Anything else is a note.
            Err(ResealError::Refused(why)) => match CoreError::from(why) {
                refused @ CoreError::Refusal { .. } => remote_refused = Some(refused),
                other => notes.push(format!(
                    "No absent object was re-sealed from the origin: {}",
                    String::from(other).trim_end_matches('.')
                )),
            },
        }
    }
    if let Ok(lost) = &lost {
        if lost.damaged_unreached > 0 {
            notes.push(format!(
                "{} damaged loose object file(s) that no history needs are left in place; storing the same content again would keep the damaged file.",
                lost.damaged_unreached
            ));
        }
        if lost.damaged_held > 0 {
            notes.push(format!(
                "{} damaged loose object file(s) sit at addresses this warehouse holds by hash alone, redacted or withheld, so they are not counted lost; they are left in place, and a read of one fails until the file is removed.",
                lost.damaged_held
            ));
        }
        if lost.damaged_records_unreached > 0 {
            notes.push(format!(
                "{} object(s) with damaged pack records that no history needs are left in place; storing the same content again would keep the damaged records.",
                lost.damaged_records_unreached
            ));
        }
        if lost.damaged_records_held > 0 {
            notes.push(format!(
                "{} object(s) with damaged pack records sit at addresses this warehouse holds by hash alone, redacted or withheld, so they are not counted lost; the records are left in place, and a read of one fails.",
                lost.damaged_records_held
            ));
        }
        // A refusal lists them among the reasons its check may be short.
        if lost.is_empty() {
            for unchecked in &lost.unchecked {
                notes.push(format!("The check for lost objects may be short: {}", unchecked));
            }
        }
    }
    let objects_lost = match &lost {
        Ok(lost) if !lost.is_empty() => Some(ObjectsLost::of(lost)),
        _ => None,
    };

    // The origin's refusal is the run's own only when nothing else stands; otherwise it is a note,
    // since a move to restore the record checks that server holds everything listed lost.
    let origin_refused = match origin {
        Some(Err(refused)) => Some(refused),
        _ => None,
    };
    if origin_refused.is_some() && (objects_lost.is_some() || lost.is_err() || !refs_refused.is_empty()) {
        let url = configured.as_deref().map(|url| url.trim_end_matches('/')).unwrap_or("<url>");
        notes.push(match (mode, &objects_lost) {
            (Some(WarehouseMode::Cache), Some(_)) => format!(
                "The origin record is missing or damaged. \"forklift remote move {}\" records it again only if that server holds every object listed lost, after which heal re-seals them.",
                url
            ),
            _ => format!(
                "The origin record is missing or damaged. \"forklift remote move {}\" records it again once it checks that server holds this warehouse's content.",
                url
            ),
        });
    }

    let report = HealReport {
        record_rebuilt,
        unavailable_rebuilt,
        redactions_applied_rebuilt: applied_rebuilt,
        checkout_scope_restored: early_scope,
        merge_state_restored: merge_repair.restored,
        fetch_scope_rebuilt: scope_rebuilt,
        parked_rebuilt,
        position_rebuilt: early_position.or(late_position),
        config_repaired,
        trust_restored: trust_repair.restored.clone().or(trust_restored_after),
        inventory_rebuilt,
        inventory_held,
        was_tainted: outcome.was_tainted,
        restaged: outcome.restaged,
        resolved: outcome.resolved,
        // A note can quote an error a step failed with, which may be a refusal's frame.
        notes: notes.iter().map(|note| forklift_core::error::rendered_frames(note)).collect(),
        operation,
        unsealed: unsealed.removed,
        newly_held_by_hash: unsealed.pushed_down,
        unseal_failed,
        resealed,
        recorded_unavailable,
        redactions_reapplied,
        restored_from_working_tree,
        refs_rebuilt: meta_rebuilt.iter().cloned().chain(refs_rebuilt).collect(),
        refs_removed,
        refs_refused,
        objects_lost,
    };

    // Ahead of every other end refusal: their remedies read the set, and an unload, the remedy for
    // lost staged content, is refused while it is damaged. Every repair above has run.
    if inventory_error.is_some() || report.inventory_held.is_some() {
        // A run that only found the step that failed says nothing of its own: the refusal speaks.
        if !output::is_json() && !(inventory_error.is_some() && !report.was_tainted && report.nothing_done()) {
            report.render_human();
        }
        let mut lines = match (&inventory_error, &report.inventory_held) {
            (Some(error), _) => vec![forklift_core::error::rendered_frames(error)],
            (None, Some(held)) => held.lines(),
            (None, None) => Vec::new(),
        };
        if let Err(error) = &lost {
            lines.push(format!(
                "heal could not finish its reachability check, so whether any object is lost is not known: {}",
                error.trim_end_matches('.')
            ));
        }
        if let Some(lost) = &report.objects_lost {
            lines.extend(objects_lost_lines(&report, lost));
        } else if !report.refs_refused.is_empty() {
            if let CoreError::Refusal { message, .. } = ref_refusal(&report, None) {
                lines.push(message);
            }
        }
        // The refusals the run would end on otherwise still stand, and are named after the rebuild.
        // A walk that did not finish leaves the origin a note, as the run without the hold does.
        let standing = match (&report.objects_lost, report.refs_refused.is_empty()) {
            (Some(_), _) => remote_refused.as_ref(),
            (None, true) if lost.is_ok() => origin_refused.as_ref(),
            _ => None,
        };
        if let Some(CoreError::Refusal { message, next_step, .. }) = standing {
            lines.push(format!("{} Its own next step, after the rebuild: {}", message, next_step));
        }
        if report.objects_lost.is_some() && (outcome.index_rebuild_skipped || origin_unanswered) {
            lines.push("The check for lost objects may be short; a heal after the rebuild checks again.".to_string());
        }
        // The step that failed leads, with its own code and next step when it has them. A failure
        // without one (a store that cannot be listed, a write that fails) stays uncoded: no code
        // names it, and a heal rerun meets it again.
        if let Some(error) = inventory_error {
            let also = lines.split_off(1);
            return Err(match CoreError::from(error) {
                refused @ CoreError::Refusal { .. } => with_report(&report, refused, also),
                // Under --json the recovery's outcome too, which no report shows there: its marker
                // is cleared, so no later run reports it.
                CoreError::Other(message) => CoreError::Other(
                    std::iter::once(message)
                        .chain(report.operation.iter().filter(|_| output::is_json()).flat_map(OperationRecovery::lines))
                        .chain(also).collect::<Vec<_>>().join("\n"),
                ),
            }.into());
        }
        return Err(CoreError::refusal_with_details(
            RefusalCode::InventoryDamaged,
            lines.join("\n"),
            "forklift heal --inventory",
            serde_json::json!(report),
        ).into());
    }

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

    if report.refs_refused.is_empty() && report.objects_lost.is_none() && origin_refused.is_none() {
        if let (Some(refused), Some(note)) = (merge_refusal, &merge_note) {
            let mut report = report;
            report.notes.retain(|kept| kept != note);
            if !output::is_json() {
                report.render_human();
            }
            return Err(with_report(&report, refused, Vec::new()).into());
        }
        output::emit(&report);
        return Ok(());
    }

    if !output::is_json() {
        report.render_human();
    }

    if let Some(lost) = &report.objects_lost {
        if let Some(refused) = remote_refused {
            return Err(with_report(&report, refused, objects_lost_lines(&report, lost)).into());
        }
        return Err(objects_lost_refusal(&report, lost, outcome.index_rebuild_skipped || origin_unanswered).into());
    }

    if report.refs_refused.is_empty() {
        if let Some(refused) = origin_refused {
            return Err(with_report(&report, refused, Vec::new()).into());
        }
    }

    Err(ref_refusal(&report, None).into())
}

/// Plan the inventory's rebuild for `marker`, and write it when it is complete or `accept_loss`.
///
/// # Returns
/// * `Ok((view, written))` - What the rebuild found, and whether it was written.
/// * `Err(String)`         - If the shard store could not be listed, or the set not written.
fn rebuild_inventory(marker: Option<&Operation>, accept_loss: bool) -> Result<(InventoryRebuilt, bool), String> {
    let rebuild = inventory_heal::plan(marker)?;
    let written = rebuild.complete() || accept_loss;
    if written {
        rebuild.write()?;
    }
    Ok((InventoryRebuilt::of(&rebuild), written))
}

/// A rebuild of the inventory's registered set of folders, as a `heal` report gives it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct InventoryRebuilt {
    /// How many folders it registers, found in the damaged file, the working tree, the tree the bay
    /// stands on or a tree an interrupted operation names, with their staging in the shard store,
    /// and their ancestors.
    registered: usize,

    /// How many shards in the store match none of those folders or their ancestors. Written, the
    /// rebuild leaves them unregistered, and whatever staging they hold is lost (a shard a removal
    /// left behind holds none).
    dropped: usize,

    /// Trees it proposed folders from and could not read: the folders only they name are left out.
    unread: Vec<TreeUnread>,

    /// Trees it could not read that this warehouse holds by hash alone. They hold nothing back, but
    /// a plain heal once the origin answers may find folders they name.
    passed_over: Vec<TreeUnread>,
}

impl InventoryRebuilt {
    fn of(rebuild: &inventory_heal::Rebuild) -> Self {
        InventoryRebuilt {
            registered: rebuild.recovered(),
            dropped: rebuild.unmatched,
            unread: rebuild.unread.clone(),
            passed_over: rebuild.passed_over.clone(),
        }
    }

    /// The sentence a written rebuild reports.
    fn sentence(&self) -> String {
        let mut sentence = format!(
            "The inventory's record of staged folders did not read and is rebuilt: {} folder(s) are registered.",
            self.registered
        );
        if self.dropped > 0 {
            sentence.push_str(&format!(
                " {} shard(s) matched no folder heal could find and are no longer registered, so whatever staging they hold is lost (a shard a removal left behind holds none).",
                self.dropped
            ));
        }
        for unread in &self.unread {
            sentence.push_str(&format!(
                " The {} tree {} could not be read, so the folders only it names were left out: {}.",
                unread.from, unread.tree, unread.reason.trim_end_matches('.')
            ));
        }
        sentence.push_str(&self.passed_over_sentence());
        sentence
    }

    fn passed_over_sentence(&self) -> String {
        let mut sentence = String::new();
        for over in &self.passed_over {
            sentence.push_str(&format!(
                " The {} tree {} was passed over: {}.",
                over.from, over.tree, over.reason.trim_end_matches('.')
            ));
        }
        if !self.passed_over.is_empty() {
            sentence.push_str(" A plain heal once the origin answers may find the folders it names.");
        }
        sentence
    }

    /// The lines of the `inventory_damaged` refusal of a run that held the rebuild.
    fn lines(&self) -> Vec<String> {
        let mut lines = vec![
            "The inventory's record of staged folders does not read, and heal did not rewrite it: the rebuild could not account for all the staging on disk.".to_string(),
        ];
        if self.dropped > 0 {
            lines.push(format!(
                "  {} shard(s) match no folder heal could find; rebuilt without them, whatever staging they hold is lost (a shard a removal left behind holds none).",
                self.dropped
            ));
        }
        for unread in &self.unread {
            lines.push(format!(
                "  The {} tree {} could not be read, so the folders only it names are not known: {}.",
                unread.from, unread.tree, unread.reason.trim_end_matches('.')
            ));
        }
        let over = self.passed_over_sentence();
        if !over.is_empty() {
            lines.push(format!("  {}", over.trim_start()));
        }
        lines
    }
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

    /// How many reachable objects have only a loose file whose bytes are not theirs. The files
    /// are kept where they are.
    corrupt: usize,

    /// Up to 20 of them, grouped by why each is lost.
    hashes: Vec<LostObject>,

    /// The pallets whose history reaches any of them.
    pallets: Vec<String>,

    /// Why `pallets` is missing or short, if the walk that finds them could not finish.
    #[serde(skip_serializing_if = "Option::is_none")]
    pallets_unknown: Option<String>,

    /// The pallets whose ref could not be read, so whether their history reaches any of them
    /// could not be told.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pallets_unread: Vec<UnreadPallet>,

    /// Whether only staged content reaches every one of them: loads never stacked, each in a
    /// checkout `staged` or `staged_elsewhere` names, which no backup is needed to mend.
    staged_only: bool,

    /// When `staged_only`, this checkout's staged paths whose content is among them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    staged: Vec<String>,

    /// When `staged_only`, the other checkouts (the main working tree, or a bay) staging content
    /// among them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    staged_elsewhere: Vec<String>,

    /// Sources the check could not read this run, so the counts may be short.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    degraded_sources: Vec<String>,

    /// Whether the pack folder holds an index of another pack format.
    #[serde(skip)]
    foreign_index: bool,
}

/// A pallet whose ref could not be read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct UnreadPallet {
    /// The pallet's wire name (`@<name>` for a metadata pallet).
    pallet: String,

    /// Why its ref could not be read.
    reason: String,
}

/// One lost object and why it is lost.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct LostObject {
    hash: String,

    /// "absent", "corrupt", or why the object could not be read.
    reason: String,
}

impl ObjectsLost {
    fn of(lost: &recovery_utils::LostObjects) -> ObjectsLost {
        let mut hashes: Vec<LostObject> = lost.absent.iter()
            .map(|hash| LostObject { hash: hash.clone(), reason: "absent".to_string() })
            .chain(lost.unreadable.iter().map(|(hash, reason)| LostObject { hash: hash.clone(), reason: reason.clone() }))
            .chain(lost.corrupt.keys().map(|hash| LostObject { hash: hash.clone(), reason: "corrupt".to_string() }))
            .collect();
        hashes.sort_by(|a, b| (&a.reason, &a.hash).cmp(&(&b.reason, &b.hash)));
        hashes.truncate(LOST_HASHES_LISTED);

        let all: std::collections::BTreeSet<String> =
            lost.absent.iter().chain(lost.unreadable.keys()).chain(lost.corrupt.keys()).cloned().collect();
        let (pallets, pallets_unread, pallets_unknown) = match recovery_utils::pallets_reaching(&all) {
            Ok(found) => (found.reaching, found.unread, None),
            Err(error) => (Vec::new(), Vec::new(), Some(error)),
        };
        // The read's own next step is left out: heal's line for that ref says what to do.
        let pallets_unread = pallets_unread.into_iter()
            .map(|(pallet, error)| UnreadPallet {
                pallet,
                reason: forklift_core::error::deframe(&error)
                    .map_or_else(|| error.clone(), |(_, message, _)| message.into_owned()),
            })
            .collect();
        // Where staged content is all that reaches them cannot be told, they read as history's.
        let corrupt: std::collections::BTreeSet<String> = lost.corrupt.keys().cloned().collect();
        let staged = recovery_utils::staged_only_losses(&all, &corrupt).unwrap_or_default();

        ObjectsLost {
            absent: lost.absent.len(),
            unreadable: lost.unreadable.len(),
            corrupt: lost.corrupt.len(),
            hashes,
            pallets,
            pallets_unknown,
            pallets_unread,
            staged_only: staged.all,
            staged: staged.here,
            staged_elsewhere: staged.elsewhere,
            degraded_sources: lost.degraded_sources.clone(),
            foreign_index: lost.foreign_index,
        }
    }

    /// The report's lines, for the refusal's message.
    fn lines(&self) -> Vec<String> {
        let total = self.absent + self.unreadable + self.corrupt;
        let mut lines = vec![format!(
            "{} object(s) that {} still reaches cannot be read from this store: {} absent, {} unreadable, {} corrupt.",
            total, if self.staged_only { "only staged content" } else { "history" }, self.absent, self.unreadable, self.corrupt
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
            (None, true) if self.staged_only => {}
            // A pallet whose ref could not be read may be the one that reaches them.
            (None, true) if !self.pallets_unread.is_empty() => {}
            (None, true) => lines.push(
                "No pallet's history reaches them; other refs (staged content, parked work, a bay's operation, a tombstone) do."
                    .to_string(),
            ),
        }
        for unread in &self.pallets_unread {
            lines.push(format!(
                "Whether the history of pallet \"{}\" reaches them could not be told: {}",
                unread.pallet, unread.reason
            ));
        }
        if !self.staged.is_empty() {
            let listed: Vec<&str> = self.staged.iter().take(LOST_HASHES_LISTED).map(String::as_str).collect();
            lines.push(format!(
                "Staged in this checkout, with nothing else reaching that content: {}{}.",
                listed.join(", "),
                match self.staged.len() - listed.len() {
                    0 => String::new(),
                    more => format!(", and {} more", more),
                }
            ));
        }
        if self.staged_only && self.corrupt > 0 {
            lines.push(
                "Some of that content is held by a damaged file, which loading the same content again keeps, so that load reads as lost again."
                    .to_string(),
            );
        }
        if !self.staged_elsewhere.is_empty() {
            lines.push(format!(
                "Staged, with nothing else reaching that content, in another checkout: {}.",
                self.staged_elsewhere.join(", ")
            ));
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
    let lines = objects_lost_lines(report, lost);

    let next_step = if lost.foreign_index {
        "re-create the warehouse with this build of Forklift: the pack folder holds an index another build wrote".to_string()
    } else if rebuild_skipped {
        "forklift heal".to_string()
    } else if lost.staged_only && !lost.staged.is_empty() {
        let unload = unload_step_when(&lost.staged, cfg!(windows));
        match lost.staged_elsewhere.is_empty() {
            true => unload,
            false => format!("{}; then, from each other checkout named above, run \"forklift heal\"", unload),
        }
    } else if lost.staged_only {
        "from each checkout named above, run \"forklift heal\"".to_string()
    } else {
        "restore the warehouse from a backup, or clone it afresh from a remote that holds these objects".to_string()
    };

    CoreError::refusal_with_details(
        RefusalCode::ObjectsLost,
        lines.join("\n"),
        next_step,
        serde_json::json!(report),
    )
}

/// The next step for lost content only staged paths of this checkout reach, as the platform
/// `windows` quotes them: unload each, which unstages it and leaves its file as it is, and load
/// again what is meant to be staged. The paths are warehouse-root-relative, so the commands run
/// from the working tree's root, each path one argument ([`tree_utils::remedy_path_when`]), or
/// every one listed behind a placeholder step when any cannot be quoted there. More paths than the
/// refusal's lines list get the placeholder step alone, pointing at the report that lists them.
fn unload_step_when(paths: &[String], windows: bool) -> String {
    // Past the lines' cap the paths are not spelled out again: the report names them all.
    if paths.len() > LOST_HASHES_LISTED {
        return format!(
            "from the working tree's root, run \"forklift unload -- <path>\" for each of the {} staged paths, \
            with the path quoted for your shell, then load again what you mean to stage; \"forklift --json heal\" \
            lists them all in `staged`",
            paths.len()
        );
    }
    let arguments: Option<Vec<String>> = paths.iter().map(|path| tree_utils::remedy_path_when(path, windows)).collect();
    match (arguments, paths) {
        (Some(arguments), [_]) => format!(
            "from the working tree's root, run \"forklift unload {}\", then load the file again if you \
            mean to stage it",
            arguments[0]
        ),
        (Some(arguments), _) => format!(
            "from the working tree's root, run each of these, then load again what you mean to stage: {}",
            arguments.iter().map(|argument| format!("\"forklift unload {}\"", argument)).collect::<Vec<_>>().join(", ")
        ),
        (None, _) => format!(
            "from the working tree's root, run \"forklift unload -- <path>\" for each of these paths, with \
            the path quoted for your shell, then load again what you mean to stage: {}",
            paths.iter().map(|path| format!("\"{}\"", path)).collect::<Vec<_>>().join(", ")
        ),
    }
}

/// The lines of an `objects_lost` refusal: what was lost, then a damaged ref left standing.
fn objects_lost_lines(report: &HealReport, lost: &ObjectsLost) -> Vec<String> {
    let mut lines = lost.lines();
    if !report.refs_refused.is_empty() {
        if let CoreError::Refusal { message, .. } = ref_refusal(report, None) {
            lines.push(message);
        }
    }
    lines
}

/// A refusal the origin's record or the origin itself made, as the run's own, followed by `also`,
/// what else the run leaves standing, and carrying the whole report. Details of its own, which its
/// remedy may read, are kept, with each report field they do not name added beside them. This
/// run's repairs stand, so its "nothing was changed" is dropped.
fn with_report(report: &HealReport, refused: CoreError, also: Vec<String>) -> CoreError {
    let CoreError::Refusal { code, message, next_step, details } = refused else {
        return refused;
    };

    let mut lines = vec![match message.strip_suffix("; nothing was changed.") {
        Some(described) => format!("{}.", described),
        None => message.strip_suffix(" Nothing was changed.").map(str::to_string).unwrap_or(message),
    }];
    lines.extend(also);

    let report = serde_json::json!(report);
    let details = match details {
        Some(serde_json::Value::Object(mut own)) => {
            if let serde_json::Value::Object(fields) = report {
                for (field, value) in fields {
                    own.entry(field).or_insert(value);
                }
            }
            serde_json::Value::Object(own)
        }
        Some(other) => other,
        None => report,
    };

    CoreError::refusal_with_details(code, lines.join("\n"), next_step, details)
}

/// The `ref_damaged` refusal of a run that left a damaged ref standing, carrying the whole report.
fn ref_refusal(report: &HealReport, unfinished: Option<&str>) -> CoreError {
    let mut lines = refusal_lines(&report.refs_refused);

    if let Some(unfinished) = unfinished {
        lines.push(format!("{}.", unfinished.trim_end_matches('.')));
    }

    // A meta ref, which --ref does not set, has its own step (ref_heal::meta_step). The office's
    // and @redactions' come first, since a user ref's rebuild, --ref's among them, is verified
    // against the one and checked against the other; any other meta ref's waits behind a user
    // ref's --ref.
    let gating = [forklift_core::util::office_utils::OFFICE_PALLET_NAME, forklift_core::util::redaction_utils::REDACTIONS_PALLET_NAME]
        .map(|name| format!("@{}", name));
    let meta_step = |refused: &ref_heal::Refused| ref_heal::meta_step(refused, &report.refs_refused);
    let gate = gating.iter().find_map(|wire| report.refs_refused.iter().find(|refused| refused.pallet == *wire));
    let next_step = match (gate, report.refs_refused.iter().find(|refused| !refused.pallet.starts_with('@'))) {
        (Some(gate), _) => meta_step(gate),
        (None, Some(user)) => format!("forklift heal --ref {}=<parcel>", user.pallet),
        (None, None) => report.refs_refused.first().map(meta_step).unwrap_or_else(|| "forklift heal".to_string()),
    };

    CoreError::refusal_with_details(
        RefusalCode::RefDamaged,
        lines.join("\n"),
        next_step,
        serde_json::json!(report),
    )
}

/// Each refused ref's line, then its candidate heads, which the hand step writes.
fn refusal_lines(all: &[ref_heal::Refused]) -> Vec<String> {
    let mut lines = Vec::new();

    for refused in all {
        lines.push(format!("The ref of pallet \"{}\" was not rebuilt: {}.", refused.pallet, refused.reason));

        if refused.candidates.is_empty() {
            lines.push("  Nothing names a head for it.".to_string());
        }
        for candidate in &refused.candidates {
            lines.push(format!("  {} ({})", candidate.head, candidate.source));
        }

        // What its removal costs, where that is its way out.
        if refused.meta.is_some() && ref_heal::meta_step(refused, all) == ref_heal::removal_step(&refused.pallet) {
            lines.push(
                "  Its removal leaves the pallet empty: what only its history reached may then be freed, and the removed files' bytes \
                are kept under removed-refs.".to_string()
            );
        }
    }

    lines
}

/// `error`, with a sentence for every ref refused before the pass stopped, naming its candidate
/// heads inline, since the message is one paragraph.
fn refused_too<'a>(error: String, refused: impl IntoIterator<Item = &'a ref_heal::Refused>) -> String {
    refused.into_iter().fold(error, |error, refused| {
        forklift_core::error::append_to_message(error, &format!(
            "The ref of pallet \"{}\" was not rebuilt: {}; {}.", refused.pallet, refused.reason.trim_end_matches('.'), candidates_clause(&refused.candidates)
        ))
    })
}

/// The heads found for a ref heal did not rebuild, in words.
fn candidates_clause(candidates: &[ref_heal::Candidate]) -> String {
    match candidates.is_empty() {
        true => "nothing names a head for it".to_string(),
        false => format!("candidates: {}", candidates.iter()
            .map(|candidate| format!("{} ({})", candidate.head, candidate.source)).collect::<Vec<_>>().join(", ")),
    }
}

/// The damaged refs a refusal names: those left for the meta pass after the taint recovery, until
/// it has run, then those refused; and the meta refs a pass that stopped never judged.
struct RefsLeft {
    deferred: Vec<ref_heal::Refused>,
    refused: Vec<ref_heal::Refused>,
    unjudged: Vec<String>,
    /// Which of `deferred` the taint held ([`ref_heal::Repair::held`]).
    held: Option<ref_heal::TaintHold>,
}

impl RefsLeft {
    /// After the meta pass after the taint, which refused `refused` and, stopping, never judged
    /// `unjudged`: one of those the pass before it deferred keeps that pass's heads, and its
    /// reason, said as judged before the taint recovery. The recovery that ran before this pass
    /// resolved the taint, so it holds none of them.
    fn after_second(deferred: &[ref_heal::Refused], refused: Vec<ref_heal::Refused>, unjudged: Vec<String>) -> RefsLeft {
        let (deferred, unjudged) = unjudged.into_iter().fold((Vec::new(), Vec::new()), |(mut kept, mut rest), wire| {
            match deferred.iter().find(|refused| refused.pallet == wire) {
                Some(first) => kept.push(ref_heal::Refused {
                    reason: format!("before the taint recovery, {}", first.reason), ..first.clone()
                }),
                None => rest.push(wire),
            }
            (kept, rest)
        });
        RefsLeft { deferred, refused, unjudged, held: None }
    }
}

/// Restore the checkout's scope from its copy, or set it to the paths `--scope` named.
fn heal_checkout_scope(scope: &[String], base: Base, notes: &mut Vec<String>) -> Result<Option<ScopeRestored>, CoreError> {
    match scope.is_empty() {
        true => checkout_scope_heal::repair(base, notes),
        false => checkout_scope_heal::set_chosen(scope, base).map(Some),
    }
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
        if let Some(way_out) = &unread.way_out {
            sentence.push(' ');
            sentence.push_str(way_out);
        }
    }

    sentence
}

/// The sentences a refusal carries for the meta refs `deferred` that the pass before the taint
/// recovery left for the pass after it, which the run did not reach; none for none. A ref `held`
/// names says why a later heal does not settle it, or what it waits on.
fn deferred_sentences(deferred: &[ref_heal::Refused], held: Option<&ref_heal::TaintHold>) -> Vec<String> {
    let is_held = |refused: &ref_heal::Refused| held.is_some_and(|held| held.refs.contains(&refused.pallet));
    let mut sentences: Vec<String> = deferred.iter().map(|refused| {
        let line = format!(
            "The ref of pallet \"{}\" was not rebuilt yet: {}; {}.",
            refused.pallet, refused.reason.trim_end_matches('.'), candidates_clause(&refused.candidates)
        );
        match held.filter(|_| is_held(refused)) {
            None => line,
            Some(held) if held.unread => format!(
                "{} The durability taint could not be read, so every object the ref's bytes name counted as possibly lost; \
                a later heal that can read the taint judges it again.", line
            ),
            // The rescan of a torn taint reads every ref, so it cannot finish past one left damaged.
            // The hand step needs a head to write: with none named, the ref waits on its own.
            Some(_) => format!(
                "{} The durability taint's record is torn, so every object the ref's bytes name counts as possibly lost, \
                and a later heal leaves it again{}", line, match refused.candidates.is_empty() {
                    true => ".",
                    false => "; a head named, once checked to be one this store holds, can be written into its file by hand, \
                        which heal does not verify.",
                }
            ),
        }
    }).collect();
    let rest = deferred.iter().filter(|refused| !is_held(refused)).count();
    let other = if rest < deferred.len() { "the other " } else { "" };
    match rest {
        0 => {}
        1 => sentences.push(format!("This run stopped before judging {}{} a second time; a later heal judges it again.",
            other, if other.is_empty() { "that ref" } else { "ref" })),
        _ => sentences.push(format!("This run stopped before judging {}{} a second time; a later heal judges them again.",
            other, if other.is_empty() { "those refs" } else { "refs" })),
    }
    sentences
}

/// The sentences a refusal carries for the lost or damaged meta refs `unjudged`, which a meta pass
/// stopped before judging.
fn unjudged_sentences(unjudged: &[String]) -> Vec<String> {
    unjudged.iter().map(|pallet| format!("The ref of pallet \"{}\" is lost or damaged; this run stopped before judging it.", pallet)).collect()
}

/// The refusal for the rebuilds a ref pass left not durable ([`ref_heal::Repair::undurable`]), or
/// `None` when it left none.
fn undurable_error(undurable: &[String]) -> Option<String> {
    (!undurable.is_empty()).then(|| format!("{} Heal went no further.", undurable.join(" ")))
}

/// Why a ref pass ends the run: where it `stopped`, and what it left `undurable` beside that.
fn stopped_with(stopped: Option<String>, undurable: &[String]) -> Option<String> {
    match stopped {
        Some(stopped) => Some(undurable.iter().fold(stopped, |error, sentence| forklift_core::error::append_to_message(error, sentence))),
        None => undurable_error(undurable),
    }
}

fn rebuilt_sentence(rebuilt: &ref_heal::Rebuilt) -> String {
    format!("Rebuilt the ref of pallet \"{}\" at {}, from {}.", rebuilt.pallet, rebuilt.head, rebuilt.source)
}

fn removed_sentence(removed: &ref_heal::Removed) -> String {
    let ref_said = match &removed.kept_ref {
        Some(kept) => format!("the damaged ref of pallet \"{}\", keeping its bytes in \"{}\"", removed.pallet, kept),
        None => format!("the lost ref of pallet \"{}\"", removed.pallet),
    };
    match removed.kind {
        ref_heal::RemovedKind::Meta => format!(
            "Removed {}{}, which nothing names a head for. The pallet now reads empty, and what only it reached is no longer held by any pallet.",
            ref_said,
            match &removed.kept_copy {
                Some(kept) => format!(", and its copy, keeping the copy's bytes in \"{}\"", kept),
                None => String::new(),
            }
        ),
        ref_heal::RemovedKind::User => format!(
            "Removed the unreadable ref of pallet \"{}\", keeping its bytes in \"{}\"; what only it reached is no longer held by any pallet.",
            removed.pallet, removed.kept_ref.as_deref().unwrap_or_default()
        ),
    }
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

    /// What this run kept of a damaged record of content this warehouse cannot get, which it
    /// rebuilt from the entries that still read whole, and where it kept a copy of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    unavailable_rebuilt: Option<UnavailableRebuilt>,

    /// What this run kept of a damaged record of applied redactions, which it rebuilt from the
    /// hashes that still read whole and that a redaction names, and where it kept a copy of it.
    #[serde(skip_serializing_if = "Option::is_none")]
    redactions_applied_rebuilt: Option<AppliedRebuilt>,

    /// The scope of the checkout this run healed, which was lost or damaged and this run wrote:
    /// restored from the copy beside it, or set to the paths `--scope` gave.
    #[serde(skip_serializing_if = "Option::is_none")]
    checkout_scope_restored: Option<ScopeRestored>,

    /// The records of the merge in progress in the checkout this run healed that were damaged or
    /// missing and this run wrote back from the copy beside each, or removed with it under an
    /// interrupted merge or pick whose recovery clears them.
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_state_restored: Option<MergeStateRestored>,

    /// The fetch scope of a sparse warehouse, which was lost or damaged and this run wrote: rebuilt
    /// from the checkouts' scopes, or set to the paths `--fetch-scope` gave.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope_rebuilt: Option<ScopeRebuilt>,

    /// The parked-work record, which was missing, damaged or incomplete and this run rebuilt from
    /// the entries that still read, the bays' own leftovers and the parked parcels the store holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    parked_rebuilt: Option<ParkedRebuilt>,

    /// The bay's position, which was lost or damaged and this run wrote: from its copy, from an
    /// operation left unfinished, or on the pallet `--position` named.
    #[serde(skip_serializing_if = "Option::is_none")]
    position_rebuilt: Option<PositionRebuilt>,

    /// The warehouse's configuration file, which was damaged and this run rewrote, first of all:
    /// what it dropped (by line and key, never by value) and where the damaged bytes went.
    #[serde(skip_serializing_if = "Option::is_none")]
    config_repaired: Option<config_utils::ConfigRepaired>,

    /// The trust file or its copy, which was missing or did not hold the anchor and this run wrote
    /// from the other, before anything read the anchor.
    #[serde(skip_serializing_if = "Option::is_none")]
    trust_restored: Option<office_utils::TrustRestored>,

    /// The inventory's registered set of folders, which did not read and this run rebuilt from the
    /// folders it could find whose staging the shard store holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory_rebuilt: Option<InventoryRebuilt>,

    /// The same rebuild, held and not written: it left staging unmatched or could not read a tree
    /// it proposed folders from. The run refuses `inventory_damaged` with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    inventory_held: Option<InventoryRebuilt>,

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

    /// Absent objects of a cache warehouse whose sealed-object records were lost, which this run
    /// recorded again as held by the origin: a local walk of the history the origin holds names
    /// each, so a read fetches it from there again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    resealed: Vec<String>,

    /// Absent user file content history still reaches that the record of content this warehouse
    /// cannot get did not hold, which this run recorded in it again: the remote's host withholds
    /// each, so a checkout or an export leaves it out, as after the fetch. Its entry was lost, or
    /// this warehouse held a copy and lost it; heal cannot tell which.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    recorded_unavailable: Vec<String>,

    /// Absent user file content history still reaches that a redaction names, whose hash the record
    /// of applied redactions did not hold, and which this run applied again: a read of it is refused
    /// as redacted and an export leaves it out. A lost or damaged record dropped its hash.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redactions_reapplied: Vec<String>,

    /// Paths this checkout's inventory lists whose content the store had lost, which this run stored
    /// again from the working tree, where the file still holds that content.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    restored_from_working_tree: Vec<String>,

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

/// What `heal` kept of a damaged record of content this warehouse cannot get.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct UnavailableRebuilt {
    /// Where the damaged record was copied to.
    set_aside: String,

    /// How many entries still read whole and were kept.
    kept: usize,

    /// How many lines did not read whole and were dropped.
    dropped: usize,
}

impl From<unavailable::Rebuilt> for UnavailableRebuilt {
    fn from(rebuilt: unavailable::Rebuilt) -> Self {
        UnavailableRebuilt {
            set_aside: rebuilt.set_aside.to_string_lossy().into_owned(),
            kept: rebuilt.kept,
            dropped: rebuilt.dropped,
        }
    }
}

impl UnavailableRebuilt {
    fn sentence(&self) -> String {
        format!(
            "The record of content this warehouse cannot get was damaged and is rebuilt from the {} \
            entr{} that still read whole; {} line(s) that did not are dropped, and a copy of it is kept \
            at \"{}\".",
            self.kept, if self.kept == 1 { "y" } else { "ies" }, self.dropped, self.set_aside
        )
    }
}

/// What `heal` kept of a damaged record of applied redactions.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct AppliedRebuilt {
    /// Where the damaged record was copied to.
    set_aside: String,

    /// How many hashes still read whole and were kept.
    kept: usize,

    /// How many lines were dropped: they did not read whole, or named a hash no redaction names.
    dropped: usize,
}

impl From<applied_redactions::Rebuilt> for AppliedRebuilt {
    fn from(rebuilt: applied_redactions::Rebuilt) -> Self {
        AppliedRebuilt {
            set_aside: rebuilt.set_aside.to_string_lossy().into_owned(),
            kept: rebuilt.kept,
            dropped: rebuilt.dropped,
        }
    }
}

impl AppliedRebuilt {
    fn sentence(&self) -> String {
        format!(
            "The record of applied redactions was damaged and is rebuilt from the {} hash(es) that \
            still read whole and that a redaction names; {} line(s) that did not are dropped, and a \
            copy of it is kept at \"{}\".",
            self.kept, self.dropped, self.set_aside
        )
    }
}

/// What the parked-work record was before `heal` rebuilt it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ParkedRecordWas {
    /// There was none.
    Missing,
    /// It did not read.
    Damaged,
    /// It read whole, but a bay still held parked work of its own.
    Whole,
}

/// What `heal` kept of a missing, damaged or incomplete parked-work record.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ParkedRebuilt {
    /// What the record was: `missing`, `damaged`, or `whole` when it read but a bay still held
    /// parked work of its own.
    record: ParkedRecordWas,

    /// Where the damaged record was copied to; absent when it was missing or read whole.
    #[serde(skip_serializing_if = "Option::is_none")]
    set_aside: Option<String>,

    /// How many entries still read whole, from the record and the bays' leftovers, and were kept.
    kept: usize,

    /// How many parked parcels the store held that no kept entry named, listed again as
    /// recovered: work popped or dropped before the damage comes back this way too.
    recovered: usize,

    /// How many lines were dropped: they did not read whole, or named no parcel the store holds.
    dropped: usize,

    /// How many objects or pack indexes the store scan could not read, and skipped.
    unreadable: usize,

    /// How many bays' own parked files were merged in and removed.
    merged_bays: usize,
}

impl From<park_utils::Rebuilt> for ParkedRebuilt {
    fn from(rebuilt: park_utils::Rebuilt) -> Self {
        ParkedRebuilt {
            record: match rebuilt.record {
                park_utils::RecordWas::Missing => ParkedRecordWas::Missing,
                park_utils::RecordWas::Damaged => ParkedRecordWas::Damaged,
                park_utils::RecordWas::Whole => ParkedRecordWas::Whole,
            },
            set_aside: rebuilt.set_aside.map(|path| path.to_string_lossy().into_owned()),
            kept: rebuilt.kept,
            recovered: rebuilt.recovered,
            dropped: rebuilt.dropped,
            unreadable: rebuilt.unreadable,
            merged_bays: rebuilt.merged_bays,
        }
    }
}

impl ParkedRebuilt {
    fn sentence(&self) -> String {
        let mut sentence = format!(
            "The parked-work record {} and is rebuilt: {} entr(ies) kept, {} parked parcel(s) found \
            in the store listed again as recovered, {} line(s) dropped",
            match self.record {
                ParkedRecordWas::Missing => "was missing",
                ParkedRecordWas::Damaged => "was damaged",
                ParkedRecordWas::Whole => "read, but a bay still held parked work of its own,",
            },
            self.kept, self.recovered, self.dropped
        );
        if self.merged_bays > 0 {
            sentence.push_str(&format!(", {} bay(s)' own parked work merged in", self.merged_bays));
        }
        if self.unreadable > 0 {
            sentence.push_str(&format!(", {} object(s) or pack index(es) that did not read skipped", self.unreadable));
        }
        match &self.set_aside {
            Some(path) => sentence.push_str(&format!("; a copy of it is kept at \"{}\".", path)),
            None => sentence.push('.'),
        }
        sentence
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

impl HealReport {
    /// Whether the run found and did nothing: no repair, recovery, note or refusal to report.
    fn nothing_done(&self) -> bool {
        self.operation.is_none() && self.unsealed.is_empty() && self.newly_held_by_hash.is_empty()
            && self.resealed.is_empty() && self.recorded_unavailable.is_empty() && self.notes.is_empty()
            && self.unseal_failed.is_none() && self.record_rebuilt.is_none() && self.unavailable_rebuilt.is_none()
            && self.redactions_applied_rebuilt.is_none() && self.redactions_reapplied.is_empty()
            && self.restored_from_working_tree.is_empty()
            && self.checkout_scope_restored.is_none() && self.merge_state_restored.is_none()
            && self.fetch_scope_rebuilt.is_none() && self.parked_rebuilt.is_none() && self.position_rebuilt.is_none()
            && self.config_repaired.is_none() && self.trust_restored.is_none()
            && self.inventory_rebuilt.is_none() && self.inventory_held.is_none()
            && self.refs_rebuilt.is_empty() && self.refs_removed.is_empty() && self.refs_refused.is_empty()
            && self.objects_lost.is_none()
    }
}

impl CommandOutput for HealReport {
    fn render_human(&self) {
        if let Some(rebuilt) = &self.record_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(rebuilt) = &self.unavailable_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(rebuilt) = &self.redactions_applied_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(restored) = &self.checkout_scope_restored {
            println!("{}", restored.sentence());
        }

        for sentence in self.merge_state_restored.iter().flat_map(MergeStateRestored::sentences) {
            println!("{}", sentence);
        }

        if let Some(rebuilt) = &self.fetch_scope_rebuilt {
            println!("{}", scope_sentence(rebuilt));
        }

        if let Some(rebuilt) = &self.parked_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(rebuilt) = &self.position_rebuilt {
            println!("{}", rebuilt.sentence());
        }

        if let Some(repaired) = &self.config_repaired {
            for sentence in repaired.sentences() {
                println!("{}", sentence);
            }
        }

        if let Some(restored) = &self.trust_restored {
            println!("{}", restored.sentence());
        }

        if let Some(rebuilt) = &self.inventory_rebuilt {
            println!("{}", rebuilt.sentence());
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
        } else if self.nothing_done() {
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

        if !self.resealed.is_empty() {
            println!(
                "{} absent object(s) whose sealed-object records were lost are recorded again as held \
                by the origin, which a read fetches them from:",
                self.resealed.len()
            );
            for hash in &self.resealed {
                println!("  {}", hash);
            }
        }

        if !self.recorded_unavailable.is_empty() {
            println!(
                "{} absent object(s) the remote's host withholds are recorded unavailable again, so a \
                checkout or an export leaves them out:",
                self.recorded_unavailable.len()
            );
            for hash in &self.recorded_unavailable {
                println!("  {}", hash);
            }
            println!("If this warehouse held a copy of one, restoring it from a backup brings the content back.");
        }

        if !self.restored_from_working_tree.is_empty() {
            println!(
                "{} path(s) whose content the store had lost are stored again from the working tree:",
                self.restored_from_working_tree.len()
            );
            for path in &self.restored_from_working_tree {
                println!("  {}", path);
            }
        }

        if !self.redactions_reapplied.is_empty() {
            println!(
                "{} absent object(s) a redaction names are applied again, so a read of one is refused \
                as redacted and an export leaves it out:",
                self.redactions_reapplied.len()
            );
            for hash in &self.redactions_reapplied {
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
    /// The recovery as report lines: its outcome, then the files it left unheld.
    fn lines(&self) -> Vec<String> {
        let at = match &self.base {
            Some(base) => format!("\"{}\" at {}", self.pallet, base),
            None => format!("\"{}\" (unborn)", self.pallet),
        };

        let mut lines = vec![match self.outcome {
            OperationOutcome::Finished => format!("The interrupted {} had already finished; the bay stands on {}.", self.operation, at),
            OperationOutcome::RolledForward => format!("Finished the interrupted {}; the bay stands on {}.", self.operation, at),
            OperationOutcome::RolledBack => format!("Undid the interrupted {}; the bay is back on {}.", self.operation, at),
            OperationOutcome::Cleared => format!("The interrupted {} had changed nothing to undo; its record is cleared.", self.operation),
        }];

        if !self.unheld.is_empty() {
            lines.push("These tracked files do not hold their recorded content and show as local changes (\"forklift restore --confirm <path>\" brings one back):".to_string());
            lines.extend(self.unheld.iter().map(|path| format!("  {}", path)));
        }
        lines
    }

    fn render_human(&self) {
        for line in self.lines() {
            println!("{}", line);
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

#[cfg(test)]
mod tests {
    use super::{deferred_sentences, ref_heal, unjudged_sentences, unload_step_when, LostObject, ObjectsLost, UnreadPallet};

    // A pallet whose ref could not be read may be the one whose history reaches the lost
    // objects, so the report names it and never says no pallet's history reaches them.
    // Mutation: print "No pallet's history reaches them" whenever no pallet reaches them → red.
    #[test]
    fn a_lost_objects_report_names_a_ref_it_could_not_read_and_never_says_nothing_reaches_them() {
        let report = |unread: Vec<UnreadPallet>| ObjectsLost {
            absent: 1,
            unreadable: 0,
            corrupt: 0,
            hashes: vec![LostObject { hash: "a".repeat(64), reason: "absent".to_string() }],
            pallets: Vec::new(),
            pallets_unknown: None,
            pallets_unread: unread,
            staged_only: false,
            staged: Vec::new(),
            staged_elsewhere: Vec::new(),
            degraded_sources: Vec::new(),
            foreign_index: false,
        }.lines();
        let nothing = "No pallet's history reaches them; other refs (staged content, parked work, a bay's operation, a tombstone) do.";

        let lines = report(vec![UnreadPallet { pallet: "@tags".to_string(), reason: "its ref is lost.".to_string() }]);
        assert!(lines.contains(&"Whether the history of pallet \"@tags\" reaches them could not be told: its ref is lost.".to_string()), "{:?}", lines);
        assert!(!lines.iter().any(|line| line.contains("No pallet's history reaches them")), "{:?}", lines);

        // With every ref read, the line stands.
        assert!(report(Vec::new()).contains(&nothing.to_string()));
    }

    #[test]
    fn a_refusal_names_each_meta_ref_left_for_after_the_taint_with_its_heads() {
        let refused = |pallet: &str, candidates: &[(&str, &str)]| ref_heal::Refused {
            pallet: pallet.to_string(),
            reason: "it waits.".to_string(),
            candidates: candidates.iter().map(|(head, source)| ref_heal::Candidate { head: head.to_string(), source: source.to_string() }).collect(),
            meta: None,
        };
        let held = |unread: bool| ref_heal::TaintHold { unread, refs: vec!["@tags".to_string()] };
        assert!(deferred_sentences(&[], None).is_empty());
        assert_eq!(deferred_sentences(&[refused("@tags", &[("h1", "the ref's copy")])], None), vec![
            "The ref of pallet \"@tags\" was not rebuilt yet: it waits; candidates: h1 (the ref's copy).".to_string(),
            "This run stopped before judging that ref a second time; a later heal judges it again.".to_string(),
        ]);
        let two = deferred_sentences(&[refused("@kv", &[]), refused("@tags", &[("h1", "the ref's copy"), ("h2", "what the ref's own bytes still name")])], None);
        assert_eq!(two, vec![
            "The ref of pallet \"@kv\" was not rebuilt yet: it waits; nothing names a head for it.".to_string(),
            "The ref of pallet \"@tags\" was not rebuilt yet: it waits; candidates: h1 (the ref's copy), h2 (what the ref's own \
            bytes still name).".to_string(),
            "This run stopped before judging those refs a second time; a later heal judges them again.".to_string(),
        ]);
        // Beside a torn taint only the ref it holds gets the hand step; the other waits as before.
        assert_eq!(deferred_sentences(&[refused("@kv", &[]), refused("@tags", &[("h1", "the ref's copy")])], Some(&held(false))), vec![
            "The ref of pallet \"@kv\" was not rebuilt yet: it waits; nothing names a head for it.".to_string(),
            "The ref of pallet \"@tags\" was not rebuilt yet: it waits; candidates: h1 (the ref's copy). The durability taint's \
            record is torn, so every object the ref's bytes name counts as possibly lost, and a later heal leaves it again; a head \
            named, once checked to be one this store holds, can be written into its file by hand, which heal does not verify.".to_string(),
            "This run stopped before judging the other ref a second time; a later heal judges it again.".to_string(),
        ]);
        // With no head named, there is nothing to write by hand.
        assert_eq!(deferred_sentences(&[refused("@tags", &[])], Some(&held(false))), vec![
            "The ref of pallet \"@tags\" was not rebuilt yet: it waits; nothing names a head for it. The durability taint's \
            record is torn, so every object the ref's bytes name counts as possibly lost, and a later heal leaves it again.".to_string(),
        ]);
        assert_eq!(deferred_sentences(&[refused("@tags", &[("h1", "the ref's copy")])], Some(&held(true))), vec![
            "The ref of pallet \"@tags\" was not rebuilt yet: it waits; candidates: h1 (the ref's copy). The durability taint \
            could not be read, so every object the ref's bytes name counted as possibly lost; a later heal that can read the \
            taint judges it again.".to_string(),
        ]);
        assert_eq!(unjudged_sentences(&["@tags".to_string()]), vec![
            "The ref of pallet \"@tags\" is lost or damaged; this run stopped before judging it.".to_string(),
        ]);
    }

    #[test]
    fn the_unload_step_quotes_each_path_or_lists_them_behind_a_placeholder() {
        // One path is one quoted argument; several get a command each; a path no Windows shell
        // reads as one literal argument puts every path behind the placeholder step.
        // Mutation: quote with the other platform's rule, or keep only the first path → red.
        let one = |path: &str, windows: bool| unload_step_when(&[path.to_string()], windows);
        assert_eq!(
            one("my docs/a.txt", false),
            "from the working tree's root, run \"forklift unload 'my docs/a.txt'\", then load the file again if you \
            mean to stage it"
        );
        assert_eq!(
            one("-a.txt", false),
            "from the working tree's root, run \"forklift unload ./-a.txt\", then load the file again if you mean \
            to stage it"
        );

        let paths = ["a.txt".to_string(), "b c.txt".to_string()];
        assert_eq!(
            unload_step_when(&paths, false),
            "from the working tree's root, run each of these, then load again what you mean to stage: \"forklift \
            unload a.txt\", \"forklift unload 'b c.txt'\""
        );
        assert_eq!(
            unload_step_when(&paths, true),
            "from the working tree's root, run \"forklift unload -- <path>\" for each of these paths, with the path \
            quoted for your shell, then load again what you mean to stage: \"a.txt\", \"b c.txt\""
        );

        // Past the refusal's cap, the step points at the report instead of spelling every path.
        // Mutation: drop the cap → red.
        let many: Vec<String> = (0..=super::LOST_HASHES_LISTED).map(|n| format!("f{}.txt", n)).collect();
        assert_eq!(
            unload_step_when(&many, false),
            format!(
                "from the working tree's root, run \"forklift unload -- <path>\" for each of the {} staged paths, with \
                the path quoted for your shell, then load again what you mean to stage; \"forklift --json heal\" lists \
                them all in `staged`",
                many.len()
            )
        );
    }
}
