//! The in-place upgrade of a warehouse from an older format to the one this build reads
//! (docs/format/WAREHOUSE_FORMAT.md).
//!
//! An upgrade moves one format at a time. Before its first change it records `upgrading F+1 0` in the
//! marker, so a crash leaves every command but the upgrade refusing; it runs that migration's steps in
//! order, recording each one done only after the step has returned, which is after its writes are
//! synced; then it records format `F+1` with no upgrade, and takes the next migration. Rerunning it
//! resumes at the first step not recorded done, so every step must be idempotent. It works under the
//! format lease held exclusively, so nothing else has the warehouse open while it changes.
//!
//! The chain is a parameter, so tests run the engine over chains of their own; [`CHAIN`] is this
//! build's. Format 1 is the first format, so the chain starts there and never reaches further back.

use std::path::{Path, PathBuf};
use crate::error::{CoreError, RefusalCode};
use crate::globals::{self, FOLDER_NAME_FORKLIFT_ROOT};
use crate::util::{bay_utils, franchise_utils, lock_utils, warehouse_utils};
use crate::util::warehouse_utils::{Head, MarkerUpgrade, WarehouseMarker, WAREHOUSE_FORMAT};

/// What a migration step is given: the warehouse's forklift root folder, and every bay-local state
/// folder in it, the main tree's first. A bay's own `.forklift` redirect is not among them: its
/// framing is frozen, so no format changes it.
#[derive(Debug)]
pub struct UpgradeContext {
    pub forklift: PathBuf,
    pub bay_state_dirs: Vec<PathBuf>,
}

/// One step of a migration. It must be idempotent, since a step interrupted part way is run again
/// from its start, and must sync what it writes before it returns, since returning records it done.
/// It runs inside the warehouse's storage-root scope, under the format lease held exclusively by
/// this process, so it must not open the warehouse through anything that takes the lease shared
/// ([`warehouse_utils::open_warehouse_root`] and its kin): that refuses as an upgrade running. A
/// step may rewrite the marker's mode word, keeping its format and upgrade entries.
pub type UpgradeStep<'a> = &'a (dyn Fn(&UpgradeContext) -> Result<(), String> + Sync);

/// The steps that move a warehouse from one format to the next.
pub struct Migration<'a> {
    pub steps: &'a [UpgradeStep<'a>],
}

/// The migrations a build upgrades with: the last one moves a warehouse to `reads`, and each one
/// before it to the format the next one starts from.
pub struct UpgradeChain<'a> {
    /// The format the chain ends at: the one the build reads.
    pub reads: u32,

    /// One migration per format, oldest first.
    pub migrations: &'a [Migration<'a>],
}

impl UpgradeChain<'_> {
    /// The oldest format the chain upgrades.
    pub fn floor(&self) -> u32 {
        self.reads - self.migrations.len() as u32
    }

    /// The migration from `format` to the next format.
    fn migration_from(&self, format: u32) -> Option<&Migration<'_>> {
        let index = format.checked_sub(self.floor())?;
        self.migrations.get(index as usize)
    }
}

/// This build's chain. Empty: format 1 is the first format, and no format before it is upgraded.
pub static CHAIN: UpgradeChain<'static> = UpgradeChain { reads: WAREHOUSE_FORMAT, migrations: &[] };

/// What an upgrade did.
#[derive(Debug, PartialEq, Eq)]
pub struct UpgradeReport {
    /// The format the warehouse was in when the upgrade started.
    pub from: u32,

    /// The format it is in now.
    pub to: u32,

    /// The unfinished upgrade it found on record and resumed, if any.
    pub resumed: Option<ResumedUpgrade>,

    /// The steps it ran.
    pub steps_run: u32,
}

