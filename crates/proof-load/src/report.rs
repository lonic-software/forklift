//! The report: JSON for machines, markdown for people, both opening with the coverage table.
//!
//! Rows are fault categories, kill points and power loss split into a client and a server row;
//! columns are platforms; each cell is "run", "sampled", or "not run: <reason>". No cell is ever
//! blank: the table is built from a total function over every (category, platform) pair.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::oracle::{Grade, Recoverability};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Category {
    DamagedFiles,
    KillPointsClient,
    KillPointsServer,
    PowerLossClient,
    PowerLossServer,
    DiskFull,
    ReadOnly,
    CaseFolding,
    NoExec,
    DeadPidLock,
    Network,
    ClockSkew,
    AwsHead,
}

impl Category {
    pub const ALL: [Category; 13] = [
        Category::DamagedFiles, Category::KillPointsClient, Category::KillPointsServer, Category::PowerLossClient,
        Category::PowerLossServer, Category::DiskFull, Category::ReadOnly, Category::CaseFolding, Category::NoExec,
        Category::DeadPidLock, Category::Network, Category::ClockSkew, Category::AwsHead,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Category::DamagedFiles => "damaged files",
            Category::KillPointsClient => "kill points (client)",
            Category::KillPointsServer => "kill points (server)",
            Category::PowerLossClient => "power loss (client)",
            Category::PowerLossServer => "power loss (server)",
            Category::DiskFull => "disk full",
            Category::ReadOnly => "read-only",
            Category::CaseFolding => "case-folding volume",
            Category::NoExec => "no-exec volume",
            Category::DeadPidLock => "dead-PID lock",
            Category::Network => "network proxy",
            Category::ClockSkew => "clock skew",
            Category::AwsHead => "AWS head",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Platform {
    Linux,
    Macos,
    Windows,
}

impl Platform {
    pub const ALL: [Platform; 3] = [Platform::Linux, Platform::Macos, Platform::Windows];

    pub fn current() -> Platform {
        if cfg!(target_os = "macos") { Platform::Macos } else if cfg!(windows) { Platform::Windows } else { Platform::Linux }
    }

    pub fn label(self) -> &'static str {
        match self { Platform::Linux => "Linux", Platform::Macos => "macOS", Platform::Windows => "Windows" }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case", deny_unknown_fields)]
pub enum Cell {
    Run,
    Sampled,
    NotRun { reason: String },
}

impl Cell {
    pub fn not_run(reason: &str) -> Cell {
        Cell::NotRun { reason: reason.to_string() }
    }

    fn text(&self) -> String {
        match self {
            Cell::Run => "run".to_string(),
            Cell::Sampled => "sampled".to_string(),
            Cell::NotRun { reason } => format!("not run: {}", reason),
        }
    }
}

/// Why a category never runs on a platform, whatever this run did; `None` where it can run.
pub fn platform_gap(category: Category, platform: Platform) -> Option<&'static str> {
    use Category::*;
    match (category, platform) {
        (ClockSkew, _) => Some("no clock-skew mechanism yet"),
        (AwsHead, _) => Some("the AWS head is not driven yet"),
        (KillPointsClient | KillPointsServer, Platform::Macos) =>
            Some("no unprivileged syscall injection for an unmodified binary (dtrace needs SIP disabled)"),
        (PowerLossClient | PowerLossServer, Platform::Macos) =>
            Some("no unprivileged syscall trace of an unmodified binary (dtrace needs SIP disabled)"),
        (KillPointsClient | KillPointsServer, Platform::Windows) => Some("no syscall injection on Windows in this harness"),
        (PowerLossClient | PowerLossServer, Platform::Windows) => Some("no syscall trace on Windows in this harness"),
        (DiskFull | CaseFolding | NoExec, Platform::Windows) => Some("no volume of this kind is built on Windows in this harness"),
        _ => None,
    }
}

/// The coverage table. Built only by `CoverageTable::new`, which fills every cell.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CoverageTable {
    pub cells: BTreeMap<Category, BTreeMap<Platform, Cell>>,
    /// Per kill-point cell, how many targets were not reached.
    pub not_reached: BTreeMap<Category, u64>,
}

impl CoverageTable {
    /// The table for a run on `ran_on`, where `ran` says what this run did for each category it
    /// attempted there. Every other platform's cells say they were not run on this machine.
    pub fn new(ran_on: Platform, mut ran: impl FnMut(Category) -> Cell) -> CoverageTable {
        let mut cells = BTreeMap::new();
        for category in Category::ALL {
            let mut row = BTreeMap::new();
            for platform in Platform::ALL {
                let cell = match platform_gap(category, platform) {
                    Some(reason) => Cell::not_run(reason),
                    None if platform != ran_on => Cell::not_run("this run was on another platform"),
                    None => ran(category),
                };
                row.insert(platform, cell);
            }
            cells.insert(category, row);
        }
        CoverageTable { cells, not_reached: BTreeMap::new() }
    }

