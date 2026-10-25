//! Runs the guide pages that teach by example, [`RUN_PAGES`]: every command in a page's `console`
//! blocks, in order, against a scratch folder of its own, checking each command's exit status and
//! everything it prints. A renamed flag, a reworded message or a reordered step a page still shows
//! fails this test, so the pages cannot drift from the CLI.
//!
//! A page is read, not mirrored: there is no second copy of the commands to keep in step. The
//! commands run through a small interpreter here, not through a shell, so they run the same on
//! every platform CI covers. Only the commands it knows are allowed, and only words a
//! shell would pass through unchanged; anything else is a failure that names the command, never a
//! skipped step or a step that runs differently from the reader's.

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

mod common;
use common::{fence, forklift_command, repository_root, server_binary, tracked_pages};

/// The pages under `docs/guide/` this test runs, each from an empty folder.
const RUN_PAGES: &[&str] = &["tutorial.md", "concepts.md"];

/// The address the pages tell the reader to serve on. The test serves on a port the server picks
/// itself, so a busy 7171 on a CI machine cannot fail it, and rewrites this address wherever a
/// page shows it.
const PAGE_ADDRESS: &str = "127.0.0.1:7171";

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

/// Read the page's steps. Every fenced block must be a `console` block fenced with backticks at
/// the start of the line, and must open with a `$` line; no `$` line may sit outside one, however
/// it is indented, and there may be no indented code block. Otherwise a command the page shows,
/// in a block of another kind or one nested in a list item, would be skipped without a word.
fn parse_steps(page: &str) -> Result<Vec<Step>, String> {
    let mut steps: Vec<Step> = Vec::new();
    let mut in_block = false;
    // Whether an indented line here would start a code block: after a blank line, a heading or
    // a fence, where it cannot continue a paragraph.
    let mut code_may_start = true;

    for (index, line) in page.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        let starts_code = std::mem::replace(
            &mut code_may_start,
            trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with("```"),
        );

        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            if trimmed.len() != line.len() || trimmed.starts_with("~~~") {
                return Err(format!(
                    "line {}: fence code blocks with ``` at the start of the line, so this test \
                    sees them", number
                ));
            }

            let info = &line[3..];
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
                    "line {}: a \"{}\" block; every block on the page is a console block, \
                    which this test runs", number, info
                ));
            }
            continue;
        }

        if !in_block {
            if trimmed.starts_with("$ ") {
                return Err(format!("line {}: a command outside a console block", number));
            }
            if starts_code && !trimmed.is_empty() && (line.starts_with("    ") || line.starts_with('\t')) {
                return Err(format!(
                    "line {}: an indented code block; show commands in a console block, which \
                    this test runs", number
                ));
            }
            continue;
        }

        if let Some(command) = line.strip_prefix("$ ") {
            if command.trim().is_empty() {
                return Err(format!("line {}: a `$` with no command", number));
            }
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

/// Why a reader's shell would not run `command` the way this test does, if it would not. The
/// test splits words as a POSIX shell does and runs them literally, so anything a shell expands
/// or interprets is refused rather than run differently: globs, `~`, variables, command
/// substitution, `!` history expansion (interactive zsh and bash expand it inside double quotes
/// too), command lists and pipes, and redirection other than one trailing `>` or `>>`.
fn shell_divergence(command: &str) -> Option<String> {
    #[derive(PartialEq)]
    enum Quote { None, Single, Double }

    let mut quote = Quote::None;
    let mut redirects = 0;
    let mut previous = ' ';

    for c in command.chars() {
        match quote {
            Quote::Single => {
                if c == '\'' {
                    quote = Quote::None;
                }
            }
            Quote::Double => match c {
                '"' => quote = Quote::None,
                '$' | '`' | '\\' | '!' => return Some(format!("{:?} inside double quotes", c)),
                _ => {}
            },
            Quote::None => match c {
                '\'' => quote = Quote::Single,
                '"' => quote = Quote::Double,
                '>' if previous == ' ' || previous == '>' => redirects += 1,
                // zsh expands a word starting with `=` to a command's path.
                '=' if previous == ' ' => return Some("a word starting with an unquoted '='".to_string()),
                '*' | '?' | '[' | ']' | '~' | '$' | '`' | '\\' | '!' | '&' | '|' | ';' | '<' | '>'
                | '(' | ')' | '{' | '}' | '#' => return Some(format!("an unquoted {:?}", c)),
                _ => {}
            },
        }
        previous = c;
    }

    if quote != Quote::None {
        return Some("an unclosed quote".to_string());
    }

    let words = shlex::split(command)?;
    if words.is_empty() {
        return Some("no command".to_string());
    }
    let redirect = words.len() >= 2 && matches!(words[words.len() - 2].as_str(), ">" | ">>");
    let expected = if !redirect { 0 } else if words[words.len() - 2] == ">>" { 2 } else { 1 };
    if redirects != expected {
        return Some("redirection other than one trailing > or >>".to_string());
    }

    if words[0] == "echo" && words.get(1).is_some_and(|word| word.starts_with('-')) {
        return Some("echo with an option, which shells read differently".to_string());
    }

    // zsh's and dash's echo expand escapes like `\n` even inside single quotes; bash's does not.
    if words[0] == "echo" && command.contains('\\') {
        return Some("echo with a backslash, which shells read differently".to_string());
    }

    None
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
/// trailing spaces dropped. Only complete colour sequences (`ESC [`, digits and `;`, then `m`)
/// are removed; any other escape is kept, so it shows up as a difference instead of hiding text.
fn printed_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");

    let mut plain = String::with_capacity(text.len());
    let mut rest = text.as_str();
    while let Some(at) = rest.find("\x1b[") {
        plain.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let parameters = after.find(|c: char| !c.is_ascii_digit() && c != ';').unwrap_or(after.len());

        if after[parameters..].starts_with('m') {
            rest = &after[parameters + 1..];
        } else {
            plain.push_str("\x1b[");
            rest = after;
        }
    }
    plain.push_str(rest);

    let mut lines: Vec<String> = plain.lines().map(|line| line.trim_end().to_string()).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// A server the page started, stopped when the run ends.
struct Server {
    line: usize,
    child: Child,
    _stdout: BufReader<ChildStdout>,
    stderr: PathBuf,
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
    /// The page being run, as failure messages name it.
    page: String,
    area: PathBuf,
    cwd: PathBuf,
    /// Where the page's server really listens, once it has started.
    address: Option<String>,
    servers: Vec<Server>,
}

impl Reader {
    /// The reader's environment: none of the developer's forklift settings, debug switches or log
    /// filters, which would change what a command prints.
    fn environment(&self, command: &mut Command) {
        for (key, _) in std::env::vars_os() {
            let key_text = key.to_string_lossy();
            if key_text.starts_with("FORKLIFT_") || key_text.starts_with("RUST_LOG") {
                command.env_remove(&key);
            }
        }

        command
            .current_dir(&self.cwd)
            .env("FORKLIFT_FSYNC", "0")
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
        let page = self.page.clone();
        let fail = |what: &dyn std::fmt::Display| -> ! {
            panic!("{} line {}: `{}`: {}", page, step.line, step.command, what)
        };

        if let Some(reason) = shell_divergence(&step.command) {
            fail(&format!(
                "a shell would not run this as this test does ({}); rephrase the step", reason
            ));
        }

        let mut words = shlex::split(&step.command).unwrap_or_else(|| fail(&"cannot split it"));

        let redirect = match words.len().checked_sub(2).map(|at| words[at].as_str()) {
            Some(">") | Some(">>") => {
                let path = words.pop().unwrap();
                let append = words.pop().unwrap() == ">>";
                Some((path, append))
            }
            _ => None,
        };

        let unsupported = || -> ! {
            fail(&"this test cannot run it; use a command it knows (forklift, forklift-server, \
                cd, mkdir, cat, echo > file) or teach it")
        };

        match (words[0].as_str(), redirect) {
            ("echo", Some((path, append))) => {
                let text = words[1..].join(" ");
                let mut file = std::fs::OpenOptions::new()
                    .create(true).write(true).append(append).truncate(!append)
                    .open(self.cwd.join(path))
                    .unwrap_or_else(|e| fail(&e));
                std::io::Write::write_all(&mut file, format!("{}\n", text).as_bytes())
                    .unwrap_or_else(|e| fail(&e));
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
                        _ => fail(&"cd takes a relative path"),
                    }
                }
                if !target.is_dir() {
                    fail(&format!("no folder {}", target.display()));
                }
                self.cwd = target;
                (Some(0), Vec::new())
            }
            ("mkdir", None) if words.len() == 2 => {
                std::fs::create_dir(self.cwd.join(&words[1])).unwrap_or_else(|e| fail(&e));
                (Some(0), Vec::new())
            }
            ("cat", None) if words.len() == 2 => {
                (Some(0), std::fs::read(self.cwd.join(&words[1])).unwrap_or_else(|e| fail(&e)))
            }
            ("forklift", None) => {
                let mut command = forklift_command();
                command.args(&words[1..]);
                self.capture(command)
            }
            ("forklift-server", None) if words.get(1).map(String::as_str) == Some("serve") => {
                // The page leaves the server running in a second terminal; here it runs in the
                // background on a port it picks itself, and what the step shows is its startup
                // line. What it prints on stderr is checked when the run ends.
                let mut command = Command::new(server_binary());
                command.args(words[1..].iter().map(|word| word.replace(PAGE_ADDRESS, "127.0.0.1:0")));
                self.environment(&mut command);

                let stderr = self.area.join(format!("server-{}.stderr", step.line));
                let stderr_file = std::fs::File::create(&stderr).unwrap();
                let mut child = command.stdout(Stdio::piped()).stderr(stderr_file).spawn().unwrap();

                let mut stdout = BufReader::new(child.stdout.take().unwrap());
                let mut line = String::new();
                stdout.read_line(&mut line).unwrap_or_else(|e| fail(&e));

                let bound = line.trim().rsplit(' ').next().unwrap_or("");
                if let Some(address) = bound.strip_prefix("http://") {
                    self.address = Some(address.to_string());
                }

                self.servers.push(Server { line: step.line, child, _stdout: stdout, stderr });
                (Some(0), line.into_bytes())
            }
            ("forklift-server", None) => {
                let mut command = Command::new(server_binary());
                command.args(&words[1..]);
                self.capture(command)
            }
            _ => unsupported(),
        }
    }

    /// Each server that has exited or printed on stderr, with what it printed; empty if none has.
    fn server_trouble(&mut self) -> String {
        let mut trouble = String::new();
        for server in &mut self.servers {
            let exited = server.child.try_wait().unwrap();
            let stderr = std::fs::read_to_string(&server.stderr).unwrap();
            if exited.is_some() || !stderr.is_empty() {
                trouble.push_str(&format!(
                    "\n\nThe server started at line {} exited ({:?}) or printed on stderr:\n{}",
                    server.line, exited, stderr
                ));
            }
        }
        trouble
    }

    /// The page's text for this machine: its server address replaced by the real one.
    fn localize(&self, text: &str) -> String {
        match &self.address {
            Some(address) => text.replace(PAGE_ADDRESS, address),
            None => text.to_string(),
        }
    }
}

