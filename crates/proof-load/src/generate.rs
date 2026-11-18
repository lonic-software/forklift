//! Generated histories for `proof-load perf`: a `git fast-import` stream, written from a named
//! shape and its fixed seed, imported into a bare repository that is then the mirror both tools'
//! subjects are built from (see `history::build_git` and `history::build_forklift`).
//!
//! The stream is written to a file, never held as snapshots: every text file is kept as one token
//! per line, so a shape's memory is its file count times its line count, not its history.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::history::splitmix;
use crate::runner::RunError;
use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation};

/// The branch every shape writes beside `main`, diverging from it by a few commits that change
/// files `main` also changes after the fork, in other lines, so a merge of it is clean.
pub const SIDE_BRANCH: &str = "perf-side";

/// The scale of one generated history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Shape {
    pub name: &'static str,
    pub seed: u64,
    /// Text files, spread evenly over the leaf directories.
    pub files: usize,
    /// Directories at each level below the root.
    pub width: usize,
    /// Levels of directories above every text file.
    pub depth: usize,
    /// Large binary files, each `binary_bytes` long, beside the text files.
    pub binaries: usize,
    pub binary_bytes: usize,
    /// Commits on `main`.
    pub commits: usize,
    /// Text files each commit after the first changes, the most-changed file among them.
    pub changed_per_commit: usize,
    /// Commits on [`SIDE_BRANCH`], forked from the commit `side_commits` before `main`'s tip.
    pub side_commits: usize,
}

/// Lines in every text file: `main`'s changes take the lower half, the side branch's the first
/// [`SIDE_LINES`], so the two never touch one hunk.
const LINES: usize = 32;
const SIDE_LINES: usize = 4;

impl Shape {
    pub const SMALL: Shape = Shape {
        name: "small", seed: 0x5eed_0001, files: 400, width: 4, depth: 3, binaries: 2, binary_bytes: 2 << 20,
        commits: 40, changed_per_commit: 8, side_commits: 3,
    };

    pub const ALL: [Shape; 1] = [Shape::SMALL];

    pub fn named(name: &str) -> Option<Shape> {
        Shape::ALL.into_iter().find(|shape| shape.name == name)
    }

    /// The shape as the perf header names it.
    pub fn describe(&self) -> String {
        format!(
            "{} text files in {} levels of {} directories, {} binary files of {} bytes, {} commits on main changing {} files each, {} on {}",
            self.files, self.depth, self.width, self.binaries, self.binary_bytes, self.commits, self.changed_per_commit, self.side_commits, SIDE_BRANCH,
        )
    }

    /// The text file at `index`'s path: its leaf directory, then its name.
    fn text_path(&self, index: usize) -> String {
        let leaves = self.width.pow(u32::try_from(self.depth).unwrap_or(0)).max(1);
        let mut leaf = index % leaves;
        let mut parts = Vec::with_capacity(self.depth + 1);
        for _ in 0..self.depth {
            parts.push(format!("d{}", leaf % self.width.max(1)));
            leaf /= self.width.max(1);
        }
        parts.push(format!("f{:05}.txt", index));
        parts.join("/")
    }

    fn binary_path(index: usize) -> String {
        format!("assets/blob{}.bin", index)
    }
}

/// What a generated history holds that the pairs need: the file blamed, and the text files a
/// diff's edit set is taken from, in path order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Generated {
    pub blame_file: String,
    pub text_files: Vec<String>,
}

