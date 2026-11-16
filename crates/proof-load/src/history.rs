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
/// build as [`RunError::BuildFailed`], the tool's own refusal; a step that could not be run, or
/// ran past `limit`, fails as [`RunError::Tool`].
#[allow(clippy::too_many_arguments)]
fn step(steps: &mut Vec<Outcome>, program: &Path, arguments: &[&str], directory: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Outcome, RunError> {
    let invocation = Invocation {
        program: program.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
        directory: directory.to_path_buf(), limit,
    };
    let outcome = tool::run(&invocation, scratch, environment).map_err(RunError::Tool)?;
    steps.push(outcome.clone());
    if !outcome.succeeded() {
        return Err(RunError::BuildFailed {
            arguments: outcome.arguments.clone(), exit: outcome.exit,
            stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(), stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
        });
    }
    Ok(outcome)
}

/// Builds git's pristine history subject at `to`, which must not exist yet, from the mirror at
/// `mirror`: the mirror copied as `to/.git` and made a work tree's, given the configuration every
/// repository git makes is given, its `HEAD` checked out, and [`PROBE_REF`] made at that tip and
/// current. The copy is the harness's, so the mirror is read only. Each step it ran is kept in
/// `steps`, the failing one too.
///
/// Returns the branch `HEAD` names, which [`build_forklift`] is held to. A detached `HEAD` is
/// refused: forklift's import makes its first branch current when `HEAD` names none, and leaves
/// out a commit no branch holds, so the two subjects' probes would sit on different commits.
#[allow(clippy::too_many_arguments)]
pub fn build_git(git: &Path, mirror: &Path, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration, steps: &mut Vec<Outcome>) -> Result<String, RunError> {
    std::fs::create_dir(to).map_err(|error| RunError::Io(format!("{}: {}", to.display(), error)))?;
    crate::scrub::copy_tree(mirror, &to.join(".git")).map_err(|error| RunError::Io(format!("{:?}", error)))?;
    let mut run = |arguments: &[&str]| step(steps, git, arguments, to, scratch, environment, limit);
    run(&["config", "core.bare", "false"])?;
    for [key, value] in crate::runner::GIT_CREATED_CONFIG {
        run(&["config", key, value])?;
    }
    let head = match run(&["symbolic-ref", "-q", "HEAD"]) {
        Ok(outcome) => String::from_utf8_lossy(&outcome.stdout).trim().to_string(),
        Err(RunError::BuildFailed { exit: Some(1), .. }) => return Err(RunError::Io(
            "the history's HEAD is detached, at no branch; put it on a branch first".to_string())),
        Err(error) => return Err(error),
    };
    let branch = head.strip_prefix("refs/heads/")
        .ok_or_else(|| RunError::Io(format!("the history's HEAD names {:?}, which is no branch", head)))?.to_string();
    run(&["checkout", "-f", "-q"])?;
    run(&["switch", "-q", "-c", PROBE_REF])?;
    Ok(branch)
}

/// Exports forklift's pristine history subject at `warehouse` to git at `to`, which must not
/// exist yet: the export runs in a copy of the warehouse at `copy`, which must not exist yet
/// either, so the subject forklift's rows run over is the one both directions compare. The
/// export must make a branch of `branch`, the pallet git's subject made its probe at, and must
/// make [`PROBE_REF`] current, the pallet forklift's subject is on: an export that does not fails
/// as the export's step. Each step it ran is kept in `steps`, the failing one too.
#[allow(clippy::too_many_arguments)]
pub fn export_forklift(forklift: &Path, warehouse: &Path, branch: &str, copy: &Path, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration, steps: &mut Vec<Outcome>) -> Result<(), RunError> {
    crate::scrub::copy_tree(warehouse, copy).map_err(|error| RunError::Io(format!("{:?}", error)))?;
    let exported = step(steps, forklift, &["--json", "git", "export", "--", &to.display().to_string()], copy, scratch, environment, limit)?;
    let data = &crate::runner::document(&exported.stdout)["data"];
    let current = data["current"].as_str();
    let made = data["branches"].as_array().is_some_and(|branches| branches.iter().any(|made| made.as_str() == Some(branch)));
    if current != Some(PROBE_REF) || !made {
        return Err(RunError::BuildFailed {
            arguments: exported.arguments.clone(), exit: exported.exit, stdout: String::new(),
            stderr: format!("forklift's export made {:?} current, not {:?}, and {} a branch {:?}", current, PROBE_REF, if made { "made" } else { "made no" }, branch),
        });
    }
    Ok(())
}

/// Builds git's pristine history subject over forklift's export at `export` ([`export_forklift`])
/// at `to`, which must not exist yet, as [`build_git`] builds one over the given history: the
/// export mirrored into `mirror`, which must not exist yet, as the given history was, so both git
/// subjects hold their refs in `packed-refs` and their objects in a pack; then, in the mirror,
/// `HEAD` put on `branch` and the probe pallet's branch the export carries deleted, so
/// [`build_git`] makes the probe ref at `branch` as it does over the given history. Each step it
/// ran is kept in `steps`, the failing one too.
#[allow(clippy::too_many_arguments)]
pub fn build_git_export(git: &Path, export: &Path, branch: &str, mirror: &Path, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration, steps: &mut Vec<Outcome>) -> Result<(), RunError> {
    let (export, mirror_text) = (export.display().to_string(), mirror.display().to_string());
    step(steps, git, &["clone", "-q", "--mirror", "--no-local", "--", &export, &mirror_text], scratch.root(), scratch, environment, limit)?;
    step(steps, git, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)], mirror, scratch, environment, limit)?;
    step(steps, git, &["update-ref", "-d", &format!("refs/heads/{}", PROBE_REF)], mirror, scratch, environment, limit)?;
    build_git(git, mirror, to, scratch, environment, limit, steps).map(|_| ())
}

