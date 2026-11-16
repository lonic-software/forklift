//! The `proof-load` CLI: what CI runs. A thin caller of `proof_load::run`, which holds every lane.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use proof_load::run::{self, Progress, RunOptions};

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
        /// hours of them. It is read only: the harness mirrors it into its scratch area first. A
        /// shallow repository is refused.
        #[arg(long)]
        history_from: Option<PathBuf>,
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
    let Command::Run {
        source, out, seed, only, forklift, git, forklift_server, budget, strace, kill, power, power_variants, jobs,
        capped_volume, disk_full, read_only, folding_volume, network, history_from,
    } = Cli::parse().command;
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
