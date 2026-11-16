//! The fidelity category: a history taken from git through Forklift and back, and one taken from
//! Forklift through git and back, each compared with what went in.
//!
//! Git to Forklift to git: the harness builds a fixture repository with git (`build_fixture`),
//! every commit shape git allows and a pinned date on each, so its commit ids are the same on
//! every run. Forklift imports it into a fresh warehouse and exports that warehouse to a second
//! repository. No commit id survives the trip, so the two are never matched by id: from each ref
//! both hold, the walk pairs tip with tip and parent i with parent i, and compares each pair field
//! by field.
//!
//! Forklift to git to Forklift: forklift builds a native history (`build_warehouse`), exports it,
//! and imports the export into a fresh warehouse. The two warehouses' `history --json` records are
//! walked the same way from each pallet both hold, and each record's tree is read through a git
//! export of its own warehouse. The same exporter reads both, so a tree difference is a loss of
//! the trip, never of the reading; a loss the exporter makes the same way twice cancels out there
//! and is not seen here.
//!
//! Every difference is a finding naming the field, where it is and both values. Nothing is
//! declared an allowed loss: a finding is reported until a fix removes it, and findings never fail
//! a run.

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
    /// A Forklift-native history exported to git, then imported into a fresh warehouse.
    ForkliftGitForklift,
}