/// Writes `shape`'s fast-import stream to `out`, the same bytes for the same shape every time.
pub fn write_stream(shape: &Shape, out: &mut impl Write) -> std::io::Result<Generated> {
    let mut state = shape.seed;
    let mut files: Vec<Vec<u64>> = (0..shape.files).map(|_| (0..LINES).map(|_| splitmix(&mut state)).collect()).collect();
    let mut mark = 0usize;
    let mut main_marks = Vec::with_capacity(shape.commits);
    let mut time = 1_700_000_000u64;

    let header = |out: &mut dyn Write, branch: &str, mark: usize, time: u64, message: &str| -> std::io::Result<()> {
        write!(out, "commit refs/heads/{}\nmark :{}\ncommitter Proof Load <proof-load@example.invalid> {} +0000\n", branch, mark, time)?;
        write!(out, "data {}\n{}\n", message.len(), message)
    };
    let text = |out: &mut dyn Write, path: &str, lines: &[u64]| -> std::io::Result<()> {
        let body: String = lines.iter().enumerate().map(|(line, token)| format!("{:02} {:016x}\n", line, token)).collect();
        write!(out, "M 100644 inline {}\ndata {}\n{}\n", path, body.len(), body)
    };
    let binary = |out: &mut dyn Write, path: &str, state: &mut u64| -> std::io::Result<()> {
        let mut body = Vec::with_capacity(shape.binary_bytes + 8);
        while body.len() < shape.binary_bytes {
            body.extend_from_slice(&splitmix(state).to_le_bytes());
        }
        body.truncate(shape.binary_bytes);
        // A NUL early on, so every tool takes the file for binary whatever bytes the seed drew.
        if let Some(first) = body.first_mut() {
            *first = 0;
        }
        write!(out, "M 100644 inline {}\ndata {}\n", path, body.len())?;
        out.write_all(&body)?;
        out.write_all(b"\n")
    };

    for commit in 0..shape.commits {
        mark += 1;
        header(out, "main", mark, time, &format!("perf commit {}", commit))?;
        if let Some(previous) = main_marks.last() {
            writeln!(out, "from :{}", previous)?;
        }
        if commit == 0 {
            for (index, lines) in files.iter().enumerate() {
                text(out, &shape.text_path(index), lines)?;
            }
            for index in 0..shape.binaries {
                binary(out, &Shape::binary_path(index), &mut state)?;
            }
        } else {
            // File 0 changes in every commit, so it is the most-changed file, the one blamed.
            let mut changed = vec![0usize];
            while changed.len() < shape.changed_per_commit.min(shape.files) {
                let index = (splitmix(&mut state) % shape.files as u64) as usize;
                if !changed.contains(&index) {
                    changed.push(index);
                }
            }
            changed.sort_unstable();
            for index in changed {
                let line = LINES / 2 + (splitmix(&mut state) % (LINES / 2) as u64) as usize;
                files[index][line] = splitmix(&mut state);
                text(out, &shape.text_path(index), &files[index])?;
            }
            if commit == shape.commits / 2 && shape.binaries > 0 {
                binary(out, &Shape::binary_path(0), &mut state)?;
            }
        }
        writeln!(out)?;
        main_marks.push(mark);
        time += 60;
    }

    // The side branch forks `side_commits` before main's tip and changes, in its own lines, the
    // most-changed file and others: files main changes after the fork too.
    let fork = shape.commits.saturating_sub(shape.side_commits + 1);
    let mut side_files = files.clone();
    let mut previous = main_marks.get(fork).copied();
    for commit in 0..shape.side_commits {
        mark += 1;
        header(out, SIDE_BRANCH, mark, time, &format!("perf side commit {}", commit))?;
        if let Some(previous) = previous {
            writeln!(out, "from :{}", previous)?;
        }
        for index in [0, 1 + commit % shape.files.saturating_sub(1).max(1)] {
            if index < shape.files {
                let line = commit % SIDE_LINES;
                side_files[index][line] = splitmix(&mut state);
                text(out, &shape.text_path(index), &side_files[index])?;
            }
        }
        writeln!(out)?;
        previous = Some(mark);
        time += 60;
    }
    files.clear();

    let mut text_files: Vec<String> = (0..shape.files).map(|index| shape.text_path(index)).collect();
    text_files.sort();
    Ok(Generated { blame_file: shape.text_path(0), text_files })
}

