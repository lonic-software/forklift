//! The progress-events channel, end to end: a real pipe handed to the binary at a known fd
//! through `PULT_EVENTS`, read back line by line. The channel exists on macOS and Linux only.
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::io::Read;
use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

/// Path to the compiled forklift binary (provided by cargo for integration tests).
const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// The fd number the child finds its events channel at.
const CHANNEL_FD: RawFd = 7;

/// The longest line the emitter may write, newline included.
const MAX_LINE_BYTES: usize = 512;

/// A temporary warehouse directory, with its global configuration and key directory kept
/// outside it, deleted when the test ends.
struct TestWarehouse {
    root: PathBuf,
    home: PathBuf,
}

impl TestWarehouse {
    fn new(name: &str) -> TestWarehouse {
        let root = std::env::temp_dir()
            .join(format!("forklift-events-{}-{}", name, std::process::id()));
        let home = std::env::temp_dir()
            .join(format!("forklift-events-{}-{}-home", name, std::process::id()));

        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        TestWarehouse { root, home }
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);

        command.args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .env_remove("PULT_EVENTS")
            .stdin(Stdio::null());

        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    /// Run with `fd` installed at [`CHANNEL_FD`] in the child and `PULT_EVENTS` naming it.
    /// The parent's copy of `fd` is closed once the child has been spawned.
    fn run_with_channel(&self, args: &[&str], fd: OwnedFd) -> Output {
        let mut command = self.command(args);
        command.env("PULT_EVENTS", CHANNEL_FD.to_string());

        let raw = fd.into_raw_fd();
        unsafe {
            command.pre_exec(move || {
                if raw == CHANNEL_FD {
                    // `dup2` onto itself is a no-op that would leave close-on-exec set.
                    if libc::fcntl(raw, libc::F_SETFD, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                } else if libc::dup2(raw, CHANNEL_FD) == -1 {
                    return Err(std::io::Error::last_os_error());
                }

                Ok(())
            });
        }

        let output = command.output();
        unsafe { libc::close(raw) };

        output.unwrap()
    }

    /// Prepare the warehouse with an operator and one stacked parcel, so there is something
    /// to compact.
    fn prepare_with_a_parcel(&self) {
        assert_success(&self.run(&["prepare"]));
        assert_success(&self.run(&["config", "--global", "operator.name", "Test Operator"]));
        assert_success(&self.run(&["config", "--global", "operator.identifier", "test@forklift"]));

        for index in 0..20 {
            self.write_file(&format!("src/file-{}.txt", index), &format!("content {}\n", index));
        }

        assert_success(&self.run(&["load", "."]));
        assert_success(&self.run(&["stack", "first parcel"]));
    }
}

impl Drop for TestWarehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed ({:?}).\nstdout: {}\nstderr: {}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

/// A pipe whose ends are both close-on-exec, so no other test's child inherits them and
/// holds the read end open past this test's child.
fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0 as RawFd; 2];

    #[cfg(target_os = "linux")]
    let created = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };

    #[cfg(target_os = "macos")]
    let created = unsafe {
        let created = libc::pipe(fds.as_mut_ptr());
        if created == 0 {
            libc::fcntl(fds[0], libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(fds[1], libc::F_SETFD, libc::FD_CLOEXEC);
        }
        created
    };

    assert_eq!(created, 0, "pipe: {}", std::io::Error::last_os_error());

    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

/// Run `args` with a fresh pipe as the channel and return the output and every line read.
fn run_and_collect(warehouse: &TestWarehouse, args: &[&str]) -> (Output, Vec<String>) {
    let (read_end, write_end) = pipe();

    // Read concurrently so a chatty run can never fill the pipe and stall on the reader.
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        std::fs::File::from(read_end).read_to_string(&mut text).unwrap();
        text
    });

    let output = warehouse.run_with_channel(args, write_end);
    let text = reader.join().unwrap();

    assert!(text.is_empty() || text.ends_with('\n'), "a partial line was written: {:?}", text);

    (output, text.lines().map(str::to_string).collect())
}

/// Every line is one well-formed `status` or `progress` event: never `step` (Forklift never
/// owns the channel), never a control character, never longer than the atomic-write bound.
fn assert_protocol(lines: &[String]) {
    for line in lines {
        assert!(line.len() < MAX_LINE_BYTES, "line over the cap: {:?}", line);
        assert!(!line.chars().any(char::is_control), "control character in {:?}", line);

        if let Some(rest) = line.strip_prefix("status ") {
            assert!(!rest.trim().is_empty(), "a status names its phase: {:?}", line);
        } else if let Some(rest) = line.strip_prefix("progress ") {
            let value = rest.split(' ').next().unwrap();
            assert!(value == "?" || value.parse::<u8>().is_ok_and(|percent| percent <= 100),
                    "not a progress value: {:?}", line);
        } else {
            panic!("not a status or progress event: {:?}", line);
        }
    }
}

/// Within each phase, numeric progress never goes down: a reset is only allowed after a status
/// naming the new phase.
fn assert_monotonic_within_phases(lines: &[String]) {
    let mut high = 0u8;

    for line in lines {
        if line.starts_with("status ") {
            high = 0;
        } else if let Some(Ok(percent)) = line.strip_prefix("progress ")
            .map(|rest| rest.split(' ').next().unwrap().parse::<u8>()) {
            assert!(percent >= high, "progress went from {} down to {} within a phase: {:?}", high, percent, lines);
            high = percent;
        }
    }
}

