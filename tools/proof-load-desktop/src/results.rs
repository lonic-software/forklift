//! A report as the Results and Coverage screens show it: every graded row worst first, the run's
//! tally, and the filters. The report is the library's (`Report::from_json`); this only orders
//! and picks from it, and nothing here draws.

use std::path::{Path, PathBuf};

use proof_load::oracle::Grade;
use proof_load::report::{Report, Row};

use crate::grades;

/// An open report and the folder its run wrote it to, which a row's details are relative to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Results {
    pub report: Report,
    pub out: PathBuf,
    /// The report's rows and its forklift-only rows, worst grade first, then by scenario.
    pub rows: Vec<Row>,
    /// The bindings the rows graded, sorted, each once.
    pub bindings: Vec<String>,
}

impl Results {
    pub fn new(report: Report, out: PathBuf) -> Results {
        let mut rows: Vec<Row> = report.rows.iter().chain(&report.forklift_only).cloned().collect();
        rows.sort_by(|a, b| grades::rank(a.grade).cmp(&grades::rank(b.grade)).then_with(|| a.scenario.cmp(&b.scenario)));
        let mut bindings: Vec<String> = rows.iter().map(|row| row.binding.clone()).collect();
        bindings.sort();
        bindings.dedup();
        Results { report, out, rows, bindings }
    }

    /// Reads `report.json` from the folder a run wrote it to. Refuses one that cannot be read, or
    /// that the library does not read, saying why.
    pub fn open(out: &Path) -> Result<Results, String> {
        let path = out.join("report.json");
        let text = std::fs::read_to_string(&path).map_err(|error| format!("cannot read {}: {}", path.display(), error))?;
        let report = Report::from_json(&text).map_err(|error| format!("{}: {}", path.display(), error))?;
        Ok(Results::new(report, out.to_path_buf()))
    }

    /// How many rows of the whole run have each grade, worst first, every grade listed.
    pub fn tally(&self) -> Vec<(Grade, usize)> {
        grades::WORST_FIRST.into_iter().map(|grade| (grade, self.rows.iter().filter(|row| row.grade == grade).count())).collect()
    }

    /// The indices into `rows` the table shows: only failing grades when `failures`, and only
    /// `binding`'s rows when one is given.
    pub fn shown(&self, failures: bool, binding: Option<&str>) -> Vec<usize> {
        self.rows.iter().enumerate()
            .filter(|(_, row)| !failures || row.grade.is_failure())
            .filter(|(_, row)| binding.is_none_or(|binding| row.binding == binding))
            .map(|(index, _)| index).collect()
    }
}

/// A row's commands to expected bytes, as the table and its readout show them: the count and its
/// unit, or "never".
pub fn to_expected(row: &Row) -> (String, &'static str) {
    match row.recoverability.commands_to_expected {
        None => ("never".to_string(), ""),
        Some(1) => ("1".to_string(), "command"),
        Some(count) => (count.to_string(), "commands"),
    }
}

/// The Outside knowledge readout: what the row needed beyond the tool's own commands.
pub fn outside_knowledge(row: &Row) -> String {
    match row.recoverability.outside_knowledge.as_slice() {
        [] => "none: the tool's own commands were enough".to_string(),
        steps => steps.join(", "),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report written by a real run of the signed scenario, and the folder it sits in.
    pub(crate) fn fixture() -> Results {
        Results::open(&Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/signed")).expect("the fixture report reads")
    }

    /// Rows sort worst first whatever order the report lists them in, forklift-only rows
    /// included, and ties sort by scenario.
    ///
    /// Revert: keep the report's order → a healed row precedes a failing one.
    #[test]
    fn rows_sort_worst_first() {
        let mut results = fixture();
        let mut report = results.report.clone();
        report.rows.reverse();
        let moved = report.rows.pop().expect("the fixture has rows");
        report.forklift_only.push(moved);
        results = Results::new(report, results.out);
        let ranks: Vec<usize> = results.rows.iter().map(|row| grades::rank(row.grade)).collect();
        let mut sorted = ranks.clone();
        sorted.sort();
        assert_eq!(ranks, sorted);
        assert!(ranks.first() < ranks.last(), "the fixture holds more than one grade: {ranks:?}");
        assert_eq!(results.rows.len(), 13, "the forklift-only row is shown too");
        for pair in results.rows.windows(2) {
            if pair[0].grade == pair[1].grade {
                assert!(pair[0].scenario <= pair[1].scenario, "{} before {}", pair[0].scenario, pair[1].scenario);
            }
        }
    }

    /// Failures shows only failing grades and All every row; a binding shows only its rows.
    ///
    /// Revert: ignore the binding filter → the forklift-server row shows under forklift.
    #[test]
    fn the_filters_pick_rows() {
        let mut results = fixture();
        let mut server = results.rows[0].clone();
        server.binding = "forklift-server".to_string();
        server.grade = Grade::Healed;
        results.rows.push(server);
        results = Results::new(Report { rows: results.rows.clone(), forklift_only: vec![], ..results.report.clone() }, results.out.clone());
        let all = results.shown(false, None);
        assert_eq!(all.len(), results.rows.len());
        let failures = results.shown(true, None);
        assert!(!failures.is_empty() && failures.len() < all.len());
        assert!(failures.iter().all(|index| results.rows[*index].grade.is_failure()));
        assert_eq!(results.bindings, ["forklift", "forklift-server"]);
        let server = results.rows.iter().position(|row| row.binding == "forklift-server").expect("the server row");
        assert_eq!(results.shown(false, Some("forklift-server")), [server]);
        assert_eq!(results.shown(false, Some("forklift")).len(), all.len() - 1);
    }

    /// The tally counts the whole run, every grade listed worst first.
    #[test]
    fn the_tally_counts_every_row() {
        let results = fixture();
        let tally = results.tally();
        assert_eq!(tally.iter().map(|(grade, _)| *grade).collect::<Vec<_>>(), grades::WORST_FIRST);
        assert_eq!(tally.iter().map(|(_, count)| count).sum::<usize>(), results.rows.len());
    }

    /// A folder with no report, or one the library refuses, is refused with the reason.
    #[test]
    fn a_report_that_cannot_be_read_is_refused() {
        let missing = Results::open(Path::new("/nonexistent/proof-load-run")).unwrap_err();
        assert!(missing.starts_with("cannot read /nonexistent/proof-load-run"), "{missing}");
        let folder = std::env::temp_dir().join(format!("proof-load-desktop-format-{}", std::process::id()));
        std::fs::create_dir_all(&folder).expect("a scratch folder");
        std::fs::write(folder.join("report.json"), "{\"format\": 99}").expect("written");
        let other = Results::open(&folder).unwrap_err();
        let reads = format!("report format 99 is not one this build reads (it reads {})", proof_load::report::REPORT_FORMAT);
        assert!(other.ends_with(&reads), "{other}");
    }

    #[test]
    fn readouts_say_never_and_none() {
        let mut row = fixture().rows[0].clone();
        row.recoverability.commands_to_expected = None;
        row.recoverability.outside_knowledge = vec![];
        assert_eq!(to_expected(&row), ("never".to_string(), ""));
        assert_eq!(outside_knowledge(&row), "none: the tool's own commands were enough");
        row.recoverability.commands_to_expected = Some(1);
        row.recoverability.outside_knowledge = vec!["free disk space".into(), "remove .git/index.lock".into()];
        assert_eq!(to_expected(&row), ("1".to_string(), "command"));
        assert_eq!(outside_knowledge(&row), "free disk space, remove .git/index.lock");
    }
}
