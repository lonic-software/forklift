//! The fidelity category: a git history taken through forklift and back, compared commit by commit
//! with what went in.
//!
//! The harness builds a fixture repository with git (`build_fixture`), every commit shape git
//! allows and a pinned date on each, so its commit ids are the same on every run. Forklift imports
//! it into a fresh warehouse and exports that warehouse to a second repository. No commit id
//! survives the trip, so the two are never matched by id: from each ref both hold, the walk pairs
//! tip with tip and parent i with parent i, and compares each pair field by field. Every
//! difference is a finding naming the field, where it is and both values. Nothing is declared an
//! allowed loss: a finding is reported until a fix removes it, and findings never fail a run.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::scrub::{Environment, Scratch};
use crate::tool::{self, Invocation, Outcome};

/// Which way a history went round.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// A git repository imported by forklift, then exported back to git.
    GitForkliftGit,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::GitForkliftGit => "git to Forklift to git",
        }
    }
}

/// One difference between what went in and what came back.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub direction: Direction,
    /// What differs, which names the loss class: `ref`, `ref kind`, `commit`, `tree entry`,
    /// `parent count`, `author name` and the other identity parts, `message`, `header <name>`, or
    /// `tag <part>` for an annotated tag's object.
    pub field: String,
    /// Where: a ref's name, or a commit's position from the ref the walk reached it by (`main~2^2`),
    /// then a path for a tree entry.
    pub at: String,
    /// The value that went in; none where nothing did.
    pub before: Option<String>,
    /// The value that came back; none where nothing did.
    pub after: Option<String>,
}

/// One round trip: the commands it ran, and what it found or why it could not finish.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Trip {
    pub direction: Direction,
    pub steps: Vec<Outcome>,
    pub findings: Result<Vec<Finding>, String>,
}

/// The fixed identity and date the fixture's commits and tags are made with, besides the ones a
/// step sets on purpose.
const FIXTURE_IDENTITY: &[(&str, &str)] = &[
    ("GIT_AUTHOR_NAME", "Fixture Author"), ("GIT_AUTHOR_EMAIL", "author@example.com"), ("GIT_AUTHOR_DATE", "1700000000 +0000"),
    ("GIT_COMMITTER_NAME", "Fixture Committer"), ("GIT_COMMITTER_EMAIL", "committer@example.com"), ("GIT_COMMITTER_DATE", "1700000000 +0000"),
];

/// Runs commands in one scratch area and keeps every outcome.
struct Steps<'a> {
    scratch: &'a Scratch,
    limit: Duration,
    outcomes: Vec<Outcome>,
}

impl Steps<'_> {
    /// Runs `program` with `arguments` in `directory`, refusing a run that does not exit 0 with
    /// what it said.
    fn run(&mut self, program: &Path, arguments: &[&str], directory: &Path, environment: &Environment) -> Result<Vec<u8>, String> {
        let invocation = Invocation {
            program: program.to_path_buf(), arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
            directory: directory.to_path_buf(), limit: self.limit,
        };
        let outcome = tool::run(&invocation, self.scratch, environment).map_err(|error| error.to_string())?;
        let stdout = outcome.stdout.clone();
        let refused = (!outcome.succeeded()).then(|| format!("`{}` exited {}: {}", arguments.join(" "),
            outcome.exit.map_or("on a signal".to_string(), |code| code.to_string()), String::from_utf8_lossy(&outcome.stderr).trim()));
        self.outcomes.push(outcome);
        match refused {
            Some(reason) => Err(reason),
            None => Ok(stdout),
        }
    }
}

/// Writes `bytes` to `path`, saying which file could not be written.
fn write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|error| format!("cannot write {}: {}", path.display(), error))
}

/// The first line of `stdout`, trimmed: the id a command printed.
fn printed(stdout: &[u8]) -> String {
    String::from_utf8_lossy(stdout).lines().next().unwrap_or_default().trim().to_string()
}

