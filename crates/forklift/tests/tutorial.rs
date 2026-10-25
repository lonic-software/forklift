//! Runs `docs/guide/tutorial.md`: every command in its `console` blocks, in order, against a
//! scratch folder, checking each command's exit status and everything it prints. A renamed flag,
//! a reworded message or a reordered step the page still shows fails this test, so the tutorial
//! cannot drift from the CLI.
//!
//! The page is read, not mirrored: there is no second copy of the commands to keep in step. The
//! tutorial runs commands through a small interpreter here, not through a shell, so it runs the
//! same on every platform CI covers. Only the commands it knows are allowed; anything else is a
//! failure that names the command, never a skipped step.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

mod common;
use common::forklift_command;

/// The address the page tells the reader to serve on. The test serves on a free port instead,
/// so a busy 7171 on a CI machine cannot fail it, and rewrites this address wherever the page
/// shows it.
const TUTORIAL_ADDRESS: &str = "127.0.0.1:7171";

/// What the page writes for text that differs from run to run (hashes, ids, dates).
const ELISION: char = '…';

/// One `$` line of the page and the output shown under it.
#[derive(Debug)]
struct Step {
    /// 1-based line number of the `$` line in the page, for failure messages.
    line: usize,
    command: String,
    expected: Vec<String>,
}

/// Read the page's steps. Every fenced block must be a `console` block and must open with a `$`
/// line, and no `$` line may sit outside one, so that a command the page shows is a command
/// this test runs: a block in another language would otherwise be skipped without a word.
fn parse_steps(page: &str) -> Result<Vec<Step>, String> {
    let mut steps: Vec<Step> = Vec::new();
    let mut in_block = false;

    for (index, line) in page.lines().enumerate() {
        let number = index + 1;

        if let Some(info) = line.strip_prefix("```") {
            if in_block {
                if !info.is_empty() {
                    return Err(format!("line {}: a block closes with a bare ```", number));
                }
                in_block = false;
            } else if info == "console" {
                in_block = true;
                steps.push(Step { line: number, command: String::new(), expected: Vec::new() });
            } else {
                return Err(format!(
                    "line {}: a \"{}\" block; every block in the tutorial is a console block, \
                    which this test runs", number, info
                ));
            }
            continue;
        }

        if !in_block {
            if line.starts_with("$ ") {
                return Err(format!("line {}: a command outside a console block", number));
            }
            continue;
        }

        if let Some(command) = line.strip_prefix("$ ") {
            let opening = steps.last_mut().filter(|step| step.command.is_empty());
            match opening {
                Some(step) => {
                    step.line = number;
                    step.command = command.to_string();
                }
                None => steps.push(Step { line: number, command: command.to_string(), expected: Vec::new() }),
            }
        } else {
            match steps.last_mut() {
                Some(step) if !step.command.is_empty() => step.expected.push(line.to_string()),
                _ => return Err(format!("line {}: a console block must open with a command", number)),
            }
        }
    }

    if in_block {
        return Err("the last console block is never closed".to_string());
    }

    if let Some(step) = steps.iter().find(|step| step.command.is_empty()) {
        return Err(format!("line {}: an empty console block", step.line));
    }

    Ok(steps)
}

/// Whether `actual` is `pattern` with each [`ELISION`] standing for any text, including none.
fn line_matches(pattern: &str, actual: &str) -> bool {
    let mut pieces = pattern.split(ELISION);
    let first = pieces.next().unwrap_or("");
    let rest: Vec<&str> = pieces.collect();

    let Some((last, middle)) = rest.split_last() else {
        return pattern == actual;
    };

    let Some(mut remaining) = actual.strip_prefix(first) else {
        return false;
    };

    for piece in middle {
        match remaining.find(piece) {
            Some(at) => remaining = &remaining[at + piece.len()..],
            None => return false,
        }
    }

    remaining.ends_with(last)
}

/// Whether the printed lines match the page's lines one for one.
fn output_matches(expected: &[String], actual: &[String]) -> bool {
    expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(pattern, line)| line_matches(pattern, line))
}

/// What a reader sees: colour codes removed, line endings normalised, trailing blank lines and
/// trailing spaces dropped.
fn printed_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");

    let mut plain = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
            continue;
        }
        plain.push(c);
    }

    let mut lines: Vec<String> = plain.lines().map(|line| line.trim_end().to_string()).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// `forklift-server`, found next to the CLI as the remote tests find it.
