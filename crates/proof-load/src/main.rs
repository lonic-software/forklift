//! The `proof-load` CLI: what CI runs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use proof_load::census::Census;
use proof_load::corpus;
use proof_load::runner;
use proof_load::scenario::{self, Tool};
use proof_load::scrub::Environment;
use proof_load::tool;
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
        /// The forklift binary under test, by path. Without it the scenarios and the census walk
        /// are reported as not run.
        #[arg(long)]
        forklift: Option<PathBuf>,
    },
}

fn main() -> ExitCode {
    let Command::Run { source, out, seed, only, forklift } = Cli::parse().command;

    let mut output = match Output::claim(&out) {
        Ok(output) => output,
        Err(error) => {
            eprintln!("proof-load: {}", error);
            return ExitCode::from(2);
        }
    };

    let census = match Census::load(&source) {
        Ok(census) => census,
        Err(error) => {
            eprintln!("proof-load: {}", error);
            return ExitCode::from(2);
        }
    };

    let scenarios: Vec<_> = scenario::catalog().into_iter().filter(|scenario| only.as_ref().is_none_or(|only| *only == scenario.id)).collect();
    let corpus = corpus::generate(seed, corpus::Shape::SMALL);
    let mut tools = BTreeMap::new();
    let mut environment_seen = BTreeMap::new();

    // Each scenario's history built with the forklift under test: what it created, and what its
    // walk found that no census row classifies.
    let mut created: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut walk_missing = Vec::new();
    let walk_cell = match (&forklift, &census) {
        (None, _) => Cell::not_run("no forklift binary given"),
        (Some(_), None) => Cell::not_run("no census for this version"),
        (Some(program), Some(census)) => {
            let program = match tool::absolute(program) {
                Ok(program) => program,
                Err(error) => {
                    eprintln!("proof-load: {}: {}", program.display(), error);
                    return ExitCode::from(2);
                }
            };
            tools.insert("forklift".to_string(), program.display().to_string());
            for scenario in &scenarios {
                let built = output.scratch(&scenario.id).map_err(|error| error.to_string())
                    .and_then(|scratch| {
                        let environment = Environment::scrubbed(&scratch, std::env::var_os("PATH"), &BTreeMap::new()).map_err(|error| error.to_string())?;
                        environment_seen = environment.variables.clone();
                        runner::build(scenario, &corpus, Tool::Forklift, &program, &scratch, &environment, std::time::Duration::from_secs(120))
                            .map_err(|error| error.to_string())
                    });
                match built {
                    Ok(built) => {
                        walk_missing.extend(census.unclassified(&built.walked).into_iter().map(|path| format!("{}: {}", scenario.id, path)));
                        created.insert(scenario.id.clone(), runner::created_names(census, &built.walked));
                    }
                    Err(error) => walk_missing.push(format!("{}: not built: {}", scenario.id, error)),
                }
            }
            Cell::Run
        }
    };
    let walk_check = Completeness { check: "census walk".to_string(), cell: walk_cell, missing: walk_missing };

    let damage_check = match &census {
        None => Completeness { check: "damage bindings".to_string(), cell: Cell::not_run("no census for this version"), missing: vec![] },
        Some(census) => {
            let missing = proof_load::bindings::check(census, proof_load::bindings::BINDINGS, proof_load::bindings::EXEMPTIONS, &scenario::catalog(), &created);
            Completeness { check: "damage bindings".to_string(), cell: Cell::Run, missing }
        }
    };

    let report = Report {
        coverage: CoverageTable::new(Platform::current(), |_: Category| Cell::not_run("no scenario runner in this build of the harness")),
        determinism: Determinism {
            seed,
            tools,
            scenarios: scenarios.iter().map(|scenario| scenario.id.clone()).collect(),
            platform: Platform::current(),
            environment: environment_seen,
        },
        completeness: vec![damage_check, walk_check],
        rows: vec![],
        forklift_only: vec![],
    };

    for (scenario, names) in &created {
        eprintln!("proof-load: {} creates {}", scenario, names.join(" "));
    }
    for (name, text) in [("report.json", report.json()), ("report.md", report.markdown())] {
        if let Err(error) = std::fs::write(output.root().join(name), text) {
            eprintln!("proof-load: cannot write {}: {}", name, error);
            return ExitCode::from(2);
        }
    }

    if report.failed() { ExitCode::from(1) } else { ExitCode::SUCCESS }
}
