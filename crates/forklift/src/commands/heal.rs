use serde::Serialize;
use forklift_core::util::lock_utils::StoreAccessLock;
use forklift_core::util::operation_utils::{self, RecoveryOutcome};
use forklift_core::util::{recovery_utils, sealed_utils};
use crate::output::{self, CommandOutput};

/// Handle the `heal` command: run the durability-taint recovery verb
/// (`forklift_core::util::recovery_utils::run`), then settle an operation interrupted in this
/// bay before it finished, if one is recorded (`forklift_core::util::operation_utils::recover`),
/// then clear the sealed-object record of every object the store holds again
/// (`forklift_core::util::sealed_utils::unseal`).
///
/// No arguments — v1 keeps the surface minimal. See `recovery_utils`'s module doc comment for
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
///
/// # Returns
/// * `Ok(())`      - Nothing was tainted, or the taint is now fully cleared.
/// * `Err(String)` - A `durability_taint` refusal (torn, or an unresolved dangling reference), or
///                   the sealed-object record could not be written.
pub async fn handle_command(store_access: &StoreAccessLock) -> Result<(), String> {
    // A torn taint's rescan (§8.3) can take a while on a large or uncompacted store — it reports
    // per-phase/per-count progress through this callback rather than staying silent. `human!` is
    // itself a no-op under `--json` (progress is never part of the result document), and
    // `forklift-core` never prints on its own (DESIGN.html §3.4) — this closure is the one bridge.
    let outcome = match recovery_utils::run(Some(&|line: &str| crate::human!("{}", line))).await {
        Ok(outcome) => outcome,
        Err(error) => {
            // An interrupted operation waits for the taint (below), so the refusal says so: it
            // is not the only thing left standing.
            let error = String::from(error);
            return Err(match operation_utils::read() {
                Ok(Some(operation)) => forklift_core::error::append_to_message(error, &format!(
                    "An interrupted {} also stands in this bay; heal finishes or undoes it once the \
                    taint is resolved.", operation.kind.as_str(),
                )),
                _ => error,
            });
        }
    };

    // After the taint: an operation's recovery reads and writes the store the taint vouches for.
    let operation = operation_utils::recover()?.map(|recovery| OperationRecovery {
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
    let unsealed = unseal_present(store_access, &mut notes)?;

    output::emit(&HealReport {
        was_tainted: outcome.was_tainted,
        restaged: outcome.restaged,
        resolved: outcome.resolved,
        notes,
        operation,
        unsealed: unsealed.removed,
        newly_held_by_hash: unsealed.pushed_down,
    });

    Ok(())
}

/// Remove the sealed-object record of every object the store holds, pushing each one's coverage
/// down first. A record that cannot be read is noted rather than failing the run: nothing is
/// written over it, and audit and the torn rescan already refuse over it.
///
/// # Returns
/// * `Ok(Unsealed)` - What was removed and newly recorded. Each object left recorded gets a note.
/// * `Err(String)`  - If the store could not be checked or the record could not be written.
fn unseal_present(store_access: &StoreAccessLock, notes: &mut Vec<String>) -> Result<sealed_utils::Unsealed, String> {
    let record = match sealed_utils::read_sealed() {
        Ok(record) => record,
        Err(error) => {
            notes.push(format!("{} No object it names was checked against the store.", error));
            return Ok(sealed_utils::Unsealed::default());
        }
    };

    let unsealed = sealed_utils::unseal(store_access, &sealed_utils::present_recorded(&record)?)?;

    for (hash, reason) in &unsealed.kept {
        notes.push(format!(
            "Object {} is on disk but stays in the sealed-object record: {}.",
            hash, reason
        ));
    }

    Ok(unsealed)
}

/// The result of a `heal` run that fully resolved (or found nothing to resolve).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct HealReport {
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
    /// The operation: shift, park, fast-forward, lower, bay creation, import-git, merge or
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
        } else if self.operation.is_none() && self.unsealed.is_empty() {
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
                "{} absent object(s) beneath them are now held by hash alone, as they were beneath \
                those records:",
                self.newly_held_by_hash.len()
            );
            for hash in &self.newly_held_by_hash {
                println!("  {}", hash);
            }
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
            println!("These tracked files do not hold their recorded content and show as local changes (\"forklift restore <path>\" brings one back):");
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