impl UpgradeReport {
    /// What the upgrade did, in lines for a person, for every head to print the same way.
    pub fn summary(&self) -> Vec<String> {
        let mut lines = Vec::new();

        if let Some(resumed) = self.resumed {
            lines.push(match resumed.step {
                0 => format!("Resumed the unfinished upgrade to format {}, which had done none of its steps.", resumed.target),
                step => format!("Resumed the unfinished upgrade to format {} at its step {}.", resumed.target, step + 1),
            });
        }

        if self.from == self.to && self.resumed.is_none() {
            lines.push(format!("The warehouse is in format {}, the one this version reads. Nothing to do.", self.to));
        } else {
            lines.push(format!("Upgraded the warehouse from format {} to format {}.", self.from, self.to));
        }

        lines
    }
}

/// An unfinished upgrade found on record.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResumedUpgrade {
    /// The format it was moving to.
    pub target: u32,

    /// The steps of it already done.
    pub step: u32,
}

/// Upgrade the warehouse whose root is `root` to `chain.reads`, or resume an upgrade on record. A
/// bay is upgraded with its warehouse, never on its own, and an unfinished clone is not upgraded at
/// all: running the clone again with this build finishes it in this build's format. Next steps are
/// written for `head`.
///
/// # Returns
/// * `Ok(UpgradeReport)` - The warehouse is in `chain.reads`, and the report says from where.
/// * `Err(CoreError)`    - A `not_a_warehouse` refusal (or another of [`warehouse_utils::open_warehouse_root`]'s
///                         refusals for what is no warehouse), a `clone_incomplete` refusal, a
///                         `warehouse_in_use` refusal, a `warehouse_format_newer` or
///                         `warehouse_upgrade_incomplete` refusal for a format past `chain.reads`, a
///                         `warehouse_format_older` refusal for one the chain does not reach, a step's
///                         error with the upgrade left on record, or the marker could not be read
///                         or written.
pub fn upgrade_warehouse(root: &Path, head: Head, chain: &UpgradeChain) -> Result<UpgradeReport, CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
    let shown = warehouse_utils::shown_root(root);

    match bay_utils::classify_forklift(&forklift)? {
        bay_utils::ForkliftKind::Warehouse => (),
        bay_utils::ForkliftKind::Bay if head == Head::Client => {
            let redirect = bay_utils::read_bay_redirect(&forklift)?;
            return Err(CoreError::refusal(
                RefusalCode::NotAWarehouse,
                format!(
                    "\"{}\" is a bay's working directory, and a bay is upgraded with its warehouse.",
                    shown.to_string_lossy()
                ),
                format!("Run \"forklift upgrade\" in \"{}\".", redirect.warehouse_root.to_string_lossy()),
            ));
        }
        // Every other kind is refused there with its own way out; a warehouse put in place since is
        // upgraded like any other.
        _ => drop(warehouse_utils::open_warehouse_root(root, head)?),
    }

    franchise_utils::gate(root)?;

    let _lease = lock_utils::FormatLease::exclusive(&forklift)?;

    let mut marker = warehouse_utils::read_marker(&forklift)?.ok_or_else(|| format!(
        "The warehouse marker in \"{}\" was removed while the warehouse was being upgraded.", forklift.to_string_lossy()
    ))?;

    let from = marker.format;
    let resumed = marker.upgrade.map(|upgrade| ResumedUpgrade { target: upgrade.target, step: upgrade.step });
    let mut steps_run = 0;

    // Steps resolve storage through the warehouse, wherever the upgrade was started.
    let _scope = globals::StorageRootScope::enter(root);

    // The bays are listed once, when the first step is about to run: no step adds or removes one.
    let mut context: Option<UpgradeContext> = None;

    loop {
        let upgrade = match marker.upgrade {
            Some(upgrade) => upgrade,
            None if marker.format == chain.reads => break,
            // A format past the chain is refused by `migration_for`, never wrapped round.
            None => MarkerUpgrade { target: marker.format.saturating_add(1), step: 0 },
        };

        let migration = migration_for(&marker, upgrade, chain, &shown, &forklift, head)?;

        if marker.upgrade.is_none() {
            marker.upgrade = Some(upgrade);
            warehouse_utils::write_marker(&forklift, &marker)?;
        }

        for (index, step) in migration.steps.iter().enumerate().skip(upgrade.step as usize) {
            if context.is_none() {
                context = Some(UpgradeContext { forklift: forklift.clone(), bay_state_dirs: bay_utils::all_bay_state_dirs()? });
            }
            step(context.as_ref().expect("listed above")).map_err(|error| format!(
                "The upgrade of the warehouse at \"{}\" to format {} stopped at its step {}: {} Every command \
                but the upgrade refuses the warehouse until it finishes; run the upgrade again to resume it.",
                shown.to_string_lossy(), upgrade.target, index + 1, error
            ))?;
            steps_run += 1;

            marker = marker_after_step(&forklift, marker.format, MarkerUpgrade { target: upgrade.target, step: index as u32 })?;
            marker.upgrade = Some(MarkerUpgrade { target: upgrade.target, step: index as u32 + 1 });
            warehouse_utils::write_marker(&forklift, &marker)?;
        }

        let done = MarkerUpgrade { target: upgrade.target, step: migration.steps.len() as u32 };
        marker = finished_marker(&forklift, marker.format, done, chain)?;
        warehouse_utils::write_marker(&forklift, &marker)?;
    }

    Ok(UpgradeReport { from, to: marker.format, resumed, steps_run })
}

