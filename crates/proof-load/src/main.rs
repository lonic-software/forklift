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
        /// An empty or new directory on a small volume of its own (a size-capped tmpfs, a RAM disk),
        /// which the disk-full faults fill. Without it they are reported as not run.
        #[arg(long)]
        capped_volume: Option<PathBuf>,
        /// Disk full under this scenario's faulted command; repeatable. Without it, none.
        #[arg(long = "disk-full")]
        disk_full: Vec<String>,
        /// A read-only warehouse under this scenario's faulted command; repeatable. Without it, none.
        #[arg(long = "read-only")]
        read_only: Vec<String>,
        /// An empty or new directory on a volume that folds case or keeps no executable bit (an
        /// exFAT or vfat image), which the volume faults clone their work tree onto. Without it
        /// they are reported as not run.
        #[arg(long)]
        folding_volume: Option<PathBuf>,
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
    let Command::Run {
        source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
        capped_volume, disk_full, read_only: read_only_lane, folding_volume,
    } = Cli::parse().command;
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

    for (flag, ids) in [("--kill", &kill), ("--power", &power), ("--disk-full", &disk_full), ("--read-only", &read_only_lane)] {
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
                                category: if run.kind.plants() { Category::DeadPidLock } else { Category::DamagedFiles },
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

    // Disk full on the capped volume and a read-only work folder, each under the faulted command of
    // the scenarios named for it, lifted before the grading readback.
    let picked = |named: &[String]| -> Vec<&scenario::Scenario> { scenarios.iter().filter(|scenario| named.contains(&scenario.id)).collect() };
    let resolved = match (&forklift, &git) {
        (None, _) => Err("no forklift binary given"),
        (_, None) => Err("no git binary given to lay out the readback"),
        (Some(forklift), Some(git)) => match (tool::absolute(forklift), tool::absolute(git)) {
            (Ok(forklift), Ok(git)) => Ok(Programs { forklift, git, server: server.clone() }),
            _ => {
                eprintln!("proof-load: cannot resolve the tool paths");
                return ExitCode::from(2);
            }
        },
    };
    let mut condition_errors: BTreeMap<Category, Vec<String>> = BTreeMap::new();
    let mut lifted = Vec::new();
    let disk_full_cell = match (&resolved, &capped_volume) {
        (Err(reason), _) => Cell::not_run(reason),
        _ if Platform::current() == Platform::Windows => Cell::not_run("no capped volume is built on Windows in this harness"),
        (_, None) => Cell::not_run("no capped volume given"),
        _ if picked(&disk_full).is_empty() => Cell::not_run("no scenario this run picked was named for it"),
        (Ok(programs), Some(volume)) => {
            let mut capped = match Output::claim(volume) {
                Ok(capped) => capped,
                Err(error) => {
                    eprintln!("proof-load: the capped volume: {}", error);
                    return ExitCode::from(2);
                }
            };
            let ballast = capped.root().join("ballast");
            let errors = condition_errors.entry(Category::DiskFull).or_default();
            for scenario in picked(&disk_full) {
                let consumed = discarded_area(&mut capped, &format!("{}-counted", scenario.id), &search_path, |scratch, environment|
                    proof_load::conditions::consumed(scenario, &faulted_corpus(seed), programs, scratch, environment, limit, &ballast));
                let consumed = match consumed {
                    Ok(consumed) => consumed,
                    Err(error) => {
                        errors.push(format!("{} disk full: {}", scenario.id, error));
                        continue;
                    }
                };
                for free in proof_load::conditions::points(consumed) {
                    let result = discarded_area(&mut capped, &format!("{}-free-{}", scenario.id, free), &search_path, |scratch, environment|
                        proof_load::conditions::disk_full(scenario, &faulted_corpus(seed), programs, scratch, environment, limit, &ballast, free)
                            .map_err(|error| error.to_string()));
                    // A fault that failed part-way may have left its ballast; the next point fills afresh.
                    let _ = proof_load::damage::apply(&ballast, proof_load::damage::Kind::Delete, 0);
                    match result {
                        Ok(run) => lifted.push((Category::DiskFull, format!("of {} B the faulted command consumes", consumed), run)),
                        Err(error) => errors.push(format!("{} free {} B: {}", scenario.id, free, error)),
                    }
                }
            }
            // A few points of a continuum of free space, so sampled; none graded is none run.
            if lifted.iter().any(|(category, _, _)| *category == Category::DiskFull) { Cell::Sampled } else { Cell::not_run("no disk-full fault graded") }
        }
    };
    let mut writable = None;
    let read_only_cell = match &resolved {
        Err(reason) => Cell::not_run(reason),
        _ if picked(&read_only_lane).is_empty() => Cell::not_run("no scenario this run picked was named for it"),
        Ok(programs) => {
            let errors = condition_errors.entry(Category::ReadOnly).or_default();
            for scenario in picked(&read_only_lane) {
                let result = output.scratch(&format!("{}-read-only", scenario.id)).map_err(|error| error.to_string()).and_then(|scratch| {
                    let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
                    proof_load::conditions::read_only(scenario, &faulted_corpus(seed), programs, &scratch, &environment, limit).map_err(|error| error.to_string())
                });
                match result {
                    Ok(Some(run)) => lifted.push((Category::ReadOnly, "write bits cleared on the work folder, no unprivileged remount".to_string(), run)),
                    Ok(None) => writable = Some(proof_load::conditions::STAYS_WRITABLE),
                    Err(error) => errors.push(format!("{} read-only: {}", scenario.id, error)),
                }
            }
            match writable {
                _ if lifted.iter().any(|(category, _, _)| *category == Category::ReadOnly) => Cell::Run,
                Some(reason) => Cell::not_run(reason),
                None => Cell::not_run("no read-only fault graded"),
            }
        }
    };
    for (category, note, run) in lifted {
        let label = format!("{}-{}", run.scenario, run.condition).replace(' ', "-");
        let _ = std::fs::write(output.root().join(format!("{}.json", label)), serde_json::to_string(&run).unwrap_or_default());
        let mut note = note;
        if !run.graded.unextractable.is_empty() {
            note.push_str(&format!("; unextractable remedies: {}", run.graded.unextractable.join("; ")));
        }
        rows.push(Row {
            scenario: format!("{} {}", run.scenario, run.condition), category, binding: "forklift".to_string(),
            grade: run.graded.grade, recoverability: run.graded.recoverability, note: Some(note),
        });
    }

    // A work tree on a folding volume, standing throughout: each volume fault runs its own
    // scenario, whose files hold only that fault, so a cell's grade answers that fault alone; each
    // runs only where the probe found its property.
    let volume_faults = [(Category::CaseFolding, scenario::Fold::Case), (Category::NoExec, scenario::Fold::Exec)];
    let folding = match (&resolved, &folding_volume) {
        (Ok(_), Some(volume)) if Platform::current() != Platform::Windows => {
            let folding = match Output::claim(volume) {
                Ok(folding) => folding,
                Err(error) => {
                    eprintln!("proof-load: the folding volume: {}", error);
                    return ExitCode::from(2);
                }
            };
            match proof_load::volume::probe(folding.root()) {
                Ok(probed) => Some((folding, probed)),
                Err(error) => {
                    eprintln!("proof-load: the folding volume: {}", error);
                    return ExitCode::from(2);
                }
            }
        }
        _ => None,
    };
    let [case_folding_cell, no_exec_cell] = volume_faults.map(|(category, fold)| {
        let scenario = scenarios.iter().find(|scenario| scenario.files == scenario::Files::Folded(fold));
        let (programs, (folding, probed), scenario) = match (&resolved, &folding, scenario) {
            (Err(reason), _, _) => return Cell::not_run(reason),
            _ if Platform::current() == Platform::Windows => return Cell::not_run("no folding volume is built on Windows in this harness"),
            (_, None, _) => return Cell::not_run("no folding volume given"),
            (_, _, None) => return Cell::not_run("no scenario this run picked holds this volume fault's files"),
            (Ok(programs), Some(folding), Some(scenario)) => (programs, folding, scenario),
        };
        if let Some(reason) = proof_load::volume::not_run_because(probed, category) {
            return Cell::not_run(reason);
        }
        let result = output.scratch(&format!("{}-folding", scenario.id)).map_err(|error| error.to_string()).and_then(|scratch| {
            let scratch = scratch.with_folding_volume(folding.root());
            let environment = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())?;
            proof_load::volume::folded(scenario, &faulted_corpus(seed), programs, &scratch, &environment, limit).map_err(|error| error.to_string())
        });
        match result {
            Ok(graded) => {
                let _ = std::fs::write(output.root().join(format!("{}-folding-volume.json", scenario.id)), serde_json::to_string(&graded).unwrap_or_default());
                let mut note = format!("work tree cloned onto the folding volume ({}, {}), standing through the retry, remedies and heal",
                    if probed.folds_case { "folds case" } else { "keeps case" },
                    if probed.keeps_executable_bit { "keeps the executable bit" } else { "no executable bit" });
                if !graded.unextractable.is_empty() {
                    note.push_str(&format!("; unextractable remedies: {}", graded.unextractable.join("; ")));
                }
                rows.push(Row {
                    scenario: format!("{} on a {}", scenario.id, category.label()), category, binding: "forklift".to_string(),
                    grade: graded.grade, recoverability: graded.recoverability, note: Some(note),
                });
                Cell::Run
            }
            Err(error) => {
                condition_errors.entry(category).or_default().push(format!("{} on the folding volume: {}", scenario.id, error));
                Cell::not_run("no volume fault graded")
            }
        }
    });

    let condition_checks: Vec<Completeness> = [
        (Category::DiskFull, "disk-full faults ran", &disk_full_cell), (Category::ReadOnly, "read-only faults ran", &read_only_cell),
        (Category::CaseFolding, "case-folding faults ran", &case_folding_cell), (Category::NoExec, "no-exec faults ran", &no_exec_cell),
    ].into_iter().map(|(category, check, cell)| Completeness {
            check: check.to_string(), cell: cell.clone(), missing: condition_errors.remove(&category).unwrap_or_default(),
        }).collect();

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
        // The dead-holder locks are planted by the damage lane, binding by binding.
        Category::DamagedFiles | Category::DeadPidLock => damaged_cell.clone(),
        Category::KillPointsClient => kill_cell.clone(),
        Category::PowerLossClient => power_cell.clone(),
        Category::DiskFull => disk_full_cell.clone(),
        Category::ReadOnly => read_only_cell.clone(),
        Category::CaseFolding => case_folding_cell.clone(),
        Category::NoExec => no_exec_cell.clone(),
        _ => Cell::not_run("no runner for this fault in this build of the harness"),
    });
    if matches!(kill_cell, Cell::Run | Cell::Sampled) || not_reached > 0 {
        coverage.not_reached.insert(Category::KillPointsClient, not_reached);
    }
    let mut completeness = vec![damage_check, walk_check, fault_check];
    completeness.extend(condition_checks);
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

