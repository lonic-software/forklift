//! End-to-end tests of `forklift mirror` and lift's mirror trigger (DESIGN.html §9.8): a real
//! `forklift-server` as the Forklift remote, a bare git repository as the mirror, and the real CLI.
//!
//! Every git call the tests make themselves, and every forklift run, ignores the machine's system
//! and global git configuration, so a developer's hooks path or credential helper cannot steer a
//! run.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

use forklift_core::globals::StorageRootScope;
use forklift_core::util::gc_utils;

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

mod common;
use common::forklift_command;

/// The git config file every launch here reads as its global one: none.
const NO_CONFIG: &str = if cfg!(windows) { "NUL" } else { "/dev/null" };

fn server_binary() -> PathBuf {
    let path = Path::new(FORKLIFT)
        .parent()
        .expect("the forklift binary has a parent directory")
        .join(format!("forklift-server{}", std::env::consts::EXE_SUFFIX));

    assert!(path.is_file(), "forklift-server is not built at {}; run the tests via a workspace build.", path.display());

    path
}

/// A scratch area for one test: warehouses, the server root, the git mirror and the shared
/// operator configuration all live under it, and it is deleted when the test ends.
struct TestArea {
    root: PathBuf,
}

impl TestArea {
    fn new(name: &str) -> TestArea {
        let root = std::env::temp_dir().join(format!("forklift-mirror-{}-{}", name, std::process::id()));

        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();

        TestArea { root }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    /// The environment of every forklift run in the area.
    fn forklift_in(&self, dir: &Path) -> Command {
        let mut command = forklift_command();
        command
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.path("area-global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.path("shared-keys"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", NO_CONFIG);
        command
    }

    fn forklift(&self, relative_dir: &str, args: &[&str]) -> Output {
        self.forklift_in(&self.path(relative_dir)).args(args).output().unwrap()
    }

    fn forklift_ok(&self, relative_dir: &str, args: &[&str]) -> Output {
        let output = self.forklift(relative_dir, args);
        assert_success(&output);
        output
    }

    /// Run a command under `--json` that must succeed, returning its `data`.
    fn json(&self, relative_dir: &str, args: &[&str]) -> serde_json::Value {
        let mut all = vec!["--json"];
        all.extend_from_slice(args);
        let output = self.forklift_ok(relative_dir, &all);

        serde_json::from_str::<serde_json::Value>(&stdout(&output)).unwrap()["data"].clone()
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.path(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// Write a file, load it and stack a parcel in a warehouse of the area.
    fn stack(&self, dir: &str, file: &str, content: &str, description: &str) {
        self.write_file(&format!("{}/{}", dir, file), content);
        self.forklift_ok(dir, &["load", "."]);
        self.forklift_ok(dir, &["stack", description]);
    }

    /// Lift under `--json`, returning lift's `data`.
    fn lift(&self, dir: &str) -> serde_json::Value {
        self.json(dir, &["lift"])
    }
}

impl Drop for TestArea {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// A running forklift-server over a bare warehouse in the test area.
struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(area: &TestArea) -> Server {
        let root = area.path("server-root");
        let root = root.to_str().unwrap();

        let prepared = Command::new(server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["prepare", "--root", root]).output().unwrap();
        assert_success(&prepared);

        let mut child = Command::new(server_binary()).env("FORKLIFT_FSYNC", "0")
            .args(["serve", "--root", root, "--addr", "127.0.0.1:0", "--open"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        assert!(url.starts_with("http://"), "unexpected startup line: {}", line);

        Server { child, url }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// git in `dir`, with no system or global configuration.
fn git(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NO_CONFIG)
        .env("GIT_AUTHOR_NAME", "Git Hand")
        .env("GIT_AUTHOR_EMAIL", "hand@git")
        .env("GIT_COMMITTER_NAME", "Git Hand")
        .env("GIT_COMMITTER_EMAIL", "hand@git")
        .output()
        .unwrap()
}

fn git_ok(dir: &Path, args: &[&str]) -> String {
    let output = git(dir, args);
    assert_success(&output);
    stdout(&output).trim().to_string()
}

/// Write a raw commit object into the repository at `dir`, returning its hash.
fn write_commit(dir: &Path, tree: &str, parents: &[&str], message: &str) -> String {
    let mut object = format!("tree {}\n", tree);
    for parent in parents {
        object.push_str(&format!("parent {}\n", parent));
    }
    object.push_str("author Git Hand <hand@git> 1700000000 +0000\n");
    object.push_str("committer Git Hand <hand@git> 1700000000 +0000\n\n");
    object.push_str(message);

    let mut child = Command::new("git")
        .args(["hash-object", "-t", "commit", "-w", "--literally", "--stdin"])
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", NO_CONFIG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(object.as_bytes()).unwrap();
    let output = child.wait_with_output().unwrap();
    assert_success(&output);

    stdout(&output).trim().to_string()
}

/// The tip of a branch in a git repository, if it has one.
fn tip(repo: &Path, branch: &str) -> Option<String> {
    let output = git(repo, &["rev-parse", "--verify", "-q", &format!("refs/heads/{}", branch)]);
    output.status.success().then(|| stdout(&output).trim().to_string())
}

/// The last line of a commit's message, read from the raw object.
fn last_line(repo: &Path, commit: &str) -> String {
    let raw = git_ok(repo, &["cat-file", "commit", commit]);
    raw.lines().last().unwrap().to_string()
}

fn parcel_line(parcel: &str) -> String {
    format!("Forklift-Parcel: {} export/1", parcel)
}

/// A server, warehouse "a" holding one lifted parcel, and an empty bare git repository at
/// "git.git" that "a" mirrors to. The first lift runs before mirror.url is set, so git is empty.
struct Fixture {
    area: TestArea,
    server: Server,
    git: PathBuf,
    first: String,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let area = TestArea::new(name);
        let server = Server::start(&area);

        prepare_warehouse(&area, "a", &server.url);
        area.stack("a", "code.txt", "v1\n", "first");
        let first = area.lift("a")["head"].as_str().unwrap().to_string();

        let git = area.path("git.git");
        git_ok(&area.root, &["init", "-q", "--bare", "git.git"]);
        area.forklift_ok("a", &["config", "mirror.url", git.to_str().unwrap()]);

        Fixture { area, server, git, first }
    }

    /// A franchise of the server at `name`, mirroring nowhere.
    fn franchise(&self, name: &str) {
        self.area.forklift_ok(".", &["franchise", &self.server.url, name]);
    }

    fn mirror_dir(&self, warehouse: &str) -> PathBuf {
        self.area.path(warehouse).join(".forklift").join("mirror.git")
    }

    fn roots_record(&self, warehouse: &str) -> PathBuf {
        self.area.path(warehouse).join(".forklift").join("mirror-roots")
    }
}

fn prepare_warehouse(area: &TestArea, name: &str, remote_url: &str) {
    std::fs::create_dir_all(area.path(name)).unwrap();

    area.forklift_ok(name, &["prepare"]);
    area.forklift_ok(name, &["config", "--global", "operator.name", "Mirror Tester"]);
    area.forklift_ok(name, &["config", "--global", "operator.identifier", "tester@forklift"]);
    area.forklift_ok(name, &["config", "remote.url", remote_url]);
}

/// The one outcome of a `--json mirror` run that succeeded, for `pallet`.
fn outcome<'a>(report: &'a serde_json::Value, pallet: &str) -> &'a serde_json::Value {
    report["pallets"].as_array().unwrap().iter()
        .find(|entry| entry["pallet"] == pallet)
        .unwrap_or_else(|| panic!("no outcome for \"{}\": {}", pallet, report))
}

/// The entries of a roots record, pallet to parcel.
fn record_entries(path: &Path) -> Vec<(String, String)> {
    let text = std::fs::read_to_string(path).unwrap();
    let mut lines = text.lines();
    assert_eq!(lines.next(), Some("forklift mirror roots 1"), "{}", text);

    lines.map(|line| {
        let (pallet, parcel) = line.split_once(' ').unwrap();
        (pallet.to_string(), parcel.to_string())
    }).collect()
}

/// Install an executable hook in a git repository.
#[cfg(unix)]
fn install_hook(repo: &Path, name: &str, body: &str) {
    use std::os::unix::fs::PermissionsExt;

    let hooks = repo.join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let path = hooks.join(name);
    std::fs::write(&path, format!("#!/bin/sh\n{}", body)).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A shell line that runs forklift in a warehouse of the area, with the area's environment and
/// none of the variables git sets for the hook running it.
#[cfg(unix)]
fn forklift_line(area: &TestArea, dir: &str, args: &str) -> String {
    format!(
        "(unset $(env | sed -n 's/^\\(GIT_[A-Za-z_]*\\)=.*/\\1/p'); cd '{}' && FORKLIFT_FSYNC=0 \
        FORKLIFT_GLOBAL_CONFIG='{}' FORKLIFT_KEYS_DIR='{}' GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null \
        HOME='{}' '{}' {}) >> '{}' 2>&1\n",
        area.path(dir).display(),
        area.path("area-global.toml").display(),
        area.path("shared-keys").display(),
        area.root.display(),
        FORKLIFT,
        args,
        area.path("hook.log").display(),
    )
}

/// Memo test 2: a second run with nothing new pushes nothing.
///
/// Revert: classify `G == N` as a push → the run reports "pushed".
#[test]
fn an_idle_run_pushes_nothing() {
    let fixture = Fixture::new("idle");
    let area = &fixture.area;

    let first = area.json("a", &["mirror"]);
    assert_eq!(outcome(&first, "main")["status"], "pushed", "{}", first);
    let pushed = tip(&fixture.git, "main").unwrap();

    let second = area.json("a", &["mirror"]);
    assert_eq!(outcome(&second, "main")["status"], "up_to_date", "{}", second);
    assert_eq!(outcome(&second, "main")["exported"], 0, "{}", second);
    assert_eq!(tip(&fixture.git, "main").unwrap(), pushed);
}

/// Memo test 3: a lift advances git by exactly one fast-forward commit, whose last line names the
/// lifted parcel and this build's layout.
///
/// Revert: drop the lift trigger → git does not move; export from scratch → `exported` is 2.
#[test]
fn a_lift_advances_git_by_one_fast_forward_naming_its_parcel() {
    let fixture = Fixture::new("one-parcel");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    let before = tip(&fixture.git, "main").unwrap();
    assert_eq!(last_line(&fixture.git, &before), parcel_line(&fixture.first));

    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    let head = lifted["head"].as_str().unwrap();
    assert_eq!(lifted["mirror"]["status"], "pushed", "{}", lifted);
    assert_eq!(lifted["mirror"]["exported"], 1, "{}", lifted);

    let after = tip(&fixture.git, "main").unwrap();
    assert_eq!(git_ok(&fixture.git, &["rev-list", "--count", &format!("{}..{}", before, after)]), "1");
    assert_eq!(git_ok(&fixture.git, &["rev-parse", &format!("{}^", after)]), before);
    assert_eq!(last_line(&fixture.git, &after), parcel_line(head));
}

/// Memo test 4: a commit made on git's branch is never overwritten. The next lift's export is a
/// sibling of it, so the pallet is refused as diverged, naming the commit.
///
/// Revert: push with `--force` → git's branch moves to the export.
#[test]
fn a_foreign_commit_on_the_git_branch_is_a_divergence_and_stays() {
    let fixture = Fixture::new("divergence");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    let mirrored = tip(&fixture.git, "main").unwrap();
    let tree = git_ok(&fixture.git, &["rev-parse", &format!("{}^{{tree}}", mirrored)]);
    let foreign = write_commit(&fixture.git, &tree, &[&mirrored], "Made in git\n");
    git_ok(&fixture.git, &["update-ref", "refs/heads/main", &foreign]);

    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    assert_eq!(lifted["mirror"]["status"], "diverged", "{}", lifted);
    assert!(lifted["mirror"]["message"].as_str().unwrap().contains(&foreign), "{}", lifted);
    assert_eq!(tip(&fixture.git, "main").unwrap(), foreign);

    // An explicit run fails, and its JSON error still carries the report.
    let explicit = area.forklift("a", &["--json", "mirror"]);
    assert_eq!(explicit.status.code(), Some(33), "{}", stdout(&explicit));
    let document: serde_json::Value = serde_json::from_str(&stdout(&explicit)).unwrap();
    assert_eq!(document["error"]["code"], "mirror_incomplete", "{}", document);
    let details = &document["error"]["details"];
    assert_eq!(details["url"], fixture.git.to_str().unwrap(), "{}", document);
    assert_eq!(outcome(details, "main")["status"], "diverged", "{}", document);
    assert_eq!(tip(&fixture.git, "main").unwrap(), foreign);
}

/// Memo test 5: a git-side amend that keeps the parcel line is a divergence, and nothing built on
/// the amended commit is pushed.
///
/// Revert: seed the export from the fetched tip → the child is exported on the amended commit and
/// pushed as a fast-forward of it.
#[test]
fn a_git_side_rewrite_is_a_divergence() {
    let fixture = Fixture::new("rewrite");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    let mirrored = tip(&fixture.git, "main").unwrap();
    let message = git_ok(&fixture.git, &["log", "-1", "--format=%B", &mirrored]);
    let empty = git_ok(&fixture.git, &["hash-object", "-t", "tree", "-w", "--stdin"]);
    let amended = write_commit(&fixture.git, &empty, &[], &format!("{}\n", message));
    assert_eq!(last_line(&fixture.git, &amended), parcel_line(&fixture.first));
    git_ok(&fixture.git, &["update-ref", "refs/heads/main", &amended]);

    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    assert_eq!(lifted["mirror"]["status"], "diverged", "{}", lifted);
    assert!(lifted["mirror"]["message"].as_str().unwrap().contains(&amended), "{}", lifted);
    assert_eq!(tip(&fixture.git, "main").unwrap(), amended);
}

/// Memo tests 6 and 8 (the race whose new tip the local repository lacks): another warehouse lifts
/// and mirrors a later parcel between this run's export and its push. The push is rejected, the
/// re-fetch finds git moved to that warehouse's export of the later lift, and the run ends as
/// already ahead, exit 0. The race is staged by a reference-transaction hook in this warehouse's
/// mirror repository, which fires once the exported ref is written.
///
/// Revert: treat every rejection as final → "failed"; drop the already-ahead case → "diverged".
#[cfg(unix)]
#[test]
fn git_already_ahead_after_a_race_the_mirror_lacks_ends_already_ahead() {
    let fixture = Fixture::new("ahead");
    let area = &fixture.area;

    fixture.franchise("b");
    area.forklift_ok("b", &["config", "mirror.url", fixture.git.to_str().unwrap()]);
    area.stack("b", "code.txt", "from b\n", "later");

    // Create the mirror repository, with nothing yet on git.
    area.forklift_ok("a", &["config", "mirror.pallets", "none-yet"]);
    area.forklift_ok("a", &["mirror"]);
    area.forklift_ok("a", &["config", "mirror.pallets", "main"]);

    install_hook(&fixture.mirror_dir("a"), "reference-transaction", &format!(
        "[ \"$1\" = committed ] || exit 0\ngrep -q refs/forklift/exported/main || exit 0\n\
        [ -e '{marker}' ] && exit 0\ntouch '{marker}'\n{lift}",
        marker = area.path("raced").display(),
        lift = forklift_line(area, "b", "lift"),
    ));

    let report = area.json("a", &["mirror"]);
    assert!(area.path("raced").exists(), "the race never ran");
    let log = std::fs::read_to_string(area.path("hook.log")).unwrap_or_default();
    assert!(log.contains("Mirrored \"main\""), "the other warehouse did not mirror: {}", log);

    let main = outcome(&report, "main");
    assert_eq!(main["status"], "already_ahead", "{}", report);
    let git_tip = tip(&fixture.git, "main").unwrap();
    assert_eq!(main["commit"], git_tip.as_str(), "{}", report);
    assert_ne!(last_line(&fixture.git, &git_tip), parcel_line(&fixture.first));
}

/// Memo test 8, the race whose new tip the local repository already holds: git's branch is moved,
/// mid-run, to a commit this warehouse exported for another pallet, at the head the Forklift remote
/// also moved to. Git rejects the push as a non-fast-forward; the run re-fetches and ends as
/// already ahead, never as a divergence.
///
/// Revert: classify before the re-fetch only (no retry) → "failed".
#[cfg(unix)]
#[test]
fn a_race_to_a_tip_the_mirror_holds_ends_already_ahead() {
    let fixture = Fixture::new("race-held");
    let area = &fixture.area;

    area.forklift_ok("a", &["palletize", "feat"]);
    area.stack("a", "code.txt", "feat\n", "on feat");
    area.lift("a");
    area.forklift_ok("a", &["shift", "main"]);

    area.forklift_ok("a", &["config", "mirror.pallets", "feat"]);
    area.forklift_ok("a", &["mirror"]);
    area.forklift_ok("a", &["config", "mirror.pallets", "main"]);

    // B moves the remote's main to feat's head, ready to lift.
    fixture.franchise("b");
    area.forklift_ok("b", &["lower", "feat"]);
    area.forklift_ok("b", &["consolidate", "feat"]);

    install_hook(&fixture.mirror_dir("a"), "reference-transaction", &format!(
        "[ \"$1\" = committed ] || exit 0\ngrep -q refs/forklift/exported/main || exit 0\n\
        [ -e '{marker}' ] && exit 0\ntouch '{marker}'\n{lift}\
        git --git-dir='{mirror}' push -q '{git}' refs/forklift/exported/feat:refs/heads/main >> '{log}' 2>&1\n",
        marker = area.path("raced").display(),
        lift = forklift_line(area, "b", "lift"),
        mirror = fixture.mirror_dir("a").display(),
        git = fixture.git.display(),
        log = area.path("hook.log").display(),
    ));

    let report = area.json("a", &["mirror"]);
    assert!(area.path("raced").exists(), "the race never ran");
    assert_eq!(tip(&fixture.git, "main"), tip(&fixture.git, "feat"),
        "{}", std::fs::read_to_string(area.path("hook.log")).unwrap_or_default());
    assert_eq!(outcome(&report, "main")["status"], "already_ahead", "{}", report);
}

/// Memo test 9: a hook's decline is quoted and reported as a failure after one re-fetch, never as
/// a lost race, since git's branch did not move.
///
/// Revert: key the race on the porcelain "!" line → "raced" after three attempts.
#[cfg(unix)]
#[test]
fn a_hook_decline_is_quoted_and_not_a_race() {
    let fixture = Fixture::new("decline");
    let area = &fixture.area;

    install_hook(&fixture.git, "pre-receive", "echo 'pushes are frozen by policy' >&2\nexit 1\n");

    let refused = area.forklift("a", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    let error = stderr(&refused);
    assert!(error.contains("pushes are frozen by policy"), "{}", error);
    assert!(error.contains("Fix the cause git reports"), "{}", error);
    assert!(!error.contains("kept moving"), "{}", error);
    assert_eq!(tip(&fixture.git, "main"), None);
}

/// Memo test 7: a commit pushed by hand on top of the mirrored tip is a divergence naming it, not
/// "already ahead", even under a later commit whose last line names a parcel of the pallet's lifted
/// history: the hand-written one carries no parcel line.
///
/// Revert: drop the N..G parcel-line check → "already ahead", exit 0.
#[test]
fn a_foreign_commit_stacked_on_the_tip_is_a_divergence() {
    let fixture = Fixture::new("stacked");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    let mirrored = tip(&fixture.git, "main").unwrap();
    let tree = git_ok(&fixture.git, &["rev-parse", &format!("{}^{{tree}}", mirrored)]);
    let foreign = write_commit(&fixture.git, &tree, &[&mirrored], "Hand-written\n");
    let labelled = write_commit(&fixture.git, &tree, &[&foreign], &format!("Copied\n\n{}\n", parcel_line(&fixture.first)));
    git_ok(&fixture.git, &["update-ref", "refs/heads/main", &labelled, &mirrored]);

    let refused = area.forklift("a", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    let error = stderr(&refused);
    assert!(error.contains(&foreign) && error.contains("not part of this warehouse's export"), "{}", error);
    assert!(!stdout(&refused).contains("already ahead"), "{}", stdout(&refused));
}

/// Memo test 10: a git branch written in another export layout is layout skew, naming both
/// layouts, not a divergence.
///
/// Revert: drop the layout case from the classification → a divergence.
#[test]
fn a_branch_in_another_layout_is_layout_skew() {
    let fixture = Fixture::new("skew");
    let area = &fixture.area;

    let empty = git_ok(&fixture.git, &["hash-object", "-t", "tree", "-w", "--stdin"]);
    let other = write_commit(&fixture.git, &empty, &[],
        &format!("first\n\nForklift-Parcel: {} export/2\n", fixture.first));
    git_ok(&fixture.git, &["update-ref", "refs/heads/main", &other]);

    let refused = area.forklift("a", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    let error = stderr(&refused);
    assert!(error.contains("export layout 2") && error.contains("export layout 1"), "{}", error);
    assert!(!error.contains("not part of this warehouse's export"), "{}", error);
    assert_eq!(tip(&fixture.git, "main").unwrap(), other);
}

/// Memo test 12: only what the Forklift remote holds is mirrored, never a stacked, unlifted parcel.
///
/// Revert: mirror the local pallet head → git receives the unlifted parcel.
#[test]
fn an_unlifted_parcel_never_reaches_git() {
    let fixture = Fixture::new("unlifted");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    let mirrored = tip(&fixture.git, "main").unwrap();
    area.stack("a", "code.txt", "local only\n", "unlifted");

    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "main")["status"], "up_to_date", "{}", report);
    assert_eq!(tip(&fixture.git, "main").unwrap(), mirrored);
}

/// Memo test 13: an exported commit whose last line was rewritten in the mirror's repository, once
/// to name another parcel of the history and once another layout, is refused with the step to
/// delete `.forklift/mirror.git`.
///
/// Revert: drop the tip's parent check → the other-parcel run exports on the rewritten commit and
/// reports a divergence instead; drop the layout check → the layout run is not refused locally.
#[test]
fn a_rewritten_exported_commit_is_refused_with_the_delete_step() {
    let fixture = Fixture::new("lockstep");
    let area = &fixture.area;

    area.stack("a", "code.txt", "v2\n", "second");
    let second = area.lift("a")["head"].as_str().unwrap().to_string();
    area.forklift_ok("a", &["mirror"]);

    let mirror = fixture.mirror_dir("a");
    let exported = git_ok(&mirror, &["rev-parse", "refs/forklift/exported/main"]);
    let parent = git_ok(&mirror, &["rev-parse", &format!("{}^", exported)]);
    let tree = git_ok(&mirror, &["rev-parse", &format!("{}^{{tree}}", exported)]);

    // Another parcel of the history, in this layout; the right parcel, in another layout.
    for line in [parcel_line(&fixture.first), format!("Forklift-Parcel: {} export/2", second)] {
        let rewritten = write_commit(&mirror, &tree, &[&parent], &format!("second\n\n{}\n", line));
        git_ok(&mirror, &["update-ref", "refs/forklift/exported/main", &rewritten]);

        let refused = area.forklift("a", &["mirror"]);
        assert!(!refused.status.success(), "{}", stdout(&refused));
        assert!(stderr(&refused).contains("Delete .forklift/mirror.git and run \"forklift mirror\""),
            "{}: {}", line, stderr(&refused));
    }
}

/// Memo test 14: a parcel whose parent list repeats a parent (imported from git) is exported with
/// the repeat dropped, and a later run that descends through it finds the commit's parents line up.
///
/// Revert: compare the commit's parents with the parcel's undeduplicated list → the second run
/// refuses its own repository.
#[test]
fn a_repeated_parent_mirrors_on_every_run() {
    let area = TestArea::new("duplicate-parent");
    let server = Server::start(&area);

    // A git history: A, then M with A as both parents, then T. Branch feat stays at A.
    let source = area.path("source");
    std::fs::create_dir_all(&source).unwrap();
    git_ok(&source, &["init", "-q", "-b", "main"]);
    area.write_file("source/code.txt", "a\n");
    git_ok(&source, &["add", "code.txt"]);
    git_ok(&source, &["commit", "-q", "-m", "A"]);
    let a = git_ok(&source, &["rev-parse", "HEAD"]);
    let tree = git_ok(&source, &["rev-parse", "HEAD^{tree}"]);
    let m = write_commit(&source, &tree, &[&a, &a], "M\n");
    let t = write_commit(&source, &tree, &[&m], "T\n");
    git_ok(&source, &["update-ref", "refs/heads/main", &t]);
    git_ok(&source, &["branch", "feat", &a]);

    area.forklift_ok("source", &["prepare"]);
    area.forklift_ok("source", &["config", "--global", "operator.name", "Mirror Tester"]);
    area.forklift_ok("source", &["config", "--global", "operator.identifier", "tester@forklift"]);
    area.forklift_ok("source", &["import-git", "."]);
    area.forklift_ok("source", &["config", "remote.url", &server.url]);
    let main = area.lift("source")["head"].as_str().unwrap().to_string();
    area.forklift_ok("source", &["shift", "feat"]);
    area.lift("source");
    area.forklift_ok("source", &["shift", "main"]);

    // The merge parcel keeps its repeated parent.
    let tip_parcel = area.json("source", &["peek", &main]);
    let merge = tip_parcel["parents"][0].as_str().unwrap().to_string();
    let merge_parcel = area.json("source", &["peek", &merge]);
    assert_eq!(merge_parcel["parents"].as_array().unwrap().len(), 2, "{}", merge_parcel);

    let git = area.path("git.git");
    git_ok(&area.root, &["init", "-q", "--bare", "git.git"]);
    area.forklift_ok("source", &["config", "mirror.url", git.to_str().unwrap()]);
    area.forklift_ok("source", &["config", "mirror.pallets", "main"]);
    area.forklift_ok("source", &["mirror"]);

    // feat's head lies below the merge, so this run descends through it from main's export.
    area.forklift_ok("source", &["config", "mirror.pallets", "main,feat"]);
    let report = area.json("source", &["mirror"]);
    assert_eq!(outcome(&report, "feat")["status"], "pushed", "{}", report);
    assert_eq!(outcome(&report, "feat")["exported"], 0, "{}", report);
    let merged = git_ok(&git, &["rev-parse", "main^"]);
    assert_eq!(git_ok(&git, &["rev-list", "--parents", "-n", "1", &merged]).split(' ').count(), 2);
}

/// Memo test 16: after mirror.git is deleted, the next run exports everything again, byte for
/// byte, so git already holds it and nothing is pushed.
///
/// Revert: any nondeterminism in the export → a divergence.
#[test]
fn a_deleted_mirror_repository_re_exports_and_pushes_nothing() {
    let fixture = Fixture::new("deleted");
    let area = &fixture.area;

    area.stack("a", "code.txt", "v2\n", "second");
    area.lift("a");
    area.forklift_ok("a", &["mirror"]);
    let mirrored = tip(&fixture.git, "main").unwrap();

    std::fs::remove_dir_all(fixture.mirror_dir("a")).unwrap();

    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "main")["status"], "up_to_date", "{}", report);
    assert_eq!(outcome(&report, "main")["exported"], 2, "{}", report);
    assert_eq!(tip(&fixture.git, "main").unwrap(), mirrored);
}

/// Memo test 17: a pallet whose push failed keeps its export in the roots record, so gc keeps its
/// history and the next run pushes the same export without writing it again. The pallet is one
/// this warehouse holds no ref for, so only the record keeps its history.
///
/// Revert: rewrite the record to the pallets that went through → the second run re-exports feat.
#[cfg(unix)]
#[test]
fn a_failed_push_resumes_without_re_exporting_after_gc() {
    let fixture = Fixture::new("resume");
    let area = &fixture.area;

    fixture.franchise("b");
    area.forklift_ok("b", &["palletize", "feat"]);
    area.stack("b", "feat.txt", "feat\n", "feat only");
    area.lift("b");

    install_hook(&fixture.git, "pre-receive",
        "while read old new ref; do [ \"$ref\" = refs/heads/feat ] && { echo 'feat is frozen' >&2; exit 1; }; done\nexit 0\n");

    let refused = area.forklift("a", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("feat is frozen"), "{}", stderr(&refused));
    assert!(tip(&fixture.git, "main").is_some());

    {
        let _scope = StorageRootScope::enter(&area.path("a"));
        gc_utils::collect_garbage(0).expect("gc runs");
    }

    std::fs::remove_file(fixture.git.join("hooks").join("pre-receive")).unwrap();
    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "feat")["status"], "pushed", "{}", report);
    assert_eq!(outcome(&report, "feat")["exported"], 0, "{}", report);
}

/// Memo test 18: the history of a mirrored pallet this warehouse holds no ref for survives gc
/// through the roots record, so after another lift only the new parcel is exported.
///
/// Revert: drop the roots record from gc's roots → feat's history is collected and re-exported.
#[test]
fn a_boundary_survives_gc_and_only_the_new_parcel_exports() {
    let fixture = Fixture::new("boundary-gc");
    let area = &fixture.area;

    fixture.franchise("b");
    area.forklift_ok("b", &["palletize", "feat"]);
    area.stack("b", "feat.txt", "feat 1\n", "feat one");
    area.lift("b");

    let first = area.json("a", &["mirror"]);
    assert_eq!(outcome(&first, "feat")["status"], "pushed", "{}", first);

    {
        let _scope = StorageRootScope::enter(&area.path("a"));
        gc_utils::collect_garbage(0).expect("gc runs");
    }

    area.stack("b", "feat.txt", "feat 2\n", "feat two");
    area.lift("b");

    let second = area.json("a", &["mirror"]);
    assert_eq!(outcome(&second, "feat")["status"], "pushed", "{}", second);
    assert_eq!(outcome(&second, "feat")["exported"], 1, "{}", second);
}

/// Memo test 20: a mirror failure never fails the lift. An unreachable mirror is reported with
/// git's message and the step `forklift mirror`; a mirror that asks for credentials fails at once,
/// because git may not prompt inside a lift.
///
/// Revert: drop GIT_TERMINAL_PROMPT=0 → git's message no longer says prompts are disabled.
#[test]
fn a_lift_reports_an_unreachable_mirror_without_failing() {
    let fixture = Fixture::new("unreachable");
    let area = &fixture.area;

    let missing = area.path("missing.git");
    area.forklift_ok("a", &["config", "mirror.url", missing.to_str().unwrap()]);
    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    assert_eq!(lifted["mirror"]["status"], "failed", "{}", lifted);
    let message = lifted["mirror"]["message"].as_str().unwrap();
    assert!(message.contains("git fetch from the mirror failed") && message.contains("forklift mirror"), "{}", message);

    // A git host that wants credentials for everything.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/repo.git", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buffer = [0u8; 4096];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"git\"\r\n\
                Content-Length: 0\r\nConnection: close\r\n\r\n");
        }
    });

    area.forklift_ok("a", &["config", "mirror.url", &url]);
    area.stack("a", "code.txt", "v3\n", "third");
    let started = std::time::Instant::now();
    let lifted = area.lift("a");
    assert!(started.elapsed() < std::time::Duration::from_secs(60));
    assert_eq!(lifted["mirror"]["status"], "failed", "{}", lifted);
    let message = lifted["mirror"]["message"].as_str().unwrap();
    assert!(message.contains("terminal prompts disabled") && message.contains("forklift mirror"), "{}", message);
}