/// The marker on disk once a step of the upgrade from `format` has returned, `upgrade` being what
/// was on record while it ran: a step may rewrite the mode word, so the marker is read back rather
/// than written from memory, and must still record that format and that upgrade.
fn marker_after_step(forklift: &Path, format: u32, upgrade: MarkerUpgrade) -> Result<WarehouseMarker, CoreError> {
    let marker = warehouse_utils::read_marker(forklift)?.ok_or_else(|| format!(
        "The warehouse marker in \"{}\" was removed while the warehouse was being upgraded.", forklift.to_string_lossy()
    ))?;

    if marker.format != format || marker.upgrade != Some(upgrade) {
        return Err(format!(
            "The warehouse marker in \"{}\" no longer records the upgrade to format {} that is running.",
            forklift.to_string_lossy(), upgrade.target
        ).into());
    }

    Ok(marker)
}

/// The marker that records the upgrade from `format` finished, `done` being every step of it
/// recorded done: the one on disk moved to the target format. At the format the chain reads its
/// mode must be one this build reads, or the upgrade stays on record rather than leave a warehouse
/// every command refuses.
fn finished_marker(forklift: &Path, format: u32, done: MarkerUpgrade, chain: &UpgradeChain) -> Result<WarehouseMarker, CoreError> {
    let marker = marker_after_step(forklift, format, done)?;

    if done.target == chain.reads && warehouse_utils::WarehouseMode::from_word(&marker.mode).is_none() {
        return Err(format!(
            "The upgrade of the warehouse in \"{}\" to format {} left the mode \"{}\", which this version of \
            Forklift does not read; the upgrade stays on record.",
            forklift.to_string_lossy(), done.target, marker.mode.escape_debug()
        ).into());
    }

    Ok(WarehouseMarker { format: done.target, mode: marker.mode, upgrade: None })
}