impl Direction {
    pub fn label(self) -> &'static str {
        match self {
            Direction::GitForkliftGit => "git to Forklift to git",
            Direction::ForkliftGitForklift => "Forklift to git to Forklift",
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
    /// `tag <part>` for an annotated tag's object; from Forklift, `pallet`, `record`, `actions`,
    /// `<action> operator` and its time and trust, `manifest entry`, `keyed value`, or `tag <part>`.
    pub field: String,
    /// Where: a ref's, pallet's or tag's name, or a commit's or record's position from the ref or
    /// pallet the walk reached it by (`main~2^2`), then a path for a tree entry; a keyed value is
    /// its key on such a position.
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

    /// Runs a query whose exit 1 means "no": its stdout on exit 0, none on exit 1, and any other
    /// end, or a command that never ran, refused with what it said.
    fn ask(&mut self, program: &Path, arguments: &[&str], directory: &Path, environment: &Environment) -> Result<Option<Vec<u8>>, String> {
        let ran = self.outcomes.len();
        match self.run(program, arguments, directory, environment) {
            Ok(stdout) => Ok(Some(stdout)),
            Err(_) if self.outcomes.len() > ran && self.outcomes.last().is_some_and(|outcome| outcome.exit == Some(1) && outcome.failure.is_none()) => Ok(None),
            Err(reason) => Err(reason),
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

/// A tree's every entry in the repository at `side`, recursively, by path: its mode, type and id.
fn tree_entries(steps: &mut Steps, git: &Path, side: &Path, tree: &str, environment: &Environment) -> Result<BTreeMap<String, String>, String> {
    let listed = steps.run(git, &["ls-tree", "-r", "-t", "-z", "--full-tree", tree], side, environment)?;
    Ok(listed.split(|byte| *byte == 0).filter(|entry| !entry.is_empty()).filter_map(|entry| {
        let tab = entry.iter().position(|byte| *byte == b'\t')?;
        Some((shown(&entry[tab + 1..]), String::from_utf8_lossy(&entry[..tab]).into_owned()))
    }).collect())
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
    /// Each commit that went in, by id, with the commit it was compared to and its position.
    paired: BTreeMap<String, (String, String)>,
    /// Each commit that came back, by id, with the commit that went in it was compared to.
    returned: BTreeMap<String, String>,
}

impl<'a, 'b> Comparison<'a, 'b> {
    fn new(steps: &'a mut Steps<'b>, git: &'a Path, environment: &'a Environment, before: &Path, after: &Path, direction: Direction) -> Self {
        Comparison {
            steps, git, environment, before: before.to_path_buf(), after: after.to_path_buf(), direction,
            findings: Vec::new(), paired: BTreeMap::new(), returned: BTreeMap::new(),
        }
    }

    fn find(&mut self, field: impl Into<String>, at: impl Into<String>, before: Option<String>, after: Option<String>) {
        self.findings.push(Finding { direction: self.direction, field: field.into(), at: at.into(), before, after });
    }

    fn cat(&mut self, side: &Path, kind: &str, id: &str) -> Result<Vec<u8>, String> {
        let side = side.to_path_buf();
        self.steps.run(self.git, &["cat-file", kind, id], &side, self.environment)
    }

    fn entries(&mut self, side: &Path, tree: &str) -> Result<BTreeMap<String, String>, String> {
        tree_entries(self.steps, self.git, side, tree, self.environment)
    }

    /// Compares the headers either object has by name, apart from `skipped`: every value of a
    /// name, in order, so one header split into two differs from it. A header one side lacks is a
    /// finding with nothing on that side. The headers of names both sides hold equally often,
    /// `skipped` ones included, in a different sequence, are a `header order` finding (`tag order`
    /// for a tag).
    fn headers(&mut self, prefix: &str, at: &str, before: &[(String, Vec<u8>)], after: &[(String, Vec<u8>)], skipped: &[&str]) {
        let names: BTreeSet<&String> = before.iter().chain(after).map(|(name, _)| name).filter(|name| !skipped.contains(&name.as_str())).collect();
        for name in &names {
            let values = |headers: &[(String, Vec<u8>)]| -> Vec<Vec<u8>> {
                headers.iter().filter(|(header, _)| header == *name).map(|(_, value)| value.clone()).collect()
            };
            let (was, now) = (values(before), values(after));
            if was != now {
                // One value on each side as it is; otherwise every value bracketed, so no single
                // value reads as several.
                let bracket = was.len() > 1 || now.len() > 1;
                let show = |values: Vec<Vec<u8>>| match values.as_slice() {
                    [] => None,
                    [value] if !bracket => Some(shown(value)),
                    values => Some(values.iter().map(|value| format!("[{}]", shown(value))).collect::<Vec<_>>().join(" ")),
                };
                self.find(format!("{}{}", prefix, name), at, show(was), show(now));
            }
        }
        // Every header in sequence, `skipped` ones too, of the names both sides hold equally many
        // of: a name whose count differs is a finding already, above or where it is compared.
        let count = |headers: &[(String, Vec<u8>)], name: &str| headers.iter().filter(|(header, _)| header == name).count();
        let order = |headers: &[(String, Vec<u8>)]| -> Vec<String> {
            headers.iter().map(|(name, _)| name.clone())
                .filter(|name| count(before, name) == count(after, name)).collect()
        };
        let (was, now) = (order(before), order(after));
        if was != now {
            self.find(format!("{}order", prefix), at, Some(was.join(" ")), Some(now.join(" ")));
        }
    }

    /// The identity headers both objects carry, the first of each name part by part; a name held a
    /// different number of times on each side is a `<name> count` finding too.
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
            let count = |headers: &[(String, Vec<u8>)]| headers.iter().filter(|(header, _)| header == name).count();
            let (was, now) = (count(before), count(after));
            if was != now && was > 0 && now > 0 {
                self.find(format!("{}{} count", prefix, name), at, Some(was.to_string()), Some(now.to_string()));
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

    /// The commit a ref's object peels to; none where it peels to no commit, and a git that fails
    /// otherwise is an error.
    fn peeled(&mut self, side: &Path, id: &str) -> Result<Option<String>, String> {
        let side = side.to_path_buf();
        Ok(self.steps.ask(self.git, &["rev-parse", "--verify", "-q", &format!("{}^{{commit}}", id)], &side, self.environment)?.map(|stdout| printed(&stdout)))
    }

    /// Compares every ref either repository holds: one only one side holds is a `ref` finding, one
    /// whose kind changed a `ref kind` finding, an annotated tag's object by content. From each ref
    /// both hold that peels to a commit on both sides, the two histories are walked in lockstep.
    /// Notes refs are compared last, by the commits they annotate, since a notes tree names each
    /// note by its commit's id and no id survives the trip.
    fn walk(mut self) -> Result<Vec<Finding>, String> {
        let (before_dir, after_dir) = (self.before.clone(), self.after.clone());
        let (git, environment) = (self.git, self.environment);
        let was_refs = refs(self.steps, git, &before_dir, environment)?;
        let now_refs = refs(self.steps, git, &after_dir, environment)?;
        let mut names: Vec<&String> = was_refs.keys().chain(now_refs.keys()).collect::<BTreeSet<_>>().into_iter().collect();
        names.sort_by_key(|name| (name.starts_with("refs/notes/"), !name.starts_with("refs/heads/")));
        // Branches first, so a commit is named by a branch position where one reaches it.
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
                    if name.starts_with("refs/notes/") {
                        self.notes(name)?;
                        continue;
                    }
                    match (self.peeled(&before_dir, was_id)?, self.peeled(&after_dir, now_id)?) {
                        (Some(was), Some(now)) => self.lockstep(name, was, now)?,
                        (None, None) => {}
                        (was, now) => {
                            let reaches = |commit: Option<String>| Some(if commit.is_some() { "a commit" } else { "no commit" }.to_string());
                            self.find("ref target", name.as_str(), reaches(was), reaches(now));
                        }
                    }
                }
                (None, None) => {}
            }
        }
        Ok(self.findings)
    }

    /// Walks from one ref's commits, `was` and `now`, breadth first: tip with tip, parent i with
    /// parent i. A commit that went in is compared once, at the first position the walk reaches
    /// it by. Reached again paired with a different commit, or a commit that came back paired with
    /// a second one that went in, it is a `commit identity` finding: a ref or a parent that came
    /// back pointing elsewhere. A parent one side has and the other lacks is a `commit` finding at
    /// its position, saying how many commits only it reaches.
    fn lockstep(&mut self, start: &str, was: String, now: String) -> Result<(), String> {
        let mut queue = VecDeque::from([(Vec::new(), was, now)]);
        while let Some((path, was, now)) = queue.pop_front() {
            let at = position(start, &path);
            if let Some((paired, first)) = self.paired.get(&was) {
                if *paired != now {
                    let first = first.clone();
                    self.find("commit identity", at, Some(format!("the commit at {}", first)), Some("a different commit".to_string()));
                }
                continue;
            }
            if let Some(other) = self.returned.get(&now) {
                let first = self.paired[other].1.clone();
                self.find("commit identity", at.as_str(), Some("a distinct commit".to_string()), Some(format!("the commit at {}", first)));
            }
            self.paired.insert(was.clone(), (now.clone(), at.clone()));
            self.returned.entry(now.clone()).or_insert_with(|| was.clone());
            let (was_parents, now_parents) = self.commit(&at, &was, &now)?;
            for index in 0..was_parents.len().max(now_parents.len()) {
                let mut next = path.clone();
                next.push(index + 1);
                match (was_parents.get(index), now_parents.get(index)) {
                    (Some(was), Some(now)) => queue.push_back((next, was.clone(), now.clone())),
                    (Some(lost), None) => {
                        let only = self.only_below(&self.before.clone(), lost, &was_parents[..now_parents.len()])?;
                        self.find("commit", position(start, &next), Some(only), None);
                    }
                    (None, Some(added)) => {
                        let only = self.only_below(&self.after.clone(), added, &now_parents[..was_parents.len()])?;
                        self.find("commit", position(start, &next), None, Some(only));
                    }
                    (None, None) => {}
                }
            }
        }
        Ok(())
    }

    /// How many commits `parent` reaches that none of `kept` does, as a finding shows it.
    fn only_below(&mut self, side: &Path, parent: &str, kept: &[String]) -> Result<String, String> {
        let side = side.to_path_buf();
        let mut arguments = vec!["rev-list", "--count", parent, "--not"];
        arguments.extend(kept.iter().map(String::as_str));
        let count = printed(&self.steps.run(self.git, &arguments, &side, self.environment)?);
        Ok(format!("present, with {} commit{} only it reaches", count, if count == "1" { "" } else { "s" }))
    }

    /// The notes of one notes ref, keyed by the commit each annotates.
    fn note_list(&mut self, side: &Path, name: &str) -> Result<BTreeMap<String, String>, String> {
        let side = side.to_path_buf();
        let listed = self.steps.run(self.git, &["notes", "--ref", name, "list"], &side, self.environment)?;
        Ok(String::from_utf8_lossy(&listed).lines().filter_map(|line| line.split_once(' ')).map(|(note, object)| (object.to_string(), note.to_string())).collect())
    }

    /// Compares a notes ref both repositories hold through the commits the walk paired: each note
    /// with the note on its commit's counterpart, by content. A note on a commit the walk never
    /// reached is a finding at that commit's id, which is all that names it.
    fn notes(&mut self, name: &str) -> Result<(), String> {
        let (before_dir, after_dir) = (self.before.clone(), self.after.clone());
        let (was_notes, now_notes) = (self.note_list(&before_dir, name)?, self.note_list(&after_dir, name)?);
        // Each note that came back is matched by every commit that went in paired with its
        // commit, so commits collapsed into one each compare with its note.
        let mut annotated = BTreeSet::new();
        for (object, was_note) in was_notes {
            let Some((counterpart, at)) = self.paired.get(&object).cloned() else {
                self.find("note", format!("{} on {}", name, object), Some("a note on a commit never compared".to_string()), None);
                continue;
            };
            let at = format!("{} on {}", name, at);
            let was = self.cat(&before_dir, "blob", &was_note)?;
            match now_notes.get(&counterpart).cloned() {
                Some(now_note) => {
                    annotated.insert(counterpart);
                    let now = self.cat(&after_dir, "blob", &now_note)?;
                    if was != now {
                        self.find("note", at, Some(shown(&was)), Some(shown(&now)));
                    }
                }
                None => self.find("note", at, Some(shown(&was)), None),
            }
        }
        for (object, now_note) in now_notes.into_iter().filter(|(object, _)| !annotated.contains(object)) {
            let now = self.cat(&after_dir, "blob", &now_note)?;
            let at = match self.returned.get(&object).map(|was| self.paired[was].1.clone()) {
                Some(at) => format!("{} on {}", name, at),
                None => format!("{} on {}", name, object),
            };
            self.find("note", at, None, Some(shown(&now)));
        }
        Ok(())
    }
}

/// Builds a Forklift-native history in a fresh warehouse at `root`, which must not exist yet: a
/// root parcel holding a CRLF file, an empty file, a binary file and a nested one, and on Unix an
/// executable file and a symlink; a parcel with a multi-line description outside ASCII; a pallet
/// `side` consolidated back into `main`; a parcel that removes a file; a signed tag; a keyed value;
/// and a note. Forklift takes its dates from the clock, so the history differs from run to run in
/// its times and parcel ids, and in nothing else.
fn build_warehouse(steps: &mut Steps, forklift: &Path, root: &Path, environment: &Environment) -> Result<(), String> {
    std::fs::create_dir(root).map_err(|error| format!("cannot create {}: {}", root.display(), error))?;
    let mut forklift_in = |arguments: &[&str]| steps.run(forklift, arguments, root, environment);
    forklift_in(&["--json", "prepare"])?;
    std::fs::create_dir_all(root.join("d/e")).map_err(|error| format!("cannot create {}: {}", root.join("d/e").display(), error))?;
    for (file, bytes) in [
        ("a.txt", &b"alpha\n"[..]), ("crlf.txt", b"one\r\ntwo\r\n"), ("empty", b""), ("bin.dat", b"\x00\x01\xfe\xff\n"),
        ("d/e/deep.txt", b"deep\n"), ("gone.txt", b"removed later\n"), ("run.sh", b"#!/bin/sh\n"),
    ] {
        write(&root.join(file), bytes)?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.join("run.sh"), std::fs::Permissions::from_mode(0o755)).map_err(|error| format!("cannot make run.sh executable: {}", error))?;
        std::os::unix::fs::symlink("a.txt", root.join("link")).map_err(|error| format!("cannot make the symlink: {}", error))?;
    }
    forklift_in(&["--json", "load", "."])?;
    forklift_in(&["--json", "stack", "-m", "root: every file shape"])?;
    write(&root.join("a.txt"), b"alpha\nbeta\n")?;
    forklift_in(&["--json", "load", "."])?;
    forklift_in(&["--json", "stack", "-m", "second: \u{fc}n\u{ef}c\u{f6}d\u{e9}\n\nA body line, below a blank one."])?;
    forklift_in(&["--json", "pallet", "create", "side"])?;
    write(&root.join("side.txt"), b"side\n")?;
    forklift_in(&["--json", "load", "."])?;
    forklift_in(&["--json", "stack", "-m", "side work"])?;
    forklift_in(&["--json", "shift", "main"])?;
    write(&root.join("main.txt"), b"main\n")?;
    forklift_in(&["--json", "load", "."])?;
    forklift_in(&["--json", "stack", "-m", "main work"])?;
    forklift_in(&["--json", "consolidate", "side"])?;
    forklift_in(&["--json", "tag", "create", "v1", "main", "-m", "release one"])?;
    forklift_in(&["--json", "manifest", "set", "main", "review.state", "done"])?;
    forklift_in(&["--json", "remove", "gone.txt"])?;
    forklift_in(&["--json", "stack", "-m", "drop a file"])?;
    forklift_in(&["--json", "manifest", "note", "-m", "a note", "main"])?;
    Ok(())
}

/// One record of a `history --json` listing: its description, its parents, its actions as name,
/// operator, time and trust, its manifest entries but keyed values (each as its JSON without the
/// subject, sorted), and the git tree its warehouse's export wrote for it.
#[derive(Clone, Debug, Default)]
struct Record {
    description: String,
    parents: Vec<String>,
    actions: Vec<[String; 4]>,
    manifest: Vec<String>,
    tree: Option<String>,
}

/// What was read of one warehouse of a Forklift round trip and of its git export.
#[derive(Clone, Debug, Default)]
struct Side {
    export: PathBuf,
    /// Each pallet by name, with its head; none for a pallet with no parcel yet.
    pallets: BTreeMap<String, Option<String>>,
    /// Every record the pallets' listings named, by parcel.
    records: BTreeMap<String, Record>,
    /// Each tag by name: its subject, message, tagger and time.
    tags: BTreeMap<String, [Option<String>; 4]>,
    /// Each keyed value by parcel and key: its value and who set it.
    keyed: BTreeMap<(String, String), (Option<String>, Option<String>)>,
}

/// The value at `key` of a JSON object as text: a string as it is, nothing as none, anything else
/// as its JSON.
fn text_at(value: &serde_json::Value, key: &str) -> Option<String> {
    match &value[key] {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

/// Reads one warehouse of a round trip, and the git export of it at `export`: its pallets, the
/// records their listings name, each record's manifest entries and exported tree, its tags and
/// its keyed values. An export commit is matched to its parcel by the `Forklift-Parcel` trailer
/// the export wrote; a record no commit names has no tree, and comparing it fails the trip.
fn read_side(steps: &mut Steps, forklift: &Path, git: &Path, environment: &Environment, warehouse: &Path, export: &Path) -> Result<Side, String> {
    let mut data = |arguments: &[&str]| -> Result<serde_json::Value, String> {
        let mut full = vec!["--json"];
        full.extend_from_slice(arguments);
        let stdout = steps.run(forklift, &full, warehouse, environment)?;
        let document: serde_json::Value = serde_json::from_slice(&stdout)
            .map_err(|error| format!("`{}` printed no JSON document: {}", arguments.join(" "), error))?;
        Ok(document["data"].clone())
    };
    let mut side = Side { export: export.to_path_buf(), ..Side::default() };
    let pallets = data(&["pallet", "list"])?;
    for pallet in pallets["pallets"].as_array().ok_or("the pallet list names no pallets")? {
        side.pallets.insert(text_at(pallet, "name").ok_or("a pallet in the list has no name")?, text_at(pallet, "head"));
    }
    for (name, head) in side.pallets.clone() {
        if head.is_none() {
            continue;
        }
        let listing = data(&["history", "--", &name])?;
        for entry in listing["entries"].as_array().ok_or_else(|| format!("the history of {} lists no entries", name))? {
            let parcel = text_at(entry, "parcel").ok_or_else(|| format!("an entry in the history of {} names no parcel", name))?;
            let parents = entry["parents"].as_array().map(|parents| parents.iter().filter_map(|parent| parent.as_str().map(str::to_string)).collect()).unwrap_or_default();
            let actions = entry["actions"].as_array().map(|actions| actions.iter().map(|action| {
                [text_at(action, "action").unwrap_or_default().trim().to_string(), text_at(action, "operator").unwrap_or_default(),
                    text_at(action, "timestamp").unwrap_or_default(), text_at(action, "trust").unwrap_or_default()]
            }).collect()).unwrap_or_default();
            side.records.insert(parcel, Record { description: text_at(entry, "description").unwrap_or_default(), parents, actions, ..Record::default() });
        }
    }
    for parcel in side.records.keys().cloned().collect::<Vec<_>>() {
        let shown = data(&["manifest", "show", &parcel])?;
        let mut entries: Vec<String> = shown["entries"].as_array().map(|entries| entries.iter().map(|entry| {
            let mut entry = entry.clone();
            if let Some(object) = entry.as_object_mut() {
                object.remove("subject");
            }
            entry.to_string()
        }).collect()).unwrap_or_default();
        entries.sort();
        side.records.get_mut(&parcel).expect("a record just listed").manifest = entries;
    }
    let tags = data(&["tag", "list"])?;
    for tag in tags["tags"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let name = text_at(tag, "name").ok_or("a tag in the list has no name")?;
        side.tags.insert(name, [text_at(tag, "subject"), text_at(tag, "message"), text_at(tag, "tagger"), text_at(tag, "tagged_at")]);
    }
    let keys = data(&["manifest", "keys"])?;
    for entry in keys["entries"].as_array().map(Vec::as_slice).unwrap_or_default() {
        let (Some(subject), Some(key)) = (text_at(entry, "subject"), text_at(entry, "key")) else { return Err("a keyed value names no parcel or key".to_string()) };
        side.keyed.insert((subject, key), (text_at(entry, "value"), text_at(entry, "operator")));
    }
    let log = steps.run(git, &["log", "--all", "--format=%x00%H %T%n%B"], export, environment)?;
    for chunk in log.split(|byte| *byte == 0).filter(|chunk| !chunk.is_empty()) {
        let text = String::from_utf8_lossy(chunk);
        let mut lines = text.lines();
        let tree = lines.next().unwrap_or_default().split_whitespace().nth(1).map(str::to_string);
        for parcel in lines.filter_map(|line| line.strip_prefix("Forklift-Parcel: ")).filter_map(|rest| rest.split_whitespace().next()) {
            if let Some(record) = side.records.get_mut(parcel) {
                record.tree = tree.clone();
            }
        }
    }
    Ok(side)
}

/// Compares two warehouses: `before`, the history that went in, and `after`, its git export
/// imported into a fresh warehouse. Each tree is read through a git export of its warehouse, the
/// same exporter for both, so a tree difference is the import's loss or the first export's, never
/// the reading of it.
struct Records<'a, 'b> {
    steps: &'a mut Steps<'b>,
    git: &'a Path,
    environment: &'a Environment,
    before: Side,
    after: Side,
    findings: Vec<Finding>,
    /// Each record that went in, by parcel, with the record it was compared to and its position.
    paired: BTreeMap<String, (String, String)>,
    /// Each record that came back, by parcel, with the record that went in it was compared to.
    returned: BTreeMap<String, String>,
}

impl Records<'_, '_> {
    fn find(&mut self, field: impl Into<String>, at: impl Into<String>, before: Option<String>, after: Option<String>) {
        self.findings.push(Finding { direction: Direction::ForkliftGitForklift, field: field.into(), at: at.into(), before, after });
    }

    /// Compares one paired record: description, parent count, actions in order and each action's
    /// operator, time and trust at its first occurrence, the exported tree entry by entry, and
    /// the manifest entries. A record either export wrote no commit for is an error.
    fn record(&mut self, at: &str, before: &str, after: &str) -> Result<(Vec<String>, Vec<String>), String> {
        let missing = |parcel: &str| format!("the history lists {} as a parent but not as a record", parcel);
        let was = self.before.records.get(before).cloned().ok_or_else(|| missing(before))?;
        let now = self.after.records.get(after).cloned().ok_or_else(|| missing(after))?;
        if was.description != now.description {
            self.find("message", at, Some(shown(was.description.as_bytes())), Some(shown(now.description.as_bytes())));
        }
        if was.parents.len() != now.parents.len() {
            self.find("parent count", at, Some(was.parents.len().to_string()), Some(now.parents.len().to_string()));
        }
        let names = |record: &Record| record.actions.iter().map(|action| action[0].clone()).collect::<Vec<_>>().join(", ");
        if names(&was) != names(&now) {
            self.find("actions", at, Some(names(&was)), Some(names(&now)));
        }
        let mut compared = BTreeSet::new();
        for was_action in was.actions.iter().filter(|action| compared.insert(action[0].clone())) {
            let Some(now_action) = now.actions.iter().find(|action| action[0] == was_action[0]) else { continue };
            for (index, part) in [(1, "operator"), (2, "time"), (3, "trust")] {
                if was_action[index] != now_action[index] {
                    self.find(format!("{} {}", was_action[0], part), at, Some(was_action[index].clone()), Some(now_action[index].clone()));
                }
            }
        }
        // A tree is read only through an export, so a record its export wrote no commit for could
        // not be read, which is the reading failing, never a finding.
        let unread = |parcel: &str| format!("the export of the record at {} ({}) wrote no commit naming it", at, parcel);
        let was_tree = was.tree.clone().ok_or_else(|| unread(before))?;
        let now_tree = now.tree.clone().ok_or_else(|| unread(after))?;
        if was_tree != now_tree {
            let (before_export, after_export) = (self.before.export.clone(), self.after.export.clone());
            let was_entries = tree_entries(self.steps, self.git, &before_export, &was_tree, self.environment)?;
            let now_entries = tree_entries(self.steps, self.git, &after_export, &now_tree, self.environment)?;
            for path in was_entries.keys().chain(now_entries.keys()).collect::<BTreeSet<_>>() {
                let (was_entry, now_entry) = (was_entries.get(path), now_entries.get(path));
                if was_entry != now_entry {
                    self.find("tree entry", format!("{} {}", at, path), was_entry.cloned(), now_entry.cloned());
                }
            }
        }
        for entry in was.manifest.iter().filter(|entry| !now.manifest.contains(entry)) {
            self.find("manifest entry", at, Some(entry.clone()), None);
        }
        for entry in now.manifest.iter().filter(|entry| !was.manifest.contains(entry)) {
            self.find("manifest entry", at, None, Some(entry.clone()));
        }
        Ok((was.parents, now.parents))
    }

    /// How many records `parent` reaches that none of `kept` does, on one side, as a finding shows
    /// it.
    fn only_below(&self, after: bool, parent: &str, kept: &[String]) -> String {
        let records = if after { &self.after.records } else { &self.before.records };
        let reach = |starts: &[String]| {
            let mut seen = BTreeSet::new();
            let mut queue: VecDeque<String> = starts.iter().cloned().collect();
            while let Some(parcel) = queue.pop_front() {
                if seen.insert(parcel.clone()) {
                    queue.extend(records.get(&parcel).map(|record| record.parents.clone()).unwrap_or_default());
                }
            }
            seen
        };
        let kept = reach(kept);
        let count = reach(&[parent.to_string()]).difference(&kept).count();
        format!("present, with {} record{} only it reaches", count, if count == 1 { "" } else { "s" })
    }

    /// Walks from one pallet's heads, breadth first, as the git walk does: head with head, parent i
    /// with parent i, each record that went in compared once, at its first position. A record
    /// reached again paired with a different one is a `record identity` finding, and a parent one
    /// side lacks a `record` finding saying how many records only it reaches.
    fn lockstep(&mut self, start: &str, was: String, now: String) -> Result<(), String> {
        let mut queue = VecDeque::from([(Vec::new(), was, now)]);
        while let Some((path, was, now)) = queue.pop_front() {
            let at = position(start, &path);
            if let Some((paired, first)) = self.paired.get(&was) {
                if *paired != now {
                    let first = first.clone();
                    self.find("record identity", at, Some(format!("the record at {}", first)), Some("a different record".to_string()));
                }
                continue;
            }
            if let Some(other) = self.returned.get(&now) {
                let first = self.paired[other].1.clone();
                self.find("record identity", at.as_str(), Some("a distinct record".to_string()), Some(format!("the record at {}", first)));
            }
            self.paired.insert(was.clone(), (now.clone(), at.clone()));
            self.returned.entry(now.clone()).or_insert_with(|| was.clone());
            let (was_parents, now_parents) = self.record(&at, &was, &now)?;
            for index in 0..was_parents.len().max(now_parents.len()) {
                let mut next = path.clone();
                next.push(index + 1);
                match (was_parents.get(index), now_parents.get(index)) {
                    (Some(was), Some(now)) => queue.push_back((next, was.clone(), now.clone())),
                    (Some(lost), None) => {
                        let only = self.only_below(false, lost, &was_parents[..now_parents.len()]);
                        self.find("record", position(start, &next), Some(only), None);
                    }
                    (None, Some(added)) => {
                        let only = self.only_below(true, added, &now_parents[..was_parents.len()]);
                        self.find("record", position(start, &next), None, Some(only));
                    }
                    (None, None) => {}
                }
            }
        }
        Ok(())
    }

    /// Where a record that went in was compared, or its parcel id where the walk never reached it.
    fn at(&self, before: &str) -> String {
        self.paired.get(before).map_or_else(|| before.to_string(), |(_, at)| at.clone())
    }

    /// Where a record that came back was compared, or its parcel id where the walk never reached it.
    fn at_returned(&self, after: &str) -> String {
        self.returned.get(after).map_or_else(|| after.to_string(), |before| self.at(before))
    }

    /// Compares every pallet either warehouse holds, walking the histories in lockstep from each
    /// pallet both hold, `main` first so a record is named from main where main reaches it; then
    /// the tags by name, and the keyed values through the records the walk paired.
    fn walk(mut self) -> Result<Vec<Finding>, String> {
        let mut names: Vec<String> = self.before.pallets.keys().chain(self.after.pallets.keys()).cloned().collect::<BTreeSet<_>>().into_iter().collect();
        names.sort_by_key(|name| name != "main");
        for name in names {
            let present = |head: &Option<String>| Some(if head.is_some() { "a pallet" } else { "a pallet with no parcel" }.to_string());
            match (self.before.pallets.get(&name).cloned(), self.after.pallets.get(&name).cloned()) {
                (Some(Some(was)), Some(Some(now))) => self.lockstep(&name, was, now)?,
                (Some(was), Some(now)) if was.is_some() != now.is_some() => self.find("pallet", name, present(&was), present(&now)),
                (was, now) if was.is_some() != now.is_some() => self.find("pallet", name, was.as_ref().and_then(present), now.as_ref().and_then(present)),
                _ => {}
            }
        }
        self.tags();
        self.keyed_values();
        Ok(self.findings)
    }

    /// Compares the tags of both warehouses by name: what each points at, through the walk's
    /// pairing, and its message, tagger and time.
    fn tags(&mut self) {
        let names: BTreeSet<String> = self.before.tags.keys().chain(self.after.tags.keys()).cloned().collect();
        for name in names {
            let at = format!("tag {}", name);
            let (Some(was), Some(now)) = (self.before.tags.get(&name).cloned(), self.after.tags.get(&name).cloned()) else {
                let held = |tags: &BTreeMap<String, [Option<String>; 4]>| tags.get(&name).map(|_| "a tag".to_string());
                let (was, now) = (held(&self.before.tags), held(&self.after.tags));
                self.find("tag", at, was, now);
                continue;
            };
            let (was_subject, now_subject) = (was[0].clone().unwrap_or_default(), now[0].clone().unwrap_or_default());
            if self.paired.get(&was_subject).map(|(paired, _)| paired) != Some(&now_subject) {
                let (was_at, now_at) = (self.at(&was_subject), self.at_returned(&now_subject));
                self.find("tag subject", at.as_str(), Some(was_at), Some(now_at));
            }
            for (index, field) in [(1, "tag message"), (2, "tag tagger"), (3, "tag time")] {
                if was[index] != now[index] {
                    let show = |value: &Option<String>| value.as_ref().map(|value| shown(value.as_bytes()));
                    self.find(field, at.as_str(), show(&was[index]), show(&now[index]));
                }
            }
        }
    }

    /// Compares the keyed values of both warehouses through the walk's pairing: each value with
    /// the one under its key on its record's counterpart, and who set it.
    fn keyed_values(&mut self) {
        let mut matched = BTreeSet::new();
        for ((subject, key), (was_value, was_operator)) in self.before.keyed.clone() {
            let at = format!("{} on {}", key, self.at(&subject));
            let counterpart = self.paired.get(&subject).map(|(now, _)| (now.clone(), key.clone()));
            match counterpart.and_then(|counterpart| self.after.keyed.get(&counterpart).cloned().map(|now| (counterpart, now))) {
                Some((counterpart, (now_value, now_operator))) => {
                    matched.insert(counterpart);
                    if was_value != now_value {
                        self.find("keyed value", at.as_str(), was_value, now_value);
                    }
                    if was_operator != now_operator {
                        self.find("keyed value operator", at.as_str(), was_operator, now_operator);
                    }
                }
                None => self.find("keyed value", at, was_value, None),
            }
        }
        for ((subject, key), (now_value, _)) in self.after.keyed.clone().into_iter().filter(|(counterpart, _)| !matched.contains(counterpart)) {
            let at = format!("{} on {}", key, self.at_returned(&subject));
            self.find("keyed value", at, None, now_value);
        }
    }
}

/// The Forklift to git to Forklift round trip in `scratch`: a native history built with
/// `forklift`, exported to git, imported into a fresh warehouse, exported again, and compared
/// record by record. Both warehouses' trees are read through their own exports.
pub fn forklift_round_trip(forklift: &Path, git: &Path, scratch: &Scratch, environment: &Environment, limit: Duration) -> Trip {
    let mut steps = Steps { scratch, limit, outcomes: Vec::new() };
    let (native, exported) = (scratch.root().join("native"), scratch.root().join("exported"));
    let (reimported, reexported) = (scratch.root().join("reimported"), scratch.root().join("reexported"));
    let findings = (|| -> Result<Vec<Finding>, String> {
        build_warehouse(&mut steps, forklift, &native, environment).map_err(|error| format!("the fixture could not be built: {}", error))?;
        steps.run(forklift, &["--json", "git", "export", "--", &exported.display().to_string()], &native, environment)?;
        std::fs::create_dir(&reimported).map_err(|error| format!("cannot create {}: {}", reimported.display(), error))?;
        steps.run(forklift, &["--json", "prepare"], &reimported, environment)?;
        steps.run(forklift, &["--json", "git", "import", "--", &exported.display().to_string()], &reimported, environment)?;
        steps.run(forklift, &["--json", "git", "export", "--", &reexported.display().to_string()], &reimported, environment)?;
        let before = read_side(&mut steps, forklift, git, environment, &native, &exported)?;
        let after = read_side(&mut steps, forklift, git, environment, &reimported, &reexported)?;
        Records { steps: &mut steps, git, environment, before, after, findings: Vec::new(), paired: BTreeMap::new(), returned: BTreeMap::new() }.walk()
    })();
    Trip { direction: Direction::ForkliftGitForklift, steps: steps.outcomes, findings }
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
        Comparison::new(&mut steps, git, environment, &fixture, &exported, Direction::GitForkliftGit).walk()
    })();
    Trip { direction: Direction::GitForkliftGit, steps: steps.outcomes, findings }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scrub::path_git;

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

    fn area(label: &str) -> (Scratch, Environment) {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-fidelity-{}-{}-{}", label, std::process::id(), nanos));
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, crate::scrub::search_path(Some(&path_git()), std::env::var_os("PATH")), &BTreeMap::new()).unwrap();
        (scratch, environment)
    }