/// A pass-through HTTP proxy that answers the requests `refuse` picks with a 500.
struct RefusingProxy {
    url: String,
}

impl RefusingProxy {
    fn start(upstream: &str, refuse: fn(&str) -> bool) -> RefusingProxy {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let upstream = upstream.trim_start_matches("http://").trim_end_matches('/').to_string();

        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(downstream) = stream else { continue };
                let upstream = upstream.clone();
                std::thread::spawn(move || RefusingProxy::serve(downstream, &upstream, refuse));
            }
        });

        RefusingProxy { url }
    }

    fn serve(mut downstream: std::net::TcpStream, upstream: &str, refuse: fn(&str) -> bool) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 8192];

        let header_end = loop {
            if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
                break position + 4;
            }
            match downstream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        };

        let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
        let mut lines = head.split("\r\n").filter(|line| !line.is_empty());
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers: Vec<String> = lines
            .filter(|line| !line.to_ascii_lowercase().starts_with("connection:"))
            .map(str::to_string)
            .collect();
        let content_length: usize = headers.iter()
            .find_map(|line| line.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().to_string()))
            .and_then(|value| value.parse().ok())
            .unwrap_or(0);

        let mut body = buffer[header_end..].to_vec();
        while body.len() < content_length {
            match downstream.read(&mut chunk) {
                Ok(0) | Err(_) => return,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }

        if refuse(&request_line) {
            let _ = downstream.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            return;
        }

        let Ok(mut server) = std::net::TcpStream::connect(upstream) else { return };
        let mut forwarded = format!("{}\r\n{}\r\nConnection: close\r\n\r\n", request_line, headers.join("\r\n")).into_bytes();
        forwarded.extend_from_slice(&body);
        if server.write_all(&forwarded).is_err() {
            return;
        }

        let _ = std::io::copy(&mut server, &mut downstream);
        let _ = downstream.shutdown(std::net::Shutdown::Both);
    }
}