fn guide_path(name: &str) -> PathBuf {
    repository_root().join("docs/guide").join(name)
}

/// Run one page of [`RUN_PAGES`] from an empty folder, failing on the first step that fails or
/// prints something other than what the page shows.
fn run_page(name: &str) {
    let label = format!("docs/guide/{}", name);
    let page = std::fs::read_to_string(guide_path(name)).unwrap();
    let steps = parse_steps(&page).unwrap_or_else(|error| panic!("{}, {}", label, error));

    let stem = name.trim_end_matches(".md");
    let area = std::env::temp_dir().join(format!("forklift-{}-{}", stem, std::process::id()));
    let _ = std::fs::remove_dir_all(&area);
    std::fs::create_dir_all(&area).unwrap();

    let mut reader = Reader {
        page: label.clone(),
        area: area.clone(),
        cwd: area.join("reader"),
        address: None,
        servers: Vec::new(),
    };
    std::fs::create_dir(&reader.cwd).unwrap();

    for step in &steps {
        let step = Step {
            line: step.line,
            command: reader.localize(&step.command),
            expected: step.expected.clone(),
        };

        let (status, output) = reader.run(&step);
        let printed = printed_lines(&output);
        let expected: Vec<String> = step.expected.iter().map(|line| reader.localize(line)).collect();

        assert!(
            status == Some(0) && output_matches(&expected, &printed),
            "{} line {}: `{}` exited {:?} and printed\n{}\n\nThe page shows\n{}{}",
            label, step.line, step.command, status, printed.join("\n"), expected.join("\n"),
            reader.server_trouble()
        );
    }

    // A server's second terminal shows its stderr too, and the page shows none. A server that
    // died mid-run is reported with the step that failed because of it.
    let trouble = reader.server_trouble();
    assert!(trouble.is_empty(), "{}:{}", label, trouble);

    drop(reader);
    let _ = std::fs::remove_dir_all(&area);
}

