//! The report: JSON for machines, markdown for people, both opening with the coverage table.
//!
//! Rows are fault categories, kill points and power loss split into a client and a server row;
//! columns are platforms; each cell is "run", "sampled", or "not run: <reason>". No cell is ever
//! blank: the table is built from a total function over every (category, platform) pair.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::damage::{Kind, Warehouse};
use crate::fidelity::{Finding, Input};
use crate::git_layout::Role;
use crate::oracle::{Grade, Recoverability, Rescue};
use crate::scenario::Tool;
use crate::tool::Outcome;

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
    Fidelity,
}

impl Category {
    pub const ALL: [Category; 14] = [
        Category::DamagedFiles, Category::KillPointsClient, Category::KillPointsServer, Category::PowerLossClient,
        Category::PowerLossServer, Category::DiskFull, Category::ReadOnly, Category::CaseFolding, Category::NoExec,
        Category::DeadPidLock, Category::Network, Category::ClockSkew, Category::AwsHead, Category::Fidelity,
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
            Category::Fidelity => "round-trip fidelity",
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
#[serde(tag = "state", rename_all = "kebab-case", try_from = "WrittenCell")]
pub enum Cell {
    Run,
    Sampled,
    NotRun { reason: String },
}

/// A cell as written, read field by field: serde's `deny_unknown_fields` lets an unknown field
/// through on an internally tagged unit variant, so `Cell` is read through this instead.
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct WrittenCell {
    state: String,
    reason: Option<String>,
}

impl TryFrom<WrittenCell> for Cell {
    type Error = String;

    fn try_from(written: WrittenCell) -> Result<Cell, String> {
        match (written.state.as_str(), written.reason) {
            ("run", None) => Ok(Cell::Run),
            ("sampled", None) => Ok(Cell::Sampled),
            ("not-run", Some(reason)) => Ok(Cell::NotRun { reason }),
            ("run" | "sampled", Some(_)) => Err(format!("a {} cell names no reason", written.state)),
            ("not-run", None) => Err("a not-run cell names its reason".to_string()),
            (state, _) => Err(format!("unknown cell state `{}`", state)),
        }
    }
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

/// What determines a run, the data plate's header: replaying with the same seed, tools,
/// configurations, scenarios and platform reproduces it.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Determinism {
    pub seed: u64,
    /// Each tool's path, then the first line of its `--version` output in parentheses.
    pub tools: BTreeMap<String, String>,
    pub scenarios: Vec<String>,
    pub platform: Platform,
    /// The operating system's name and release.
    pub os: String,
    /// The type of the filesystem the run's folder is on, where every scratch area lives except
    /// the volumes a lane is given.
    pub filesystem: String,
    /// Each git subject's configuration by its label: what every repository the harness creates
    /// for it holds, and for git tuned the options it runs with besides. Empty with no git given.
    pub configurations: BTreeMap<String, String>,
    /// The history `--history-from` named, when the fidelity round trips took it: its path, then
    /// its refs' count and a digest of their listing, since a path alone does not pin a history.
    pub history: Option<String>,
    /// The environment the tools saw, scratch paths included.
    pub environment: BTreeMap<String, String>,
}

/// One graded row.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    /// The tool under test whose result the row is: forklift, git or git tuned.
    pub subject: Tool,
    pub scenario: String,
    pub category: Category,
    /// What the row faulted: `forklift`, `forklift-server` for a hosted scenario's server, or for a
    /// git subject its label (`git`, `git-tuned`: one program, two configurations). A row's steps
    /// are the commands of its subject that its scenario ran.
    pub binding: String,
    pub grade: Grade,
    pub recoverability: Recoverability,
    pub note: Option<String>,
    /// Where the row's details are, relative to the output directory: a file name, then `#` and a
    /// JSON pointer to the object in it that holds the row's `steps`, each step's arguments, exit
    /// and output. `None` where the run wrote no details for the row.
    pub detail: Option<String>,
    /// What sets the row beside other subjects' rows on the data plate: a damage row's. None for
    /// a row of any other lane, where no subject but forklift runs yet.
    pub pairing: Option<Pairing>,
}

/// A damage row's place on the data plate: rows of every subject with the same scenario, side,
/// role and kind of damage are one line of it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pairing {
    pub scenario: String,
    /// Whose files were damaged: the client's, or the host's the scenario pushed to.
    pub side: Warehouse,
    /// The role of the damaged path, from the role map: what the path is for in either subject.
    pub role: Role,
    pub kind: Kind,
}

/// One subject's standing on a line of the data plate: its worst row there, by grade and then by
/// rescue kind, since a role is as robust as its weakest path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Standing {
    pub grade: Grade,
    pub rescued_by: Option<Rescue>,
    /// The worst row's details, as the row names them.
    pub detail: Option<String>,
    /// How many of the subject's rows the line holds.
    pub rows: usize,
}

impl Standing {
    /// The plate's order, lower is better: the grade's severity, then the rescue kind, none
    /// before a local repair before a re-clone. Step counts never enter it.
    pub fn rank(&self) -> (u8, Option<Rescue>) {
        (self.grade.severity(), self.rescued_by)
    }
}