/// Builds the fixture repository at `root`, which must not exist yet, with `git`: a root commit
/// whose author and committer differ in name and zone, holding an executable file, a symlink and
/// a CRLF file; an empty commit; one with an empty message; one in ISO-8859-1 with its encoding
/// header; one with a trailer; a branch merged back; a gitlink; a three-parent octopus; a commit
/// with a gpgsig header; a lightweight and an annotated tag; a note; and a remote-tracking ref.
/// Every date is pinned, so the fixture's commit ids are the same on every run.
fn build_fixture(steps: &mut Steps, git: &Path, root: &Path, environment: &Environment) -> Result<(), String> {
    let mut fixed = environment.clone();
    fixed.variables.extend(FIXTURE_IDENTITY.iter().map(|(name, value)| (name.to_string(), value.to_string())));
    let parent = root.parent().ok_or("the fixture has no parent folder")?.to_path_buf();
    let name = root.file_name().ok_or("the fixture has no name")?.to_string_lossy().into_owned();
    steps.run(git, &["init", "-q", "-b", "main", &name], &parent, &fixed)?;
    let mut git_in = |arguments: &[&str], environment: &Environment| steps.run(git, arguments, root, environment);
    for [key, value] in [["core.autocrlf", "false"], ["gc.auto", "0"], ["commit.gpgSign", "false"], ["tag.gpgSign", "false"]] {
        git_in(&["config", key, value], &fixed)?;
    }
    for (file, bytes) in [("a.txt", &b"a\n"[..]), ("run.sh", b"#!/bin/sh\n"), ("crlf.txt", b"crlf\r\n")] {
        write(&root.join(file), bytes)?;
    }
    write(&parent.join("link-target"), b"a.txt")?;
    git_in(&["add", "--", "a.txt", "run.sh", "crlf.txt"], &fixed)?;
    git_in(&["update-index", "--chmod=+x", "--", "run.sh"], &fixed)?;
    let link = printed(&git_in(&["hash-object", "-w", "--", &parent.join("link-target").display().to_string()], &fixed)?);
    git_in(&["update-index", "--add", "--cacheinfo", &format!("120000,{},link", link)], &fixed)?;
    let mut zoned = fixed.clone();
    zoned.variables.extend([("GIT_AUTHOR_DATE", "1700000000 +0130"), ("GIT_COMMITTER_DATE", "1700000100 -0700")]
        .map(|(name, value)| (name.to_string(), value.to_string())));
    git_in(&["commit", "-q", "-m", "root: author and committer differ, two zones"], &zoned)?;
    git_in(&["commit", "-q", "--allow-empty", "-m", "empty commit"], &fixed)?;
    git_in(&["commit", "-q", "--allow-empty", "--allow-empty-message", "-m", ""], &fixed)?;
    write(&parent.join("latin1-message"), b"latin-1\n\ncaf\xe9\n")?;
    git_in(&["-c", "i18n.commitEncoding=ISO-8859-1", "commit", "-q", "--allow-empty", "-F", &parent.join("latin1-message").display().to_string()], &fixed)?;
    write(&parent.join("trailer-message"), b"subject\n\nbody line\n\nSigned-off-by: X <x@example.com>\n")?;
    git_in(&["commit", "-q", "--allow-empty", "-F", &parent.join("trailer-message").display().to_string()], &fixed)?;
    git_in(&["switch", "-q", "-c", "side"], &fixed)?;
    write(&root.join("side.txt"), b"side\n")?;
    git_in(&["add", "--", "side.txt"], &fixed)?;
    git_in(&["commit", "-q", "-m", "on side"], &fixed)?;
    git_in(&["switch", "-q", "main"], &fixed)?;
    write(&root.join("a.txt"), b"a\nb\n")?;
    // Never `commit -a`: the symlink and the gitlink are in the index only, and the work tree's
    // run.sh has no executable bit, so `-a` would stage all three as changed.
    git_in(&["add", "--", "a.txt"], &fixed)?;
    git_in(&["commit", "-q", "-m", "on main"], &fixed)?;
    git_in(&["merge", "-q", "--no-edit", "side"], &fixed)?;
    git_in(&["tag", "light"], &fixed)?;
    git_in(&["tag", "-a", "-m", "annotated", "annot"], &fixed)?;
    git_in(&["update-index", "--add", "--cacheinfo", "160000,1111111111111111111111111111111111111111,sub"], &fixed)?;
    git_in(&["commit", "-q", "-m", "gitlink"], &fixed)?;
    // Two branches off the gitlink commit's parent, neither descending from the other, merged
    // into main as one commit of three parents.
    let base = printed(&git_in(&["rev-parse", "HEAD~1"], &fixed)?);
    let base_tree = printed(&git_in(&["rev-parse", "HEAD~1^{tree}"], &fixed)?);
    let mut sides = Vec::new();
    for branch in ["o1", "o2"] {
        let commit = printed(&git_in(&["commit-tree", &base_tree, "-p", &base, "-m", branch], &fixed)?);
        git_in(&["update-ref", &format!("refs/heads/{}", branch), &commit], &fixed)?;
        sides.push(commit);
    }
    let head = printed(&git_in(&["rev-parse", "HEAD"], &fixed)?);
    let head_tree = printed(&git_in(&["rev-parse", "HEAD^{tree}"], &fixed)?);
    let octopus = printed(&git_in(&["commit-tree", &head_tree, "-p", &head, "-p", &sides[0], "-p", &sides[1], "-m", "octopus: three parents"], &fixed)?);
    git_in(&["update-ref", "refs/heads/main", &octopus], &fixed)?;
    let signed = format!("tree {}\nparent {}\nauthor S <s@example.com> 1700000200 +0000\ncommitter S <s@example.com> 1700000200 +0000\n\
        gpgsig -----BEGIN PGP SIGNATURE-----\n \n fixture\n -----END PGP SIGNATURE-----\n\nsigned\n", head_tree, octopus);
    write(&parent.join("signed-commit"), signed.as_bytes())?;
    let signed = printed(&git_in(&["hash-object", "-t", "commit", "-w", "--", &parent.join("signed-commit").display().to_string()], &fixed)?);
    git_in(&["update-ref", "refs/heads/signed", &signed], &fixed)?;
    git_in(&["notes", "add", "-m", "note", "HEAD"], &fixed)?;
    git_in(&["update-ref", "refs/remotes/origin/main", "HEAD"], &fixed)?;
    Ok(())
}

