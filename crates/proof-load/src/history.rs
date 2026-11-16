//! The history subjects of the damage category over a real history: git's and forklift's
//! pristine subjects built from one mirrored repository, the probe ref made current in each, and
//! the sample of positions every damage row reads back.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::Duration;

use crate::faulted::{Position, PROBE_REF};
use crate::runner::RunError;
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome};

/// The positions a history's damage rows read back, per subject.
pub const SAMPLED: usize = 8;

/// Runs one build step in `directory`, kept in `steps`; a step that does not exit 0 fails the
/// build, naming it.
#[allow(clippy::too_many_arguments)]
fn step(steps: &mut Vec<Outcome>, program: &Path, arguments: &[&str], directory: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Outcome, RunError> {
    let invocation = Invocation {
        program: program.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
        directory: directory.to_path_buf(), limit,
    };
    let outcome = tool::run(&invocation, scratch, environment).map_err(RunError::Tool)?;
    steps.push(outcome.clone());
    if !outcome.succeeded() {
        return Err(RunError::Io(format!("the history subject could not be built: {:?} exited {:?}: {}", outcome.arguments, outcome.exit, String::from_utf8_lossy(&outcome.stderr).trim())));
    }
    Ok(outcome)
}

/// Builds git's pristine history subject at `to`, which must not exist yet, from the mirror at
/// `mirror`: the mirror copied as `to/.git` and made a work tree's, given the configuration every
/// repository git makes is given, its `HEAD` checked out, and [`PROBE_REF`] made at that tip and
/// current. The copy is the harness's, so the mirror is read only. With the steps it ran.
pub fn build_git(git: &Path, mirror: &Path, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Vec<Outcome>, RunError> {
    std::fs::create_dir(to).map_err(|error| RunError::Io(format!("{}: {}", to.display(), error)))?;
    crate::scrub::copy_tree(mirror, &to.join(".git")).map_err(|error| RunError::Io(format!("{:?}", error)))?;
    let mut steps = Vec::new();
    let mut run = |arguments: &[&str]| step(&mut steps, git, arguments, to, scratch, environment, limit);
    run(&["config", "core.bare", "false"])?;
    for [key, value] in crate::runner::GIT_CREATED_CONFIG {
        run(&["config", key, value])?;
    }
    run(&["checkout", "-f", "-q"])?;
    run(&["switch", "-q", "-c", PROBE_REF])?;
    Ok(steps)
}

/// Builds forklift's pristine history subject at `to`, which must not exist yet: a fresh
/// warehouse that imports the mirror at `mirror`, its work tree restored (an import leaves it
/// empty), and [`PROBE_REF`] made as a pallet at the current head and shifted to. With the steps
/// it ran.
pub fn build_forklift(forklift: &Path, mirror: &Path, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Vec<Outcome>, RunError> {
    std::fs::create_dir(to).map_err(|error| RunError::Io(format!("{}: {}", to.display(), error)))?;
    let mut steps = Vec::new();
    let mirror = mirror.display().to_string();
    let mut run = |arguments: &[&str]| step(&mut steps, forklift, arguments, to, scratch, environment, limit);
    run(&["--json", "prepare"])?;
    run(&["--json", "git", "import", "--", &mirror])?;
    run(&["--json", "restore", "."])?;
    run(&["--json", "pallet", "create", PROBE_REF])?;
    Ok(steps)
}

/// Each local branch of git's subject at `subject` with its first-parent length, the probe ref
/// left out: the positions a sample can name on it.
pub fn git_lengths(git: &Path, subject: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<BTreeMap<String, usize>, RunError> {
    let mut steps = Vec::new();
    let listed = step(&mut steps, git, &["for-each-ref", "--format=%(refname)", "refs/heads/"], subject, scratch, environment, limit)?;
    let mut lengths = BTreeMap::new();
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        let Some(name) = line.strip_prefix("refs/heads/") else { continue };
        if name == PROBE_REF {
            continue;
        }
        let counted = step(&mut steps, git, &["rev-list", "--first-parent", "--count", line], subject, scratch, environment, limit)?;
        let count = String::from_utf8_lossy(&counted.stdout).trim().parse::<usize>()
            .map_err(|error| RunError::Io(format!("git counted {:?} for {}: {}", String::from_utf8_lossy(&counted.stdout), line, error)))?;
        lengths.insert(name.to_string(), count);
    }
    Ok(lengths)
}

/// The pallets forklift's subject at `subject` lists, the probe ref left out.
pub fn pallets(forklift: &Path, subject: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<BTreeSet<String>, RunError> {
    let mut steps = Vec::new();
    let listed = step(&mut steps, forklift, &["--json", "pallet", "list"], subject, scratch, environment, limit)?;
    let document = crate::runner::document(&listed.stdout);
    Ok(document["data"]["pallets"].as_array().into_iter().flatten()
        .filter_map(|pallet| pallet["name"].as_str())
        .filter(|name| *name != PROBE_REF)
        .map(str::to_string)
        .collect())
}

/// `count` distinct positions picked by `seed` among every depth of every ref in `lengths` that
/// `present` also holds, sorted; all of them when there are fewer. Each is drawn as an index into
/// the refs' lengths laid end to end, so no candidate list is built. Positions are distinct, not
/// commits: refs sharing a trunk name its commits under several positions, and two picks may read
/// one commit.
pub fn sample(lengths: &BTreeMap<String, usize>, present: &BTreeSet<String>, count: usize, seed: u64) -> Vec<Position> {
    let refs: Vec<(&String, usize)> = lengths.iter().filter(|(reference, _)| present.contains(*reference)).map(|(reference, &length)| (reference, length)).collect();
    let total: u64 = refs.iter().map(|(_, length)| *length as u64).sum();
    let mut state = seed;
    let mut drawn = BTreeSet::new();
    while (drawn.len() as u64) < total.min(count as u64) {
        drawn.insert(splitmix(&mut state) % total);
    }
    let mut picked: Vec<Position> = drawn.into_iter().map(|mut index| {
        let mut refs = refs.iter();
        loop {
            let (reference, length) = refs.next().expect("an index below the total falls in some ref");
            if index < *length as u64 {
                return Position { reference: reference.to_string(), depth: index as usize };
            }
            index -= *length as u64;
        }
    }).collect();
    picked.sort();
    picked
}

/// One step of splitmix64 over `state`.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::damage::Kind;
    use crate::faulted::{baseline, run_history, Baseline, HistorySubject, Programs, PROBE_BYTES, PROBE_FILE};
    use crate::oracle::{Grade, Tree};
    use crate::scenario::Tool;
    use crate::scrub::path_git;
    use std::path::PathBuf;

    const LIMIT: Duration = Duration::from_secs(60);

    fn scratch(label: &str) -> (Scratch, Environment) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-history-{}-{}-{}", label, std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let search = crate::scrub::search_path(Some(&path_git()), std::env::var_os("PATH"));
        let environment = Environment::scrubbed(&scratch, search, &BTreeMap::new()).unwrap();
        (scratch, environment)
    }

    fn git(arguments: &[&str], directory: &Path, scratch: &Scratch, environment: &Environment) -> String {
        let mut steps = Vec::new();
        let outcome = step(&mut steps, &path_git(), arguments, directory, scratch, environment, LIMIT).unwrap();
        String::from_utf8_lossy(&outcome.stdout).trim().to_string()
    }

    fn tree(entries: &[(&str, &str)]) -> Tree {
        entries.iter().map(|(path, contents)| (path.to_string(), crate::oracle::digest(contents.as_bytes()))).collect()
    }

    /// A small repository mirrored in `scratch`: `main` of three commits, each adding a file,
    /// then a merge of `side`, one commit off `main`'s first, so a first-parent walk of `main`
    /// differs from a walk of every parent.
    fn mirrored(scratch: &Scratch, environment: &Environment) -> PathBuf {
        let source = scratch.root().join("source");
        std::fs::create_dir(&source).unwrap();
        git(&["init", "-q", "-b", "main"], &source, scratch, environment);
        for (index, name) in ["a", "b", "c"].iter().enumerate() {
            std::fs::write(source.join(name), name).unwrap();
            git(&["add", "-A"], &source, scratch, environment);
            git(&["-c", "user.name=p", "-c", "user.email=p@example.invalid", "commit", "-q", "-m", &format!("record {}", index)], &source, scratch, environment);
            if index == 0 {
                git(&["branch", "side"], &source, scratch, environment);
            }
        }
        git(&["switch", "-q", "side"], &source, scratch, environment);
        std::fs::write(source.join("s"), "s").unwrap();
        git(&["add", "-A"], &source, scratch, environment);
        git(&["-c", "user.name=p", "-c", "user.email=p@example.invalid", "commit", "-q", "-m", "side"], &source, scratch, environment);
        git(&["switch", "-q", "main"], &source, scratch, environment);
        git(&["-c", "user.name=p", "-c", "user.email=p@example.invalid", "merge", "-q", "--no-ff", "--no-edit", "side"], &source, scratch, environment);
        let mirror = scratch.root().join("mirror");
        git(&["clone", "-q", "--mirror", "--no-local", "--", &source.display().to_string(), &mirror.display().to_string()], scratch.root(), scratch, environment);
        mirror
    }

    fn programs(forklift: PathBuf) -> Programs {
        Programs { forklift, git: path_git(), server: None }
    }

    /// The sample names only depths each ref has, only refs both subjects hold, never repeats a
    /// position, and is fixed by its seed; asked for more than there are, it names them all.
    ///
    /// Revert: drop the `present` filter → a ref only one subject holds is named; keep every draw
    /// rather than distinct ones → a position repeats.
    #[test]
    fn the_sample_stays_inside_both_subjects() {
        let lengths: BTreeMap<String, usize> = [("main".to_string(), 5), ("side".to_string(), 2), ("gone".to_string(), 4)].into_iter().collect();
        let present: BTreeSet<String> = ["main".to_string(), "side".to_string()].into_iter().collect();
        for seed in 0..50 {
            let picked = sample(&lengths, &present, 4, seed);
            assert_eq!(picked.len(), 4);
            assert!(picked.iter().all(|position| position.reference != "gone" && position.depth < lengths[&position.reference]), "{:?}", picked);
            assert_eq!(picked.iter().collect::<BTreeSet<_>>().len(), 4, "{:?}", picked);
            assert_eq!(picked, sample(&lengths, &present, 4, seed));
        }
        assert_eq!(sample(&lengths, &present, 100, 1).len(), 7);
    }

    /// Git's history subject reads back every sampled position as that commit's tree, through
    /// its own refs and parent links, and the probe ref as the tip's tree before the probe
    /// record and with the probe file after it.
    ///
    /// Revert: resolve positions without the depth → every position reads as its ref's tip;
    /// write the probe through the corpus writer → the history's files are removed and `after`
    /// holds the probe alone.
    #[test]
    fn git_reads_a_real_history_by_position() {
        let (shared, environment) = scratch("git-baseline");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT).unwrap();
        let lengths = git_lengths(&path_git(), &pristine, &shared, &environment, LIMIT).unwrap();
        assert_eq!(lengths, [("main".to_string(), 4), ("side".to_string(), 2)].into_iter().collect());
        let positions = vec![
            Position { reference: "main".into(), depth: 0 }, Position { reference: "main".into(), depth: 1 },
            Position { reference: "main".into(), depth: 3 }, Position { reference: "side".into(), depth: 0 },
        ];
        let (row, environment) = scratch("git-baseline-row");
        let got = baseline(Tool::Git, &pristine, &positions, &programs(PathBuf::from("forklift-not-used")), &row, &environment, LIMIT).unwrap();
        let tip = tree(&[("a", "a"), ("b", "b"), ("c", "c"), ("s", "s")]);
        let mut after = tip.clone();
        after.insert(PROBE_FILE.to_string(), crate::oracle::digest(PROBE_BYTES));
        let positions = vec![tip.clone(), tree(&[("a", "a"), ("b", "b"), ("c", "c")]), tree(&[("a", "a")]), tree(&[("a", "a"), ("s", "s")])];
        assert_eq!(got, Baseline { positions, before: tip, after });
    }

    /// Git's whole-store check names damage the sample did not reach: a commit only an unsampled
    /// branch holds, zeroed, leaves every sampled position and the probe record as they were, so
    /// the row grades healed, and `fsck` still reports it. That branch's ref deleted instead
    /// leaves the commit dangling, which is no damage to the store: the check passes.
    ///
    /// Revert: skip the whole-store check → nothing is unreached.
    #[test]
    fn git_names_damage_the_sample_did_not_reach() {
        let (shared, environment) = scratch("git-unreached");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT).unwrap();
        // A loose commit on a branch of its own, holding the tip's tree: nothing sampled reaches it.
        let tip_tree = git(&["rev-parse", "main^{tree}"], &pristine, &shared, &environment);
        let lone = git(&["-c", "user.name=p", "-c", "user.email=p@example.invalid", "commit-tree", "-m", "lone", &tip_tree], &pristine, &shared, &environment);
        git(&["update-ref", "refs/heads/lone", &lone], &pristine, &shared, &environment);
        let object = format!("objects/{}/{}", &lone[..2], &lone[2..]);
        let positions = vec![Position { reference: "main".into(), depth: 1 }, Position { reference: "side".into(), depth: 0 }];
        let programs = programs(PathBuf::from("forklift-not-used"));
        let (row, environment) = scratch("git-unreached-base");
        let base = baseline(Tool::Git, &pristine, &positions, &programs, &row, &environment, LIMIT).unwrap();
        let subject = HistorySubject { tool: Tool::Git, pristine: &pristine, positions: &positions, baseline: &base };
        for (kind, name, target, unreached) in [(Kind::Zero, "objects/<xx>/<hash>", object.as_str(), true), (Kind::Delete, "refs/heads/**", "refs/heads/lone", false)] {
            let (row, environment) = scratch("git-unreached-row");
            let faulted = run_history(&subject, name, kind, &programs, &row, &environment, LIMIT, 0).unwrap();
            assert_eq!(faulted.target, target, "{:?}: the lone commit is the only loose object, its ref the first loose ref", kind);
            assert_eq!(faulted.grade, Grade::Healed, "{:?}: {:?}", kind, faulted.readbacks);
            assert_eq!(faulted.unreached.is_some(), unreached, "{:?}: {:?}", kind, faulted.unreached);
            assert_eq!(faulted.steps.last().map(|step| step.arguments[0].as_str()), Some("fsck"), "{:?}", kind);
        }
    }

    /// Damage to the refs the sample reads is reached by it: with `packed-refs` deleted, every
    /// sampled branch fails to resolve, which git refuses per record, so the row is never healed
    /// and nothing is reported unreached.
    ///
    /// A pin, not a falsifier of one line: git refuses an unresolvable ref however it is asked.
    /// How a position is resolved is what `git_reads_a_real_history_by_position` falsifies.
    #[test]
    fn git_reaches_damage_to_the_sampled_refs() {
        let (shared, environment) = scratch("git-reached");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT).unwrap();
        let positions = vec![Position { reference: "main".into(), depth: 1 }, Position { reference: "side".into(), depth: 0 }];
        let programs = programs(PathBuf::from("forklift-not-used"));
        let (row, environment) = scratch("git-reached-base");
        let base = baseline(Tool::Git, &pristine, &positions, &programs, &row, &environment, LIMIT).unwrap();
        let subject = HistorySubject { tool: Tool::Git, pristine: &pristine, positions: &positions, baseline: &base };
        let (row, environment) = scratch("git-reached-row");
        let faulted = run_history(&subject, "packed-refs", Kind::Delete, &programs, &row, &environment, LIMIT, 0).unwrap();
        assert_ne!(faulted.grade, Grade::Healed, "{:?}", faulted.readbacks);
        assert!(faulted.unreached.is_none(), "{:?}", faulted.unreached);
    }

    /// Forklift's history subject, built by importing the same mirror, reads back every position
    /// as git's does, through its own pallets, history walk and a fresh bay per position; a
    /// deleted pallet is reached by the sample.
    ///
    /// Revert: create the position's pallet under the bay's own name → every position refuses
    /// and the baseline fails; follow a merge's last parent → `main~1` reads `side`'s commit.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn forklift_reads_a_real_history_as_git_does() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (shared, environment) = scratch("forklift-baseline");
        let mirror = mirrored(&shared, &environment);
        let (git_pristine, forklift_pristine) = (shared.root().join("git"), shared.root().join("forklift"));
        build_git(&path_git(), &mirror, &git_pristine, &shared, &environment, LIMIT).unwrap();
        build_forklift(&forklift, &mirror, &forklift_pristine, &shared, &environment, LIMIT).unwrap();
        let lengths = git_lengths(&path_git(), &git_pristine, &shared, &environment, LIMIT).unwrap();
        let present = pallets(&forklift, &forklift_pristine, &shared, &environment, LIMIT).unwrap();
        assert_eq!(present, ["main".to_string(), "side".to_string()].into_iter().collect());
        let positions = sample(&lengths, &present, SAMPLED, 0);
        assert_eq!(positions.len(), 6);
        let programs = programs(forklift);
        let (row, environment) = scratch("forklift-baseline-git");
        let git_base = baseline(Tool::Git, &git_pristine, &positions, &programs, &row, &environment, LIMIT).unwrap();
        let (row, environment) = scratch("forklift-baseline-forklift");
        let forklift_base = baseline(Tool::Forklift, &forklift_pristine, &positions, &programs, &row, &environment, LIMIT).unwrap();
        assert_eq!(forklift_base, git_base);

        // A deleted pallet is reached by the sample: its positions no longer read, so the row is
        // never healed, and nothing is reported unreached.
        let forklift_pristine = forklift_pristine.as_path();
        let subject = HistorySubject { tool: Tool::Forklift, pristine: forklift_pristine, positions: &positions, baseline: &forklift_base };
        let (row, environment) = scratch("forklift-row");
        let faulted = run_history(&subject, "pallets/side", Kind::Delete, &programs, &row, &environment, LIMIT, 0).unwrap();
        assert_eq!(faulted.target, "pallets/side");
        assert_ne!(faulted.grade, Grade::Healed, "{:?}", faulted.readbacks);
        assert!(faulted.unreached.is_none(), "{:?}", faulted.unreached);
    }
}