    fn compare(steps: &mut Steps, environment: &Environment, before: &Path, after: &Path) -> Vec<Finding> {
        let git = path_git();
        Comparison::new(steps, &git, environment, before, after, Direction::GitForkliftGit).walk().unwrap()
    }

    /// A query's exit 1 is "no", but a command that never ran is an error, whatever the command
    /// before it ended with.
    ///
    /// Revert: judge by the last outcome alone → the missing program after an exit 1 reads as "no".
    #[test]
    fn a_query_that_never_ran_is_an_error_not_a_no() {
        let (scratch, environment) = area("ask");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = path_git();
        steps.run(&git, &["init", "-q", "repository"], scratch.root(), &environment).unwrap();
        let repository = scratch.root().join("repository");
        assert_eq!(steps.ask(&git, &["rev-parse", "--verify", "-q", "HEAD^{commit}"], &repository, &environment), Ok(None));
        let missing = scratch.root().join("no-such-program");
        assert!(steps.ask(&missing, &["rev-parse"], &repository, &environment).is_err());
    }

    /// Every value of a header is compared in order, so one header split into two is a finding,
    /// and the names both objects have, in another order, are one too, as is an identity header
    /// that came back twice.
    ///
    /// Revert: join a name's values into one before comparing → the split gpgsig compares equal.
    /// Revert: order by each name's first appearance → the interleaved mergetags compare equal.
    /// Revert: leave `skipped` names out of the order → gpgsig moved before committer is no finding.
    /// Revert: drop the identity count check → the second author is no finding.
    #[test]
    fn a_split_header_and_a_reordered_one_are_findings() {
        let (scratch, environment) = area("headers");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = path_git();
        let mut comparison = Comparison::new(&mut steps, &git, &environment, scratch.root(), scratch.root(), Direction::GitForkliftGit);
        let header = |name: &str, value: &[u8]| (name.to_string(), value.to_vec());
        comparison.headers("header ", "main", &[header("encoding", b"x"), header("gpgsig", b"A\nB")], &[header("encoding", b"x"), header("gpgsig", b"A"), header("gpgsig", b"B")], &[]);
        comparison.headers("header ", "main~1", &[header("encoding", b"x"), header("mergetag", b"m")], &[header("mergetag", b"m"), header("encoding", b"x")], &[]);
        comparison.headers("header ", "main~2", &[header("mergetag", b"a"), header("encoding", b"x"), header("mergetag", b"b")],
            &[header("mergetag", b"a"), header("mergetag", b"b"), header("encoding", b"x")], &[]);
        comparison.headers("header ", "main~3", &[header("committer", b"c"), header("gpgsig", b"g")], &[header("gpgsig", b"g"), header("committer", b"c")], &["committer"]);
        let author = header("author", b"A <a@example.com> 1 +0000");
        comparison.identities("", "main~4", std::slice::from_ref(&author), &[author.clone(), header("author", b"B <b@example.com> 1 +0000")], &["author"]);
        let found: Vec<(String, String, Option<String>, Option<String>)> = comparison.findings.into_iter().map(|finding| (finding.field, finding.at, finding.before, finding.after)).collect();
        assert_eq!(found, [
            ("header gpgsig".to_string(), "main".to_string(), Some("[A\\nB]".to_string()), Some("[A] [B]".to_string())),
            ("header order".to_string(), "main~1".to_string(), Some("encoding mergetag".to_string()), Some("mergetag encoding".to_string())),
            ("header order".to_string(), "main~2".to_string(), Some("mergetag encoding mergetag".to_string()), Some("mergetag mergetag encoding".to_string())),
            ("header order".to_string(), "main~3".to_string(), Some("committer gpgsig".to_string()), Some("gpgsig committer".to_string())),
            ("author count".to_string(), "main~4".to_string(), Some("1".to_string()), Some("2".to_string())),
        ]);
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
        let git = path_git();
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
    /// commit with an executable file, under the branch `moved`, a lightweight tag, a notes ref of
    /// its own and a note; main a two-parent merge carrying a gpgsig header; a tag of a tree.
    /// After: the file not executable, the names and zones changed, a trailer on the root's
    /// message, main with one parent and no header, `moved` at main instead, the tag annotated,
    /// the tree's tag on the root, no second notes ref, and the note's text changed.
    ///
    /// Revert: compare only the tree id, not its entries → no `tree entry` finding.
    #[test]
    fn the_walk_reports_each_field_a_changed_copy_differs_in() {
        let (scratch, environment) = area("walk");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = path_git();
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
        for (name, id) in [("refs/heads/main", &merge), ("refs/heads/moved", &first), ("refs/tags/v", &first), ("refs/notes/extra", &first), ("refs/tags/t", &tree)] {
            in_before(&["update-ref", name, id]);
        }
        in_before(&["notes", "add", "-m", "went in", &first]);
        let (tree, first) = make(&mut steps, &after, "author@example.com +0000", "100644", "same\n\nForklift-Parcel: x");
        let mut in_after = |arguments: &[&str]| printed(&steps.run(&git, arguments, &after, &fixed).unwrap());
        let merge = in_after(&["commit-tree", &tree, "-p", &first, "-m", "merge"]);
        for (name, id) in [("refs/heads/main", &merge), ("refs/heads/moved", &merge), ("refs/tags/t", &first)] {
            in_after(&["update-ref", name, id]);
        }
        in_after(&["tag", "-a", "-m", "v", "v", &first]);
        in_after(&["notes", "add", "-m", "came back", &first]);

        let findings = compare(&mut steps, &environment, &before, &after);
        let mut found: Vec<(String, String)> = findings.iter().map(|finding| (finding.field.clone(), finding.at.clone())).collect();
        found.sort();
        let mut expected: Vec<(String, String)> = [
            ("author name", "main"), ("author zone", "main"), ("commit", "main^2"), ("header gpgsig", "main"), ("parent count", "main"), ("tree entry", "main tool"),
            ("author name", "main~1"), ("author zone", "main~1"), ("message", "main~1"), ("tree entry", "main~1 tool"),
            ("ref", "refs/notes/extra"), ("ref kind", "refs/tags/v"),
            ("commit identity", "moved"), ("note", "refs/notes/commits on main~1"), ("ref target", "refs/tags/t"),
        ].map(|(field, at)| (field.to_string(), at.to_string())).into_iter().collect();
        expected.sort();
        assert_eq!(found, expected);
        let value = |field: &str| findings.iter().find(|finding| finding.field == field).map(|finding| (finding.before.clone(), finding.after.clone())).unwrap();
        assert_eq!(value("author zone"), (Some("+0130".to_string()), Some("+0000".to_string())));
        assert_eq!(value("parent count"), (Some("2".to_string()), Some("1".to_string())));
        assert_eq!(value("header gpgsig"), (Some("A\\nB".to_string()), None));
        assert_eq!(value("ref"), (Some("note".to_string()), None));
        assert_eq!(value("ref kind"), (Some("lightweight tag".to_string()), Some("annotated tag".to_string())));
        assert_eq!(value("commit"), (Some("present, with 1 commit only it reaches".to_string()), None));
        assert_eq!(value("commit identity"), (Some("the commit at main~1".to_string()), Some("a different commit".to_string())));
        assert_eq!(value("note"), (Some("went in\\n".to_string()), Some("came back\\n".to_string())));
        assert_eq!(value("ref target"), (Some("no commit".to_string()), Some("a commit".to_string())));
        let entry = value("tree entry");
        assert!(entry.0.unwrap().starts_with("100755 blob ") && entry.1.unwrap().starts_with("100644 blob "));
    }

    /// The round trip runs against a real forklift and finishes, with every command it ran kept.
    ///
    /// A native history and its copy under other parcel ids: the copy as it is has no finding, and
    /// a copy changed in every field the record walk reads has one finding per change, at the
    /// position the walk reached it by.
    ///
    /// Revert: drop any one comparison in `Records` → its expected finding is missing.
    #[test]
    fn the_record_walk_reports_each_field_a_changed_copy_differs_in() {
        let (scratch, environment) = area("records");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = path_git();
        let export = scratch.root().join("export");
        steps.run(&git, &["init", "-q", "export"], scratch.root(), &environment).unwrap();
        let mut tree = |bytes: &[u8]| {
            write(&export.join("a"), bytes).unwrap();
            steps.run(&git, &["add", "a"], &export, &environment).unwrap();
            printed(&steps.run(&git, &["write-tree"], &export, &environment).unwrap())
        };
        let (kept, changed) = (tree(b"kept\n"), tree(b"changed\n"));
        let record = |description: &str, parents: &[&str]| Record {
            description: description.to_string(), parents: parents.iter().map(|parent| parent.to_string()).collect(),
            actions: [["author", "op", "t1", "recorded"], ["stack", "op", "t1", "recorded"]].map(|action| action.map(str::to_string)).to_vec(),
            manifest: Vec::new(), tree: Some(kept.clone()),
        };
        let some = |text: &str| Some(text.to_string());
        let side = |prefix: &str| {
            let id = |name: &str| format!("{}{}", prefix, name);
            let mut side = Side { export: export.clone(), ..Side::default() };
            side.records.insert(id("3"), Record { manifest: vec![r#"{"kind":"note"}"#.to_string()], ..record("merge", &[&id("2"), &id("s")]) });
            side.records.insert(id("2"), record("second", &[&id("1")]));
            side.records.insert(id("1"), record("root", &[]));
            side.records.insert(id("s"), record("side", &[&id("1")]));
            side.pallets = BTreeMap::from([("main".to_string(), Some(id("3"))), ("side".to_string(), Some(id("s")))]);
            side.tags = BTreeMap::from([("v1".to_string(), [Some(id("3")), some("one"), some("op"), some("t1")]), ("v2".to_string(), [Some(id("3")), some("two"), some("op"), some("t1")])]);
            side.keyed = BTreeMap::from([((id("3"), "k".to_string()), (some("v"), some("op"))), ((id("2"), "j".to_string()), (some("v"), some("op")))]);
            side
        };
        let walk = |steps: &mut Steps, before: Side, after: Side| -> Vec<(String, String)> {
            let records = Records { steps, git: &git, environment: &environment, before, after, findings: Vec::new(), paired: BTreeMap::new(), returned: BTreeMap::new() };
            records.walk().unwrap().into_iter().map(|finding| (finding.field, finding.at)).collect()
        };
        assert_eq!(walk(&mut steps, side("w"), side("n")), Vec::<(String, String)>::new(), "a copy as it is has no finding");

        let mut before = side("w");
        before.pallets.insert("gone".to_string(), Some("w1".to_string()));
        before.pallets.insert("other".to_string(), Some("w2".to_string()));
        before.tags.insert("v3".to_string(), [some("w1"), None, None, None]);
        before.keyed.insert(("w1".to_string(), "y".to_string()), (some("v"), some("op")));
        let mut after = side("n");
        after.pallets.insert("other".to_string(), Some("n1".to_string()));
        let merge = after.records.get_mut("n3").unwrap();
        merge.description = "changed".to_string();
        merge.parents.truncate(1);
        merge.manifest.clear();
        let second = after.records.get_mut("n2").unwrap();
        second.actions[0][1] = "someone else".to_string();
        second.actions[1][2] = "t2".to_string();
        second.actions.push(["pick", "op", "t1", "recorded"].map(str::to_string));
        let root = after.records.get_mut("n1").unwrap();
        root.actions[0][3] = "claimed".to_string();
        root.tree = Some(changed.clone());
        after.tags.get_mut("v1").unwrap()[1] = some("one, edited");
        after.tags.get_mut("v2").unwrap()[0] = some("n2");
        after.keyed.insert(("n3".to_string(), "k".to_string()), (some("w"), some("op")));
        after.keyed.insert(("n2".to_string(), "j".to_string()), (some("v"), some("another")));
        after.keyed.insert(("n1".to_string(), "x".to_string()), (some("new"), some("op")));
        let found: BTreeSet<(String, String)> = walk(&mut steps, before, after).into_iter().collect();
        let expected: BTreeSet<(String, String)> = [
            ("message", "main"), ("parent count", "main"), ("manifest entry", "main"), ("record", "main^2"),
            ("author operator", "main~1"), ("stack time", "main~1"), ("actions", "main~1"),
            ("author trust", "main~2"), ("tree entry", "main~2 a"),
            ("pallet", "gone"), ("record identity", "other"),
            ("tag message", "tag v1"), ("tag subject", "tag v2"), ("tag", "tag v3"),
            ("keyed value", "k on main"), ("keyed value operator", "j on main~1"), ("keyed value", "x on main~2"), ("keyed value", "y on main~2"),
        ].into_iter().map(|(field, at)| (field.to_string(), at.to_string())).collect();
        assert_eq!(found, expected);
    }

    /// A lost parent is counted by the records only it reaches, and a record the export never
    /// wrote is the reading failing, never a finding.
    #[test]
    fn a_lost_parent_counts_what_only_it_reaches_and_an_unexported_record_is_an_error() {
        let (scratch, environment) = area("lost");
        let mut steps = Steps { scratch: &scratch, limit: Duration::from_secs(60), outcomes: Vec::new() };
        let git = path_git();
        let record = |parents: &[&str]| Record { parents: parents.iter().map(|parent| parent.to_string()).collect(), tree: Some("t".to_string()), ..Record::default() };
        let before = Side {
            records: BTreeMap::from([("w3".to_string(), record(&["w2", "wb"])), ("w2".to_string(), record(&["w1"])), ("w1".to_string(), record(&[])),
                ("wb".to_string(), record(&["wa"])), ("wa".to_string(), record(&["w1"]))]),
            pallets: BTreeMap::from([("main".to_string(), Some("w3".to_string()))]),
            ..Side::default()
        };
        let mut after = Side {
            records: BTreeMap::from([("n3".to_string(), record(&["n2"])), ("n2".to_string(), record(&["n1"])), ("n1".to_string(), record(&[]))]),
            pallets: BTreeMap::from([("main".to_string(), Some("n3".to_string()))]),
            ..Side::default()
        };
        let records = Records { steps: &mut steps, git: &git, environment: &environment, before: before.clone(), after: after.clone(), findings: Vec::new(), paired: BTreeMap::new(), returned: BTreeMap::new() };
        let findings = records.walk().unwrap();
        let lost: Vec<_> = findings.iter().filter(|finding| finding.field == "record").collect();
        assert_eq!(lost.len(), 1, "{:?}", findings);
        assert_eq!((lost[0].at.as_str(), lost[0].before.as_deref(), lost[0].after.as_deref()), ("main^2", Some("present, with 2 records only it reaches"), None));

        after.records.get_mut("n2").unwrap().tree = None;
        let records = Records { steps: &mut steps, git: &git, environment: &environment, before, after, findings: Vec::new(), paired: BTreeMap::new(), returned: BTreeMap::new() };
        let error = records.walk().unwrap_err();
        assert!(error.contains("main~1") && error.contains("wrote no commit"), "{}", error);
    }

    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn the_round_trip_finishes_against_forklift() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let (scratch, environment) = area("trip");
        let trip = git_round_trip(&forklift, &path_git(), &scratch, &environment, Duration::from_secs(120));
        let findings = trip.findings.unwrap();
        for finding in &findings {
            println!("{:?}", finding);
        }
        assert!(trip.steps.iter().any(|step| step.arguments.iter().any(|argument| argument == "import")));
    }

    /// Needs a forklift binary: `PROOF_LOAD_FORKLIFT=<path> cargo test -p proof-load -- --ignored`.
    #[test]
    #[ignore = "drives a forklift binary named by PROOF_LOAD_FORKLIFT"]
    fn the_native_round_trip_finishes_against_forklift() {
        let forklift = PathBuf::from(std::env::var_os("PROOF_LOAD_FORKLIFT").expect("PROOF_LOAD_FORKLIFT names the forklift binary under test"));
        let (scratch, environment) = area("native-trip");
        let trip = forklift_round_trip(&forklift, &path_git(), &scratch, &environment, Duration::from_secs(120));
        let findings = trip.findings.unwrap();
        for finding in &findings {
            println!("{:?}", finding);
        }
        assert!(trip.steps.iter().any(|step| step.arguments.iter().any(|argument| argument == "import")));
    }
}
