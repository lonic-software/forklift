//! Harness shared by the integration test files that launch the `forklift` binary.

use std::process::Command;

/// `forklift` with fsync off. Durability is not what these tests check, and paying an fsync per
/// write made the suite much slower. `crash_consistency.rs` forces it on, and the in-process
/// tests leave it on.
///
/// A child that panics is not caught here: this builds the command and runs nothing, so a test
/// that asserts only that the command failed also passes on a panic, the debug build's
/// durable-path assertions included. Every test that runs a child it expects to fail,
/// through this or [`test_command`], is that uncovered case. So, partly, is a launched
/// `forklift-server`: a debug build raises a request's storage panic again on a task of the
/// server process, so the client sees only a dropped connection and the server keeps serving.
/// A storage panic a reply swallowed aborts a debug server instead, and a harness that holds its
/// server in a [`ServerProcess`] fails the test on that, if the abort came before the stop. A
/// server launched without one is not checked. The same work under `forklift-aws-lambda`'s
/// `spawn_blocking` turns a panic into an error reply.
pub fn forklift_command() -> Command {
    test_command(env!("CARGO_BIN_EXE_forklift"))
}

/// `program` in the environment every launch here shares, for a test that reaches `forklift`
/// another way (through an alias) and must not differ from [`forklift_command`] in anything else.
pub fn test_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("FORKLIFT_FSYNC", "0");
    command
}

/// `forklift-server`, found next to the CLI. Cargo exposes `CARGO_BIN_EXE_*` only for this
/// package's own binaries, but a workspace build puts both in the same target directory.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn server_binary() -> std::path::PathBuf {
    let path = std::path::Path::new(env!("CARGO_BIN_EXE_forklift"))
        .parent()
        .expect("the forklift binary has a parent directory")
        .join(format!("forklift-server{}", std::env::consts::EXE_SUFFIX));

    assert!(
        path.is_file(),
        "forklift-server is not built at {}; run the tests via a workspace build \
        (plain \"cargo test\").",
        path.display()
    );

    path
}

/// A launched `forklift-server`, stopped (killed and waited for) when it is dropped. A server
/// that had already ended when the test stops it fails the test, with how it ended: a request
/// that the ending broke shows the client only a failed request, which a test expecting a failure
/// accepts. A debug server aborts on a storage panic a reply swallowed, so this is where that
/// abort fails the test.
///
/// The check is whether the process had exited when it is stopped, so an exit in the moment
/// between that look and the kill goes unseen. Nothing is checked once the thread is already
/// panicking: a second panic there would abort the whole test binary, and the first one has
/// failed the test already.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub struct ServerProcess {
    child: std::process::Child,
    stopped: bool,
}

#[allow(dead_code)] // Used by only some of the test files that include this module.
impl ServerProcess {
    pub fn new(child: std::process::Child) -> ServerProcess {
        ServerProcess { child, stopped: false }
    }

    /// The process, for taking its pipes.
    pub fn child(&mut self) -> &mut std::process::Child {
        &mut self.child
    }

    /// How the process ended, if it has, without stopping it.
    pub fn exited(&mut self) -> Option<std::process::ExitStatus> {
        self.child.try_wait().unwrap()
    }

    /// Stop the server, failing the test if it had ended on its own first. While the thread is
    /// already panicking it only kills and waits, ignoring what either returns.
    pub fn stop(&mut self) {
        if std::thread::panicking() {
            if !self.stopped {
                self.stopped = true;
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            return;
        }

        if let Some(status) = self.stop_and_report() {
            panic!(
                "the server this test launched ended on its own ({}) before the test stopped it; \
                its stderr says why. A debug build aborts on a storage panic a reply swallowed.",
                status
            );
        }
    }

    /// Stop the server and return how it ended if it had ended on its own first, for a caller
    /// that reports that itself. A server already stopped returns `None`. A kill or wait that
    /// fails fails the test, so a server stopped here has ended.
    pub fn stop_and_report(&mut self) -> Option<std::process::ExitStatus> {
        if self.stopped {
            return None;
        }
        self.stopped = true;
        let exited = self.exited();
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        exited
    }
}

impl Drop for ServerProcess {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Hold the store-access lock of the warehouse at `root` from this test process, the way another
/// forklift process would: shared like a writer in another bay, or exclusive like a running
/// repack, gc or scope prune. The kernel lock is held for as long as the returned file is open.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn hold_store_access(root: &std::path::Path, exclusive: bool) -> std::fs::File {
    let path = root.join(".forklift").join("store-access.lock");
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(&path)
        .unwrap_or_else(|e| panic!("open {}: {}", path.display(), e));

