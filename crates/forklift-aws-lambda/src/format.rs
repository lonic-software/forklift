//! The AWS head's warehouse format (docs/format/WAREHOUSE_FORMAT.md): what a request checks before
//! it serves a warehouse, the fence every write carries, and the operator's in-place upgrade.
//!
//! A warehouse records its format in one DynamoDB item, `entity = "format"`, whose `state` is the
//! format number, or `N>M owner step` while an upgrade from `N` to `M` holds it. Every request reads
//! it strongly consistent and is refused unless it is exactly [`AWS_FORMAT`] ([`gate`]). Every ref
//! and trust write carries what the gate read as a [`FormatFence`] in its transaction, and a write
//! whose fence no longer holds is refused and gated again ([`fenced`]): so once an upgrade has
//! marked the item, no request prepared before the mark can write.
//!
//! An item that is absent is a new warehouse when nothing at all is stored in its partition: its
//! first write stamps the item at this format. When something is stored there, the warehouse was
//! written before formats were recorded, and it is refused rather than read on trust.
//!
//! The upgrade ([`upgrade`]) runs from the operator binary, `forklift-aws-upgrade`, never from a
//! Lambda. It moves one format at a time: it swaps the state for a mark carrying a fresh owner token
//! and step 0, waits out every request that gated before the mark, runs that migration's steps,
//! advancing the mark's step after each, and swaps the mark for the new format. Every write a step
//! makes is fenced on the exact mark, so an operator that stalls and is taken over can no longer
//! write. A mark on record is taken over only when the operator says the run that left it is dead
//! (`--takeover`). Steps are confined to the warehouse's own DynamoDB partition: the bucket's
//! `objects/` and `signatures/` keys are shared by every warehouse and named by their content, and
//! `staging/` and `responses/` hold nothing a step migrates.
//!
//! The chain is a parameter, so tests run the upgrade over chains of their own; [`CHAIN`] is this
//! build's, and empty: format 1 is the first.

use std::fmt;
use std::time::Duration;

use crate::error::{HeadError, HeadResult};
use crate::store::{FormatFence, FormatStore, RefStore};

/// The format this build of the AWS head reads.
pub const AWS_FORMAT: u32 = 1;

/// What a warehouse's format item holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatState {
    /// The warehouse is in this format.
    At(u32),
    /// An upgrade from `from` to `to` holds the warehouse: `owner`'s run, with `step` of that
    /// migration's steps done.
    Upgrading { from: u32, to: u32, owner: String, step: u32 },
}

impl FormatState {
    /// Parse a stored state. Strict: a state that does not print back to exactly the same text is
    /// not one this build wrote, and is `None`, never a guess.
    pub fn parse(text: &str) -> Option<FormatState> {
        let state = match text.split_once('>') {
            None => FormatState::At(text.parse().ok()?),
            Some((from, rest)) => {
                let mut fields = rest.split(' ');
                let to = fields.next()?.parse().ok()?;
                let owner = fields.next()?.to_string();
                let step = fields.next()?.parse().ok()?;

                if fields.next().is_some() || owner.is_empty() {
                    return None;
                }

                FormatState::Upgrading { from: from.parse().ok()?, to, owner, step }
            }
        };

        (state.to_string() == text).then_some(state)
    }
}

impl fmt::Display for FormatState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FormatState::At(format) => write!(f, "{}", format),
            FormatState::Upgrading { from, to, owner, step } => write!(f, "{}>{} {} {}", from, to, owner, step),
        }
    }
}

/// Decide whether this build serves the warehouse behind `refs`, and how its writes are fenced.
///
/// # Returns
/// * `Ok(FormatFence)` - The warehouse is in [`AWS_FORMAT`], or is new and its first write stamps it.
/// * `Err(HeadError)`  - A `503` naming why it is not served: an older or newer format, an upgrade
///                       in progress, no format recorded over stored data, or a state this build
///                       cannot read. A `500` when the store could not be read.
pub fn gate<R: RefStore + ?Sized>(refs: &R) -> HeadResult<FormatFence> {
    gate_for(refs, AWS_FORMAT)
}