/// A ref's kind, as the comparison names it.
fn ref_kind(name: &str, object_type: &str) -> &'static str {
    match (name.split('/').nth(1), object_type) {
        (Some("heads"), _) => "branch",
        (Some("tags"), "tag") => "annotated tag",
        (Some("tags"), _) => "lightweight tag",
        (Some("notes"), _) => "note",
        (Some("remotes"), _) => "remote-tracking ref",
        _ => "other ref",
    }
}

/// Every ref of the repository at `root`: its name, then its object's type and id.
fn refs(steps: &mut Steps, git: &Path, root: &Path, environment: &Environment) -> Result<BTreeMap<String, (String, String)>, String> {
    let listed = steps.run(git, &["for-each-ref", "--format=%(refname)%09%(objecttype)%09%(objectname)"], root, environment)?;
    Ok(String::from_utf8_lossy(&listed).lines().filter_map(|line| {
        let mut parts = line.split('\t');
        Some((parts.next()?.to_string(), (parts.next()?.to_string(), parts.next()?.to_string())))
    }).collect())
}

/// A commit or tag object split into its headers, in order, and its message. A header's
/// continuation lines are part of its value.
fn parse_object(bytes: &[u8]) -> (Vec<(String, Vec<u8>)>, Vec<u8>) {
    let mut headers: Vec<(String, Vec<u8>)> = Vec::new();
    let mut rest = bytes;
    while !rest.is_empty() {
        let end = rest.iter().position(|byte| *byte == b'\n').unwrap_or(rest.len());
        let line = &rest[..end];
        rest = rest.get(end + 1..).unwrap_or_default();
        if line.is_empty() {
            return (headers, rest.to_vec());
        }
        match (line.first(), headers.last_mut()) {
            (Some(b' '), Some((_, value))) => {
                value.push(b'\n');
                value.extend_from_slice(&line[1..]);
            }
            _ => {
                let space = line.iter().position(|byte| *byte == b' ').unwrap_or(line.len());
                headers.push((String::from_utf8_lossy(&line[..space]).into_owned(), line.get(space + 1..).unwrap_or_default().to_vec()));
            }
        }
    }
    (headers, Vec::new())
}