/// The migration that carries out `upgrade` on a warehouse with `marker`, whose absolute root is
/// shown as `shown`: refused when it is past what `chain` reads or before what it reaches.
fn migration_for<'a>(
    marker: &WarehouseMarker,
    upgrade: MarkerUpgrade,
    chain: &'a UpgradeChain<'a>,
    shown: &Path,
    forklift: &Path,
    head: Head,
) -> Result<&'a Migration<'a>, CoreError> {
    let root = shown.to_string_lossy();

    if marker.format >= chain.reads && marker.upgrade.is_none() {
        return Err(CoreError::refusal(
            RefusalCode::WarehouseFormatNewer,
            format!(
                "The warehouse at \"{}\" is in format {}, newer than the format {} this version of Forklift reads.",
                root, marker.format, chain.reads
            ),
            "Update this Forklift binary to one that reads that format. Upgrades are one-way.",
        ));
    }

    // Every upgrade a build starts moves one format on.
    if upgrade.target != marker.format.saturating_add(1) {
        return Err(format!(
            "The warehouse marker in \"{}\" records an upgrade from format {} to format {}, which no version of \
            Forklift starts, so Forklift cannot tell what it changed.",
            forklift.to_string_lossy(), marker.format, upgrade.target
        ).into());
    }

    if upgrade.target > chain.reads {
        return Err(CoreError::refusal(
            RefusalCode::WarehouseUpgradeIncomplete,
            format!(
                "An upgrade of the warehouse at \"{}\" to format {} did not finish, and this version of Forklift \
                reads format {}, so it cannot finish it.",
                root, upgrade.target, chain.reads
            ),
            format!(
                "Update this Forklift binary to one that reads format {}, and run its upgrade; it resumes this one.",
                upgrade.target
            ),
        ));
    }

    let migration = chain.migration_from(marker.format).ok_or_else(|| CoreError::refusal(
        RefusalCode::WarehouseFormatOlder,
        format!(
            "The warehouse at \"{}\" is in format {}, and this version of Forklift upgrades no format older \
            than format {}.",
            root, marker.format, chain.floor()
        ),
        head.recreate_step(forklift, shown),
    ))?;

    if upgrade.step as usize > migration.steps.len() {
        return Err(format!(
            "The warehouse marker in \"{}\" records {} steps done of the upgrade to format {}, which has {} in \
            this version of Forklift, so Forklift cannot tell what it changed.",
            forklift.to_string_lossy(), upgrade.step, upgrade.target, migration.steps.len()
        ).into());
    }

    Ok(migration)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use crate::util::franchise_utils::{Attempt, BundleState, FranchiseRecord, Request};

    fn fresh_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-upgrade-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn prepare_at(root: &Path) {
        let _scope = globals::StorageRootScope::enter(root);
        warehouse_utils::prepare_warehouse(warehouse_utils::PrepareMode::Full).unwrap();
    }

    fn fresh_warehouse(label: &str) -> PathBuf {
        let root = fresh_dir(label);
        prepare_at(&root);
        root
    }

    fn marker_path(root: &Path) -> PathBuf {
        root.join(FOLDER_NAME_FORKLIFT_ROOT).join(warehouse_utils::FILE_NAME_WAREHOUSE_MARKER)
    }

    fn set_marker(root: &Path, entries: &str) {
        std::fs::write(marker_path(root), format!("/forklift-warehouse 3\n{}", entries)).unwrap();
    }

    fn marker_entries(root: &Path) -> String {
        let text = std::fs::read_to_string(marker_path(root)).unwrap();
        text.strip_prefix("/forklift-warehouse 3\n").unwrap().to_string()
    }

    fn code_of(error: &CoreError) -> Option<RefusalCode> {
        match error {
            CoreError::Refusal { code, .. } => Some(*code),
            CoreError::Other(_) => None,
        }
    }

    /// The marker as a step sees it: its format, then the upgrade on record or `-`.
    fn upgrade_on_record(context: &UpgradeContext) -> String {
        let marker = warehouse_utils::read_marker(&context.forklift).unwrap().unwrap();
        match marker.upgrade {
            Some(upgrade) => format!("{}@{}/{}", marker.format, upgrade.target, upgrade.step),
            None => format!("{}@-", marker.format),
        }
    }

    /// A chain reading format 3 that upgrades format 1 with steps `a` and `b` and format 2 with
    /// step `c`, logging each step with the marker it saw, failing the step named `fails`, and
    /// aborting the process at the step named `aborts`.
    struct LoggedChain {
        log: Mutex<Vec<String>>,
        fails: Option<&'static str>,
        aborts: Option<&'static str>,
    }

    impl LoggedChain {
        fn new() -> LoggedChain {
            LoggedChain { log: Mutex::new(Vec::new()), fails: None, aborts: None }
        }

        fn step(&self, name: &'static str, context: &UpgradeContext) -> Result<(), String> {
            if self.aborts == Some(name) {
                std::process::abort();
            }
            if self.fails == Some(name) {
                return Err(format!("step {} failed.", name));
            }
            self.log.lock().unwrap().push(format!("{} {}", name, upgrade_on_record(context)));
            Ok(())
        }

        fn run(&self, root: &Path) -> Result<UpgradeReport, CoreError> {
            let a = |context: &UpgradeContext| self.step("a", context);
            let b = |context: &UpgradeContext| self.step("b", context);
            let c = |context: &UpgradeContext| self.step("c", context);
            let first: [UpgradeStep; 2] = [&a, &b];
            let second: [UpgradeStep; 1] = [&c];
            let migrations = [Migration { steps: &first }, Migration { steps: &second }];

            upgrade_warehouse(root, Head::Client, &UpgradeChain { reads: 3, migrations: &migrations })
        }

        fn taken(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    /// This build reads format 1 and upgrades nothing older: no format before the first is migrated.
    #[test]
    fn this_builds_chain_ends_at_its_format_and_starts_at_format_1() {
        assert_eq!(CHAIN.reads, WAREHOUSE_FORMAT);
        assert_eq!(CHAIN.floor(), 1);
    }

    /// Each migration runs in order with its own upgrade on record, a step sees the steps before it
    /// recorded done, and the mode is kept.
    ///
    /// Revert: record the next format before the steps run → a step sees `2@-` → red.
    #[test]
    fn an_upgrade_runs_every_migration_in_order_with_each_on_record() {
        let root = fresh_warehouse("walk");
        set_marker(&root, "format 1\nsparse\n");

        let chain = LoggedChain::new();
        let report = chain.run(&root).unwrap();

        assert_eq!(chain.taken(), ["a 1@2/0", "b 1@2/1", "c 2@3/0"]);
        assert_eq!(report, UpgradeReport { from: 1, to: 3, resumed: None, steps_run: 3 });
        assert_eq!(marker_entries(&root), "format 3\nsparse\n");

        std::fs::remove_dir_all(&root).ok();
    }

    /// Every bay's state folder is handed to the steps, the main tree's first, so a migration never
    /// leaves a bay in the old format.
    ///
    /// Revert: hand the steps only the main tree's folder → red.
    #[test]
    fn the_steps_are_given_every_bays_state_folder() {
        let root = fresh_warehouse("bays");
        set_marker(&root, "format 1\nfull\n");
        let bays = root.join(FOLDER_NAME_FORKLIFT_ROOT).join("bays");
        std::fs::create_dir_all(bays.join("feature")).unwrap();

        let seen = Mutex::new(Vec::new());
        let record = |context: &UpgradeContext| {
            seen.lock().unwrap().push(context.bay_state_dirs.clone());
            Ok(())
        };
        let steps: [UpgradeStep; 1] = [&record];
        let migrations = [Migration { steps: &steps }];
        upgrade_warehouse(&root, Head::Client, &UpgradeChain { reads: 2, migrations: &migrations }).unwrap();

        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.len(), 1);
        let names: Vec<_> = seen[0].iter().map(|dir| dir.strip_prefix(&root).unwrap().to_path_buf()).collect();
        assert_eq!(names, [PathBuf::from(FOLDER_NAME_FORKLIFT_ROOT), Path::new(FOLDER_NAME_FORKLIFT_ROOT).join("bays").join("feature")]);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A failing step leaves the upgrade on record with the steps before it done, and the next run
    /// resumes at that step, never running a recorded step again.
    ///
    /// Revert: resume one step past the one recorded → the rerun skips `b` → red.
    #[test]
    fn a_failed_step_is_resumed_at_that_step() {
        let root = fresh_warehouse("fail");
        set_marker(&root, "format 1\nfull\n");

        let failing = LoggedChain { fails: Some("b"), ..LoggedChain::new() };
        let error = failing.run(&root).unwrap_err();
        assert_eq!(code_of(&error), None);
        assert!(error.to_string().contains("to format 2 stopped at its step 2: step b failed."), "{}", error);
        assert_eq!(failing.taken(), ["a 1@2/0"]);
        assert_eq!(marker_entries(&root), "format 1\nfull\nupgrading 2 1\n");

        let chain = LoggedChain::new();
        let report = chain.run(&root).unwrap();
        assert_eq!(chain.taken(), ["b 1@2/1", "c 2@3/0"]);
        assert_eq!(report, UpgradeReport { from: 1, to: 3, resumed: Some(ResumedUpgrade { target: 2, step: 1 }), steps_run: 2 });
        assert_eq!(marker_entries(&root), "format 3\nfull\n");

        std::fs::remove_dir_all(&root).ok();
    }

    const CRASH_ROOT: &str = "FORKLIFT_TEST_UPGRADE_CRASH_ROOT";

    /// A process killed inside a step leaves the upgrade on record at that step, and the next run
    /// resumes it there. The kill is real: the test runs itself again in a child process whose
    /// step aborts.
    ///
    /// Revert: resume one step past the one recorded → the rerun skips `b` → red.
    #[test]
    fn a_process_killed_in_a_step_is_resumed_at_that_step() {
        if let Some(root) = std::env::var_os(CRASH_ROOT) {
            let chain = LoggedChain { aborts: Some("b"), ..LoggedChain::new() };
            let _ = chain.run(Path::new(&root));
            unreachable!("the step aborts the process");
        }

        let root = fresh_warehouse("crash");
        set_marker(&root, "format 1\nfull\n");

        let name = format!("{}::a_process_killed_in_a_step_is_resumed_at_that_step", module_path!());
        let name = name.strip_prefix("forklift_core::").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([name, "--exact", "--test-threads=1"])
            .env(CRASH_ROOT, &root)
            .output()
            .unwrap();
        assert!(!output.status.success(), "the child was not killed: {}", String::from_utf8_lossy(&output.stdout));
        assert_eq!(marker_entries(&root), "format 1\nfull\nupgrading 2 1\n");

        let chain = LoggedChain::new();
        let report = chain.run(&root).unwrap();
        assert_eq!(chain.taken(), ["b 1@2/1", "c 2@3/0"]);
        assert_eq!(report.resumed, Some(ResumedUpgrade { target: 2, step: 1 }));
        assert_eq!(marker_entries(&root), "format 3\nfull\n");

        std::fs::remove_dir_all(&root).ok();
    }

    /// While anything holds the format lease the upgrade refuses at once, before it changes anything.
    ///
    /// Revert: take the lease shared → the upgrade runs → red.
    #[test]
    fn an_upgrade_refuses_a_warehouse_in_use() {
        let root = fresh_warehouse("busy");
        set_marker(&root, "format 1\nfull\n");

        let held = lock_utils::FormatLease::shared(&root.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let chain = LoggedChain::new();
        let error = chain.run(&root).unwrap_err();
        assert_eq!(code_of(&error), Some(RefusalCode::WarehouseInUse), "{}", error);
        assert_eq!(chain.taken(), Vec::<String>::new());
        assert_eq!(marker_entries(&root), "format 1\nfull\n");

        drop(held);
        chain.run(&root).unwrap();
        assert_eq!(marker_entries(&root), "format 3\nfull\n");

        std::fs::remove_dir_all(&root).ok();
    }

    /// A warehouse already in the format the build reads is left as it is.
    #[test]
    fn a_current_warehouse_is_left_untouched() {
        let root = fresh_warehouse("current");
        let before = std::fs::read(marker_path(&root)).unwrap();

        let report = upgrade_warehouse(&root, Head::Client, &CHAIN).unwrap();
        assert_eq!(report, UpgradeReport { from: WAREHOUSE_FORMAT, to: WAREHOUSE_FORMAT, resumed: None, steps_run: 0 });
        assert_eq!(std::fs::read(marker_path(&root)).unwrap(), before);

        std::fs::remove_dir_all(&root).ok();
    }

    /// A marker the chain cannot carry to its format is refused with nothing changed: a newer format,
    /// an unfinished upgrade to a newer one, a format older than the chain reaches, an upgrade no
    /// build starts, and more steps done than the migration has.
    ///
    /// Revert: drop any one of the checks → its case runs steps → red.
    #[test]
    fn a_marker_the_chain_cannot_carry_is_refused_unchanged() {
        let cases: [(&str, Option<RefusalCode>, &str); 6] = [
            ("format 4\nfull\n", Some(RefusalCode::WarehouseFormatNewer), "newer than the format 3"),
            ("format 3\nfull\nupgrading 4 0\n", Some(RefusalCode::WarehouseUpgradeIncomplete), "to format 4 did not finish"),
            ("format 0\nfull\n", Some(RefusalCode::WarehouseFormatOlder), "upgrades no format older than format 1"),
            ("format 1\nfull\nupgrading 3 0\n", None, "from format 1 to format 3, which no version"),
            ("format 1\nfull\nupgrading 2 3\n", None, "records 3 steps done of the upgrade to format 2, which has 2"),
            ("format 4294967295\nfull\n", Some(RefusalCode::WarehouseFormatNewer), "newer than the format 3"),
        ];

        for (entries, code, said) in cases {
            let root = fresh_warehouse("refused");
            set_marker(&root, entries);

            let chain = LoggedChain::new();
            let error = chain.run(&root).unwrap_err();
            assert_eq!(code_of(&error), code, "{}: {}", entries, error);
            assert!(error.to_string().contains(said), "{}: {}", entries, error);
            assert_eq!(chain.taken(), Vec::<String>::new(), "{}", entries);
            assert_eq!(marker_entries(&root), entries);

            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// A step runs in the warehouse's storage-root scope, wherever the upgrade was started from.
    ///
    /// Revert: enter the scope only to list the bays → the step resolves the process's own root → red.
    #[test]
    fn a_step_resolves_storage_in_the_warehouse() {
        let root = fresh_warehouse("scope");
        set_marker(&root, "format 1\nfull\n");

        let seen = Mutex::new(None);
        let record = |_: &UpgradeContext| {
            *seen.lock().unwrap() = Some(globals::forklift_root());
            Ok(())
        };
        let steps: [UpgradeStep; 1] = [&record];
        let migrations = [Migration { steps: &steps }];
        upgrade_warehouse(&root, Head::Client, &UpgradeChain { reads: 2, migrations: &migrations }).unwrap();

        assert_eq!(seen.into_inner().unwrap(), Some(root.join(FOLDER_NAME_FORKLIFT_ROOT)));

        std::fs::remove_dir_all(&root).ok();
    }

    /// A step may rewrite the mode word, and the finished marker keeps what it wrote; a mode this
    /// build does not read at the format it reads leaves the upgrade on record instead of finishing.
    ///
    /// Revert: carry the mode read before the steps into the finished marker → `full` → red.
    #[test]
    fn the_finished_marker_keeps_the_mode_a_step_wrote_and_refuses_one_unread() {
        for (word, finishes) in [("cache", true), ("something-new", false)] {
            let root = fresh_warehouse("mode");
            set_marker(&root, "format 1\nfull\n");

            let rewrite = |context: &UpgradeContext| {
                let mut marker = warehouse_utils::read_marker(&context.forklift).unwrap().unwrap();
                marker.mode = word.to_string();
                warehouse_utils::write_marker(&context.forklift, &marker)
            };
            let steps: [UpgradeStep; 1] = [&rewrite];
            let migrations = [Migration { steps: &steps }];
            let result = upgrade_warehouse(&root, Head::Client, &UpgradeChain { reads: 2, migrations: &migrations });

            if finishes {
                result.unwrap();
                assert_eq!(marker_entries(&root), format!("format 2\n{}\n", word));
            } else {
                let error = result.unwrap_err();
                assert!(error.to_string().contains("does not read; the upgrade stays on record"), "{}", error);
                assert_eq!(marker_entries(&root), format!("format 1\n{}\nupgrading 2 1\n", word));
            }

            std::fs::remove_dir_all(&root).ok();
        }
    }

    /// The summary says where a resumed upgrade picked up, and that none of it was done when the
    /// crash came before its first step finished.
    #[test]
    fn the_summary_names_the_step_a_resume_starts_at() {
        let report = |step| UpgradeReport { from: 1, to: 2, resumed: Some(ResumedUpgrade { target: 2, step }), steps_run: 1 };
        assert_eq!(report(0).summary()[0], "Resumed the unfinished upgrade to format 2, which had done none of its steps.");
        assert_eq!(report(1).summary()[0], "Resumed the unfinished upgrade to format 2 at its step 2.");
        assert_eq!(UpgradeReport { from: 1, to: 1, resumed: None, steps_run: 0 }.summary(),
                   ["The warehouse is in format 1, the one this version reads. Nothing to do."]);
    }

    /// The format-older refusal's way out is making the warehouse again.
    #[test]
    fn a_format_older_than_the_chain_is_made_again() {
        let root = fresh_warehouse("older");
        set_marker(&root, "format 0\nfull\n");

        let Err(CoreError::Refusal { next_step, .. }) = upgrade_warehouse(&root, Head::Client, &CHAIN) else {
            panic!("format 0 was not refused");
        };
        assert!(next_step.starts_with("Move \""), "{}", next_step);
        assert!(next_step.contains("\"forklift prepare\""), "{}", next_step);

        std::fs::remove_dir_all(&root).ok();
    }

    /// An unfinished clone is not upgraded: running the clone again finishes it in this build's format.
    ///
    /// Revert: drop the franchise gate → the clone's warehouse is upgraded → red.
    #[test]
    fn an_unfinished_clone_is_not_upgraded() {
        let dir = fresh_dir("clone");
        let root = dir.join("t");
        let record = FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: None,
            request: Request::default(),
            genesis: None,
            bundle: BundleState::None,
            attempt: Attempt::Running,
            start_census: 0,
            format: WAREHOUSE_FORMAT,
            last_failure: None,
        };
        franchise_utils::claim(&root, &record).unwrap();
        prepare_at(&root);
        set_marker(&root, "format 1\nfull\n");

        let chain = LoggedChain::new();
        let error = chain.run(&root).unwrap_err();
        assert_eq!(code_of(&error), Some(RefusalCode::CloneIncomplete), "{}", error);
        assert_eq!(chain.taken(), Vec::<String>::new());
        assert_eq!(marker_entries(&root), "format 1\nfull\n");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A bay is upgraded with its warehouse, and the refusal names where.
    #[test]
    fn a_bay_is_sent_to_its_warehouse() {
        let root = fresh_warehouse("bay");
        let bay = root.join("feature");
        std::fs::create_dir_all(&bay).unwrap();
        bay_utils::write_bay_redirect(&bay, &root, "feature").unwrap();

        let Err(CoreError::Refusal { code, next_step, .. }) = upgrade_warehouse(&bay, Head::Client, &CHAIN) else {
            panic!("the bay was not refused");
        };
        assert_eq!(code, RefusalCode::NotAWarehouse);
        assert_eq!(next_step, format!("Run \"forklift upgrade\" in \"{}\".", root.to_string_lossy()));

        std::fs::remove_dir_all(&root).ok();
    }
}