/// Builds forklift's pristine history subject at `to`, which must not exist yet: a fresh
/// warehouse that imports the mirror at `mirror`, its work tree restored (an import leaves it
/// empty), and [`PROBE_REF`] made as a pallet at the current head and shifted to. The import must
/// make `branch` current, the branch git's subject made its probe at ([`build_git`]), or the two
/// probes would sit on different commits, which fails the build as the import's step. Each step it
/// ran is kept in `steps`, the failing one too.
#[allow(clippy::too_many_arguments)]
pub fn build_forklift(forklift: &Path, mirror: &Path, branch: &str, to: &Path, scratch: &Scratch, environment: &Environment, limit: Duration, steps: &mut Vec<Outcome>) -> Result<(), RunError> {
    std::fs::create_dir(to).map_err(|error| RunError::Io(format!("{}: {}", to.display(), error)))?;
    let mirror = mirror.display().to_string();
    let mut run = |arguments: &[&str]| step(steps, forklift, arguments, to, scratch, environment, limit);
    run(&["--json", "prepare"])?;
    let imported = run(&["--json", "git", "import", "--", &mirror])?;
    let current = crate::runner::document(&imported.stdout)["data"]["current"].as_str().map(str::to_string);
    if current.as_deref() != Some(branch) {
        // Forklift's own: the import ran and misbuilt, so it is charged as a failed build step.
        return Err(RunError::BuildFailed {
            arguments: imported.arguments.clone(), exit: imported.exit, stdout: String::new(),
            stderr: format!("forklift's import made {:?} current, not {:?}, the branch git's HEAD names: the probes would sit on different commits", current, branch),
        });
    }
    run(&["--json", "restore", "."])?;
    run(&["--json", "pallet", "create", PROBE_REF])?;
    Ok(())
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
pub(crate) fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::damage::Kind;
    use crate::faulted::{baseline, run_history, Baseline, HistorySubject, Programs, PROBE_BYTES, PROBE_FILE};
    use crate::oracle::{Grade, Tree};
    use crate::scenario::Tool;
    use crate::scrub::path_git;
    use std::path::PathBuf;

    const LIMIT: Duration = Duration::from_secs(60);

    pub(crate) fn scratch(label: &str) -> (Scratch, Environment) {
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
    /// differs from a walk of every parent; the merge tagged, as a real history's releases are.
    pub(crate) fn mirrored(scratch: &Scratch, environment: &Environment) -> PathBuf {
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
        git(&["tag", "v1"], &source, scratch, environment);
        let mirror = scratch.root().join("mirror");
        git(&["clone", "-q", "--mirror", "--no-local", "--", &source.display().to_string(), &mirror.display().to_string()], scratch.root(), scratch, environment);
        mirror
    }

    fn programs(forklift: PathBuf) -> Programs {
        Programs { forklift, git: path_git(), server: None }
    }

    fn census() -> crate::census::Census {
        crate::census::Census::load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")).unwrap().unwrap()
    }

    /// Git's history binding set covers its history subject as built from a mirror: every layout
    /// row needing damage the subject holds is bound or exempt, every binding it holds, and no
    /// exemption names what it holds. Without the history set's own bindings, the fixture set
    /// fails over the same walk, on the rows only a mirror writes.
    ///
    /// Revert: drop the `packed-refs` binding → the row is named unbound; exempt the loose objects
    /// no more → their rows are named unbound.
    #[test]
    fn git_history_bindings_cover_the_history_subject() {
        let (shared, environment) = scratch("git-set");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let walked = crate::census::walk(&pristine.join(".git")).unwrap();
        let census = census();
        for tool in [Tool::Git, Tool::GitTuned] {
            assert_eq!(crate::bindings::check_history_subject(tool, &census, &walked), Vec::<String>::new(), "{:?}: {:?}", tool, walked);
        }
        let scenario = crate::faulted::HISTORY_SCENARIO.to_string();
        let walks = [((scenario.clone(), crate::damage::Warehouse::Client), walked)].into_iter().collect();
        let fixture = crate::bindings::check_git(&[], crate::bindings::GIT_EXEMPTIONS, &[scenario], &walks);
        assert!(fixture.iter().any(|missing| missing.starts_with("stale exemption: packed-refs")), "{:?}", fixture);
    }

    /// Git's subject makes its probe at the branch the mirror's `HEAD` names and says which, so
    /// forklift's is held to the same one; a detached `HEAD` is refused, since forklift's import
    /// would make its first branch current instead. `HEAD` is put on `side`, which is neither
    /// `main` nor the first branch.
    ///
    /// Revert: drop the `symbolic-ref` read → the detached mirror builds; return a fixed branch →
    /// `side` reads as it.
    #[test]
    fn git_s_probe_sits_on_the_branch_head_names() {
        let (shared, environment) = scratch("git-head");
        let mirror = mirrored(&shared, &environment);
        git(&["symbolic-ref", "HEAD", "refs/heads/side"], &mirror, &shared, &environment);
        let pristine = shared.root().join("pristine");
        assert_eq!(build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap(), "side");
        let side = git(&["rev-parse", "refs/heads/side"], &pristine, &shared, &environment);
        assert_eq!(git(&["rev-parse", PROBE_REF], &pristine, &shared, &environment), side);
        let main = git(&["rev-parse", "refs/heads/main"], &mirror, &shared, &environment);
        git(&["update-ref", "--no-deref", "HEAD", &main], &mirror, &shared, &environment);
        let detached = build_git(&path_git(), &mirror, &shared.root().join("detached"), &shared, &environment, LIMIT, &mut Vec::new());
        assert!(matches!(&detached, Err(RunError::Io(reason)) if reason.contains("HEAD is detached")), "{:?}", detached);
    }

    /// Forklift's subject is held to the branch git's made its probe at: the import making another
    /// current fails the build as the import's step, naming both, and the one it made current
    /// builds.
    ///
    /// Revert: drop the check of the import's current pallet → the build held to `main` passes;
    /// refuse it as the harness's own error → it is not charged to forklift.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn forklift_s_probe_is_held_to_git_s_branch() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (shared, environment) = scratch("forklift-head");
        let mirror = mirrored(&shared, &environment);
        git(&["symbolic-ref", "HEAD", "refs/heads/side"], &mirror, &shared, &environment);
        build_forklift(&forklift, &mirror, "side", &shared.root().join("held"), &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let other = build_forklift(&forklift, &mirror, "main", &shared.root().join("other"), &shared, &environment, LIMIT, &mut Vec::new());
        assert!(matches!(&other, Err(RunError::BuildFailed { stderr, .. }) if stderr.contains(r#"made Some("side") current, not "main""#)), "{:?}", other);
    }

    /// Forklift's history binding set covers its history subject as an import builds it: every
    /// census name needing damage the subject holds is bound, and every binding is held.
    ///
    /// Revert: drop the `meta/<name>` binding → the imported tag's name is named unbound.
    #[test]
    #[ignore = "needs PROOF_LOAD_FORKLIFT"]
    fn forklift_history_bindings_cover_the_history_subject() {
        let forklift = PathBuf::from(std::env::var("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT"));
        let (shared, environment) = scratch("forklift-set");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_forklift(&forklift, &mirror, "main", &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let walked = crate::census::walk(&pristine.join(".forklift")).unwrap();
        assert_eq!(crate::bindings::check_history_subject(Tool::Forklift, &census(), &walked), Vec::<String>::new(), "{:?}", walked);
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
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let lengths = git_lengths(&path_git(), &pristine, &shared, &environment, LIMIT).unwrap();
        assert_eq!(lengths, [("main".to_string(), 4), ("side".to_string(), 2)].into_iter().collect());
        let positions = vec![
            Position { reference: "main".into(), depth: 0 }, Position { reference: "main".into(), depth: 1 },
            Position { reference: "main".into(), depth: 3 }, Position { reference: "side".into(), depth: 0 },
        ];
        let (row, environment) = scratch("git-baseline-row");
        let got = baseline(Tool::Git, &pristine, &positions, &programs(PathBuf::from("forklift-not-used")), &row, &environment, LIMIT).unwrap().unwrap();
        let tip = tree(&[("a", "a"), ("b", "b"), ("c", "c"), ("s", "s")]);
        let mut after = tip.clone();
        after.insert(PROBE_FILE.to_string(), crate::oracle::digest(PROBE_BYTES));
        let positions = vec![tip.clone(), tree(&[("a", "a"), ("b", "b"), ("c", "c")]), tree(&[("a", "a")]), tree(&[("a", "a"), ("s", "s")])];
        assert_eq!(got, Baseline { positions, before: tip, after });
    }

    /// A subject that does not read back unfaulted refuses its baseline, which is the subject's:
    /// its packs zeroed, git serves no position. A pristine subject that is not there is the
    /// harness's own failure.
    ///
    /// Revert: return the readback's refusal as the harness's → the first assertion fails.
    #[test]
    fn a_refused_baseline_is_the_subjects_not_the_harnesss() {
        let (shared, environment) = scratch("git-baseline-refused");
        let mirror = mirrored(&shared, &environment);
        let pristine = shared.root().join("pristine");
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let packs = pristine.join(crate::runner::state_folder(Tool::Git)).join("objects/pack");
        let mut zeroed = 0;
        for entry in std::fs::read_dir(&packs).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|extension| extension == "pack") {
                let metadata = std::fs::metadata(&path).unwrap();
                let mut permissions = metadata.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                permissions.set_readonly(false);
                std::fs::set_permissions(&path, permissions).unwrap();
                let length = metadata.len() as usize;
                std::fs::write(&path, vec![0u8; length]).unwrap();
                zeroed += 1;
            }
        }
        assert!(zeroed > 0, "no pack under {}", packs.display());
        let tip = vec![Position { reference: "main".into(), depth: 0 }];
        let (row, environment) = scratch("git-baseline-refused-row");
        let programs = programs(PathBuf::from("forklift-not-used"));
        let refused = baseline(Tool::Git, &pristine, &tip, &programs, &row, &environment, LIMIT).unwrap();
        // The refusal names the read that refused, so a run's report says which command did.
        assert!(refused.as_ref().is_err_and(|refusal| refusal.contains("the unfaulted readback did not serve Position(0): ")), "{:?}", refused);
        let (row, environment) = scratch("git-baseline-absent-row");
        assert!(baseline(Tool::Git, &shared.root().join("absent"), &tip, &programs, &row, &environment, LIMIT).is_err());
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
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        // A loose commit on a branch of its own, holding the tip's tree: nothing sampled reaches it.
        let tip_tree = git(&["rev-parse", "main^{tree}"], &pristine, &shared, &environment);
        let lone = git(&["-c", "user.name=p", "-c", "user.email=p@example.invalid", "commit-tree", "-m", "lone", &tip_tree], &pristine, &shared, &environment);
        git(&["update-ref", "refs/heads/lone", &lone], &pristine, &shared, &environment);
        let object = format!("objects/{}/{}", &lone[..2], &lone[2..]);
        let positions = vec![Position { reference: "main".into(), depth: 1 }, Position { reference: "side".into(), depth: 0 }];
        let programs = programs(PathBuf::from("forklift-not-used"));
        let (row, environment) = scratch("git-unreached-base");
        let base = baseline(Tool::Git, &pristine, &positions, &programs, &row, &environment, LIMIT).unwrap().unwrap();
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
        build_git(&path_git(), &mirror, &pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let positions = vec![Position { reference: "main".into(), depth: 1 }, Position { reference: "side".into(), depth: 0 }];
        let programs = programs(PathBuf::from("forklift-not-used"));
        let (row, environment) = scratch("git-reached-base");
        let base = baseline(Tool::Git, &pristine, &positions, &programs, &row, &environment, LIMIT).unwrap().unwrap();
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
        build_git(&path_git(), &mirror, &git_pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        build_forklift(&forklift, &mirror, "main", &forklift_pristine, &shared, &environment, LIMIT, &mut Vec::new()).unwrap();
        let lengths = git_lengths(&path_git(), &git_pristine, &shared, &environment, LIMIT).unwrap();
        let present = pallets(&forklift, &forklift_pristine, &shared, &environment, LIMIT).unwrap();
        assert_eq!(present, ["main".to_string(), "side".to_string()].into_iter().collect());
        let positions = sample(&lengths, &present, SAMPLED, 0);
        assert_eq!(positions.len(), 6);
        let programs = programs(forklift);
        let (row, environment) = scratch("forklift-baseline-git");
        let git_base = baseline(Tool::Git, &git_pristine, &positions, &programs, &row, &environment, LIMIT).unwrap().unwrap();
        let (row, environment) = scratch("forklift-baseline-forklift");
        let forklift_base = baseline(Tool::Forklift, &forklift_pristine, &positions, &programs, &row, &environment, LIMIT).unwrap().unwrap();
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