/// An identity header's parts: name, email, time and zone.
fn identity_parts(value: &[u8]) -> [(&'static str, String); 4] {
    let text = String::from_utf8_lossy(value);
    let (name, rest) = text.split_once(" <").unwrap_or((&text, ""));
    let (email, when) = rest.split_once("> ").unwrap_or((rest, ""));
    let (time, zone) = when.split_once(' ').unwrap_or((when, ""));
    [("name", name.to_string()), ("email", email.to_string()), ("time", time.to_string()), ("zone", zone.to_string())]
}

/// Bytes as a finding shows them: printable ASCII as is, anything else escaped, cut at 200
/// characters.
fn shown(bytes: &[u8]) -> String {
    let text = bytes.escape_ascii().to_string();
    if text.chars().count() > 200 { format!("{}…", text.chars().take(200).collect::<String>()) } else { text }
}

/// A commit's position: the ref the walk reached it from, then each parent step, a run of first
/// parents written `~n`.
fn position(start: &str, path: &[usize]) -> String {
    let mut text = start.trim_start_matches("refs/heads/").to_string();
    let mut firsts = 0;
    for (index, step) in path.iter().enumerate() {
        if *step == 1 {
            firsts += 1;
        }
        if *step != 1 || index + 1 == path.len() {
            match firsts {
                0 => {}
                1 => text.push_str("~1"),
                n => text.push_str(&format!("~{}", n)),
            }
            firsts = 0;
            if *step != 1 {
                text.push_str(&format!("^{}", step));
            }
        }
    }
    text
}

/// Compares two repositories: `before`, what went in, and `after`, what came back.
struct Comparison<'a, 'b> {
    steps: &'a mut Steps<'b>,
    git: &'a Path,
    environment: &'a Environment,
    before: PathBuf,
    after: PathBuf,
    direction: Direction,
    findings: Vec<Finding>,
}

impl Comparison<'_, '_> {
    fn find(&mut self, field: impl Into<String>, at: impl Into<String>, before: Option<String>, after: Option<String>) {
        self.findings.push(Finding { direction: self.direction, field: field.into(), at: at.into(), before, after });
    }

    fn cat(&mut self, side: &Path, kind: &str, id: &str) -> Result<Vec<u8>, String> {
        let side = side.to_path_buf();
        self.steps.run(self.git, &["cat-file", kind, id], &side, self.environment)
    }

    /// A tree's every entry, recursively, by path: its mode, type and id.
    fn entries(&mut self, side: &Path, tree: &str) -> Result<BTreeMap<String, String>, String> {
        let side = side.to_path_buf();
        let listed = self.steps.run(self.git, &["ls-tree", "-r", "-t", "-z", "--full-tree", tree], &side, self.environment)?;
        Ok(listed.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()).filter_map(|entry| {
            let tab = entry.iter().position(|byte| *byte == b'\t')?;
            Some((shown(&entry[tab + 1..]), String::from_utf8_lossy(&entry[..tab]).into_owned()))
        }).collect())
    }

    /// Compares the headers both objects have by name, apart from `skipped`; a header one side
    /// lacks is a finding with nothing on that side.
    fn headers(&mut self, prefix: &str, at: &str, before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)], skipped: &[&str]) {
        let names: BTreeSet<&String> = before.iter().chain(after).map(|(name, _)| name).filter(|name| !skipped.contains(&name.as_str())).collect();
        for name in names {
            let value = |headers: &[(String, Vec<u8>)]| -> Option<Vec<u8>> {
                let values: Vec<&[u8]> = headers.iter().filter(|(header, _)| header == name).map(|(_, value)| value.as_slice()).collect();
                (!values.is_empty()).then(|| values.join(&b"\n"[..]))
            };
            let (was, now) = (value(before), value(after));
            if was != now {
                self.find(format!("{}{}", prefix, name), at, was.as_deref().map(shown), now.as_deref().map(shown));
            }
        }
    }

    /// The identity headers both objects carry, part by part.
    fn identities(&mut self, prefix: &str, at: &str, before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)], names: &[&str]) {
        for name in names {
            let get = |headers: &[(String, Vec<u8>)]| headers.iter().find(|(header, _)| header == name).map(|(_, value)| identity_parts(value));
            match (get(before), get(after)) {
                (Some(was), Some(now)) => {
                    for ((part, was), (_, now)) in was.into_iter().zip(now) {
                        if was != now {
                            self.find(format!("{}{} {}", prefix, name, part), at, Some(was), Some(now));
                        }
                    }
                }
                (None, None) => {}
                (was, now) => self.find(format!("{}{}", prefix, name), at, was.map(|_| "present".to_string()), now.map(|_| "present".to_string())),
            }
        }
    }

    /// Compares one pair of commits and returns their parents, in order.
    fn commit(&mut self, at: &str, before: &str, after: &str) -> Result<(Vec<String>, Vec<String>), String> {
        let (before_dir, after_dir) = (self.before.clone(), self.after.clone());
        let (was, was_message) = parse_object(&self.cat(&before_dir, "commit", before)?);
        let (now, now_message) = parse_object(&self.cat(&after_dir, "commit", after)?);
        let all = |headers: &[(String, Vec<u8>)], name: &str| -> Vec<String> {
            headers.iter().filter(|(header, _)| header == name).map(|(_, value)| String::from_utf8_lossy(value).into_owned()).collect()
        };
        let (was_tree, now_tree) = (all(&was, "tree").concat(), all(&now, "tree").concat());
        if was_tree != now_tree {
            let (was_entries, now_entries) = (self.entries(&before_dir, &was_tree)?, self.entries(&after_dir, &now_tree)?);
            let paths: BTreeSet<&String> = was_entries.keys().chain(now_entries.keys()).collect();
            for path in paths {
                let (was_entry, now_entry) = (was_entries.get(path), now_entries.get(path));
                if was_entry != now_entry {
                    self.find("tree entry", format!("{} {}", at, path), was_entry.cloned(), now_entry.cloned());
                }
            }
        }
        let (was_parents, now_parents) = (all(&was, "parent"), all(&now, "parent"));
        if was_parents.len() != now_parents.len() {
            self.find("parent count", at, Some(was_parents.len().to_string()), Some(now_parents.len().to_string()));
        }
        self.identities("", at, &was, &now, &["author", "committer"]);
        if was_message != now_message {
            self.find("message", at, Some(shown(&was_message)), Some(shown(&now_message)));
        }
        self.headers("header ", at, &was, &now, &["tree", "parent", "author", "committer"]);
        Ok((was_parents, now_parents))
    }

    /// Compares two annotated tags' objects, apart from the ids they point at.
    fn tag(&mut self, at: &str, before: &str, after: &str) -> Result<(), String> {
        let (before_dir, after_dir) = (self.before.clone(), self.after.clone());
        let (was, was_message) = parse_object(&self.cat(&before_dir, "tag", before)?);
        let (now, now_message) = parse_object(&self.cat(&after_dir, "tag", after)?);
        self.identities("tag ", at, &was, &now, &["tagger"]);
        if was_message != now_message {
            self.find("tag message", at, Some(shown(&was_message)), Some(shown(&now_message)));
        }
        self.headers("tag ", at, &was, &now, &["object", "tagger"]);
        Ok(())
    }

    /// The commit a ref's object peels to, if any.
    fn peeled(&mut self, side: &Path, id: &str) -> Option<String> {
        let side = side.to_path_buf();
        self.steps.run(self.git, &["rev-parse", "--verify", "-q", &format!("{}^{{commit}}", id)], &side, self.environment).ok().map(|stdout| printed(&stdout))
    }

    /// Walks both repositories in lockstep from every ref either holds.
    fn walk(mut self) -> Result<Vec<Finding>, String> {
        let (before_dir, after_dir) = (self.before.clone(), self.after.clone());
        let (git, environment) = (self.git, self.environment);
        let was_refs = refs(self.steps, git, &before_dir, environment)?;
        let now_refs = refs(self.steps, git, &after_dir, environment)?;
        let names: BTreeSet<&String> = was_refs.keys().chain(now_refs.keys()).collect();
        // Each commit that went in is compared once, at the first position the walk reaches it by:
        // ref by ref in name order, so a branch reaches a commit before a tag on it does.
        let mut seen = BTreeSet::new();
        for name in names {
            match (was_refs.get(name), now_refs.get(name)) {
                (Some((kind, _)), None) => self.find("ref", name.as_str(), Some(ref_kind(name, kind).to_string()), None),
                (None, Some((kind, _))) => self.find("ref", name.as_str(), None, Some(ref_kind(name, kind).to_string())),
                (Some((was_type, was_id)), Some((now_type, now_id))) => {
                    let (was_kind, now_kind) = (ref_kind(name, was_type), ref_kind(name, now_type));
                    if was_kind != now_kind {
                        self.find("ref kind", name.as_str(), Some(was_kind.to_string()), Some(now_kind.to_string()));
                    } else if was_type == "tag" {
                        self.tag(name, was_id, now_id)?;
                    }
                    if let (Some(was), Some(now)) = (self.peeled(&before_dir, was_id), self.peeled(&after_dir, now_id)) {
                        self.lockstep(name, was, now, &mut seen)?;
                    }
                }
                (None, None) => {}
            }
        }
        Ok(self.findings)
    }

    /// Walks from one ref's commits, `was` and `now`, breadth first: tip with tip, parent i with
    /// parent i, skipping a commit that went in which is `seen` already. A parent one side has and
    /// the other lacks is a `commit` finding at its position.
    fn lockstep(&mut self, start: &str, was: String, now: String, seen: &mut BTreeSet<String>) -> Result<(), String> {
        let mut queue = VecDeque::from([(Vec::new(), was, now)]);
        while let Some((path, was, now)) = queue.pop_front() {
            if !seen.insert(was.clone()) {
                continue;
            }
            let at = position(start, &path);
            let (was_parents, now_parents) = self.commit(&at, &was, &now)?;
            for index in 0..was_parents.len().max(now_parents.len()) {
                let mut next = path.clone();
                next.push(index + 1);
                match (was_parents.get(index), now_parents.get(index)) {
                    (Some(was), Some(now)) => queue.push_back((next, was.clone(), now.clone())),
                    (Some(_), None) => self.find("commit", position(start, &next), Some("present".to_string()), None),
                    (None, Some(_)) => self.find("commit", position(start, &next), None, Some("present".to_string())),
                    (None, None) => {}
                }
            }
        }
        Ok(())
    }
}