/// Runs `run` in a new scratch area `label` of `volume`, then removes the area, failed or not: what
/// one leaves on a small capped volume would take the free space of the next, and the details of
/// each graded fault are written to the output.
fn discarded_area<T>(
    volume: &mut Output, label: &str, search_path: &Option<std::ffi::OsString>,
    run: impl FnOnce(&proof_load::scrub::Scratch, &Environment) -> Result<T, String>,
) -> Result<T, String> {
    let scratch = volume.scratch(label).map_err(|error| error.to_string())?;
    let root = scratch.root().to_path_buf();
    let result = Environment::scrubbed(&scratch, search_path.clone(), &BTreeMap::new()).map_err(|error| error.to_string())
        .and_then(|environment| run(&scratch, &environment));
    drop(scratch);
    #[allow(clippy::disallowed_methods, reason = "a scratch area the harness made itself on the capped volume; no retry is wanted")]
    let _ = std::fs::remove_dir_all(&root);
    result
}

/// The corpus a faulted command runs over, shared by every lane: the small shape with one record
/// past the history, the one the faulted command writes.
fn faulted_corpus(seed: u64) -> corpus::Corpus {
    corpus::generate(seed, corpus::Shape { records: corpus::Shape::SMALL.records + 1, ..corpus::Shape::SMALL })
}