/// [`gate`] for a build that reads `reads`.
pub fn gate_for<R: RefStore + ?Sized>(refs: &R, reads: u32) -> HeadResult<FormatFence> {
    let state = match refs.read_format().map_err(HeadError::internal)? {
        Some(state) => state,
        None if !refs.partition_has_items().map_err(HeadError::internal)? => {
            return Ok(FormatFence::Stamp(FormatState::At(reads).to_string()))
        }
        // A first writer may have stamped the item between the two reads above: its transaction
        // writes the stamp with its first item, so a partition holding anything shows the stamp to
        // a strongly consistent read made after it.
        None => match refs.read_format().map_err(HeadError::internal)? {
            Some(state) => state,
            None => return Err(unstamped()),
        },
    };

    match FormatState::parse(&state) {
        Some(FormatState::At(format)) if format == reads => Ok(FormatFence::At(state)),
        Some(FormatState::At(format)) if format < reads => Err(HeadError::service_unavailable(format!(
            "This warehouse is in format {}, older than format {}, the one this deployment reads. \
            Its operator has to upgrade it; nothing in it was changed.",
            format, reads
        ))),
        Some(FormatState::At(format)) => Err(HeadError::service_unavailable(format!(
            "This warehouse is in format {}, newer than format {}, the one this deployment reads. \
            Update the deployment to a version that reads format {}; nothing in it was changed.",
            format, reads, format
        ))),
        Some(FormatState::Upgrading { from, to, .. }) => Err(HeadError::service_unavailable(format!(
            "This warehouse is being upgraded from format {} to format {}. Try again once the \
            upgrade has finished; if it stopped part way, its operator runs it again.",
            from, to
        ))),
        None => Err(HeadError::service_unavailable(format!(
            "This warehouse's format record holds \"{}\", which this deployment cannot read. It \
            refuses the warehouse rather than guess at its format; nothing in it was changed.",
            state
        ))),
    }
}

fn unstamped() -> HeadError {
    HeadError::service_unavailable(
        "This warehouse holds data but records no format: it was written by a deployment from \
        before warehouse formats were recorded, and this one cannot read it. Make the warehouse \
        again under a new id and lift its pallets into it; nothing in it was changed."
            .to_string(),
    )
}

/// Run a write fenced on the warehouse's format: gate, run `write` with the fence, and when
/// `moved` says the fence no longer held, gate again and run it once more under the new fence.
///
/// A fence moves from a stamp to the format a concurrent first write stamped, which the next gate
/// serves; any other move is a mark or a format this build refuses, which the gate answers. A gate
/// that returns the very fence that just failed answers `503` rather than loop, so this ends.
///
/// # Returns
/// * `Ok(T)`          - The outcome of the last `write`, never one `moved` names.
/// * `Err(HeadError)` - The gate's refusal, or `write`'s own error.
pub fn fenced<R: RefStore + ?Sized, T>(
    refs: &R,
    mut write: impl FnMut(&FormatFence) -> HeadResult<T>,
    moved: impl Fn(&T) -> bool,
) -> HeadResult<T> {
    let mut fence = gate(refs)?;

    loop {
        let outcome = write(&fence)?;

        if !moved(&outcome) {
            return Ok(outcome);
        }

        let next = gate(refs)?;

        if next == fence {
            return Err(HeadError::service_unavailable(
                "This warehouse's format record changed while the write was being made, and \
                changed back. Nothing was written; run the request again."
                    .to_string(),
            ));
        }

        fence = next;
    }
}

/// What a migration step is given: the warehouse it upgrades, its ref store, and the fence every
/// write it makes must carry, the exact mark on record while it runs.
pub struct StepContext<'a> {
    pub warehouse: &'a str,
    pub refs: &'a dyn RefStore,
    pub fence: FormatFence,
}

/// One step of an AWS migration. It must be idempotent, since a step interrupted part way is run
/// again from its start, and it writes only through `refs` with `fence`, inside the warehouse's
/// own partition.
pub type AwsUpgradeStep<'a> = &'a (dyn Fn(&StepContext) -> Result<(), String> + Sync);

/// The steps that move a warehouse from one format to the next.
pub struct AwsMigration<'a> {
    pub steps: &'a [AwsUpgradeStep<'a>],
}

