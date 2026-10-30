//! Harness shared by the integration test files that launch the `forklift` binary.

use std::process::Command;

/// `forklift` with fsync off. Durability is not what these tests check, and paying an fsync per
/// write made the suite much slower. `crash_consistency.rs` forces it on, and the in-process
/// tests leave it on.
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
