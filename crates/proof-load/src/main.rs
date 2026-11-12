//! The `proof-load` CLI: what CI runs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use proof_load::census::Census;
use proof_load::damage;
use proof_load::report::{Category, Cell, Completeness, CoverageTable, Determinism, Platform, Report};
use proof_load::scrub::Output;

#[derive(Parser)]
#[command(name = "proof-load", about = "Fault-injection harness for Forklift")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the completeness checks and the scenarios, and writes report.json and report.md.
    Run {
        /// The version under test's source tree, read only, for its census.
        #[arg(long)]
        source: PathBuf,
        /// An empty or new directory for scratch areas and the report.
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 1)]
        seed: u64,
        /// Run only this scenario.
        #[arg(long)]
        only: Option<String>,
    },
}

fn main() -> ExitCode {
    let Command::Run { source, out, seed, only } = Cli::parse().command;

    let output = match Output::claim(&out) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("proof-load: {}", error);
            return ExitCode::from(2);
        }
    };

    let damage_check = match Census::load(&source) {
        Err(error) => {
            eprintln!("proof-load: {}", error);
            return ExitCode::from(2);
        }
        Ok(None) => Completeness { check: "damage bindings".to_string(), cell: Cell::not_run("no census for this version"), missing: vec![] },
        Ok(Some(census)) => {
            let mut missing: Vec<String> = damage::uncovered(&census, &[], &[]).into_iter().map(|name| name.path).collect();
            missing.extend(damage::stale(&census, &[], &[]).into_iter().map(|name| format!("stale binding: {}", name)));
            Completeness { check: "damage bindings".to_string(), cell: Cell::Run, missing }
        }
    };

    let report = Report {
        coverage: CoverageTable::new(Platform::current(), |_: Category| Cell::not_run("no scenario runner in this build of the harness")),
        determinism: Determinism {
            seed,
            tools: BTreeMap::new(),
            scenarios: only.into_iter().collect(),
            platform: Platform::current(),
            environment: BTreeMap::new(),
        },
        completeness: vec![damage_check],
        rows: vec![],
        forklift_only: vec![],
    };

    for (name, text) in [("report.json", report.json()), ("report.md", report.markdown())] {
        if let Err(error) = std::fs::write(output.root().join(name), text) {
            eprintln!("proof-load: cannot write {}: {}", name, error);
            return ExitCode::from(2);
        }
    }

    if report.failed() { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
