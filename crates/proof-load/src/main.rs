//! The `proof-load` CLI: what CI runs.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use proof_load::damage::Warehouse;
use proof_load::census::Census;
use proof_load::corpus;
use proof_load::runner;
use proof_load::scenario::{self, Tool};
use proof_load::scrub::Environment;
use proof_load::tool;
use proof_load::faulted::{self, Programs};
use proof_load::kill;
use proof_load::report::{Category, Cell, Completeness, CoverageTable, Determinism, Platform, Report, Row};
use proof_load::scrub::Output;
use proof_load::writeops;

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
        /// The git binary the harness lays out exported records with, by path. Without it the
        /// damaged-file faults are reported as not run.
        #[arg(long)]
        git: Option<PathBuf>,
        /// The forklift-server binary under test, by path. Without it a scenario that hosts a
        /// warehouse is not built, and its walk reports that.
        #[arg(long)]
        forklift_server: Option<PathBuf>,
        /// The wall-clock budget in seconds. A run that takes longer still runs every scenario,
        /// then fails.
        #[arg(long)]
        budget: Option<u64>,
        /// strace, by path: the faulted command killed at each call it makes that changes or syncs
        /// a file, on Linux. Without it the kill points are reported as not run.
        #[arg(long)]
        strace: Option<PathBuf>,
        /// Kill points of this scenario only; repeatable. Without it or `--power`, every
        /// scenario's; with only `--power` named, none.
        #[arg(long = "kill")]
        kill: Vec<String>,
        /// Power loss of this scenario only; repeatable. Without it or `--kill`, every scenario's;
        /// with only `--kill` named, none. Needs strace, like the kill points.
        #[arg(long = "power")]
        power: Vec<String>,
        /// How many seeded states between the durable and the volatile one each scenario's power
        /// loss draws, at seeded moments; a draw that repeats a state already graded is not
        /// graded again, so fewer may be.
        #[arg(long, default_value_t = 0)]
        power_variants: usize,
        /// How many injected runs at once. Defaults to the machine's parallelism.
        #[arg(long)]
        jobs: Option<usize>,
    },
}

/// A tool as the determinism record names it: its path and the first line of its `--version`.
/// Asked with nothing inherited but PATH, from the temp directory, so nothing of the user's is read.
fn described(program: &std::path::Path) -> String {
    let version = std::process::Command::new(program).arg("--version").env_clear()
        .envs(std::env::var_os("PATH").map(|path| ("PATH", path))).current_dir(std::env::temp_dir()).output().ok()
        .and_then(|output| String::from_utf8_lossy(&output.stdout).lines().next().map(|line| line.trim().to_string()))
        .filter(|line| !line.is_empty()).unwrap_or_else(|| "no --version output".to_string());
    format!("{} ({})", program.display(), version)
}

