//! The `proof-load` CLI: what CI runs. A thin caller of `proof_load::run`, which holds every lane.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use proof_load::perf;
use proof_load::run::{self, Progress, RunOptions};

#[derive(Parser)]
#[command(name = "proof-load", about = "Fault-injection harness for Forklift")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
#[allow(clippy::large_enum_variant, reason = "parsed once per process, so the size of the larger variant costs nothing")]
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
        /// scenario's; with only `--power` named, none. A hosted scenario's server is killed at
        /// each of its own calls too, given a forklift-server.
        #[arg(long = "kill")]
        kill: Vec<String>,
        /// Power loss of this scenario only; repeatable. Without it or `--kill`, every scenario's;
        /// with only `--kill` named, none. A hosted scenario's server loses power too, given a
        /// forklift-server. Needs strace, like the kill points.
        #[arg(long = "power")]
        power: Vec<String>,
        /// How many seeded states between the durable and the volatile one each scenario's power
        /// loss draws, at seeded moments; a draw that repeats a state already graded is not
        /// graded again, so fewer may be. A server's are shared among the runs its power loss is
        /// cut into.
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
        /// Network faults under this scenario's faulted command, one of each kind at a seeded
        /// request through the proxy every host is served behind; repeatable. Without it, none.
        #[arg(long = "network")]
        network: Vec<String>,
        /// A git repository whose history the fidelity round trips also take, both ways, besides
        /// their fixtures, and the damaged-file rows over a sample of its positions, up to two
        /// hours of them and none past --budget: git's rows over the history itself and over
        /// forklift's export of its import of it, forklift's over that import. It is read only:
        /// the harness mirrors it into its scratch area first. A shallow repository is refused,
        /// and the damaged-file rows refuse one whose HEAD is detached.
        #[arg(long)]
        history_from: Option<PathBuf>,
    },
    /// Times Forklift side by side with git on a generated or a real history, one command at a
    /// time, judges the rows under a gate, and writes perf.json and perf.md.
    Perf {
        /// An empty or new directory for the subjects and the perf report.
        #[arg(long)]
        out: PathBuf,
        /// The forklift binary to time, by path: a release build.
        #[arg(long)]
        forklift: PathBuf,
        /// The git binary to time, by path; it also builds the history.
        #[arg(long)]
        git: PathBuf,
        /// The generated history to time on: small, medium or huge. Small when neither it nor
        /// --history-from is given.
        #[arg(long, conflicts_with = "history_from")]
        shape: Option<String>,
        /// A git repository whose history to time on instead of a generated one. It is read only:
        /// the harness mirrors it into its scratch area first. A shallow repository is refused.
        #[arg(long)]
        history_from: Option<PathBuf>,
        /// The branch of --history-from's history that switch and merge time against. Without it
        /// those rows are reported as not run.
        #[arg(long, requires = "history_from")]
        side_branch: Option<String>,
        /// Which checks fail the run: self, the ones a shared machine can hold (Forklift against
        /// its own ratios, a gross regression against tuned git, a gated row that could not run),
        /// or all, which adds any row that could not run and a clearly slower row no ticket tracks.
        /// All on a generated shape and self on --history-from, which all is refused with: tickets
        /// name a generated shape, so none can track a real history's row.
        #[arg(long, value_parser = ["self", "all"])]
        gate: Option<String>,
        /// Timed repetitions per cell, after one untimed run on a warm cache.
        #[arg(long, default_value_t = 5)]
        reps: usize,
        /// A program, by path, run with no arguments before each cold repetition to drop the
        /// operating system's caches. Without it the cold rows are reported as not run.
        #[arg(long)]
        drop_caches: Option<PathBuf>,
        /// The forklift-server binary, by path, that serves Forklift's remotes for the clone, push
        /// and pull rows. Without it those rows are reported as not run.
        #[arg(long)]
        forklift_server: Option<PathBuf>,
        /// An earlier run's perf.json, by path. The report then sets each row's Forklift median,
        /// as a share of tuned git's, beside the earlier run's. It never fails the run; an earlier
        /// report this build does not read, or of another history or platform, is reported as
        /// not compared.
        #[arg(long)]
        against: Option<PathBuf>,
    },
}

/// Shows a run's messages on stderr; the report itself is the files the run writes.
struct Stderr;

impl Progress for Stderr {
    fn message(&self, text: &str) {
        eprintln!("proof-load: {}", text);
    }
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Run {
            source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
            capped_volume, disk_full, read_only, folding_volume, network, history_from,
        } => {
            let options = RunOptions {
                source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
                capped_volume, disk_full, read_only, folding_volume, network, history_from,
            };
            match run::run(&options, &Stderr) {
                Ok(report) if report.failed() => ExitCode::from(1),
                Ok(_) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("proof-load: {}", error);
                    ExitCode::from(2)
                }
            }
        }
        Command::Perf { out, forklift, git, shape, history_from, side_branch, gate, reps, drop_caches, forklift_server, against } => {
            let gate = gate.map(|gate| perf::Gate::named(&gate).expect("clap admits only the gates perf names"));
            let options = perf::PerfOptions { out, forklift, git, shape, history_from, side_branch, reps, gate, drop_caches, forklift_server, against };
            match perf::run(&options, &|text| eprintln!("proof-load: {}", text)) {
                Ok(report) if report.failed() => ExitCode::from(1),
                Ok(_) => ExitCode::SUCCESS,
                Err(error) => {
                    eprintln!("proof-load: {}", error);
                    ExitCode::from(2)
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// The guide page names every flag of every `proof-load` subcommand in its prose or tables,
    /// outside its code blocks, so a new flag fails until the page mentions it where it explains
    /// things; an example command line alone does not count.
    ///
    /// Revert: drop `--jobs` from the guide → it is named missing; count the code blocks → a flag
    /// only the example shows passes; look up `run` alone → a `perf` flag the guide leaves out
    /// passes.
    #[test]
    fn the_guide_names_every_subcommand_flag() {
        let prose = |page: &str| -> String {
            let mut fenced = false;
            page.lines().filter(|line| {
                if line.trim_start().starts_with("```") {
                    fenced = !fenced;
                    return false;
                }
                !fenced
            }).collect::<Vec<_>>().join("\n")
        };
        assert_eq!(prose("one `--seed`\n```sh\nproof-load run --jobs 2\n```\ntwo"), "one `--seed`\ntwo");
        let guide = prose(&std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide/proof-load.md")).unwrap());
        let cli = super::Cli::command();
        let subcommands: Vec<&clap::Command> = cli.get_subcommands().collect();
        assert_eq!(subcommands.iter().map(|subcommand| subcommand.get_name()).collect::<Vec<_>>(), ["run", "perf"]);
        // A flag counts where it stands alone: `--forklift` inside `--forklift-server` is not it.
        let named = |flag: &str| guide.match_indices(flag).any(|(at, _)| !guide[at + flag.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '-'));
        for subcommand in subcommands {
            let flags: Vec<String> = subcommand.get_arguments().filter_map(|argument| argument.get_long()).map(|long| format!("--{}", long)).collect();
            assert!(flags.len() >= 5, "{}: {:?}", subcommand.get_name(), flags);
            let missing: Vec<&String> = flags.iter().filter(|flag| !named(flag)).collect();
            assert_eq!(missing, Vec::<&String>::new(), "docs/guide/proof-load.md does not name these flags of {}", subcommand.get_name());
        }
    }
}