/// One line of the data plate: a pairing and the standing of each subject with rows under it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlateLine {
    pub pairing: Pairing,
    pub standings: BTreeMap<Tool, Standing>,
}

impl PlateLine {
    /// The git subjects that do better than forklift on this line: a better grade, or the same
    /// grade and a better rescue kind. A line with no counterpart is never a win, nor is one
    /// forklift has no row on.
    pub fn git_does_better(&self) -> Vec<Tool> {
        if self.pairing.role == Role::NoCounterpart {
            return vec![];
        }
        let Some(forklift) = self.standings.get(&Tool::Forklift) else { return vec![] };
        self.standings.iter().filter(|(subject, standing)| **subject != Tool::Forklift && standing.rank() < forklift.rank())
            .map(|(subject, _)| *subject).collect()
    }
}

impl Row {
    /// The commands the row's details hold, in order: the file `detail` names, under `out`, the
    /// directory the run wrote the report to, and the `steps` of the object its JSON pointer picks.
    /// Refuses a row with no details, a `detail` without its `#`, a file name that leaves `out`,
    /// and a file, pointer or steps that cannot be read, saying which.
    pub fn steps(&self, out: &std::path::Path) -> Result<Vec<Outcome>, String> {
        let detail = self.detail.as_deref().ok_or("the run wrote no details for this row")?;
        let (file, pointer) = detail.split_once('#').ok_or_else(|| format!("the row's details `{}` name no JSON pointer", detail))?;
        let relative = std::path::Path::new(file);
        if file.is_empty() || !relative.components().all(|part| matches!(part, std::path::Component::Normal(_))) {
            return Err(format!("the row's details file `{}` is not a file name inside the run's folder", file));
        }
        let path = out.join(relative);
        let text = std::fs::read_to_string(&path).map_err(|error| format!("cannot read {}: {}", path.display(), error))?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|error| format!("{} is not JSON: {}", path.display(), error))?;
        let object = value.pointer(pointer).ok_or_else(|| format!("{} holds nothing at `{}`", path.display(), pointer))?;
        let steps = object.get("steps").ok_or_else(|| format!("{} holds no steps at `{}`", path.display(), pointer))?;
        serde_json::from_value(steps.clone()).map_err(|error| format!("the steps in {} at `{}` cannot be read: {}", path.display(), pointer, error))
    }
}

/// A completeness check's outcome.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Completeness {
    /// The subject the check is about; none for one about the run itself.
    pub subject: Option<Tool>,
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
            None => Completeness { subject: None, check, cell: Cell::not_run("no budget given"), missing: vec![] },
            Some(budget) => {
                let missing = if elapsed > budget {
                    vec![format!("the run took {} s, over its budget of {} s", elapsed.as_secs(), budget.as_secs())]
                } else {
                    vec![]
                };
                Completeness { subject: None, check, cell: Cell::Run, missing }
            }
        }
    }
}

/// The report's format. A reader refuses any other, so a report written by another build of the
/// harness is never read as if it meant the same thing.
pub const REPORT_FORMAT: u32 = 6;

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
    /// What each round trip changed or dropped: findings, never grades, so none fails the run.
    pub fidelity: Vec<Finding>,
}

impl Report {
    /// Whether the run failed: a failing grade of forklift's, or a completeness check of forklift's
    /// or of the run itself that found a gap. Another subject's grade or gap is a comparison, never
    /// a failure of the run: a git gap shows as git not fully measured.
    pub fn failed(&self) -> bool {
        self.rows.iter().chain(&self.forklift_only).any(|row| row.subject == Tool::Forklift && row.grade.is_failure())
            || self.completeness.iter().any(|check| check.subject.is_none_or(|subject| subject == Tool::Forklift) && !check.missing.is_empty())
    }

    /// The git subjects' completeness checks that found a gap or did not run: where any is, git
    /// was not fully measured. None in a run given no git, which asked for no comparison: the run
    /// names git among its tools, by its label, exactly when it was given one.
    pub fn git_gaps(&self) -> Vec<&Completeness> {
        if !self.determinism.tools.contains_key(Tool::Git.label()) {
            return vec![];
        }
        self.completeness.iter()
            .filter(|check| check.subject.is_some_and(|subject| subject != Tool::Forklift))
            .filter(|check| !check.missing.is_empty() || matches!(check.cell, Cell::NotRun { .. }))
            .collect()
    }

    /// The data plate: every paired row, one line per pairing, in the pairings' order.
    pub fn plate(&self) -> Vec<PlateLine> {
        let mut lines: BTreeMap<Pairing, BTreeMap<Tool, Standing>> = BTreeMap::new();
        for row in &self.rows {
            let Some(pairing) = &row.pairing else { continue };
            let standing = Standing { grade: row.grade, rescued_by: row.recoverability.rescued_by, detail: row.detail.clone(), rows: 1 };
            let standings = lines.entry(pairing.clone()).or_default();
            match standings.get_mut(&row.subject) {
                None => { standings.insert(row.subject, standing); }
                Some(held) => {
                    let rows = held.rows + 1;
                    if standing.rank() > held.rank() {
                        *held = standing;
                    }
                    held.rows = rows;
                }
            }
        }
        lines.into_iter().map(|(pairing, standings)| PlateLine { pairing, standings }).collect()
    }