    fn markdown(&self, out: &mut String) {
        let _ = writeln!(out, "| Fault | {} |", Platform::ALL.map(Platform::label).join(" | "));
        let _ = writeln!(out, "|---|{}", "---|".repeat(Platform::ALL.len()));
        for (category, row) in &self.cells {
            let mut label = category.label().to_string();
            if let Some(count) = self.not_reached.get(category) {
                let _ = write!(label, " ({} not reached)", count);
            }
            let cells: Vec<String> = row.values().map(Cell::text).collect();
            let _ = writeln!(out, "| {} | {} |", label, cells.join(" | "));
        }
    }
}

/// What determines a run: replaying with the same five reproduces it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    pub seed: u64,
    /// Each tool's path, then the first line of its `--version` output in parentheses.
    pub tools: BTreeMap<String, String>,
    pub scenarios: Vec<String>,
    pub platform: Platform,
    /// The environment the tools saw, scratch paths included.
    pub environment: BTreeMap<String, String>,
}

/// One graded row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub scenario: String,
    pub category: Category,
    /// The tool and binding the row graded: `forklift`, `git`, `git-tuned`.
    pub binding: String,
    pub grade: Grade,
    pub recoverability: Recoverability,
    pub note: Option<String>,
    /// Where the row's details are, relative to the output directory: a file name, then `#` and a
    /// JSON pointer to the object in it that holds the row's `steps`, each step's arguments, exit
    /// and output. `None` where the run wrote no details for the row.
    pub detail: Option<String>,
}

/// A completeness check's outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completeness {
    pub check: String,
    pub cell: Cell,
    /// What the check found missing; a run cell with anything here is a failure.
    pub missing: Vec<String>,
}

impl Completeness {
    /// The wall-clock budget: a run that took longer than `budget` fails, though every scenario
    /// ran, so a slow scenario is noticed rather than dropped. No budget given is not run.
    pub fn budget(elapsed: std::time::Duration, budget: Option<std::time::Duration>) -> Completeness {
        let check = "wall-clock budget".to_string();
        match budget {
            None => Completeness { check, cell: Cell::not_run("no budget given"), missing: vec![] },
            Some(budget) => {
                let missing = if elapsed > budget {
                    vec![format!("the run took {} s, over its budget of {} s", elapsed.as_secs(), budget.as_secs())]
                } else {
                    vec![]
                };
                Completeness { check, cell: Cell::Run, missing }
            }
        }
    }
}

/// The report's format. A reader refuses any other, so a report written by another build of the
/// harness is never read as if it meant the same thing.
pub const REPORT_FORMAT: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// Always `REPORT_FORMAT` when written.
    pub format: u32,
    pub coverage: CoverageTable,
    pub determinism: Determinism,
    pub completeness: Vec<Completeness>,
    /// Rows every bound tool ran.
    pub rows: Vec<Row>,
    /// Rows of scenarios with an operation git has no binding for, never counted as git losses.
    pub forklift_only: Vec<Row>,
}

impl Report {
    /// Whether the run failed: a failing grade, or a completeness check that ran and found a gap.
    pub fn failed(&self) -> bool {
        self.rows.iter().chain(&self.forklift_only).any(|row| row.grade.is_failure())
            || self.completeness.iter().any(|check| !check.missing.is_empty())
    }