/// The migrations a build upgrades with: the last one moves a warehouse to `reads`, and each one
/// before it to the format the next one starts from.
pub struct AwsUpgradeChain<'a> {
    /// The format the chain ends at: the one the build reads.
    pub reads: u32,

    /// One migration per format, oldest first.
    pub migrations: &'a [AwsMigration<'a>],
}

impl AwsUpgradeChain<'_> {
    /// The oldest format the chain upgrades.
    pub fn floor(&self) -> u32 {
        u32::try_from(self.migrations.len())
            .ok()
            .and_then(|count| self.reads.checked_sub(count))
            .expect("a chain has no more migrations than there are formats before the one it reads")
    }

    fn migration_from(&self, format: u32) -> Option<&AwsMigration<'_>> {
        let index = format.checked_sub(self.floor())?;
        self.migrations.get(index as usize)
    }
}

/// This build's chain. Empty: format 1 is the first format, and no format before it is upgraded.
pub static CHAIN: AwsUpgradeChain<'static> = AwsUpgradeChain { reads: AWS_FORMAT, migrations: &[] };

/// How an upgrade runs.
pub struct UpgradeOptions {
    /// The token this run marks the warehouse with; fresh for every run.
    pub owner: String,

    /// Take over a mark another run left on record. The operator says so only once that run is
    /// known to be dead.
    pub takeover: bool,

    /// How long to wait after marking a warehouse before its first step: longer than any request
    /// that gated before the mark can still be running.
    pub wait: Duration,
}

/// What an upgrade did to one warehouse.
#[derive(Debug, PartialEq, Eq)]
pub struct AwsUpgradeReport {
    pub warehouse: String,

    /// The format the warehouse was in when the upgrade started, or was moving from.
    pub from: u32,

    /// The format it is in now.
    pub to: u32,

    /// The upgrade on record that this run took over: its target format and the steps it had done.
    pub took_over: Option<(u32, u32)>,

    /// The steps this run ran.
    pub steps_run: u32,
}

impl AwsUpgradeReport {
    /// What the upgrade did, in lines for a person.
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some((target, step)) = self.took_over {
            lines.push(format!(
                "Warehouse {}: took over the upgrade to format {}, which had done {} of its steps.",
                self.warehouse, target, step
            ));
        }

        if self.from == self.to && self.took_over.is_none() {
            lines.push(format!(
                "Warehouse {}: in format {}, the one this version reads. Nothing to do.",
                self.warehouse, self.to
            ));
        } else {
            lines.push(format!(
                "Warehouse {}: upgraded from format {} to format {}.",
                self.warehouse, self.from, self.to
            ));
        }

        lines
    }
}