    /// Reads a `report.json`. Refuses a format other than `REPORT_FORMAT` by number before
    /// anything else is read, and any field this build does not know.
    pub fn from_json(text: &str) -> Result<Report, String> {
        let value: serde_json::Value = serde_json::from_str(text).map_err(|error| format!("not a report: {}", error))?;
        match value.get("format") {
            Some(format) if format.as_u64() == Some(u64::from(REPORT_FORMAT)) => {}
            Some(format) => return Err(format!("report format {} is not one this build reads (it reads {})", format, REPORT_FORMAT)),
            None => return Err("not a report: it names no format".to_string()),
        }
        serde_json::from_value(value).map_err(|error| format!("not a report of format {}: {}", REPORT_FORMAT, error))
    }

    pub fn json(&self) -> String {
        serde_json::to_string_pretty(self).unwrap_or_else(|error| format!("{{\"error\":\"{}\"}}", error))
    }

    pub fn markdown(&self) -> String {
        let mut out = String::from("# Proof Load report\n\n## Header\n\n");
        let determinism = &self.determinism;
        let _ = writeln!(out, "- seed: {}\n- platform: {} ({})\n- filesystem: {}", determinism.seed, determinism.platform.label(), determinism.os, determinism.filesystem);
        for (tool, version) in &determinism.tools {
            let _ = writeln!(out, "- {}: {}", tool, version);
        }
        for (subject, configuration) in &determinism.configurations {
            let _ = writeln!(out, "- {} configuration: {}", subject, configuration);
        }
        if let Some(history) = &determinism.history {
            let _ = writeln!(out, "- history from: {}", history);
        }
        let _ = writeln!(out, "- scenarios: {}", determinism.scenarios.join(", "));
        let _ = writeln!(out, "- environment: {}", determinism.environment.iter().map(|(k, v)| format!("{}={}", k, v)).collect::<Vec<_>>().join(" "));

        out.push_str("\n## Coverage\n\n");
        self.coverage.markdown(&mut out);

        out.push_str("\n## Completeness\n\n");
        for check in &self.completeness {
            let subject = check.subject.map(|subject| format!(" ({})", subject.label())).unwrap_or_default();
            let _ = writeln!(out, "- {}{}: {}", check.check, subject, check.cell.text());
            for missing in &check.missing {
                let _ = writeln!(out, "  - missing: {}", missing);
            }
        }
        let gaps = self.git_gaps();
        if !gaps.is_empty() {
            let named: Vec<String> = gaps.iter().map(|check| format!("{} ({})", check.check, check.subject.map_or("", Tool::label))).collect();
            let _ = writeln!(out, "\ngit: not fully measured, by {}. Git's gaps never fail the run.", named.join(", "));
        }

        let plate = self.plate();
        let subjects = [Tool::Forklift, Tool::Git, Tool::GitTuned];
        out.push_str("\n## Data plate\n\n");
        if plate.is_empty() {
            out.push_str("None.\n");
        } else {
            let _ = writeln!(out, "| Scenario | Side | Role | Damage | {} |", subjects.map(Tool::label).join(" | "));
            let _ = writeln!(out, "|---|---|---|---|{}", "---|".repeat(subjects.len()));
            for line in &plate {
                let cells: Vec<String> = subjects.iter().map(|subject| match line.standings.get(subject) {
                    None if line.pairing.role == Role::NoCounterpart => "no counterpart".to_string(),
                    None => "not measured".to_string(),
                    Some(standing) => standing_text(standing),
                }).collect();
                let _ = writeln!(out, "| {} | {} | {} | {} | {} |", line.pairing.scenario, kebab(line.pairing.side), kebab(line.pairing.role), kebab(line.pairing.kind), cells.join(" | "));
            }
            out.push_str("\nA subject's cell is its worst row on the line, by grade and then by rescue kind. A subject with no row on a line is not measured there, unless the role has no counterpart in it: such a line is never compared.\n");
        }

        out.push_str("\n## Git does better\n\n");
        let wins: Vec<(&PlateLine, Tool)> = plate.iter().flat_map(|line| line.git_does_better().into_iter().map(move |subject| (line, subject))).collect();
        if wins.is_empty() {
            out.push_str("None.\n");
        } else {
            for (line, subject) in wins {
                let _ = writeln!(out, "- {} {} {} {}: {} {}, forklift {}", line.pairing.scenario, kebab(line.pairing.side), kebab(line.pairing.role), kebab(line.pairing.kind),
                    subject.label(), standing_text(&line.standings[&subject]), standing_text(&line.standings[&Tool::Forklift]));
            }
            out.push_str("\nEach entry is a better grade, or the same grade and a better rescue kind. Each gets a ticket.\n");
        }

        for (title, rows) in [("Grades", &self.rows), ("Forklift-only grades", &self.forklift_only)] {
            let _ = writeln!(out, "\n## {}\n", title);
            if rows.is_empty() {
                out.push_str("None.\n");
                continue;
            }
            out.push_str("| Subject | Scenario | Fault | Binding | Grade | Commands to expected | Right remedy | Rescued by | Outside knowledge |\n|---|---|---|---|---|---|---|---|---|\n");
            for row in rows {
                let commands = row.recoverability.commands_to_expected.map_or("never".to_string(), |n| n.to_string());
                let outside = if row.recoverability.outside_knowledge.is_empty() { "none".to_string() } else { row.recoverability.outside_knowledge.join("; ") };
                let rescued = row.recoverability.rescued_by.map_or("none", Rescue::label);
                let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} | {} | {} | {} |", row.subject.label(), row.scenario, row.category.label(), row.binding, kebab(row.grade), commands,
                    if row.recoverability.named_right_remedy { "yes" } else { "no" }, rescued, outside);
            }
            if rows.iter().any(|row| row.grade == Grade::DataLost) {
                out.push_str("\nData lost is decided by readback: lost as far as the tool can tell the user. The bytes may still sit undecoded in the store.\n");
            }
        }

        out.push_str("\n## Fidelity\n\n");
        let (fixture, history): (Vec<&Finding>, Vec<&Finding>) = self.fidelity.iter().partition(|finding| finding.input == Input::Fixture);
        let cell = |value: &Option<String>| value.as_deref().map_or("nothing".to_string(), |value| value.replace('|', "\\|"));
        if fixture.is_empty() {
            out.push_str("None from the fixtures.\n");
        } else {
            out.push_str("| Direction | Field | At | Went in | Came back |\n|---|---|---|---|---|\n");
            for finding in &fixture {
                let _ = writeln!(out, "| {} | {} | {} | {} | {} |", finding.direction.label(), finding.field.replace('|', "\\|"), finding.at.replace('|', "\\|"), cell(&finding.before), cell(&finding.after));
            }
        }
        // A real history's findings run to thousands, so the markdown counts them by field and
        // shows one of each, the first the walk found; report.json lists every one.
        if history.is_empty() && self.determinism.history.is_some() {
            out.push_str("\nNone from the given history.\n");
        } else if !history.is_empty() {
            let mut fields: BTreeMap<(crate::fidelity::Direction, &str), (usize, &Finding)> = BTreeMap::new();
            for finding in &history {
                fields.entry((finding.direction, finding.field.as_str())).or_insert((0, finding)).0 += 1;
            }
            let _ = writeln!(out, "\nFrom the given history, {} finding{} by field (report.json lists each):\n", history.len(), if history.len() == 1 { "" } else { "s" });
            out.push_str("| Direction | Field | Findings | One at | Went in | Came back |\n|---|---|---|---|---|---|\n");
            for ((direction, field), (count, first)) in &fields {
                let _ = writeln!(out, "| {} | {} | {} | {} | {} | {} |", direction.label(), field.replace('|', "\\|"), count, first.at.replace('|', "\\|"), cell(&first.before), cell(&first.after));
            }
        }
        if !self.fidelity.is_empty() {
            out.push_str("\nEach finding is a field a round trip changed or dropped, compared commit by commit or record by record, never by id. Findings never fail the run; each loss class gets a ticket.\n");
        }
        out
    }
}