/// Memo test 21: a lift whose meta pallet fails after the working pallet landed ends in an error,
/// and no automatic mirror runs.
///
/// Revert: mirror before the meta-failure check → git receives the pallet.
#[test]
fn a_failed_meta_pallet_lift_runs_no_mirror() {
    let fixture = Fixture::new("meta-failed");
    let area = &fixture.area;

    area.forklift_ok("a", &["office", "enroll"]);
    area.forklift_ok("a", &["manifest", "approve", "main", "-m", "LGTM"]);
    area.stack("a", "code.txt", "v2\n", "second");

    let proxy = RefusingProxy::start(&fixture.server.url, |request| {
        request.starts_with("POST ") && (request.contains("/v1/pallets/@manifest") || request.contains("/v1/pallets/%40manifest"))
    });
    area.forklift_ok("a", &["config", "remote.url", &proxy.url]);

    let lifted = area.forklift("a", &["lift"]);
    assert!(!lifted.status.success(), "{}", stdout(&lifted));
    assert!(stderr(&lifted).contains("@manifest"), "{}", stderr(&lifted));
    assert!(stdout(&lifted).contains("Lifted pallet \"main\""), "the working pallet must land: {}", stdout(&lifted));
    assert!(!stdout(&lifted).contains("Mirrored"), "{}", stdout(&lifted));
    assert_eq!(tip(&fixture.git, "main"), None);
}