#[test]
fn tutorial_runs_as_written() {
    run_page("tutorial.md");
}

#[test]
fn concepts_run_as_written() {
    run_page("concepts.md");
}

/// A page that shows commands in a `console` block promises they run as shown, so a page with one
/// is either run here or a failure: adding examples to a page cannot leave them unrun. Every page
/// in the repository is read, and a fence counts in any form `parse_steps` would refuse as well as
/// the one it runs: either fence character, any case, indented, in a list item or a quote.
#[test]
fn every_page_with_console_blocks_is_run() {
    for path in tracked_pages() {
        let page = std::fs::read_to_string(&path).unwrap();
        let has_console = page.lines().any(|line| {
            fence(line.trim_start_matches(|c: char| c == '>' || c.is_whitespace())).is_some_and(|(_, _, info)| info.split_whitespace().next().is_some_and(|word| word.eq_ignore_ascii_case("console")))
        });
        if !has_console {
            continue;
        }

        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            path == guide_path(&name) && RUN_PAGES.contains(&name.as_str()),
            "{} has console blocks; move it to docs/guide and add it to RUN_PAGES",
            path.display()
        );
    }

    for name in RUN_PAGES {
        assert!(guide_path(name).is_file(), "RUN_PAGES names docs/guide/{}, which does not exist", name);
    }
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
    assert_eq!(printed_lines(b"\x1b[1;32mok\x1b[m"), vec!["ok"]);

    // Anything but a colour sequence stays, so it cannot swallow the text after it.
    assert_eq!(printed_lines(b"a\x1b[12\nb"), vec!["a\x1b[12", "b"]);
    assert_eq!(printed_lines(b"\x1b]8;;x\x07link"), vec!["\x1b]8;;x\x07link"]);
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

    // A block nested in a list item, a tilde fence, and an indented code block.
    assert!(refused("1. Prepare:\n   ```console\n   $ forklift prepare\n   ```\n"));
    assert!(refused("~~~console\n$ forklift prepare\n~~~\n"));
    assert!(refused("text\n\n    $ forklift prepare\n"));
    assert!(refused("text\n\n    forklift prepare\n"));
    assert!(refused("text\n\n\tforklift prepare\n"));
    assert!(refused("## Heading\n    forklift prepare\n"));
    assert!(refused("```console\n$ a\n```\n    forklift prepare\n"));
    assert!(refused("```console\n$   \n```\n"));
    assert!(parse_steps("- item\n    continued\n").is_ok());

    let steps = parse_steps("text\n```console\n$ a\nout\n$ b\n```\n\n```console\n$ c\n```\n").unwrap();
    let commands: Vec<&str> = steps.iter().map(|step| step.command.as_str()).collect();
    assert_eq!(commands, ["a", "b", "c"]);
    assert_eq!(steps[0].expected, ["out"]);
    assert!(steps[1].expected.is_empty());
}