/// Upgrade `warehouse`, whose ref store is `store`, to `chain.reads`, or take over an upgrade on
/// record when `options.takeover` says its run is dead.
///
/// # Returns
/// * `Ok(AwsUpgradeReport)` - The warehouse is in `chain.reads`.
/// * `Err(String)`          - Nothing is stored for the warehouse; it records no format over
///                            stored data; its state cannot be read; it is in a format past
///                            `chain.reads` or before the chain's floor; another run's mark is on
///                            record and `takeover` is off; another run took the mark from this
///                            one; a step failed, leaving the mark on record for a rerun with
///                            `takeover`; or the store failed.
pub fn upgrade<S: RefStore + FormatStore>(
    store: &S,
    chain: &AwsUpgradeChain,
    warehouse: &str,
    options: &UpgradeOptions,
) -> Result<AwsUpgradeReport, String> {
    let mut took_over = None;
    let mut steps_run = 0;
    let mut from = None;

    loop {
        let text = match store.read_format()? {
            Some(text) => text,
            None if store.partition_has_items()? => {
                return Err(format!(
                    "Warehouse {} holds data but records no format: it was written before warehouse \
                    formats were recorded, and no upgrade reaches it. Make it again under a new id.",
                    warehouse
                ))
            }
            None => {
                return Err(format!(
                    "Warehouse {} has nothing stored; there is nothing to upgrade. Check the id.",
                    warehouse
                ))
            }
        };

        let state = FormatState::parse(&text)
            .ok_or_else(|| format!("Warehouse {}'s format record holds \"{}\", which this version cannot read.", warehouse, text))?;

        let (mark, migration, step) = match state {
            FormatState::At(format) if format == chain.reads => {
                return Ok(AwsUpgradeReport {
                    warehouse: warehouse.to_string(),
                    from: from.unwrap_or(format),
                    to: format,
                    took_over,
                    steps_run,
                })
            }
            FormatState::At(format) if format > chain.reads => {
                return Err(format!(
                    "Warehouse {} is in format {}, newer than format {}, the one this version reads. \
                    Upgrade with a version that reads it.",
                    warehouse, format, chain.reads
                ))
            }
            FormatState::At(format) => {
                let migration = chain.migration_from(format).ok_or_else(|| {
                    format!(
                        "Warehouse {} is in format {}, older than format {}, the oldest this version \
                        upgrades. Make it again under a new id.",
                        warehouse,
                        format,
                        chain.floor()
                    )
                })?;
                let mark = FormatState::Upgrading { from: format, to: format + 1, owner: options.owner.clone(), step: 0 };

                if !store.swap_format(&text, &mark.to_string())? {
                    continue;
                }

                from.get_or_insert(format);

                // Requests that gated before the mark may still be running. Their writes are
                // fenced on the format the mark replaced and so fail, but they may still read
                // what the steps are about to change.
                std::thread::sleep(options.wait);

                (mark, migration, 0)
            }
            FormatState::Upgrading { from: marked_from, to, owner, step } => {
                if !options.takeover {
                    return Err(format!(
                        "Warehouse {} is being upgraded from format {} to format {} by run {}, at \
                        step {}. If that run is dead, run the upgrade again with --takeover.",
                        warehouse, marked_from, to, owner, step
                    ));
                }

                let migration = chain
                    .migration_from(marked_from)
                    .filter(|_| to == marked_from + 1 && to <= chain.reads)
                    .ok_or_else(|| {
                        format!(
                            "Warehouse {} is marked as being upgraded from format {} to format {}, \
                            which this version does not do. Take it over with a version that does.",
                            warehouse, marked_from, to
                        )
                    })?;

                if step as usize > migration.steps.len() {
                    return Err(format!(
                        "Warehouse {}'s upgrade to format {} is marked at step {}, past the {} steps \
                        this version has for it. Take it over with the version that marked it.",
                        warehouse,
                        to,
                        step,
                        migration.steps.len()
                    ));
                }

                let mark = FormatState::Upgrading { from: marked_from, to, owner: options.owner.clone(), step };

                if !store.swap_format(&text, &mark.to_string())? {
                    continue;
                }

                from.get_or_insert(marked_from);
                took_over.get_or_insert((to, step));

                // The run taken over may have died before its own wait was out, so requests that
                // gated before its mark may still be running.
                std::thread::sleep(options.wait);

                (mark, migration, step)
            }
        };

        let FormatState::Upgrading { from: marked_from, to, .. } = mark else { unreachable!() };
        let mut mark = mark;

        for (index, run_step) in migration.steps.iter().enumerate().skip(step as usize) {
            let context = StepContext { warehouse, refs: store, fence: FormatFence::At(mark.to_string()) };

            run_step(&context).map_err(|error| {
                format!(
                    "Warehouse {}: step {} of the upgrade to format {} failed: {}. The upgrade stays on \
                    record; run it again with --takeover.",
                    warehouse,
                    index + 1,
                    to,
                    error
                )
            })?;
            steps_run += 1;

            let next = FormatState::Upgrading { from: marked_from, to, owner: options.owner.clone(), step: index as u32 + 1 };

            if !store.swap_format(&mark.to_string(), &next.to_string())? {
                return Err(lost_the_mark(warehouse));
            }

            mark = next;
        }

        if !store.swap_format(&mark.to_string(), &FormatState::At(to).to_string())? {
            return Err(lost_the_mark(warehouse));
        }
    }
}