/// Memo test 22: a scoped bay holds only part of each tree, so lift's mirror is skipped there and
/// `forklift mirror` refuses.
///
/// Revert: drop the scope gate → the lift mirrors a partial tree.
#[test]
fn a_scoped_bay_skips_the_lift_mirror_and_refuses_mirror() {
    let fixture = Fixture::new("scoped");
    let area = &fixture.area;

    area.stack("a", "docs/readme.txt", "docs\n", "docs");
    area.lift("a");
    let bay = area.path("bay");
    area.forklift_ok("a", &["bay", "add", "scoped", bay.to_str().unwrap(), "--scope", "docs"]);

    area.stack("bay", "docs/readme.txt", "docs 2\n", "from the bay");
    let lifted = area.lift("bay");
    assert_eq!(lifted["mirror"]["status"], "skipped", "{}", lifted);

    let refused = area.forklift("bay", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("scoped bay"), "{}", stderr(&refused));
}

/// Memo test 23: a run that finds the mirror lock held reports busy without waiting, and a holder
/// killed with SIGKILL releases it. The holder is a run held inside its push by a pre-push hook;
/// the other runs start in a second bay, since each bay has its own warehouse lock and the mirror
/// lock is the one they share.
///
/// Revert: a create-new lock file → the run after the kill still reports busy.
#[cfg(unix)]
#[test]
fn a_busy_mirror_is_reported_and_a_killed_holder_releases_it() {
    let fixture = Fixture::new("lock");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    area.forklift_ok("a", &["config", "mirror.auto", "off"]);
    area.stack("a", "code.txt", "v2\n", "second");
    area.lift("a");
    let bay = area.path("bay");
    area.forklift_ok("a", &["bay", "add", "full", bay.to_str().unwrap()]);

    // Holds the lock in the exported ref's update, before the push.
    install_hook(&fixture.mirror_dir("a"), "reference-transaction", &format!(
        "[ \"$1\" = committed ] || exit 0\ngrep -q refs/forklift/exported/main || exit 0\n\
        touch '{}'\nsleep 30\n",
        area.path("holding").display()
    ));

    let mut holder = area.forklift_in(&area.path("a")).arg("mirror")
        .stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
    let started = std::time::Instant::now();
    while !area.path("holding").exists() {
        assert!(started.elapsed() < std::time::Duration::from_secs(60), "the holder never reached its push");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let busy = area.forklift("bay", &["mirror"]);
    assert!(!busy.status.success(), "{}", stdout(&busy));
    assert!(stderr(&busy).contains("Another forklift mirror run"), "{}", stderr(&busy));

    holder.kill().unwrap();
    holder.wait().unwrap();
    std::fs::remove_file(fixture.mirror_dir("a").join("hooks").join("reference-transaction")).unwrap();

    let report = area.json("bay", &["mirror"]);
    assert_eq!(outcome(&report, "main")["status"], "pushed", "{}", report);
}

/// Memo test 24: a configured pallet the remote lacks, and one git refuses as a branch name, are
/// skipped without writing a ref, and the run succeeds.
///
/// Revert: check the branch name after the export → an exported ref is written for it.
#[test]
fn unmirrorable_pallets_are_skipped_without_refs() {
    let fixture = Fixture::new("unmirrorable");
    let area = &fixture.area;

    area.forklift_ok("a", &["palletize", "feature.lock"]);
    area.stack("a", "code.txt", "lock\n", "on feature.lock");
    area.lift("a");
    area.forklift_ok("a", &["shift", "main"]);

    area.forklift_ok("a", &["config", "mirror.pallets", "ghost,feature.lock"]);
    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "ghost")["status"], "skipped", "{}", report);
    assert_eq!(outcome(&report, "feature.lock")["status"], "skipped", "{}", report);

    let refs = git_ok(&fixture.mirror_dir("a"), &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(refs, "", "{}", refs);
    assert_eq!(git_ok(&fixture.git, &["for-each-ref"]), "");
}