/// The git to forklift to git round trip in `scratch`: the fixture built with `git`, imported by
/// `forklift` into a fresh warehouse, exported to a new repository and compared with the fixture.
pub fn git_round_trip(forklift: &Path, git: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Trip {
    let mut steps = Steps { scratch, limit, outcomes: Vec::new() };
    let fixture = scratch.root().join("fixture");
    let warehouse = scratch.root().join("imported");
    let exported = scratch.root().join("exported");
    let findings = (|| -> Result<Vec<Finding>, String> {
        build_fixture(&mut steps, git, &fixture, environment).map_err(|error| format!("the fixture could not be built: {}", error))?;
        std::fs::create_dir(&warehouse).map_err(|error| format!("cannot create {}: {}", warehouse.display(), error))?;
        steps.run(forklift, &["--json", "prepare"], &warehouse, environment)?;
        steps.run(forklift, &["--json", "git", "import", "--", &fixture.display().to_string()], &warehouse, environment)?;
        steps.run(forklift, &["--json", "git", "export", "--", &exported.display().to_string()], &warehouse, environment)?;
        Comparison { steps: &mut steps, git, environment, before: fixture.clone(), after: exported.clone(), direction: Direction::GitForkliftGit, findings: Vec::new() }.walk()
    })();
    Trip { direction: Direction::GitForkliftGit, steps: steps.outcomes, findings }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_writes_first_parent_runs_as_a_count() {
        assert_eq!(position("refs/heads/main", &[]), "main");
        assert_eq!(position("refs/heads/main", &[1]), "main~1");
        assert_eq!(position("refs/heads/main", &[1, 1, 1]), "main~3");
        assert_eq!(position("refs/heads/main", &[2]), "main^2");
        assert_eq!(position("refs/heads/main", &[1, 3, 1, 1]), "main~1^3~2");
        assert_eq!(position("refs/tags/light", &[2, 1]), "refs/tags/light^2~1");
    }

    #[test]
    fn an_object_splits_into_headers_with_continuations_and_a_message() {
        let (headers, message) = parse_object(b"tree t\nparent p\ngpgsig A\n \n B\nauthor N <e> 1 +0000\n\nsubject\n\nbody\n");
        assert_eq!(headers.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(), ["tree", "parent", "gpgsig", "author"]);
        assert_eq!(headers[2].1, b"A\n\nB");
        assert_eq!(message, b"subject\n\nbody\n");
        let (headers, message) = parse_object(b"tree t\n\n");
        assert_eq!((headers.len(), message), (1, Vec::new()));
    }

    #[test]
    fn an_identity_splits_into_name_email_time_and_zone() {
        assert_eq!(identity_parts(b"A Name <a@example.com> 1700000000 +0130").map(|(_, value)| value),
            ["A Name", "a@example.com", "1700000000", "+0130"].map(str::to_string));
    }

    #[test]
    fn a_ref_kind_follows_its_namespace_and_object() {
        assert_eq!(ref_kind("refs/heads/main", "commit"), "branch");
        assert_eq!(ref_kind("refs/tags/annot", "tag"), "annotated tag");
        assert_eq!(ref_kind("refs/tags/light", "commit"), "lightweight tag");
        assert_eq!(ref_kind("refs/notes/commits", "commit"), "note");
        assert_eq!(ref_kind("refs/remotes/origin/main", "commit"), "remote-tracking ref");
        assert_eq!(ref_kind("refs/stash", "commit"), "other ref");
    }

    #[test]
    fn shown_bytes_escape_what_is_not_printable_and_stop_at_200() {
        assert_eq!(shown(b"caf\xe9\n"), "caf\\xe9\\n");
        assert_eq!(shown(&[b'x'; 300]).chars().count(), 201);
    }

    fn git() -> PathBuf {
        let name = if cfg!(windows) { "git.exe" } else { "git" };
        std::env::split_paths(&std::env::var_os("PATH").expect("a PATH")).map(|folder| folder.join(name))
            .find(|path| path.is_file()).expect("a git on PATH")
    }

    fn area(label: &str) -> (Scratch, Environment) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-fidelity-{}-{}-{}", label, std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, crate::scrub::search_path(Some(&git()), std::env::var_os("PATH")), &BTreeMap::new()).unwrap();
        (scratch, environment)
    }

    fn compare(steps: &mut Steps, environment: &Environment, before: &Path, after: &Path) -> Vec<Finding> {
        let git = git();
        Comparison { steps, git: &git, environment, before: before.to_path_buf(), after: after.to_path_buf(), direction: Direction::GitForkliftGit, findings: Vec::new() }
            .walk().unwrap()
    }

    /// Two builds of the fixture are the same commits, and compared they differ in nothing: a
    /// finding is never the fixture's own doing. The fixture holds every shape the comparison is
    /// for, each checked here by git, not by the walk.
    ///
    /// Revert: unpin FIXTURE_IDENTITY's committer date → the two builds' main differ.
    #[test]
    fn the_fixture_is_the_same_on_every_build_and_holds_every_shape() {
        let (scratch, environment) = area("fixture");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = git();
        let (one, two) = (scratch.root().join("one"), scratch.root().join("two"));
        build_fixture(&mut steps, &git, &one, &environment).unwrap();
        // A second apart, so an unpinned date would differ between the two builds.
        let now = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
        let first = now();
        while now() == first {
            std::thread::sleep(Duration::from_millis(50));
        }
        build_fixture(&mut steps, &git, &two, &environment).unwrap();
        let mut in_repo = |root: &Path, arguments: &[&str]| String::from_utf8_lossy(&steps.run(&git, arguments, root, &environment).unwrap()).into_owned();
        assert_eq!(in_repo(&one, &["rev-parse", "main", "signed"]), in_repo(&two, &["rev-parse", "main", "signed"]));
        assert_eq!(in_repo(&one, &["rev-list", "--min-parents=3", "--count", "main"]).trim(), "1");
        assert!(in_repo(&one, &["cat-file", "commit", "signed"]).contains("\ngpgsig "));
        assert_eq!(in_repo(&one, &["log", "--format=%e", "main"]).lines().filter(|line| *line == "ISO-8859-1").count(), 1);
        assert!(in_repo(&one, &["ls-tree", "main"]).lines().any(|line| line.starts_with("160000 commit ") && line.ends_with("\tsub")));
        assert!(in_repo(&one, &["ls-tree", "main"]).lines().any(|line| line.starts_with("120000 blob ") && line.ends_with("\tlink")));
        assert!(in_repo(&one, &["ls-tree", "main"]).lines().any(|line| line.starts_with("100755 blob ") && line.ends_with("\trun.sh")));
        let kinds = in_repo(&one, &["for-each-ref", "--format=%(objecttype) %(refname)"]);
        for kind in ["commit refs/tags/light", "tag refs/tags/annot", "commit refs/notes/commits", "commit refs/remotes/origin/main"] {
            assert!(kinds.lines().any(|line| line == kind), "{} in {}", kind, kinds);
        }
        assert_eq!(compare(&mut steps, &environment, &one, &two), Vec::<Finding>::new());
    }

    /// Each kind of difference the walk is for is found, named by its field and its position, with
    /// both values: two small repositories built by hand to differ in exactly those. Before: a root
    /// commit with an executable file under a lightweight tag and a note, and main a two-parent
    /// merge carrying a gpgsig header. After: the file not executable, the names and zones
    /// changed, a trailer on the root's message, main with one parent and no header, the tag
    /// annotated and no note.
    ///
    /// Revert: compare only the tree id, not its entries → no `tree entry` finding.
    #[test]
    fn the_walk_reports_each_field_a_changed_copy_differs_in() {
        let (scratch, environment) = area("walk");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = git();
        let (before, after) = (scratch.root().join("before"), scratch.root().join("after"));
        let mut fixed = environment.clone();
        fixed.variables.extend(FIXTURE_IDENTITY.iter().map(|(name, value)| (name.to_string(), value.to_string())));
        let make = |steps: &mut Steps, root: &Path, author: &str, mode: &str, root_message: &str| -> (String, String) {
            let mut identity = fixed.clone();
            let (name, zone) = author.split_once(' ').unwrap();
            identity.variables.extend([("GIT_AUTHOR_NAME", name), ("GIT_AUTHOR_DATE", &format!("1700000000 {}", zone))].map(|(key, value)| (key.to_string(), value.to_string())));
            steps.run(&git, &["init", "-q", "-b", "main", &root.file_name().unwrap().to_string_lossy()], scratch.root(), &identity).unwrap();
            let mut in_repo = |arguments: &[&str]| printed(&steps.run(&git, arguments, root, &identity).unwrap());
            write(&root.join("tool"), b"x\n").unwrap();
            let blob = in_repo(&["hash-object", "-w", "--", "tool"]);
            in_repo(&["update-index", "--add", "--cacheinfo", &format!("{},{},tool", mode, blob)]);
            let tree = in_repo(&["write-tree"]);
            let first = in_repo(&["commit-tree", &tree, "-m", root_message]);
            (tree, first)
        };
        let (tree, first) = make(&mut steps, &before, "Ann +0130", "100755", "same");
        let mut in_before = |arguments: &[&str]| printed(&steps.run(&git, arguments, &before, &fixed).unwrap());
        let side = in_before(&["commit-tree", &tree, "-p", &first, "-m", "side"]);
        let merge = format!("tree {}\nparent {}\nparent {}\nauthor Ann <author@example.com> 1700000000 +0130\n\
            committer Fixture Committer <committer@example.com> 1700000000 +0000\ngpgsig A\n B\n\nmerge\n", tree, first, side);
        write(&scratch.root().join("merge"), merge.as_bytes()).unwrap();
        let merge = in_before(&["hash-object", "-t", "commit", "-w", "--", &scratch.root().join("merge").display().to_string()]);
        for (name, id) in [("refs/heads/main", &merge), ("refs/tags/v", &first), ("refs/notes/extra", &first)] {
            in_before(&["update-ref", name, id]);
        }
        let (tree, first) = make(&mut steps, &after, "author@example.com +0000", "100644", "same\n\nForklift-Parcel: x");
        let mut in_after = |arguments: &[&str]| printed(&steps.run(&git, arguments, &after, &fixed).unwrap());
        let merge = in_after(&["commit-tree", &tree, "-p", &first, "-m", "merge"]);
        in_after(&["update-ref", "refs/heads/main", &merge]);
        in_after(&["tag", "-a", "-m", "v", "v", &first]);

        let mut found: Vec<(String, String)> = compare(&mut steps, &environment, &before, &after).into_iter().map(|finding| (finding.field, finding.at)).collect();
        found.sort();
        let mut expected: Vec<(String, String)> = [
            ("author name", "main"), ("author zone", "main"), ("commit", "main^2"), ("header gpgsig", "main"), ("parent count", "main"), ("tree entry", "main tool"),
            ("author name", "main~1"), ("author zone", "main~1"), ("message", "main~1"), ("tree entry", "main~1 tool"),
            ("ref", "refs/notes/extra"), ("ref kind", "refs/tags/v"),
        ].map(|(field, at)| (field.to_string(), at.to_string())).into_iter().collect();
        expected.sort();
        assert_eq!(found, expected);
        let findings = compare(&mut steps, &environment, &before, &after);
        let value = |field: &str| findings.iter().find(|finding| finding.field == field).map(|finding| (finding.before.clone(), finding.after.clone())).unwrap();
        assert_eq!(value("author zone"), (Some("+0130".to_string()), Some("+0000".to_string())));
        assert_eq!(value("parent count"), (Some("2".to_string()), Some("1".to_string())));
        assert_eq!(value("header gpgsig"), (Some("A\\nB".to_string()), None));
        assert_eq!(value("ref"), (Some("note".to_string()), None));
        assert_eq!(value("ref kind"), (Some("lightweight tag".to_string()), Some("annotated tag".to_string())));
        let entry = value("tree entry");
        assert!(entry.0.unwrap().starts_with("100755 blob ") && entry.1.unwrap().starts_with("100644 blob "));
    }

    /// The round trip runs against a real forklift and finishes, with every command it ran kept.
    ///
    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn the_round_trip_finishes_against_forklift() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let (scratch, environment) = area("trip");
        let trip = git_round_trip(&forklift, &git(), &scratch, &environment, Duration::from_secs(120));
        let findings = trip.findings.unwrap();
        for finding in &findings {
            println!("{:?}", finding);
        }
        assert!(trip.steps.iter().any(|step| step.arguments.iter().any(|argument| argument == "import")));
    }
}