    if exclusive {
        file.try_lock().unwrap_or_else(|e| panic!("lock {} exclusively: {}", path.display(), e));
    } else {
        file.try_lock_shared().unwrap_or_else(|e| panic!("lock {} shared: {}", path.display(), e));
    }

    file
}

/// The repository's root folder.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn repository_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().unwrap()
}

/// Every Markdown page in the repository, sorted: the ones git tracks and the new ones it does not
/// ignore, so a page is checked before it is committed. Asking git, not walking folders, keeps a
/// page anywhere in the tree from being missed and a build folder from being read.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn tracked_pages() -> Vec<std::path::PathBuf> {
    let root = repository_root();
    let output = Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.md"])
        .output()
        .expect("the documentation tests list the pages with git, which must be on PATH");
    assert!(
        output.status.success(),
        "the documentation tests need a git checkout to list the pages: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let mut pages: Vec<std::path::PathBuf> = String::from_utf8(output.stdout)
        .unwrap()
        .split('\0')
        .filter(|name| !name.is_empty())
        .map(|name| root.join(name))
        .filter(|path| path.is_file()) // A page deleted but not yet staged is still listed.
        .collect();
    pages.sort();
    pages.dedup();
    pages
}

/// A Markdown fence line: its character (`` ` `` or `~`), how many of them, and its info string.
/// Up to three spaces of indent are allowed, as CommonMark allows.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn fence(line: &str) -> Option<(char, usize, &str)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    let rest = &line[indent..];
    let character = rest.chars().next().filter(|&c| c == '`' || c == '~')?;
    let length = rest.len() - rest.trim_start_matches(character).len();
    if indent > 3 || length < 3 {
        return None;
    }
    Some((character, length, rest[length..].trim()))
}

/// The page's lines with fenced code blocks blanked, so text shown inside an example is not read
/// as prose. Line numbers are kept. A fence closes only on a line of the same character, at least
/// as long, with no info string.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn prose_lines(page: &str) -> Vec<&str> {
    let mut open: Option<(char, usize)> = None;
    page.lines()
        .map(|line| match (open, fence(line)) {
            (None, Some((character, length, _))) => {
                open = Some((character, length));
                ""
            }
            (Some((character, length)), Some((closing, closing_length, info)))
                if closing == character && closing_length >= length && info.is_empty() =>
            {
                open = None;
                ""
            }
            (Some(_), _) => "",
            (None, None) => line,
        })
        .collect()
}

/// Move a file's mtime `seconds` into the past, leaving its content alone. The next command's
/// stat then proves the file's content without waiting until it is old enough: a stat proves an
/// entry only when the file's mtime is two whole seconds older than the clock probe read before
/// it, and the file is on the probe's device.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn backdate(path: &std::path::Path, seconds: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(seconds)).unwrap();
}

/// [`backdate`] every regular file under `root` outside its `.forklift` and `.git` folders; links
/// are left alone. Directories keep their mtimes: a stat proof reads only a file's.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn backdate_tree(root: &std::path::Path, seconds: u64) {
    for entry in std::fs::read_dir(root).unwrap() {
        let entry = entry.unwrap();
        let kind = entry.file_type().unwrap();
        if kind.is_dir() && entry.file_name() != ".forklift" && entry.file_name() != ".git" {
            backdate_tree(&entry.path(), seconds);
        } else if kind.is_file() {
            backdate(&entry.path(), seconds);
        }
    }
}