/// Memo test 25: git's main fast-forwarded to feat's export carries this layout's parcel lines,
/// but feat's head is not in main's history on the Forklift remote, so it is a divergence.
///
/// Revert: drop the check that git's parcel is in the pallet's lifted history → "already ahead".
#[test]
fn a_fast_forward_to_another_pallet_is_a_divergence() {
    let fixture = Fixture::new("other-pallet");
    let area = &fixture.area;

    area.forklift_ok("a", &["palletize", "feat"]);
    area.stack("a", "code.txt", "feat\n", "on feat");
    area.lift("a");
    area.forklift_ok("a", &["shift", "main"]);
    area.forklift_ok("a", &["mirror"]);

    let feat = tip(&fixture.git, "feat").unwrap();
    git_ok(&fixture.git, &["push", "-q", ".", "refs/heads/feat:refs/heads/main"]);

    area.forklift_ok("a", &["config", "mirror.pallets", "main"]);
    let refused = area.forklift("a", &["mirror"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains(&feat) && stderr(&refused).contains("not part of this warehouse's export"),
        "{}", stderr(&refused));
}

/// Memo test 26, the mirror's half: an unreadable roots record is rebuilt from every exported ref,
/// by an explicit run and by lift's trigger, not left holding the one pallet the run touched.
///
/// Revert: start from an empty record when it cannot be read → the trigger leaves only main.
#[test]
fn an_unreadable_roots_record_is_rebuilt_from_the_exported_refs() {
    let fixture = Fixture::new("record");
    let area = &fixture.area;

    area.forklift_ok("a", &["palletize", "feat"]);
    area.stack("a", "code.txt", "feat\n", "on feat");
    area.lift("a");
    area.forklift_ok("a", &["shift", "main"]);
    area.forklift_ok("a", &["mirror"]);

    let pallets = |entries: Vec<(String, String)>| entries.into_iter().map(|(pallet, _)| pallet).collect::<Vec<_>>();

    std::fs::write(fixture.roots_record("a"), "not a record\n").unwrap();
    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    assert_eq!(lifted["mirror"]["status"], "pushed", "{}", lifted);
    assert_eq!(pallets(record_entries(&fixture.roots_record("a"))), ["feat", "main"]);

    std::fs::write(fixture.roots_record("a"), "not a record\n").unwrap();
    area.forklift_ok("a", &["mirror"]);
    assert_eq!(pallets(record_entries(&fixture.roots_record("a"))), ["feat", "main"]);
}

/// A relative `mirror.url` is read from the warehouse's root, from the main tree and from a bay
/// alike, never from the mirror's own repository.
///
/// Revert: run git's network calls in the mirror's repository → "../git.git" names a folder
/// inside `.forklift`, and the run fails.
#[test]
fn a_relative_url_is_read_from_the_warehouse_root() {
    let fixture = Fixture::new("relative");
    let area = &fixture.area;

    area.forklift_ok("a", &["config", "mirror.url", "../git.git"]);
    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "main")["status"], "pushed", "{}", report);

    area.forklift_ok("a", &["config", "mirror.auto", "off"]);
    area.stack("a", "code.txt", "v2\n", "second");
    let second = area.lift("a")["head"].as_str().unwrap().to_string();
    let bay = area.path("bay");
    area.forklift_ok("a", &["bay", "add", "full", bay.to_str().unwrap()]);

    let report = area.json("bay", &["mirror"]);
    assert_eq!(outcome(&report, "main")["status"], "pushed", "{}", report);
    assert_eq!(last_line(&fixture.git, &tip(&fixture.git, "main").unwrap()), parcel_line(&second));
}