fn lost_the_mark(warehouse: &str) -> String {
    format!(
        "Warehouse {}: another run took this upgrade over, so this one stopped. That run finishes it.",
        warehouse
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::MemoryRefStore;
    use crate::store::{CasOutcome, ExpectedHead, OfficePrecondition, RefHead};
    use forklift_core::util::pallet_utils::PalletNamespace;
    use std::sync::Mutex;

    const HEAD_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const HEAD_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn options(owner: &str, takeover: bool) -> UpgradeOptions {
        UpgradeOptions { owner: owner.to_string(), takeover, wait: Duration::ZERO }
    }

    fn lift(refs: &dyn RefStore, fence: &FormatFence, expected: ExpectedHead, new: &str) -> CasOutcome {
        refs.compare_and_set_head(PalletNamespace::User, "main", expected, new, OfficePrecondition::NotConsumed, None, fence, "t")
            .expect("the fake does not fail")
    }

    fn store_at(state: Option<&str>) -> MemoryRefStore {
        let store = MemoryRefStore::new();
        store.set_format(state);
        store
    }

    #[test]
    fn a_state_parses_only_from_the_text_it_prints() {
        for text in ["1", "2>3 abc 0", "1>2 owner 4"] {
            assert_eq!(FormatState::parse(text).map(|state| state.to_string()).as_deref(), Some(text));
        }

        for text in ["", "01", "1 ", "+1", "1>2", "1>2 owner", "1>2  0", "1>2 owner 0 x", "x"] {
            assert_eq!(FormatState::parse(text), None, "{:?}", text);
        }
    }

    #[test]
    fn an_empty_partition_is_stamped_by_its_first_write() {
        let store = store_at(None);
        let fence = gate(&store).unwrap();

        assert_eq!(fence, FormatFence::Stamp(AWS_FORMAT.to_string()));
        assert_eq!(lift(&store, &fence, ExpectedHead::Unborn, HEAD_A), CasOutcome::Committed);
        assert_eq!(store.read_format().unwrap(), Some(AWS_FORMAT.to_string()));
        assert_eq!(gate(&store).unwrap(), FormatFence::At(AWS_FORMAT.to_string()));
    }

    #[test]
    fn a_second_first_writer_finds_the_stamp_and_writes_under_it() {
        let store = store_at(None);
        let mut fences = Vec::new();
        let outcome = fenced(
            &store,
            |fence| {
                fences.push(fence.clone());

                // Another first writer commits between this request's gate and its write.
                if fences.len() == 1 {
                    let theirs = gate(&store).unwrap();
                    assert_eq!(lift(&store, &theirs, ExpectedHead::Unborn, HEAD_A), CasOutcome::Committed);
                }

                Ok(lift(&store, fence, ExpectedHead::At(HEAD_A), HEAD_B))
            },
            |outcome| *outcome == CasOutcome::FormatMoved,
        )
        .unwrap();

        assert_eq!(outcome, CasOutcome::Committed);
        assert_eq!(fences, vec![FormatFence::Stamp("1".into()), FormatFence::At("1".into())]);
        assert_eq!(store.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Live(HEAD_B.into()));
    }

    #[test]
    fn stored_data_without_a_format_is_refused_not_stamped() {
        let store = store_at(None);
        let fence = FormatFence::At(AWS_FORMAT.to_string());

        store.set_format(Some("1"));
        assert_eq!(lift(&store, &fence, ExpectedHead::Unborn, HEAD_A), CasOutcome::Committed);
        store.set_format(None);

        let refusal = gate(&store).unwrap_err();
        assert_eq!(refusal.status.as_u16(), 503);
        assert!(refusal.message.contains("records no format"), "{}", refusal.message);
        assert_eq!(store.read_format().unwrap(), None);
    }

    #[test]
    fn the_gate_refuses_every_state_but_this_format() {
        for (state, says) in [
            ("0", "older than format 1"),
            ("2", "newer than format 1"),
            ("1>2 run 0", "being upgraded from format 1 to format 2"),
            ("one", "cannot read"),
        ] {
            let refusal = gate(&store_at(Some(state))).unwrap_err();

            assert_eq!(refusal.status.as_u16(), 503, "{}", state);
            assert!(refusal.message.contains(says), "{}: {}", state, refusal.message);
        }
    }

    #[test]
    fn a_mark_refuses_a_write_prepared_before_it() {
        let store = store_at(Some("1"));
        let fence = gate(&store).unwrap();

        store.set_format(Some("1>2 run 0"));

        assert_eq!(lift(&store, &fence, ExpectedHead::Unborn, HEAD_A), CasOutcome::FormatMoved);
        assert_eq!(store.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Absent);

        let refusal = fenced(&store, |fence| Ok(lift(&store, fence, ExpectedHead::Unborn, HEAD_A)), |outcome| {
            *outcome == CasOutcome::FormatMoved
        })
        .unwrap_err();
        assert!(refusal.message.contains("being upgraded"), "{}", refusal.message);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1>2 run 0"));
    }

    #[test]
    fn a_fence_that_fails_and_reads_back_the_same_is_refused_not_retried() {
        let store = store_at(Some("1"));
        let mut attempts = 0;

        let refusal = fenced(
            &store,
            |_| {
                attempts += 1;
                Ok(CasOutcome::FormatMoved)
            },
            |outcome| *outcome == CasOutcome::FormatMoved,
        )
        .unwrap_err();

        assert_eq!(attempts, 1);
        assert_eq!(refusal.status.as_u16(), 503);
    }

    /// A chain from format 1 to 2 whose steps record what they ran and lift `main` under the mark.
    fn recording_chain<'a>(steps: &'a [AwsUpgradeStep<'a>]) -> [AwsMigration<'a>; 1] {
        [AwsMigration { steps }]
    }

    #[test]
    fn an_upgrade_marks_runs_every_step_under_the_mark_and_stamps() {
        let store = store_at(Some("1"));
        let seen = Mutex::new(Vec::new());
        let step = |context: &StepContext| -> Result<(), String> {
            seen.lock().unwrap().push(context.fence.clone());
            match lift(context.refs, &context.fence, ExpectedHead::Unborn, HEAD_A) {
                CasOutcome::Committed | CasOutcome::Conflict { .. } => Ok(()),
                other => Err(format!("{:?}", other)),
            }
        };
        let steps: [AwsUpgradeStep; 2] = [&step, &step];
        let migrations = recording_chain(&steps);
        let chain = AwsUpgradeChain { reads: 2, migrations: &migrations };

        let report = upgrade(&store, &chain, "w", &options("me", false)).unwrap();

        assert_eq!(report, AwsUpgradeReport { warehouse: "w".into(), from: 1, to: 2, took_over: None, steps_run: 2 });
        assert_eq!(
            *seen.lock().unwrap(),
            vec![FormatFence::At("1>2 me 0".into()), FormatFence::At("1>2 me 1".into())]
        );
        assert_eq!(store.read_format().unwrap().as_deref(), Some("2"));
        assert_eq!(store.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Live(HEAD_A.into()));
        assert_eq!(upgrade(&store, &chain, "w", &options("again", false)).unwrap().steps_run, 0);
    }

    #[test]
    fn this_builds_chain_has_nothing_to_do_at_its_format() {
        let store = store_at(Some("1"));
        let report = upgrade(&store, &CHAIN, "w", &options("me", false)).unwrap();

        assert_eq!(report.summary(), vec!["Warehouse w: in format 1, the one this version reads. Nothing to do."]);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn a_mark_on_record_is_refused_without_takeover_and_resumed_with_it() {
        let store = store_at(Some("1>2 dead 1"));
        let ran = Mutex::new(Vec::new());
        let first = |_: &StepContext| -> Result<(), String> {
            ran.lock().unwrap().push(1);
            Ok(())
        };
        let second = |_: &StepContext| -> Result<(), String> {
            ran.lock().unwrap().push(2);
            Ok(())
        };
        let steps: [AwsUpgradeStep; 2] = [&first, &second];
        let migrations = recording_chain(&steps);
        let chain = AwsUpgradeChain { reads: 2, migrations: &migrations };

        let refusal = upgrade(&store, &chain, "w", &options("me", false)).unwrap_err();
        assert!(refusal.contains("--takeover"), "{}", refusal);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1>2 dead 1"));

        // The run taken over may have died mid-wait, so the takeover waits before its first step.
        let wait = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let report = upgrade(&store, &chain, "w", &UpgradeOptions { wait, ..options("me", true) }).unwrap();
        assert!(started.elapsed() >= wait, "the takeover ran its steps without waiting");
        assert_eq!(report.took_over, Some((2, 1)));
        assert_eq!(*ran.lock().unwrap(), vec![2]);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("2"));
    }

    #[test]
    #[should_panic(expected = "no more migrations than there are formats")]
    fn a_chain_longer_than_its_formats_is_refused_loudly() {
        let step = |_: &StepContext| -> Result<(), String> { Ok(()) };
        let steps: [AwsUpgradeStep; 1] = [&step];
        let migrations = [AwsMigration { steps: &steps }, AwsMigration { steps: &steps }];

        AwsUpgradeChain { reads: 1, migrations: &migrations }.floor();
    }

    #[test]
    fn a_stalled_run_cannot_write_once_taken_over() {
        let store = store_at(Some("1"));
        let taken_over = |context: &StepContext| -> Result<(), String> {
            // While this run's step is running, an operator takes the upgrade over.
            store.set_format(Some("1>2 other 0"));
            match lift(context.refs, &context.fence, ExpectedHead::Unborn, HEAD_A) {
                CasOutcome::FormatMoved => Err("fenced out".into()),
                other => Err(format!("wrote through a stale mark: {:?}", other)),
            }
        };
        let steps: [AwsUpgradeStep; 1] = [&taken_over];
        let migrations = recording_chain(&steps);
        let chain = AwsUpgradeChain { reads: 2, migrations: &migrations };

        let error = upgrade(&store, &chain, "w", &options("me", false)).unwrap_err();

        assert!(error.contains("fenced out"), "{}", error);
        assert_eq!(store.get_head(PalletNamespace::User, "main").unwrap(), RefHead::Absent);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1>2 other 0"));
    }

    #[test]
    fn a_run_that_loses_its_mark_between_steps_stops() {
        let store = store_at(Some("1"));
        let step = |_: &StepContext| -> Result<(), String> {
            store.set_format(Some("1>2 other 0"));
            Ok(())
        };
        let steps: [AwsUpgradeStep; 1] = [&step];
        let migrations = recording_chain(&steps);
        let chain = AwsUpgradeChain { reads: 2, migrations: &migrations };

        let error = upgrade(&store, &chain, "w", &options("me", false)).unwrap_err();

        assert!(error.contains("another run took this upgrade over"), "{}", error);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1>2 other 0"));
    }

    #[test]
    fn a_failed_step_leaves_the_mark_on_record() {
        let store = store_at(Some("1"));
        let step = |_: &StepContext| -> Result<(), String> { Err("disk on fire".into()) };
        let steps: [AwsUpgradeStep; 1] = [&step];
        let migrations = recording_chain(&steps);
        let chain = AwsUpgradeChain { reads: 2, migrations: &migrations };

        let error = upgrade(&store, &chain, "w", &options("me", false)).unwrap_err();

        assert!(error.contains("disk on fire") && error.contains("--takeover"), "{}", error);
        assert_eq!(store.read_format().unwrap().as_deref(), Some("1>2 me 0"));
        assert!(gate_for(&store, 2).is_err());
    }

    #[test]
    fn an_upgrade_refuses_what_it_cannot_reach() {
        let migrations: [AwsMigration; 0] = [];
        let chain = AwsUpgradeChain { reads: 1, migrations: &migrations };

        for (state, says) in [
            (Some("0"), "oldest this version upgrades"),
            (Some("2"), "newer than format 1"),
            (Some("x"), "cannot read"),
            (Some("0>1 dead 0"), "which this version does not do"),
            (None, "nothing stored"),
        ] {
            let error = upgrade(&store_at(state), &chain, "w", &options("me", true)).unwrap_err();
            assert!(error.contains(says), "{:?}: {}", state, error);
        }

        let unstamped = store_at(Some("1"));
        lift(&unstamped, &FormatFence::At("1".into()), ExpectedHead::Unborn, HEAD_A);
        unstamped.set_format(None);

        let error = upgrade(&unstamped, &chain, "w", &options("me", false)).unwrap_err();
        assert!(error.contains("records no format"), "{}", error);
    }
}