    /// Reads a `report.json`. Refuses a format other than `REPORT_FORMAT` by number before
    /// anything else is read, and any field this build does not know.
    pub fn from_json(text: &str) -> Result<Report, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("not a report: {}", error))?;
        match value.get("format").and_then(serde_json::Value::as_u64) {
            Some(format) if format == u64::from(REPORT_FORMAT) => {}
            Some(format) => return Err(format!("report format {} is not one this build reads (it reads {})", format, REPORT_FORMAT)),
            None => return Err("not a report: it names no format".to_string()),
        }
        serde_json::from_value(value).map_err(|error| format!("not a report of format {}: {}", REPORT_FORMAT, error))
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| format!("{{\"error\":\"{}\"}}", error))
    }

    pub fn markdown(&self) -> String {
        let mut out = String::from("# Proof Load report\n\n## Coverage\n\n");
        self.coverage.markdown(&mut out);

        out.push_str("\n## Completeness\n\n");
        for check in &self.completeness {
            let _ = writeln!(out, "- {}: {}", check.check, check.cell.text());
            for missing in &check.missing {
                let _ = writeln!(out, "  - missing: {}", missing);
            }
        }

        for (title, rows) in [("Grades", &self.rows), ("Forklift-only grades", &self.forklift_only)] {
            let _ = writeln!(out, "\n## {}\n", title);
            if rows.is_empty() {
                out.push_str("None.\n");
                continue;
            }
            out.push_str("| Scenario | Fault | Binding | Grade | Commands to expected | Right remedy | Outside knowledge |\n|---|---|---|---|---|---|---|\n");
            for row in rows {
                let grade = serde_json::to_value(row.grade).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default();
                let commands = row.recoverability.commands_to_expected.map_or("never".to_string(), |n| n.to_string());
                let outside = if row.recoverability.outside_knowledge.is_empty() { "none".to_string() } else { row.recoverability.outside_knowledge.join("; ") };
                let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} | {} |", row.scenario, row.category.label(), row.binding, grade, commands,
                    if row.recoverability.named_right_remedy { "yes" } else { "no" }, outside);
            }
            if rows.iter().any(|row| row.grade == Grade::DataLost) {
                out.push_str("\nData lost is decided by readback: lost as far as the tool can tell the user. The bytes may still sit undecoded in the store.\n");
            }
        }

        let _ = writeln!(out, "\n## Determinism\n\n- seed: {}\n- platform: {}", self.determinism.seed, self.determinism.platform.label());
        for (tool, version) in &self.determinism.tools {
            let _ = writeln!(out, "- {}: {}", tool, version);
        }
        let _ = writeln!(out, "- scenarios: {}", self.determinism.scenarios.join(", "));
        let _ = writeln!(out, "- environment: {}", self.determinism.environment.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(" "));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every (category, platform) cell is filled, and the documented gaps hold whatever a run says.
    ///
    /// Revert: let `ran` decide a gapped cell → macOS kill points read "run".
    #[test]
    fn every_cell_is_filled_and_gaps_are_not_overridden() {
        let table = CoverageTable::new(Platform::Macos, |_| Cell::Run);
        assert_eq!(table.cells.len(), Category::ALL.len());
        for row in table.cells.values() {
            assert_eq!(row.len(), Platform::ALL.len());
        }
        assert!(matches!(table.cells[&Category::KillPointsClient][&Platform::Macos], Cell::NotRun { .. }));
        assert_eq!(table.cells[&Category::DamagedFiles][&Platform::Macos], Cell::Run);
        assert!(matches!(table.cells[&Category::DamagedFiles][&Platform::Linux], Cell::NotRun { .. }));
    }

    #[test]
    fn the_markdown_opens_with_the_coverage_table() {
        let report = Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |_| Cell::not_run("not built yet")),
            determinism: Determinism { seed: 7, tools: BTreeMap::new(), scenarios: vec![], platform: Platform::Linux, environment: BTreeMap::new() },
            completeness: vec![],
            rows: vec![],
            forklift_only: vec![],
        };
        let markdown = report.markdown();
        assert!(markdown.starts_with("# Proof Load report\n\n## Coverage\n\n| Fault | Linux | macOS | Windows |"));
        assert!(!markdown.contains("|  |"), "no blank cell");
        assert!(!report.failed());
    }

    /// Revert: ignore completeness gaps in `failed` → a missing binding passes the lane.
    #[test]
    fn a_completeness_gap_fails_the_run() {
        let report = Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |_| Cell::Run),
            determinism: Determinism { seed: 1, tools: BTreeMap::new(), scenarios: vec![], platform: Platform::Linux, environment: BTreeMap::new() },
            completeness: vec![Completeness { check: "damage".to_string(), cell: Cell::Run, missing: vec!["trust".to_string()] }],
            rows: vec![],
            forklift_only: vec![],
        };
        assert!(report.failed());
    }

    /// A run over its budget fails, one within it does not, and without a budget the check says
    /// it did not run.
    ///
    /// Revert: compare against the budget with `<` → the slow run passes and the quick one fails.
    #[test]
    fn a_run_over_its_budget_fails() {
        use std::time::Duration;
        let over = Completeness::budget(Duration::from_secs(61), Some(Duration::from_secs(60)));
        assert_eq!(over.missing, vec!["the run took 61 s, over its budget of 60 s"]);
        assert_eq!(over.cell, Cell::Run);
        assert_eq!(Completeness::budget(Duration::from_secs(59), Some(Duration::from_secs(60))).missing, Vec::<String>::new());
        let unset = Completeness::budget(Duration::from_secs(10_000), None);
        assert!(unset.missing.is_empty() && matches!(unset.cell, Cell::NotRun { .. }));
    }

    fn sample() -> Report {
        Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |category| match category {
                Category::DamagedFiles => Cell::Run,
                _ => Cell::not_run("not asked for"),
            }),
            determinism: Determinism { seed: 3, tools: BTreeMap::new(), scenarios: vec!["trust".to_string()], platform: Platform::Linux, environment: BTreeMap::new() },
            completeness: vec![Completeness { check: "damage".to_string(), cell: Cell::Run, missing: vec![] }],
            rows: vec![Row {
                scenario: "trust".to_string(), category: Category::DamagedFiles, binding: "forklift".to_string(), grade: Grade::RefusedWorkingRemedy,
                recoverability: Recoverability { commands_to_expected: Some(2), named_right_remedy: true, outside_knowledge: vec![] },
                note: None, detail: Some("trust-head.json#".to_string()),
            }],
            forklift_only: vec![],
        }
    }

    /// Changes the sample's JSON with `change`, then reads it back.
    fn read_changed(change: impl FnOnce(&mut serde_json::Value)) -> Result<Report, String> {
        let mut value: serde_json::Value = serde_json::from_str(&sample().json()).expect("the sample is JSON");
        change(&mut value);
        Report::from_json(&value.to_string())
    }

    /// What a run writes, a reader reads back whole.
    #[test]
    fn a_report_reads_back_what_was_written() {
        let report = sample();
        let read = Report::from_json(&report.json()).expect("the report reads back");
        assert_eq!(read.json(), report.json());
        assert_eq!(read.rows, report.rows);
    }

    /// A report of another format, or of none, is refused by number before anything else is read.
    ///
    /// Revert: read any format → the report of format 2 reads.
    #[test]
    fn a_report_of_another_format_is_refused() {
        let other = read_changed(|value| value["format"] = serde_json::json!(REPORT_FORMAT + 1)).map(|_| ()).unwrap_err();
        assert_eq!(other, format!("report format {} is not one this build reads (it reads {})", REPORT_FORMAT + 1, REPORT_FORMAT));
        let none = read_changed(|value| { value.as_object_mut().expect("an object").remove("format"); }).map(|_| ()).unwrap_err();
        assert_eq!(none, "not a report: it names no format");
        assert!(read_changed(|_| ()).is_ok(), "the unchanged sample reads");
    }

    /// A field this build does not know is refused, never dropped, in a row and in a coverage cell.
    ///
    /// Revert: drop `deny_unknown_fields` from `Row` or from `Cell` → that report reads.
    #[test]
    fn a_report_with_an_unknown_field_is_refused() {
        let in_row = read_changed(|value| value["rows"][0]["weight"] = serde_json::json!(1)).map(|_| ()).unwrap_err();
        assert!(in_row.contains("unknown field `weight`"), "{}", in_row);
        let in_cell = read_changed(|value| value["coverage"]["cells"]["kill-points-client"]["linux"]["weight"] = serde_json::json!(1)).map(|_| ()).unwrap_err();
        assert!(in_cell.contains("unknown field `weight`"), "{}", in_cell);
    }
}