/// The hooks of the mirror's own repository (and so the user's global hooks folder) never run
/// on the mirror's fetches and pushes.
///
/// Revert: drop the empty hooks path → the pre-push hook runs and declines the push.
#[cfg(unix)]
#[test]
fn hooks_never_run_on_the_mirror_network_calls() {
    let fixture = Fixture::new("no-hooks");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    install_hook(&fixture.mirror_dir("a"), "pre-push", &format!(
        "touch '{}'\nexit 1\n", area.path("hooked").display()
    ));

    area.stack("a", "code.txt", "v2\n", "second");
    let lifted = area.lift("a");
    assert_eq!(lifted["mirror"]["status"], "pushed", "{}", lifted);
    assert!(!area.path("hooked").exists(), "the pre-push hook ran");
}

/// A push still rejected on its last attempt is classified once more after the re-fetch: git's
/// branch, moved by the racer to the export itself, ends the run up to date, not raced.
///
/// Revert: report "raced" straight after the last rejection → "raced".
#[cfg(unix)]
#[test]
fn a_race_that_ends_at_the_export_is_up_to_date() {
    let fixture = Fixture::new("last-race");
    let area = &fixture.area;

    area.forklift_ok("a", &["mirror"]);
    area.forklift_ok("a", &["config", "mirror.auto", "off"]);
    for version in ["v2", "v3", "v4"] {
        area.stack("a", "code.txt", &format!("{}\n", version), version);
        area.lift("a");
    }

    // Export the three new parcels elsewhere, and give git their commits off its branch.
    let scratch = area.path("scratch.git");
    git_ok(&area.root, &["init", "-q", "--bare", "scratch.git"]);
    area.forklift_ok("a", &["config", "mirror.url", scratch.to_str().unwrap()]);
    area.forklift_ok("a", &["mirror"]);
    let export = tip(&scratch, "main").unwrap();
    git_ok(&scratch, &["push", "-q", fixture.git.to_str().unwrap(), "main:refs/held/main"]);
    area.forklift_ok("a", &["config", "mirror.url", fixture.git.to_str().unwrap()]);

    // Every push is declined while main moves one step closer, reaching the export on the third.
    install_hook(&fixture.git, "pre-receive", &format!(
        "n=$(cat '{count}' 2>/dev/null || echo 0); n=$((n+1)); echo $n > '{count}'\n\
        case $n in 1) t='{export}~2';; 2) t='{export}~1';; *) t='{export}';; esac\n\
        (unset $(env | sed -n 's/^\\(GIT_[A-Za-z_]*\\)=.*/\\1/p'); \
        git --git-dir='{git}' update-ref refs/heads/main \"$t\") >> '{log}' 2>&1\nexit 1\n",
        count = area.path("attempts").display(),
        export = export,
        git = fixture.git.display(),
        log = area.path("hook.log").display(),
    ));

    let report = area.json("a", &["mirror"]);
    assert_eq!(std::fs::read_to_string(area.path("attempts")).unwrap().trim(), "3", "{}", report);
    assert_eq!(outcome(&report, "main")["status"], "up_to_date", "{}", report);
    assert_eq!(tip(&fixture.git, "main").unwrap(), export);
}