/// A value as the JSON names it: a grade, a role, a side or a kind.
fn kebab(value: impl serde::Serialize) -> String {
    serde_json::to_value(value).ok().and_then(|value| value.as_str().map(str::to_string)).unwrap_or_default()
}

/// A subject's standing as a plate cell: its grade, what rescued it, a link to its details and,
/// where the line holds more than one of its rows, how many.
fn standing_text(standing: &Standing) -> String {
    let mut text = kebab(standing.grade);
    if let Some(rescue) = standing.rescued_by {
        let _ = write!(text, ", {}", rescue.label());
    }
    if let Some(detail) = &standing.detail {
        text = format!("[{}]({})", text, detail);
    }
    if standing.rows > 1 {
        let _ = write!(text, " (worst of {})", standing.rows);
    }
    text
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
    fn the_markdown_opens_with_the_header_then_the_coverage_table() {
        let report = Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |_| Cell::not_run("not built yet")),
            determinism: determinism(7, vec![]),
            completeness: vec![],
            rows: vec![],
            forklift_only: vec![],
            fidelity: vec![],
        };
        let markdown = report.markdown();
        assert!(markdown.starts_with("# Proof Load report\n\n## Header\n\n- seed: 7\n- platform: Linux (Linux 6.1)\n- filesystem: ext2/ext3/ext4\n"), "{}", markdown);
        assert!(markdown.contains("- git-tuned configuration: core.commitGraph=true\n"), "{}", markdown);
        assert!(markdown.contains("\n\n## Coverage\n\n| Fault | Linux | macOS | Windows |"));
        assert!(!markdown.contains("|  |"), "no blank cell");
        assert!(!report.failed());
    }

    /// Revert: ignore completeness gaps in `failed` → a missing binding passes the lane.
    #[test]
    fn a_completeness_gap_fails_the_run() {
        let report = Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |_| Cell::Run),
            determinism: determinism(1, vec![]),
            completeness: vec![Completeness { subject: Some(Tool::Forklift), check: "damage".to_string(), cell: Cell::Run, missing: vec!["trust".to_string()] }],
            rows: vec![],
            forklift_only: vec![],
            fidelity: vec![],
        };
        assert!(report.failed());
    }

    /// A git subject's failing row is reported, not failed on: only forklift's grades fail the run.
    ///
    /// Revert: drop the subject test in `failed` → git's data loss fails the run.
    #[test]
    fn only_forklift_grades_fail_the_run() {
        let mut report = sample();
        report.rows[0].subject = Tool::Git;
        report.rows[0].grade = Grade::DataLost;
        assert!(!report.failed());
        report.rows[0].subject = Tool::Forklift;
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

    fn determinism(seed: u64, scenarios: Vec<String>) -> Determinism {
        let configurations = BTreeMap::from([("git-tuned".to_string(), "core.commitGraph=true".to_string())]);
        Determinism { seed, tools: BTreeMap::new(), scenarios, platform: Platform::Linux, os: "Linux 6.1".to_string(), filesystem: "ext2/ext3/ext4".to_string(),
            configurations, history: None, environment: BTreeMap::new() }
    }

    fn sample() -> Report {
        Report {
            format: REPORT_FORMAT,
            coverage: CoverageTable::new(Platform::Linux, |category| match category {
                Category::DamagedFiles => Cell::Run,
                _ => Cell::not_run("not asked for"),
            }),
            determinism: determinism(3, vec!["trust".to_string()]),
            completeness: vec![Completeness { subject: Some(Tool::Forklift), check: "damage".to_string(), cell: Cell::Run, missing: vec![] }],
            rows: vec![Row { subject: Tool::Forklift,
                scenario: "trust".to_string(), category: Category::DamagedFiles, binding: "forklift".to_string(), grade: Grade::RefusedWorkingRemedy,
                recoverability: Recoverability { commands_to_expected: Some(2), named_right_remedy: true, outside_knowledge: vec![], rescued_by: None },
                note: None, detail: Some("trust-head.json#".to_string()), pairing: None,
            }],
            forklift_only: vec![],
            fidelity: vec![],
        }
    }

    fn damage_row(subject: Tool, side: Warehouse, role: Role, kind: Kind, grade: Grade, rescued_by: Option<Rescue>, commands: Option<u32>) -> Row {
        Row { subject, grade, binding: subject.label().to_string(),
            recoverability: Recoverability { commands_to_expected: commands, named_right_remedy: false, outside_knowledge: vec![], rescued_by },
            detail: Some(format!("{}-{:?}-{:?}.json#", subject.label(), role, grade)),
            pairing: Some(Pairing { scenario: "history".to_string(), side, role, kind }),
            ..sample().rows.remove(0) }
    }

    fn with_rows(rows: Vec<Row>) -> Report {
        Report { rows, ..sample() }
    }

    /// Rows pair only on all four of scenario, side, role and kind; a subject with several rows on
    /// a line stands at its worst; an unpaired row is not on the plate. A subject absent from a
    /// line reads as not measured, or as no counterpart on a line whose role has none.
    ///
    /// Revert: keep a subject's first row on a line instead of its worst → forklift stands healed.
    #[test]
    fn the_plate_pairs_damage_rows_on_all_four_and_keeps_the_worst() {
        use Warehouse::*;
        let report = with_rows(vec![
            damage_row(Tool::Forklift, Client, Role::Object, Kind::Delete, Grade::Healed, None, Some(1)),
            damage_row(Tool::Forklift, Client, Role::Object, Kind::Delete, Grade::DataLost, None, None),
            damage_row(Tool::Git, Client, Role::Object, Kind::Delete, Grade::Healed, None, Some(1)),
            damage_row(Tool::Git, Host, Role::Object, Kind::Delete, Grade::Healed, None, Some(1)),
            damage_row(Tool::Git, Client, Role::Object, Kind::Truncate, Grade::Healed, None, Some(1)),
            damage_row(Tool::Git, Client, Role::Log, Kind::Delete, Grade::Healed, None, Some(1)),
            Row { pairing: None, ..damage_row(Tool::Git, Client, Role::Config, Kind::Delete, Grade::Silent, None, None) },
            damage_row(Tool::Forklift, Host, Role::NoCounterpart, Kind::Delete, Grade::Healed, None, Some(1)),
        ]);
        let plate = report.plate();
        let keys: Vec<(Warehouse, Role, Kind, Vec<Tool>)> = plate.iter()
            .map(|line| (line.pairing.side, line.pairing.role, line.pairing.kind, line.standings.keys().copied().collect())).collect();
        assert_eq!(keys, [
            (Client, Role::Object, Kind::Truncate, vec![Tool::Git]),
            (Client, Role::Object, Kind::Delete, vec![Tool::Forklift, Tool::Git]),
            (Client, Role::Log, Kind::Delete, vec![Tool::Git]),
            (Host, Role::Object, Kind::Delete, vec![Tool::Git]),
            (Host, Role::NoCounterpart, Kind::Delete, vec![Tool::Forklift]),
        ]);
        let forklift = &plate[1].standings[&Tool::Forklift];
        assert_eq!((forklift.grade, forklift.rows, forklift.detail.as_deref()), (Grade::DataLost, 2, Some("forklift-Object-DataLost.json#")));
        let markdown = report.markdown();
        assert!(markdown.contains("| history | client | object | delete | [data-lost](forklift-Object-DataLost.json#) (worst of 2) | [healed](git-Object-Healed.json#) | not measured |"), "{}", markdown);
        assert!(markdown.contains("| history | host | no-counterpart | delete | [healed](forklift-NoCounterpart-Healed.json#) | no counterpart | no counterpart |"), "{}", markdown);
    }

    /// Git does better by the plate's grade order, then by rescue kind, and never by step count;
    /// a role with no counterpart, or a line forklift has no row on, is never a win.
    ///
    /// Revert: rank grades in their declaration order → silent beats data lost and the
    /// data-lost-over-silent win is missed.
    #[test]
    fn git_does_better_by_grade_then_rescue_kind() {
        use Warehouse::Client;
        let wins = |forklift: Option<(Grade, Option<Rescue>, Option<u32>)>, git: (Grade, Option<Rescue>, Option<u32>), role: Role| -> Vec<Tool> {
            let mut rows = vec![damage_row(Tool::GitTuned, Client, role, Kind::Zero, git.0, git.1, git.2)];
            rows.extend(forklift.map(|(grade, rescue, commands)| damage_row(Tool::Forklift, Client, role, Kind::Zero, grade, rescue, commands)));
            with_rows(rows).plate().remove(0).git_does_better()
        };
        let object = Role::Object;
        assert_eq!(wins(Some((Grade::DataLost, None, None)), (Grade::Healed, None, Some(1)), object), [Tool::GitTuned]);
        assert_eq!(wins(Some((Grade::Silent, None, None)), (Grade::DataLost, None, None), object), [Tool::GitTuned]);
        assert_eq!(wins(Some((Grade::DataLost, None, None)), (Grade::Silent, None, None), object), []);
        assert_eq!(wins(Some((Grade::RefusedNoRemedy, None, None)), (Grade::RefusedWorkingRemedy, None, Some(3)), object), [Tool::GitTuned]);
        assert_eq!(wins(Some((Grade::Healed, Some(Rescue::Reclone), Some(4))), (Grade::Healed, Some(Rescue::LocalRepair), Some(9)), object), [Tool::GitTuned]);
        assert_eq!(wins(Some((Grade::Healed, Some(Rescue::LocalRepair), Some(4))), (Grade::Healed, None, Some(9)), object), [Tool::GitTuned]);
        assert_eq!(wins(Some((Grade::Healed, None, Some(9))), (Grade::Healed, None, Some(1)), object), [], "fewer steps is not a win");
        assert_eq!(wins(Some((Grade::Healed, None, Some(1))), (Grade::Healed, Some(Rescue::LocalRepair), Some(1)), object), []);
        assert_eq!(wins(Some((Grade::DataLost, None, None)), (Grade::Healed, None, Some(1)), Role::NoCounterpart), [], "no counterpart");
        assert_eq!(wins(None, (Grade::Healed, None, Some(1)), object), [], "a line with no row of forklift's");

        let report = with_rows(vec![
            damage_row(Tool::Forklift, Client, object, Kind::Zero, Grade::DataLost, None, None),
            damage_row(Tool::Git, Client, object, Kind::Zero, Grade::Healed, Some(Rescue::Reclone), Some(3)),
        ]);
        let markdown = report.markdown();
        assert!(markdown.contains("## Git does better\n\n- history client object zero: git [healed, re-clone](git-Object-Healed.json#), forklift [data-lost](forklift-Object-DataLost.json#)\n"), "{}", markdown);
        assert!(with_rows(vec![]).markdown().contains("## Git does better\n\nNone.\n"));
    }

    /// A git subject's gap, or a git check that did not run, shows as git not fully measured and
    /// never fails the run; forklift's gap and a gap of the run itself do. A run given no git
    /// asked for no comparison, so its git checks that did not run are no gap.
    ///
    /// Revert: count every subject's completeness gap in `failed` → git's gap fails the run.
    #[test]
    fn a_git_gap_is_shown_and_never_fails_the_run() {
        let check = |subject: Option<Tool>, cell: Cell, missing: &[&str]| Completeness { subject, check: "git layout walk".to_string(), cell, missing: missing.iter().map(|m| m.to_string()).collect() };
        let report = |checks: Vec<Completeness>| {
            let mut report = Report { completeness: checks, ..sample() };
            report.determinism.tools.insert("git".to_string(), "/usr/bin/git (git version 2.56.1)".to_string());
            report
        };
        let git = report(vec![check(Some(Tool::GitTuned), Cell::Run, &["history: info/sparse"])]);
        assert!(!git.failed());
        assert_eq!(git.git_gaps().len(), 1);
        assert!(git.markdown().contains("\ngit: not fully measured, by git layout walk (git-tuned). Git's gaps never fail the run.\n"), "{}", git.markdown());
        let unrun = report(vec![check(Some(Tool::Git), Cell::not_run("--only builds one scenario"), &[])]);
        assert!(!unrun.failed());
        assert_eq!(unrun.git_gaps().len(), 1);
        let no_git = Report { completeness: vec![check(Some(Tool::Git), Cell::not_run("no git binary given"), &[])], ..sample() };
        assert!(no_git.git_gaps().is_empty() && !no_git.markdown().contains("not fully measured"));
        let measured = report(vec![check(Some(Tool::Git), Cell::Run, &[]), check(Some(Tool::Forklift), Cell::not_run("no census"), &[])]);
        assert!(measured.git_gaps().is_empty());
        assert!(!measured.markdown().contains("not fully measured"));
        assert!(report(vec![check(Some(Tool::Forklift), Cell::Run, &["trust"])]).failed());
        assert!(report(vec![check(None, Cell::Run, &["over budget"])]).failed());
    }

    /// Fidelity findings are listed, one row each with both values and nothing for a side that has
    /// none, read back whole, and never fail the run.
    ///
    /// Revert: escape no `|` in a value or a field → the zone row or the header row gains a column.
    #[test]
    fn fidelity_findings_are_listed_and_never_fail_the_run() {
        use crate::fidelity::Direction;
        let finding = |field: &str, at: &str, before: Option<&str>, after: Option<&str>| Finding {
            direction: Direction::GitForkliftGit, input: Input::Fixture, field: field.to_string(), at: at.to_string(), before: before.map(str::to_string), after: after.map(str::to_string),
        };
        let report = Report { fidelity: vec![finding("author zone", "main~8", Some("+0130"), Some("+0|00")), finding("ref", "refs/notes/commits", Some("note"), None), finding("header x|y", "main", Some("v"), None)], ..sample() };
        assert!(!report.failed());
        let markdown = report.markdown();
        assert!(markdown.contains("\n| git to Forklift to git | author zone | main~8 | +0130 | +0\\|00 |\n"), "{}", markdown);
        assert!(markdown.contains("\n| git to Forklift to git | ref | refs/notes/commits | note | nothing |\n"), "{}", markdown);
        assert!(markdown.contains("\n| git to Forklift to git | header x\\|y | main | v | nothing |\n"), "{}", markdown);
        assert!(!markdown.contains("From the given history"), "{}", markdown);
        assert!(sample().markdown().contains("\n## Fidelity\n\nNone from the fixtures.\n"));
        assert_eq!(Report::from_json(&report.json()).expect("the report reads back").fidelity, report.fidelity);
    }

    /// A given history's findings are counted by direction and field, the first of each shown,
    /// beside the fixture's listed one by one; report.json keeps every one with its input.
    ///
    /// Revert: list the history's findings in the fixture table → the trailer row of the history
    /// shows up there and the count row goes; count by field alone → the two directions' message
    /// rows merge into one counting 3.
    #[test]
    fn history_findings_are_counted_by_field() {
        use crate::fidelity::Direction;
        let finding = |direction: Direction, input: Input, field: &str, at: &str, before: &str| Finding {
            direction, input, field: field.to_string(), at: at.to_string(), before: Some(before.to_string()), after: None,
        };
        let report = Report { fidelity: vec![
            finding(Direction::GitForkliftGit, Input::Fixture, "gpgsig", "main~3", "sig"),
            finding(Direction::GitForkliftGit, Input::History, "message", "main", "first"),
            finding(Direction::GitForkliftGit, Input::History, "message", "main~1", "second"),
            finding(Direction::ForkliftGitForklift, Input::History, "message", "main~4", "third"),
        ], ..sample() };
        let markdown = report.markdown();
        assert!(markdown.contains("\n| git to Forklift to git | gpgsig | main~3 | sig | nothing |\n"), "{}", markdown);
        assert!(!markdown.contains("| git to Forklift to git | message | main~1 |"), "{}", markdown);
        assert!(markdown.contains("\nFrom the given history, 3 findings by field (report.json lists each):\n"), "{}", markdown);
        assert!(markdown.contains("\n| git to Forklift to git | message | 2 | main | first | nothing |\n"), "{}", markdown);
        assert!(markdown.contains("\n| Forklift to git to Forklift | message | 1 | main~4 | third | nothing |\n"), "{}", markdown);
        let mut clean = sample();
        clean.determinism.history = Some("/history (3 refs, blake3 00 of their listing)".to_string());
        assert!(clean.markdown().contains("\n- history from: /history (3 refs, blake3 00 of their listing)\n"), "{}", clean.markdown());
        assert!(clean.markdown().contains("\nNone from the fixtures.\n\nNone from the given history.\n"), "{}", clean.markdown());
        assert!(!sample().markdown().contains("None from the given history"));
        let only_history = Report { fidelity: report.fidelity[1..].to_vec(), ..sample() };
        assert!(only_history.markdown().contains("\n## Fidelity\n\nNone from the fixtures.\n\nFrom the given history, 3 findings"), "{}", only_history.markdown());
        assert_eq!(Report::from_json(&report.json()).expect("the report reads back").fidelity, report.fidelity);
        let written: serde_json::Value = serde_json::from_str(&report.json()).unwrap();
        assert_eq!(written["fidelity"][1]["input"], "history");
        assert_eq!(written["fidelity"][0]["input"], "fixture");
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
        let quoted = read_changed(|value| value["format"] = serde_json::json!("1")).map(|_| ()).unwrap_err();
        assert_eq!(quoted, format!("report format \"1\" is not one this build reads (it reads {})", REPORT_FORMAT));
        assert!(read_changed(|_| ()).is_ok(), "the unchanged sample reads");
    }

    /// A field this build does not know is refused, never dropped, in a row and in a coverage cell.
    ///
    /// Revert: drop `deny_unknown_fields` from `Row` or from `Cell` → that report reads.
    #[test]
    fn a_report_with_an_unknown_field_is_refused() {
        let in_row = read_changed(|value| value["rows"][0]["weight"] = serde_json::json!(1)).map(|_| ()).unwrap_err();
        assert!(in_row.contains("unknown field `weight`"), "{}", in_row);
        for category in ["kill-points-client", "damaged-files"] {
            let in_cell = read_changed(|value| value["coverage"]["cells"][category]["linux"]["weight"] = serde_json::json!(1)).map(|_| ()).unwrap_err();
            assert!(in_cell.contains("unknown field `weight`"), "{}: {}", category, in_cell);
        }
        let sampled = read_changed(|value| value["completeness"][0]["cell"] = serde_json::json!({"state": "sampled", "weight": 1})).map(|_| ()).unwrap_err();
        assert!(sampled.contains("unknown field `weight`"), "{}", sampled);
        let reasoned = read_changed(|value| value["completeness"][0]["cell"] = serde_json::json!({"state": "run", "reason": "x"})).map(|_| ()).unwrap_err();
        assert!(reasoned.contains("names no reason"), "{}", reasoned);
    }
    /// A scratch folder of its own for one test.
    fn folder(label: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |time| time.as_nanos());
        let folder = std::env::temp_dir().join(format!("proof-load-steps-{}-{}-{}", label, std::process::id(), nanos));
        std::fs::create_dir_all(&folder).expect("a scratch folder");
        folder
    }

    fn step(arguments: &[&str], exit: i32) -> Outcome {
        Outcome { arguments: arguments.iter().map(|argument| argument.to_string()).collect(), exit: Some(exit), stdout: b"out".to_vec(), stderr: vec![], failure: None }
    }

    fn row_at(detail: Option<&str>) -> Row {
        Row { subject: Tool::Forklift, detail: detail.map(str::to_string), ..sample().rows.remove(0) }
    }

    /// A row's steps are the ones at its pointer, in its file under the run's folder: a details
    /// file holding other steps at its root and in another file is read where the row points.
    ///
    /// Revert: read `steps` at the file's root whatever the pointer → the root's steps come back.
    #[test]
    fn a_rows_steps_are_read_at_its_pointer() {
        let out = folder("pointer");
        let graded = vec![step(&["heal"], 0), step(&["load", "."], 2)];
        let document = serde_json::json!({"steps": [step(&["root"], 9)], "killed": [{"graded": {"steps": graded}}]});
        std::fs::write(out.join("kills.json"), document.to_string()).expect("written");
        std::fs::write(out.join("other.json"), serde_json::json!({"steps": [step(&["other"], 7)]}).to_string()).expect("written");
        assert_eq!(row_at(Some("kills.json#/killed/0/graded")).steps(&out), Ok(graded));
        assert_eq!(row_at(Some("kills.json#")).steps(&out), Ok(vec![step(&["root"], 9)]));
        assert_eq!(row_at(Some("other.json#")).steps(&out), Ok(vec![step(&["other"], 7)]));
    }

    /// Every way a row's details can fail to lead to its steps is refused and says which.
    ///
    /// Revert: allow any relative path → `../kills.json` reads the file above the run's folder.
    #[test]
    fn a_row_whose_details_cannot_be_read_is_refused() {
        let parent = folder("refused");
        let out = parent.join("run");
        std::fs::create_dir_all(&out).expect("the run's folder");
        let document = serde_json::json!({"steps": [step(&["heal"], 0)]}).to_string();
        std::fs::write(parent.join("kills.json"), &document).expect("written");
        std::fs::write(out.join("kills.json"), &document).expect("written");
        std::fs::write(out.join("odd.json"), serde_json::json!({"steps": [{"arguments": [], "exit": 0, "stdout": [], "stderr": [], "failure": null, "weight": 1}]}).to_string()).expect("written");
        std::fs::write(out.join("bare.json"), "{\"graded\": {}}").expect("written");
        assert!(row_at(Some("kills.json#")).steps(&out).is_ok(), "the well-formed row reads");
        let refusal = |detail: Option<&str>| row_at(detail).steps(&out).unwrap_err();
        assert_eq!(refusal(None), "the run wrote no details for this row");
        assert_eq!(refusal(Some("kills.json")), "the row's details `kills.json` name no JSON pointer");
        for outside in ["../kills.json#", "#", "./kills.json#"] {
            let file = outside.trim_end_matches('#');
            assert_eq!(refusal(Some(outside)), format!("the row's details file `{}` is not a file name inside the run's folder", file), "{}", outside);
        }
        let absolute = format!("{}#", parent.join("kills.json").display());
        assert!(refusal(Some(&absolute)).contains("is not a file name inside the run's folder"), "{}", absolute);
        assert!(refusal(Some("gone.json#")).starts_with("cannot read "), "a missing file");
        assert!(refusal(Some("bare.json#/killed")).ends_with("holds nothing at `/killed`"), "a pointer to nothing");
        assert!(refusal(Some("bare.json#/graded")).ends_with("holds no steps at `/graded`"), "an object without steps");
        assert!(refusal(Some("odd.json#")).contains("unknown field `weight`"), "a step with a field this build does not know");
    }
}