/// Writes `shape`'s history as a bare repository at `mirror`, which must not exist yet, through
/// `git fast-import`, its `HEAD` on `main`. The stream is written beside it first, at
/// `mirror` with `.fi` appended.
pub fn mirror(git: &Path, shape: &Shape, mirror: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Result<Generated, RunError> {
    let io = |error: std::io::Error| RunError::Io(format!("{}: {}", mirror.display(), error));
    let stream_path = PathBuf::from(format!("{}.fi", mirror.display()));
    let generated = {
        let mut stream = std::io::BufWriter::new(std::fs::File::create(&stream_path).map_err(io)?);
        let generated = write_stream(shape, &mut stream).map_err(io)?;
        stream.flush().map_err(io)?;
        generated
    };
    std::fs::create_dir(mirror).map_err(io)?;
    let step = |arguments: &[&str], input: Option<&Path>| -> Result<(), RunError> {
        let invocation = Invocation { program: git.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: mirror.to_path_buf(), limit };
        let outcome = match input {
            Some(input) => tool::run_with_input(&invocation, input, scratch, environment),
            None => tool::run(&invocation, scratch, environment),
        }.map_err(RunError::Tool)?;
        if !outcome.succeeded() {
            return Err(RunError::BuildFailed {
                arguments: outcome.arguments, exit: outcome.exit,
                stderr: String::from_utf8_lossy(&outcome.stderr).into_owned(), stdout: String::from_utf8_lossy(&outcome.stdout).into_owned(),
            });
        }
        Ok(())
    };
    step(&["init", "-q", "--bare"], None)?;
    step(&["symbolic-ref", "HEAD", "refs/heads/main"], None)?;
    step(&["fast-import", "--quiet"], Some(&stream_path))?;
    Ok(generated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    const TINY: Shape = Shape { name: "tiny", seed: 7, files: 12, width: 2, depth: 2, binaries: 1, binary_bytes: 4096, commits: 6, changed_per_commit: 3, side_commits: 2 };

    /// One shape writes one stream: the same bytes on every run, and another seed writes another.
    #[test]
    fn a_shape_writes_the_same_stream_every_time() {
        let (mut first, mut second, mut other) = (Vec::new(), Vec::new(), Vec::new());
        let generated = write_stream(&TINY, &mut first).unwrap();
        write_stream(&TINY, &mut second).unwrap();
        write_stream(&Shape { seed: 8, ..TINY }, &mut other).unwrap();
        assert_eq!(first, second);
        assert_ne!(first, other);
        assert_eq!(generated.text_files.len(), 12);
        assert_eq!(generated.blame_file, "d0/d0/f00000.txt");
    }

    /// The mirror git builds from the stream holds every file on main, the most-changed file
    /// changed in every commit, and a side branch that diverges from main and merges into it
    /// cleanly, as the switch and merge pairs need.
    #[test]
    fn the_mirror_holds_the_shape_and_a_side_branch_that_merges() {
        let git = crate::scrub::path_git();
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-generate-{}-{}", std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, std::env::var_os("PATH"), &BTreeMap::new()).unwrap();
        let limit = Duration::from_secs(60);
        let path = scratch.work().join("mirror.git");
        let generated = mirror(&git, &TINY, &path, &scratch, &environment, limit).unwrap();

        let read = |arguments: &[&str], directory: &Path| -> String {
            let invocation = Invocation { program: git.clone(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(), directory: directory.to_path_buf(), limit };
            let outcome = tool::run(&invocation, &scratch, &environment).unwrap();
            assert!(outcome.succeeded(), "{:?}: {}", arguments, String::from_utf8_lossy(&outcome.stderr));
            String::from_utf8_lossy(&outcome.stdout).into_owned()
        };
        assert_eq!(read(&["symbolic-ref", "HEAD"], &path).trim(), "refs/heads/main");
        assert_eq!(read(&["rev-list", "--count", "main"], &path).trim(), "6");
        assert_eq!(read(&["ls-tree", "-r", "--name-only", "main"], &path).lines().count(), 13);
        assert_eq!(read(&["rev-list", "--count", "main", "--", &generated.blame_file], &path).trim(), "6");
        assert_eq!(read(&["rev-list", "--count", "main..perf-side"], &path).trim(), "2");
        assert_eq!(read(&["rev-list", "--count", "perf-side..main"], &path).trim(), "2");

        let work = scratch.work().join("merge");
        read(&["clone", "-q", "--", &path.display().to_string(), &work.display().to_string()], scratch.work().as_path());
        read(&["-c", "user.name=t", "-c", "user.email=t@example.invalid", "merge", "-q", "--no-edit", "origin/perf-side"], &work);
        #[allow(clippy::disallowed_methods)] // a test's own temporary folder
        let _ = std::fs::remove_dir_all(&root);
    }
}