/// An exported ref whose parcel this warehouse fetches only during the run becomes a boundary
/// once fetched: a franchise handed another warehouse's mirror repository writes nothing again.
///
/// Revert: take the boundaries once, before the fetch → feat's parcel is exported again.
#[test]
fn an_exported_parcel_fetched_by_the_run_is_built_on() {
    let fixture = Fixture::new("promote");
    let area = &fixture.area;

    fixture.franchise("b");
    area.forklift_ok("a", &["config", "mirror.auto", "off"]);
    area.forklift_ok("a", &["palletize", "feat"]);
    area.stack("a", "feat.txt", "feat\n", "feat only");
    area.lift("a");
    area.forklift_ok("a", &["config", "mirror.pallets", "main,feat"]);
    let report = area.json("a", &["mirror"]);
    assert_eq!(outcome(&report, "feat")["status"], "pushed", "{}", report);

    copy_dir(&fixture.mirror_dir("a"), &fixture.mirror_dir("b"));
    area.forklift_ok("b", &["config", "mirror.url", fixture.git.to_str().unwrap()]);
    area.forklift_ok("b", &["config", "mirror.pallets", "main,feat"]);

    let report = area.json("b", &["mirror"]);
    assert_eq!(outcome(&report, "feat")["status"], "up_to_date", "{}", report);
    assert_eq!(outcome(&report, "feat")["exported"], 0, "{}", report);
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &to.join(entry.file_name()));
        } else {
            std::fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

/// A pallet whose exported commit is damaged is refused alone: the other pallets of the run still
/// go through.
///
/// Revert: fail the run on the first exported ref that cannot become a boundary → main is not
/// pushed.
#[test]
fn a_damaged_export_refuses_only_its_own_pallet() {
    let fixture = Fixture::new("damaged-one");
    let area = &fixture.area;

    area.forklift_ok("a", &["config", "mirror.auto", "off"]);
    area.forklift_ok("a", &["palletize", "feat"]);
    area.stack("a", "feat.txt", "feat\n", "feat only");
    area.lift("a");
    area.forklift_ok("a", &["shift", "main"]);
    area.forklift_ok("a", &["config", "mirror.pallets", "main,feat"]);
    area.forklift_ok("a", &["mirror"]);

    // feat's tip now names main's first parcel, which has no parent.
    let mirror = fixture.mirror_dir("a");
    let exported = git_ok(&mirror, &["rev-parse", "refs/forklift/exported/feat"]);
    let parent = git_ok(&mirror, &["rev-parse", &format!("{}^", exported)]);
    let tree = git_ok(&mirror, &["rev-parse", &format!("{}^{{tree}}", exported)]);
    let rewritten = write_commit(&mirror, &tree, &[&parent], &format!("first\n\n{}\n", parcel_line(&fixture.first)));
    git_ok(&mirror, &["update-ref", "refs/forklift/exported/feat", &rewritten]);

    area.stack("a", "code.txt", "v2\n", "second");
    let second = area.lift("a")["head"].as_str().unwrap().to_string();

    let run = area.forklift("a", &["--json", "mirror"]);
    assert_eq!(run.status.code(), Some(33), "{}", stdout(&run));
    let document: serde_json::Value = serde_json::from_str(&stdout(&run)).unwrap();
    let details = &document["error"]["details"];
    assert_eq!(outcome(details, "main")["status"], "pushed", "{}", document);
    assert_eq!(outcome(details, "feat")["status"], "damaged", "{}", document);
    assert!(outcome(details, "feat")["message"].as_str().unwrap().contains("Delete .forklift/mirror.git"), "{}", document);
    assert_eq!(last_line(&fixture.git, &tip(&fixture.git, "main").unwrap()), parcel_line(&second));
}