/// The statuses seen are `known`'s phases in order, and include every phase in `kept`.
///
/// A status still queued when the next status arrives is replaced by it, so a phase with no
/// progress before the next one may legitimately never reach the reader. `kept` names only the
/// phases that cannot be replaced that way: one followed by progress, or the run's last.
fn assert_phases(lines: &[String], known: &[&str], kept: &[&str]) {
    let seen: Vec<&str> = lines.iter().filter_map(|line| line.strip_prefix("status ")).collect();

    let mut remaining = known.iter();
    for phase in &seen {
        assert!(remaining.any(|known| known == phase),
                "status {:?} is unknown or out of order; expected a subsequence of {:?}, saw {:?}",
                phase, known, seen);
    }

    for phase in kept {
        assert!(seen.contains(phase), "missing status {:?} in {:?}", phase, lines);
    }
}

#[test]
fn compact_reports_its_phases_on_the_channel() {
    let warehouse = TestWarehouse::new("compact");
    warehouse.prepare_with_a_parcel();

    let (output, lines) = run_and_collect(&warehouse, &["--json", "compact"]);
    assert_success(&output);

    assert_protocol(&lines);
    assert_monotonic_within_phases(&lines);

    assert_phases(&lines,
                  &["Collecting objects", "Packing objects", "Removing packed originals"],
                  &["Packing objects", "Removing packed originals"]);

    // The channel is separate from stdout, which stays exactly one JSON document.
    let document: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON document ({}): {}",
                                       error, String::from_utf8_lossy(&output.stdout)));
    assert_eq!(document["ok"], true, "{}", document);
}

#[test]
fn import_git_reports_its_phases_on_the_channel() {
    let warehouse = TestWarehouse::new("import-git");

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&warehouse.root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };

    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("a.txt", "hello\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first commit"]);
    warehouse.write_file("b.txt", "more\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "second commit"]);

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Test Operator"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.identifier", "test@forklift"]));

    let (output, lines) = run_and_collect(&warehouse, &["import-git", "."]);
    assert_success(&output);

    assert_protocol(&lines);
    assert_monotonic_within_phases(&lines);

    assert_phases(&lines,
                  &["Converting commits", "Publishing objects", "Building the inventory"],
                  &["Converting commits"]);
}

#[test]
fn a_reader_that_has_gone_away_never_kills_the_operation() {
    let warehouse = TestWarehouse::new("gone");
    warehouse.prepare_with_a_parcel();

    // Close the read end before the child starts: every write it attempts hits a broken pipe.
    let (read_end, write_end) = pipe();
    drop(read_end);

    let output = warehouse.run_with_channel(&["--json", "compact"], write_end);

    assert_eq!(output.status.signal(), None, "the process was killed by a signal: {:?}", output.status);
    assert_success(&output);
    serde_json::from_slice::<serde_json::Value>(&output.stdout)
        .unwrap_or_else(|error| panic!("stdout is not one JSON document ({})", error));
}

#[test]
fn a_regular_file_is_never_written_as_a_channel() {
    let warehouse = TestWarehouse::new("regular-file");
    warehouse.prepare_with_a_parcel();

    let path = warehouse.home.join("not-a-channel");
    let file = std::fs::File::create(&path).unwrap();

    let output = warehouse.run_with_channel(&["compact"], OwnedFd::from(file));
    assert_success(&output);

    assert_eq!(std::fs::read(&path).unwrap(), b"", "the emitter wrote to a regular file");
}

#[test]
fn a_standard_stream_is_never_used_as_a_channel() {
    let warehouse = TestWarehouse::new("stdout");
    warehouse.prepare_with_a_parcel();

    for value in ["0", "1", "2"] {
        let output = warehouse.command(&["--json", "compact"])
            .env("PULT_EVENTS", value)
            .output()
            .unwrap();
        assert_success(&output);

        // A stray event on stdout would break the single document.
        let document: serde_json::Value = serde_json::from_slice(&output.stdout)
            .unwrap_or_else(|error| panic!("PULT_EVENTS={}: stdout is not one JSON document ({}): {}",
                                           value, error, String::from_utf8_lossy(&output.stdout)));
        assert_eq!(document["ok"], true);

        assert!(!String::from_utf8_lossy(&output.stderr).contains("status "),
                "PULT_EVENTS={}: an event reached stderr", value);
    }
}

#[test]
fn an_unusable_variable_leaves_the_command_unaffected() {
    let warehouse = TestWarehouse::new("unusable");
    warehouse.prepare_with_a_parcel();

    // Not digits, a sign, whitespace, and a number with nothing open behind it.
    for value in ["", "seven", "+7", " 7", "7 ", "-1", "99999999999999999999", "200"] {
        let output = warehouse.command(&["--json", "compact"])
            .env("PULT_EVENTS", value)
            .output()
            .unwrap();

        assert_success(&output);
        serde_json::from_slice::<serde_json::Value>(&output.stdout)
            .unwrap_or_else(|error| panic!("PULT_EVENTS={:?}: stdout is not one JSON document ({})", value, error));
    }
}