fn server_binary() -> PathBuf {
    let path = Path::new(env!("CARGO_BIN_EXE_forklift"))
        .parent()
        .expect("the forklift binary has a parent directory")
        .join(format!("forklift-server{}", std::env::consts::EXE_SUFFIX));

    assert!(
        path.is_file(),
        "forklift-server is not built at {}; run the tests via a workspace build",
        path.display()
    );

    path
}

/// A server the page started, stopped when the run ends.
struct Server {
    child: Child,
    _stdout: BufReader<ChildStdout>,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The reader's machine: a scratch folder, their current folder inside it, and a global
/// configuration and key folder of their own, so the run never touches the developer's.
struct Reader {
    area: PathBuf,
    cwd: PathBuf,
    servers: Vec<Server>,
}

impl Reader {
    fn environment(&self, command: &mut Command) {
        command
            .current_dir(&self.cwd)
            .env("FORKLIFT_GLOBAL_CONFIG", self.area.join("global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.area.join("keys"))
            .stdin(Stdio::null());
    }

    /// Run a program with stdout and stderr on one file, as a terminal would interleave them,
    /// and return its exit status and what it printed.
    fn capture(&self, mut command: Command) -> (Option<i32>, Vec<u8>) {
        let path = self.area.join("output");
        let file = std::fs::File::create(&path).unwrap();
        self.environment(&mut command);
        command.stdout(file.try_clone().unwrap()).stderr(file);

        let status = command.status().unwrap();
        (status.code(), std::fs::read(&path).unwrap())
    }

    /// Run one step and return its exit status and what it printed.
    fn run(&mut self, step: &Step) -> (Option<i32>, Vec<u8>) {
        let mut words = shlex::split(&step.command)
            .unwrap_or_else(|| panic!("line {}: cannot split {:?}", step.line, step.command));

        let redirect = match words.len().checked_sub(2).map(|at| words[at].as_str()) {
            Some(">") | Some(">>") => {
                let path = words.pop().unwrap();
                let append = words.pop().unwrap() == ">>";
                Some((path, append))
            }
            _ => None,
        };

        let unsupported = || -> ! {
            panic!(
                "line {}: the tutorial runs {:?}, which this test cannot run; use a command \
                it knows (forklift, forklift-server, cd, mkdir, cat, echo > file) or teach it",
                step.line, step.command
            )
        };

        match (words[0].as_str(), redirect) {
            ("echo", Some((path, append))) => {
                let text = words[1..].join(" ");
                // A shell's echo treats a backslash differently from one shell to the next.
                assert!(!text.contains('\\'), "line {}: echo with a backslash", step.line);

                let mut file = std::fs::OpenOptions::new()
                    .create(true).write(true).append(append).truncate(!append)
                    .open(self.cwd.join(path)).unwrap();
                std::io::Write::write_all(&mut file, format!("{}\n", text).as_bytes()).unwrap();
                (Some(0), Vec::new())
            }
            (_, Some(_)) => unsupported(),
            ("cd", None) if words.len() == 2 => {
                // Resolved by name, as a shell does, rather than canonicalised: Windows would
                // hand back a verbatim path the page's commands never see.
                let mut target = self.cwd.clone();
                for part in Path::new(&words[1]).components() {
                    match part {
                        std::path::Component::ParentDir => assert!(target.pop()),
                        std::path::Component::Normal(name) => target.push(name),
                        _ => panic!("line {}: cd takes a relative path", step.line),
                    }
                }
                assert!(target.is_dir(), "line {}: no folder {}", step.line, target.display());
                self.cwd = target;
                (Some(0), Vec::new())
            }
            ("mkdir", None) if words.len() == 2 => {
                std::fs::create_dir(self.cwd.join(&words[1])).unwrap();
                (Some(0), Vec::new())
            }
            ("cat", None) if words.len() == 2 => {
                (Some(0), std::fs::read(self.cwd.join(&words[1])).unwrap())
            }
            ("forklift", None) => {
                let mut command = forklift_command();
                command.args(&words[1..]);
                self.capture(command)
            }
            ("forklift-server", None) if words.get(1).map(String::as_str) == Some("serve") => {
                // The page leaves the server running in a second terminal; here it runs in the
                // background, and what the step shows is its startup line.
                let mut command = Command::new(server_binary());
                command.args(&words[1..]).env("FORKLIFT_FSYNC", "0");
                self.environment(&mut command);
                let mut child = command.stdout(Stdio::piped()).stderr(Stdio::inherit()).spawn().unwrap();

                let mut stdout = BufReader::new(child.stdout.take().unwrap());
                let mut line = String::new();
                stdout.read_line(&mut line).unwrap();

                self.servers.push(Server { child, _stdout: stdout });
                (Some(0), line.into_bytes())
            }
            ("forklift-server", None) => {
                let mut command = Command::new(server_binary());
                command.args(&words[1..]).env("FORKLIFT_FSYNC", "0");
                self.capture(command)
            }
            _ => unsupported(),
        }
    }
}

/// A free local port for the tutorial's server.
fn free_address() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

fn tutorial_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide/tutorial.md")
}

#[test]
fn tutorial_runs_as_written() {
    let page = std::fs::read_to_string(tutorial_path()).unwrap();
    let steps = parse_steps(&page).unwrap_or_else(|error| panic!("docs/guide/tutorial.md, {}", error));

    let area = std::env::temp_dir().join(format!("forklift-tutorial-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&area);
    std::fs::create_dir_all(&area).unwrap();

    let address = free_address();
    let mut reader = Reader { area: area.clone(), cwd: area.join("reader"), servers: Vec::new() };
    std::fs::create_dir(&reader.cwd).unwrap();

    for step in &steps {
        let step = Step {
            line: step.line,
            command: step.command.replace(TUTORIAL_ADDRESS, &address),
            expected: step.expected.iter().map(|line| line.replace(TUTORIAL_ADDRESS, &address)).collect(),
        };

        let (status, output) = reader.run(&step);
        let printed = printed_lines(&output);

        assert!(
            status == Some(0) && output_matches(&step.expected, &printed),
            "docs/guide/tutorial.md line {}: `{}` exited {:?} and printed\n{}\n\nThe page shows\n{}",
            step.line, step.command, status, printed.join("\n"), step.expected.join("\n")
        );
    }

    drop(reader);
    let _ = std::fs::remove_dir_all(&area);
}

#[test]
fn elision_stands_for_any_text_within_one_line() {
    assert!(line_matches("Stacked parcel … on pallet \"main\".", "Stacked parcel 8faf on pallet \"main\"."));
    assert!(line_matches("…", ""));
    assert!(line_matches("a … b … c", "a 1 b 2 c"));
    assert!(line_matches("exact", "exact"));

    assert!(!line_matches("Stacked parcel … on pallet \"main\".", "Stacked parcel 8faf on pallet \"shout\"."));
    assert!(!line_matches("exact", "exact "));
    assert!(!line_matches("a…a", "a"));
    assert!(!line_matches("a … b … c", "a 1 c 2 b"));
}

#[test]
fn output_must_match_line_for_line() {
    let lines = |text: &str| text.lines().map(str::to_string).collect::<Vec<_>>();

    assert!(output_matches(&lines("…\nb"), &lines("a\nb")));
    assert!(output_matches(&[], &[]));

    assert!(!output_matches(&[], &lines("unexpected")));
    assert!(!output_matches(&lines("…"), &lines("a\nb")));
    assert!(!output_matches(&lines("a\nb"), &lines("a")));
}

#[test]
fn printed_lines_are_what_a_reader_sees() {
    assert_eq!(printed_lines(b"\x1b[33mparcel 1\x1b[0m  \r\nnext\n\n"), vec!["parcel 1", "next"]);
}

#[test]
fn every_command_on_the_page_is_one_the_test_runs() {
    let refused = |page: &str| parse_steps(page).is_err();

    assert!(refused("```sh\nforklift prepare\n```\n"));
    assert!(refused("```\nforklift prepare\n```\n"));
    assert!(refused("$ forklift prepare\n"));
    assert!(refused("```console\nPrepared warehouse.\n```\n"));
    assert!(refused("```console\n```\n"));
    assert!(refused("```console\n$ forklift prepare\n"));

    let steps = parse_steps("text\n```console\n$ a\nout\n$ b\n```\n\n```console\n$ c\n```\n").unwrap();
    let commands: Vec<&str> = steps.iter().map(|step| step.command.as_str()).collect();
    assert_eq!(commands, ["a", "b", "c"]);
    assert_eq!(steps[0].expected, ["out"]);
    assert!(steps[1].expected.is_empty());
}