#[test]
fn only_commands_a_shell_runs_unchanged_are_run() {
    let runs = |command: &str| shell_divergence(command).is_none();

    assert!(runs("forklift stack \"First parcel\""));
    assert!(runs("echo 'Hello, world!' > readme.md"));
    assert!(runs("echo \"text\" >> notes.md"));
    assert!(runs("echo '^build(/.*)?$' > .forkliftignore"));
    assert!(runs("cd ../hello"));

    assert!(!runs("echo \"Hello, world!\" > readme.md"));
    assert!(!runs("forklift load *.md"));
    assert!(!runs("cd ~"));
    assert!(!runs("echo $HOME > f"));
    assert!(!runs("echo \"$HOME\" > f"));
    assert!(!runs("forklift load a && forklift stack m"));
    assert!(!runs("forklift history | head"));
    assert!(!runs("forklift load a; forklift stack m"));
    assert!(!runs("echo -n x > f"));
    assert!(!runs("echo a>f"));
    assert!(!runs("echo a > f > g"));
    assert!(!runs("forklift stack \"unclosed"));
    assert!(!runs("forklift stack m # a comment"));
    assert!(!runs("echo 'a\\nb' > f"));
    assert!(!runs("forklift config operator.name =ada"));
    assert!(runs("forklift config remote.url --opt=a=b"));
    assert!(!runs("  "));
}