fn main() -> ExitCode {
    let Command::Run { source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs } = Cli::parse().command;
    let started = std::time::Instant::now();

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

    for (flag, ids) in [("--kill", &kill), ("--power", &power)] {
        if let Some(unknown) = ids.iter().find(|id| !scenario::catalog().iter().any(|scenario| &scenario.id == *id)) {
            eprintln!("proof-load: {} {}: no such scenario", flag, unknown);
            return ExitCode::from(2);
        }
    }
    let scenarios: Vec<_> = scenario::catalog().into_iter().filter(|scenario| only.as_ref().is_none_or(|only| *only == scenario.id)).collect();
    let corpus = corpus::generate(seed, corpus::Shape::SMALL);
    let mut tools = BTreeMap::new();
    let search_path = proof_load::scrub::search_path(git.as_deref().and_then(|git| tool::absolute(git).ok()).as_deref(), std::env::var_os("PATH"));
    let mut environment_seen = BTreeMap::new();
    let server = match forklift_server.as_deref().map(tool::absolute).transpose() {
        Ok(server) => server,
        Err(error) => {
            eprintln!("proof-load: the forklift-server path: {}", error);
            return ExitCode::from(2);
        }
    };
    if let Some(server) = &server {
        tools.insert("forklift-server".to_string(), described(server));
    }

    // The version under test's write-op universe: its schema's command ids and the server's
    // leaves. Without a schema command the harness refuses to run; it keeps no list of its own.
    let program = match forklift.as_deref().map(tool::absolute).transpose() {
        Ok(program) => program,
        Err(error) => {
            eprintln!("proof-load: the client binary's path: {}", error);
            return ExitCode::from(2);
        }
    };
    let mut asked = |label: &str| -> Result<(proof_load::scrub::Scratch, Environment), String> {
        let scratch = output.scratch(label).map_err(|error| error.to_string())?;
        let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
        Ok((scratch, environment))
    };
    let limit = std::time::Duration::from_secs(120);
    let schema = match &program {
        None => None,
        Some(program) => match asked("schema").and_then(|(scratch, environment)| writeops::read_schema(program, &scratch, &environment, limit)) {
            Ok(schema) => Some(schema),
            Err(error) => {
                eprintln!("proof-load: {}", error);
                return ExitCode::from(2);
            }
        },
    };
    let leaves = match &server {
        None => vec![],
        Some(server) => match asked("server-help").and_then(|(scratch, environment)| writeops::read_server_leaves(server, &scratch, &environment, limit)) {
            Ok(leaves) => leaves,
            Err(error) => {
                eprintln!("proof-load: {}", error);
                return ExitCode::from(2);
            }
        },
    };
    let traced = match &strace {
        _ if Platform::current() != Platform::Linux => Err("write-op traces run on Linux only"),
        None => Err("no strace given"),
        Some(strace) => tool::absolute(strace).map_err(|_| "cannot resolve the strace path"),
    };
    let traces = writeops::traces_folder(output.root());
    if let Err(error) = std::fs::create_dir_all(&traces) {
        eprintln!("proof-load: {}: {}", traces.display(), error);
        return ExitCode::from(2);
    }
    let read_only: std::collections::BTreeSet<String> = proof_load::bindings::WRITE_EXEMPTIONS.iter()
        .filter(|(_, exemption)| matches!(exemption, writeops::Exemption::ReadOnly(_))).map(|(id, _)| id.to_string()).collect();

    // Each scenario's history built with the forklift under test: what it created, what its walk
    // found that no census row classifies, and what each step wrote. The coverage scenarios are
    // built only for what their steps write.
    let built_scenarios: Vec<_> = scenarios.iter().cloned()
        .chain(scenario::coverage().into_iter().filter(|scenario| only.as_ref().is_none_or(|only| *only == scenario.id))).collect();
    let mut created: BTreeMap<(String, Warehouse), Vec<String>> = BTreeMap::new();
    let mut walk_missing = Vec::new();
    let mut evidence = Vec::new();
    if let (Some(program), Some(schema)) = (&program, &schema) {
        tools.insert("forklift".to_string(), described(program));
        if let Ok(strace) = &traced {
            tools.insert("strace".to_string(), described(strace));
        }
        for scenario in &built_scenarios {
            let observer = writeops::Observer::new(traced.as_ref().ok().map(PathBuf::as_path), &traces, schema, &leaves, &read_only, &scenario.id);
            let built = output.scratch(&scenario.id).map_err(|error| error.to_string())
                .and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    environment_seen = environment.variables.clone();
                    runner::build(scenario, &corpus, Tool::Forklift, program, server.as_deref(), &scratch, &environment, limit, Some(&observer))
                        .map_err(|error| error.to_string())
                });
            evidence.extend(observer.evidence());
            match (built, &census) {
                (Ok(built), Some(census)) => {
                    walk_missing.extend(census.unclassified(&built.walked).into_iter().map(|path| format!("{}: {}", scenario.id, path)));
                    walk_missing.extend(census.unclassified(&built.host_walked).into_iter().map(|path| format!("{} host: {}", scenario.id, path)));
                    created.insert((scenario.id.clone(), Warehouse::Client), runner::created_names(census, &built.walked));
                    created.insert((scenario.id.clone(), Warehouse::Host), runner::created_names(census, &built.host_walked));
                }
                (Ok(_), None) => {}
                (Err(error), _) => walk_missing.push(format!("{}: not built: {}", scenario.id, error)),
            }
        }
    }
    let written = serde_json::to_string(&evidence).map_err(|error| error.to_string())
        .and_then(|text| std::fs::write(output.root().join("write-ops.json"), text).map_err(|error| error.to_string()));
    if let Err(error) = written {
        eprintln!("proof-load: cannot write write-ops.json: {}", error);
        return ExitCode::from(2);
    }
    let not_run = |reason: &str| ["write-op coverage", "read-only exemptions"].map(|check| Completeness { check: check.to_string(), cell: Cell::not_run(reason), missing: vec![] }).to_vec();
    let write_checks = match (&schema, &server) {
        (None, _) => not_run("no forklift binary given"),
        (Some(_), None) => not_run("no forklift-server binary given"),
        // Run in full only: a filtered run builds too few scenarios to cover the universe.
        (Some(_), Some(_)) if only.is_some() => not_run("--only builds one scenario"),
        (Some(schema), Some(_)) => writeops::check(&writeops::universe(schema, &leaves), &evidence, proof_load::bindings::WRITE_EXEMPTIONS, traced.as_ref().err().copied()),
    };
    let walk_cell = match (&program, &census) {
        (None, _) => Cell::not_run("no forklift binary given"),
        (Some(_), None) => Cell::not_run("no census for this version"),
        (Some(_), Some(_)) => Cell::Run,
    };
    let walk_check = Completeness { check: "census walk".to_string(), cell: walk_cell, missing: walk_missing };

    let damage_check = match &census {
        None => Completeness { check: "damage bindings".to_string(), cell: Cell::not_run("no census for this version"), missing: vec![] },
        Some(census) => {
            let missing = proof_load::bindings::check(census, proof_load::bindings::BINDINGS, proof_load::bindings::EXEMPTIONS, &scenario::catalog(), &created);
            Completeness { check: "damage bindings".to_string(), cell: Cell::Run, missing }
        }
    };

    // Every damage binding, every kind it names, each on a fresh build of its scenario.
    let mut rows = Vec::new();
    let mut fault_errors = Vec::new();
    let damaged_cell = match (&forklift, &git, &census) {
        (None, _, _) => Cell::not_run("no forklift binary given"),
        (_, None, _) => Cell::not_run("no git binary given to lay out the readback"),
        (_, _, None) => Cell::not_run("no census for this version"),
        (Some(forklift), Some(git), Some(_)) => {
            let (Ok(forklift), Ok(git)) = (tool::absolute(forklift), tool::absolute(git)) else {
                eprintln!("proof-load: cannot resolve the tool paths");
                return ExitCode::from(2);
            };
            tools.insert("git".to_string(), described(&git));
            let programs = Programs { forklift, git, server: server.clone() };
            let faulted_corpus = faulted_corpus(seed);
            for binding in proof_load::bindings::BINDINGS {
                let Some(scenario) = scenarios.iter().find(|scenario| scenario.id == binding.scenario) else { continue };
                for kind in binding.kinds {
                    let label = format!("{}-{}-{:?}", binding.scenario, binding.name, kind).replace(['/', '<', '>', '{', '}'], "_");
                    let result = output.scratch(&label).map_err(|error| error.to_string()).and_then(|scratch| {
                        let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                        faulted::run(scenario, binding.name, binding.warehouse, *kind, &faulted_corpus, &programs, &scratch, &environment, std::time::Duration::from_secs(120), seed)
                            .map_err(|error| error.to_string())
                    });
                    match result {
                        Ok(run) => {
                            let detail = serde_json::to_string(&run).unwrap_or_default();
                            let _ = std::fs::write(output.root().join(format!("{}.json", label)), detail);
                            rows.push(Row {
                                scenario: format!("{} {}{} {:?}", run.scenario, if run.warehouse == Warehouse::Host { "host:" } else { "" }, run.target, run.kind),
                                category: Category::DamagedFiles,
                                binding: "forklift".to_string(),
                                grade: run.grade,
                                recoverability: run.recoverability,
                                note: (!run.unextractable.is_empty()).then(|| format!("unextractable remedies: {}", run.unextractable.join("; "))),
                            });
                        }
                        Err(error) => fault_errors.push(format!("{} {:?}: {}", binding.name, kind, error)),
                    }
                }
            }
            Cell::Run
        }
    };
    let fault_check = Completeness { check: "damaged-file faults ran".to_string(), cell: damaged_cell.clone(), missing: fault_errors };

    // Every scenario's faulted command killed at each call that changes or syncs a file, and its
    // power loss at each of those moments, Linux only.
    let mut kill_completeness = Vec::new();
    let mut power_completeness = Vec::new();
    let mut not_reached = 0;
    let traced_lanes = match (&forklift, &git, &strace) {
        _ if Platform::current() != Platform::Linux => Err(("kill points run on Linux only", "power loss runs on Linux only")),
        (None, _, _) => Err(("no forklift binary given", "no forklift binary given")),
        (_, None, _) => Err(("no git binary given to lay out the readback", "no git binary given to lay out the readback")),
        (_, _, None) => Err(("no strace given", "no strace given")),
        (Some(forklift), Some(git), Some(strace)) => {
            let (Ok(forklift), Ok(git), Ok(strace)) = (tool::absolute(forklift), tool::absolute(git), tool::absolute(strace)) else {
                eprintln!("proof-load: cannot resolve the tool paths");
                return ExitCode::from(2);
            };
            tools.insert("strace".to_string(), described(&strace));
            let programs = Programs { forklift, git, server: server.clone() };
            let faulted_corpus = faulted_corpus(seed);
            let settings = kill::Settings {
                strace: &strace, search_path: search_path.clone(), limit: std::time::Duration::from_secs(120),
                jobs: jobs.unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from)), traces: &traces,
                schema: schema.as_ref().expect("a forklift binary was given, so its schema was read"),
            };
            // One template and one counting run per scenario feed both lanes; the write dumps are
            // taken only when the power loss will read them.
            let (mut all_kills, mut all_power) = (Vec::new(), Vec::new());
            for scenario in &scenarios {
                let (killing, powering) = kill::lanes(&kill, &power, &scenario.id);
                if !killing && !powering {
                    continue;
                }
                let counted = output.scratch(&format!("{}-traced", scenario.id)).map_err(|error| error.to_string())
                    .and_then(|template| kill::count(scenario, &faulted_corpus, &programs, &settings, &template, powering).map(|counting| (template, counting)).map_err(|error| error.to_string()));
                let (template, counting) = match counted {
                    Ok(counted) => counted,
                    Err(error) => {
                        if killing {
                            all_kills.push(kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error.clone()], ..Default::default() });
                        }
                        if powering {
                            all_power.push(proof_load::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error], ..Default::default() });
                        }
                        continue;
                    }
                };
                if killing {
                    match kill::run(scenario, &programs, &settings, &template, &counting) {
                        Ok(kills) => {
                            let _ = std::fs::write(output.root().join(format!("kills-{}.json", scenario.id)), serde_json::to_string(&kills).unwrap_or_default());
                            all_kills.push(kills);
                        }
                        Err(error) => all_kills.push(kill::ScenarioKills { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() }),
                    }
                }
                if powering {
                    match proof_load::power::run(scenario, &programs, &settings, &template, &counting, power_variants, seed) {
                        Ok(power) => {
                            let _ = std::fs::write(output.root().join(format!("power-{}.json", scenario.id)), serde_json::to_string(&power).unwrap_or_default());
                            all_power.push(power);
                        }
                        Err(error) => all_power.push(proof_load::power::ScenarioPower { scenario: scenario.id.clone(), errors: vec![error.to_string()], ..Default::default() }),
                    }
                }
            }
            // A lane no scenario was picked for is not run, which fails nothing: it was not asked
            // for, or what it named is not among the scenarios this run picked.
            let unasked = || Cell::not_run("no scenario this run picked was named for it");
            let kill_cell = if all_kills.is_empty() {
                unasked()
            } else {
                let summary = kill::summarize(&all_kills);
                rows.extend(summary.rows);
                kill_completeness = summary.completeness;
                not_reached = summary.not_reached;
                summary.cell
            };
            let power_cell = if all_power.is_empty() {
                unasked()
            } else {
                let summary = proof_load::power::summarize(&all_power);
                rows.extend(summary.rows);
                power_completeness = summary.completeness;
                summary.cell
            };
            Ok((kill_cell, power_cell))
        }
    };
    let (kill_cell, power_cell) = match traced_lanes {
        Ok(cells) => cells,
        Err((kill, power)) => (Cell::not_run(kill), Cell::not_run(power)),
    };
    if kill_completeness.is_empty() {
        kill_completeness.push(Completeness { check: "kill-point units".to_string(), cell: kill_cell.clone(), missing: vec![] });
    }
    if power_completeness.is_empty() {
        power_completeness.push(Completeness { check: "power-loss states".to_string(), cell: power_cell.clone(), missing: vec![] });
    }

    let mut coverage = CoverageTable::new(Platform::current(), |category: Category| match category {
        Category::DamagedFiles => damaged_cell.clone(),
        Category::KillPointsClient => kill_cell.clone(),
        Category::PowerLossClient => power_cell.clone(),
        _ => Cell::not_run("no runner for this fault in this build of the harness"),
    });
    if matches!(kill_cell, Cell::Run | Cell::Sampled) || not_reached > 0 {
        coverage.not_reached.insert(Category::KillPointsClient, not_reached);
    }
    let mut completeness = vec![damage_check, walk_check, fault_check];
    completeness.extend(write_checks);
    completeness.extend(kill_completeness);
    completeness.extend(power_completeness);
    completeness.push(Completeness::budget(started.elapsed(), budget.map(std::time::Duration::from_secs)));
    let report = Report {
        coverage,
        determinism: Determinism {
            seed,
            tools,
            scenarios: scenarios.iter().map(|scenario| scenario.id.clone()).collect(),
            platform: Platform::current(),
            environment: environment_seen,
        },
        completeness,
        rows,
        forklift_only: vec![],
    };

    for ((scenario, warehouse), names) in &created {
        if !names.is_empty() {
            eprintln!("proof-load: {} creates{} {}", scenario, if *warehouse == Warehouse::Host { " on its host" } else { "" }, names.join(" "));
        }
    }
    for (name, text) in [("report.json", report.json()), ("report.md", report.markdown())] {
        if let Err(error) = std::fs::write(output.root().join(name), text) {
            eprintln!("proof-load: cannot write {}: {}", name, error);
            return ExitCode::from(2);
        }
    }

    if report.failed() { ExitCode::from(1) } else { ExitCode::SUCCESS }
}

/// The corpus a faulted command runs over, shared by every lane: the small shape with one record
/// past the history, the one the faulted command writes.
fn faulted_corpus(seed: u64) -> corpus::Corpus {
    corpus::generate(seed, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL })
}
