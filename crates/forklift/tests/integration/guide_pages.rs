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
//!
//! A page that follows several people marks whose machine each block runs on with a line
//! `<!-- machine: NAME -->` between blocks, which a rendered page does not show; each machine
//! has a folder, a global configuration and keys of its own. A value one command prints and a later
//! one reuses, such as a key, is written `<name>`: in the output a page shows, it stands for one
//! word and binds it, and in a later command it is replaced by what it bound.
//!
//! A server the page starts runs in the background, as the page's second terminal. The first
//! line under its `$` line is the line it starts with; any lines after it are everything that
//! terminal prints later, while the rest of the page runs, checked when the run ends. A server
//! that prints anything else, or exits, fails the page.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::common;
use common::{fence, forklift_command, repository_root, server_binary, tracked_pages};

/// The pages under `docs/guide/` this test runs, each from an empty folder.
const RUN_PAGES: &[&str] =
    &["tutorial.md", "concepts.md", "team.md", "cache.md", "leaving.md", "leaked-secret.md"];

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
    /// The machine the step runs on, from the last `<!-- machine: NAME -->` line above it; `None`
    /// on a page that has none.
    machine: Option<String>,
    command: String,
    expected: Vec<String>,
}

/// Whether `text`, the inside of an HTML comment, reads as a machine marker: after any hyphens
/// and spaces, it starts with `machine` in any case, not as part of a hyphenated word such as
/// `machine-readable`. A near miss such as `machines: bob` reads as one, to be refused.
fn names_a_machine(text: &str) -> bool {
    let text = text.trim_start_matches(|c: char| c == '-' || c.is_whitespace());
    text.get(..7).is_some_and(|word| word.eq_ignore_ascii_case("machine"))
        && !text[7..].starts_with('-')
}

/// The machine a `<!-- machine: NAME -->` line names, `None` for any other line, and an error
/// for a line that reads like one but is not exactly one, so a mistyped marker cannot leave its
/// blocks running on the previous machine.
fn machine_marker(line: &str) -> Result<Option<String>, String> {
    let trimmed = line.trim();
    if !trimmed.strip_prefix("<!--").is_some_and(names_a_machine) {
        return Ok(None);
    }
    let name = trimmed
        .strip_prefix("<!-- machine: ")
        .and_then(|rest| rest.strip_suffix(" -->"))
        .filter(|name| is_name(name) && line == trimmed);
    match name {
        Some(name) => Ok(Some(name.to_string())),
        None => Err("write a machine marker as `<!-- machine: NAME -->`, alone on its line, with a \
            lowercase name".to_string()),
    }
}

/// Whether `text` is a machine or placeholder name: a lowercase letter, then lowercase letters,
/// digits and hyphens.
fn is_name(text: &str) -> bool {
    text.starts_with(|c: char| c.is_ascii_lowercase())
        && text.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Read the page's steps. Every fenced block must be a `console` block fenced with backticks at
/// the start of the line, and must open with a `$` line; no `$` line may sit outside one, however
/// it is indented, and there may be no indented code block. Otherwise a command the page shows,
/// in a block of another kind or one nested in a list item, would be skipped without a word.
/// On a page with machine markers, the first comes before the first block, so no block runs on a
/// machine the page never named.
fn parse_steps(page: &str) -> Result<Vec<Step>, String> {
    let mut steps: Vec<Step> = Vec::new();
    let mut machine: Option<String> = None;
    let mut in_block = false;
    // Whether the line is inside an HTML comment that opened on an earlier line.
    let mut in_comment = false;
    // Whether an indented line here would start a code block: after a blank line, a heading, a
    // fence or a one-line comment, where it cannot continue a paragraph.
    let mut code_may_start = true;

    for (index, line) in page.lines().enumerate() {
        let number = index + 1;
        let trimmed = line.trim_start();
        // Indented four spaces or more, a `<!--` is the text of a code block, not a comment.
        let opens_comment = trimmed.starts_with("<!--") && line.len() - trimmed.len() < 4;
        let one_line_comment = opens_comment && trimmed.trim_end().ends_with("-->");
        let starts_code = std::mem::replace(
            &mut code_may_start,
            trimmed.is_empty()
                || trimmed.starts_with('#')
                || trimmed.starts_with("```")
                || one_line_comment,
        );

        if in_comment || (!in_block && opens_comment && !one_line_comment) {
            if names_a_machine(trimmed.trim_start_matches("<!--")) {
                return Err(format!(
                    "line {}: a machine marker split over lines; write it as \
                    `<!-- machine: NAME -->` on one line", number
                ));
            }
            // Open after this line if a comment opens after the last one closes.
            in_comment = match (line.rfind("<!--"), line.rfind("-->")) {
                (Some(open), Some(close)) => open > close,
                (Some(_), None) => true,
                (None, Some(_)) => false,
                (None, None) => in_comment,
            };
            code_may_start = !in_comment;
            continue;
        }

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
                steps.push(Step {
                    line: number,
                    machine: machine.clone(),
                    command: String::new(),
                    expected: Vec::new(),
                });
            } else {
                return Err(format!(
                    "line {}: a \"{}\" block; every block on the page is a console block, \
                    which this test runs", number, info
                ));
            }
            continue;
        }

        if !in_block {
            let marker = machine_marker(line).map_err(|e| format!("line {}: {}", number, e))?;
            if let Some(name) = marker {
                if machine.is_none() && !steps.is_empty() {
                    return Err(format!(
                        "line {}: a machine marker after the first block; name the first \
                        block's machine too", number
                    ));
                }
                machine = Some(name);
                continue;
            }
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
                None => steps.push(Step {
                    line: number,
                    machine: machine.clone(),
                    command: command.to_string(),
                    expected: Vec::new(),
                }),
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
    if in_comment {
        return Err("a comment is never closed, which hides the rest of the page".to_string());
    }

    if let Some(step) = steps.iter().find(|step| step.command.is_empty()) {
        return Err(format!("line {}: an empty console block", step.line));
    }

    // A placeholder is there to carry a value to a later use, so one used once is a typo, which
    // would otherwise bind any word and check nothing.
    let mut uses: HashMap<&str, (usize, usize)> = HashMap::new();
    for step in &steps {
        // A step's output lines are the lines straight after its `$` line.
        let lines = std::iter::once(&step.command).chain(&step.expected).zip(step.line..);
        for (text, line) in lines {
            let mut rest = text.as_str();
            while let Some((_, end, name)) = next_placeholder(rest) {
                uses.entry(name).or_insert((line, 0)).1 += 1;
                rest = &rest[end..];
            }
        }
    }
    let once = uses.iter().filter(|(_, (_, count))| *count == 1);
    if let Some((name, (line, _))) = once.min_by_key(|(_, (line, _))| *line) {
        return Err(format!(
            "line {}: <{}> is used only once; a placeholder carries a value to a later use",
            line, name
        ));
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

/// One piece of a line of output the page shows.
#[derive(Debug)]
enum Piece<'a> {
    Text(&'a str),
    /// [`ELISION`]: any text, including none.
    Elided,
    /// `<name>`: one word, the same wherever the name appears.
    Placeholder(&'a str),
}

/// Where the next `<name>` in `text` is, with the name; a `<` not followed by a name and `>` is
/// plain text.
fn next_placeholder(text: &str) -> Option<(usize, usize, &str)> {
    let mut from = 0;
    while let Some(start) = text[from..].find('<').map(|at| from + at) {
        if let Some(length) = text[start + 1..].find('>') {
            let name = &text[start + 1..start + 1 + length];
            if is_name(name) {
                return Some((start, start + length + 2, name));
            }
        }
        from = start + 1;
    }
    None
}

fn pieces(pattern: &str) -> Vec<Piece<'_>> {
    let mut pieces = Vec::new();
    for (index, text) in pattern.split(ELISION).enumerate() {
        if index > 0 {
            pieces.push(Piece::Elided);
        }
        let mut rest = text;
        while let Some((start, end, name)) = next_placeholder(rest) {
            pieces.push(Piece::Text(&rest[..start]));
            pieces.push(Piece::Placeholder(name));
            rest = &rest[end..];
        }
        pieces.push(Piece::Text(rest));
    }
    pieces
}

/// Whether `actual` matches `pieces`, binding placeholders not yet in `bound`; on a match,
/// `bound` holds the new bindings too, and otherwise it is as it was. `previous` is the character
/// just before `actual` in the line, if any.
fn pieces_match(
    pieces: &[Piece],
    actual: &str,
    previous: Option<char>,
    bound: &mut HashMap<String, String>,
) -> bool {
    let Some((piece, rest)) = pieces.split_first() else {
        return actual.is_empty();
    };
    let last = |text: &str| text.chars().next_back().or(previous);

    match piece {
        Piece::Text(text) => actual
            .strip_prefix(text)
            .is_some_and(|after| pieces_match(rest, after, last(text), bound)),
        Piece::Elided => actual
            .char_indices()
            .map(|(at, _)| at)
            .chain([actual.len()])
            .any(|at| pieces_match(rest, &actual[at..], last(&actual[..at]), bound)),
        Piece::Placeholder(name) => {
            // A word starts and ends where no letter or digit is next to it, so a bound `0c80`
            // is neither the start nor the end of `dead0c80c7`.
            if previous.is_some_and(char::is_alphanumeric) {
                return false;
            }
            let ends_word =
                |after: &str| !after.starts_with(|c: char| c.is_alphanumeric());
            if let Some(value) = bound.get(*name).cloned() {
                let after = actual.strip_prefix(value.as_str()).filter(|after| ends_word(after));
                return after.is_some_and(|after| pieces_match(rest, after, last(&value), bound));
            }
            // The longest word first, so `<key>.` binds the key without the full stop.
            let word = actual.find(char::is_whitespace).unwrap_or(actual.len());
            let mut ends: Vec<usize> =
                actual[..word].char_indices().map(|(at, c)| at + c.len_utf8()).collect();
            ends.retain(|&end| ends_word(&actual[end..]));
            ends.reverse();
            for end in ends {
                bound.insert(name.to_string(), actual[..end].to_string());
                if pieces_match(rest, &actual[end..], last(&actual[..end]), bound) {
                    return true;
                }
                bound.remove(*name);
            }
            false
        }
    }
}

/// Whether `actual` is `pattern` with each [`ELISION`] standing for any text, including none, and
/// each `<name>` for one word, bound on its first use and the same on every later one.
fn line_matches(pattern: &str, actual: &str, bound: &mut HashMap<String, String>) -> bool {
    pieces_match(&pieces(pattern), actual, None, bound)
}

/// Whether the printed lines match the page's lines one for one; on a match, `bound` holds the
/// placeholders they bound, and otherwise it is as it was. Each line binds the first way it
/// matches.
fn output_matches(
    expected: &[String],
    actual: &[String],
    bound: &mut HashMap<String, String>,
) -> bool {
    let mut trial = bound.clone();
    let matched = expected.len() == actual.len()
        && expected.iter().zip(actual).all(|(pattern, line)| {
            line_matches(pattern, line, &mut trial)
        });
    if matched {
        *bound = trial;
    }
    matched
}

/// `command` with each `<name>` replaced by what it bound, or why it cannot be: a name nothing
/// bound yet, or one inside quotes, where a reader's shell would pass `<name>` on as it is.
fn substitute(command: &str, bound: &HashMap<String, String>) -> Result<String, String> {
    let mut result = String::new();
    let mut rest = command;
    while let Some((start, end, name)) = next_placeholder(rest) {
        let before = &command[..command.len() - rest.len() + start];
        let quote = before.chars().fold(None, |quote, c| match (quote, c) {
            (None, '\'' | '"') => Some(c),
            (Some(open), _) if open == c => None,
            _ => quote,
        });
        if quote.is_some() {
            return Err(format!("<{}> is inside quotes, where a shell would not replace it", name));
        }
        let value = bound.get(name).ok_or_else(|| format!("no output above binds <{}>", name))?;
        result.push_str(&rest[..start]);
        result.push_str(value);
        rest = &rest[end..];
    }
    result.push_str(rest);
    Ok(result)
}

/// What a reader sees: line endings normalised, trailing blank lines and trailing spaces dropped.
/// Escapes are kept. The commands print into a file, where forklift writes no colour, so an
/// escape sequence in the output is a difference from the page, never something to strip.
fn printed_lines(bytes: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(bytes).replace("\r\n", "\n");

    let mut lines: Vec<String> = text.lines().map(|line| line.trim_end().to_string()).collect();
    while lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// A server the page started, stopped when the run ends.
struct Server {
    line: usize,
    /// What the page shows the server printing on stderr after its startup line.
    later: Vec<String>,
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

/// The reader's machines, each a scratch folder with a global configuration and key folder of its
/// own, so the run never touches the developer's, and a current folder inside it. Commands run on
/// the current machine, the one the page last named.
struct Reader {
    /// The page being run, as failure messages name it.
    page: String,
    area: PathBuf,
    /// The current machine, `None` on a page that names none.
    machine: Option<String>,
    /// The current machine's folder, holding its global configuration and keys.
    home: PathBuf,
    cwd: PathBuf,
    /// The folder and current folder of every other machine the page has used.
    others: HashMap<Option<String>, (PathBuf, PathBuf)>,
    /// What each `<name>` in the output so far stood for.
    bound: HashMap<String, String>,
    /// Where the page's server really listens, once it has started.
    address: Option<String>,
    servers: Vec<Server>,
}

/// A new machine's folder and current folder, made empty: the run area itself on a page that
/// names no machine.
fn machine_folders(area: &Path, machine: &Option<String>) -> (PathBuf, PathBuf) {
    let home = match machine {
        Some(name) => area.join("machines").join(name),
        None => area.to_path_buf(),
    };
    let cwd = home.join("reader");
    std::fs::create_dir_all(&cwd).unwrap();
    (home, cwd)
}

impl Reader {
    /// Make `machine` the current one, giving it an empty folder the first time.
    fn switch_to(&mut self, machine: Option<String>) {
        if machine == self.machine {
            return;
        }
        let (home, cwd) =
            self.others.remove(&machine).unwrap_or_else(|| machine_folders(&self.area, &machine));
        let previous = std::mem::replace(&mut self.machine, machine);
        let home = std::mem::replace(&mut self.home, home);
        let cwd = std::mem::replace(&mut self.cwd, cwd);
        self.others.insert(previous, (home, cwd));
    }

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
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
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
                cd, mkdir, cat, echo > file, forklift > file) or teach it")
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
            ("forklift", Some((path, append))) => {
                // As a shell runs it: stdout into the file, stderr still on the terminal.
                let file = std::fs::OpenOptions::new()
                    .create(true).write(true).append(append).truncate(!append)
                    .open(self.cwd.join(path))
                    .unwrap_or_else(|e| fail(&e));
                let mut command = forklift_command();
                command.args(&words[1..]);
                self.environment(&mut command);
                let output = command.stdout(file).stderr(Stdio::piped()).output().unwrap_or_else(|e| fail(&e));
                (output.status.code(), output.stderr)
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

                self.servers.push(Server { line: step.line, later: Vec::new(), child, _stdout: stdout, stderr });
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

    /// Each server that has exited, or whose stderr is not what the page shows it printing, with
    /// what it printed and what the page shows; empty if every server matches. After a failed
    /// step it reports what the servers have printed so far. Once the run is over (`finished`),
    /// a server still printing what the commands it served left it to do is given until one
    /// deadline shared by every server to catch up, kept running a moment past a match so a line
    /// printed just after it is seen, and then stopped before its stderr is read for the last
    /// time, so nothing it printed goes unchecked.
    fn server_mismatch(&mut self, finished: bool) -> String {
        let read = |server: &Server| printed_lines(&std::fs::read(&server.stderr).unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut mismatch = String::new();
        for server in &mut self.servers {
            let exited = server.child.try_wait().unwrap();
            if finished && exited.is_none() {
                loop {
                    let printed = read(server);
                    let matches = output_matches(&server.later, &printed, &mut self.bound.clone());
                    if matches || printed.len() > server.later.len() || std::time::Instant::now() > deadline {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                std::thread::sleep(std::time::Duration::from_millis(500));
                let _ = server.child.kill();
                let _ = server.child.wait();
            }
            let printed = read(server);
            if exited.is_some() || !output_matches(&server.later, &printed, &mut self.bound.clone()) {
                mismatch.push_str(&format!(
                    "\n\nThe server started at line {} exited ({:?}) or printed on stderr\n{}\n\nThe \
                    page shows it printing\n{}",
                    server.line, exited, printed.join("\n"), server.later.join("\n")
                ));
            }
        }
        mismatch
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

    // The run starts on the first block's machine.
    let machine = steps.first().and_then(|step| step.machine.clone());
    let (home, cwd) = machine_folders(&area, &machine);
    let mut reader = Reader {
        page: label.clone(),
        area: area.clone(),
        machine,
        home,
        cwd,
        others: HashMap::new(),
        bound: HashMap::new(),
        address: None,
        servers: Vec::new(),
    };

    for step in &steps {
        reader.switch_to(step.machine.clone());
        let command = substitute(&reader.localize(&step.command), &reader.bound)
            .unwrap_or_else(|why| {
                panic!("{} line {}: `{}`: {}", label, step.line, step.command, why)
            });
        let step = Step {
            line: step.line,
            machine: step.machine.clone(),
            command,
            expected: step.expected.clone(),
        };

        let servers = reader.servers.len();
        let (status, output) = reader.run(&step);
        let printed = printed_lines(&output);
        let mut expected: Vec<String> = step.expected.iter().map(|line| reader.localize(line)).collect();
        if reader.servers.len() > servers {
            // Below a server's startup line is what it prints later, checked when the run ends.
            let later = expected.split_off(expected.len().min(1));
            reader.servers.last_mut().unwrap().later = later;
        }

        assert!(
            status == Some(0) && output_matches(&expected, &printed, &mut reader.bound),
            "{} line {}: `{}` exited {:?} and printed\n{}\n\nThe page shows\n{}{}",
            label, step.line, step.command, status, printed.join("\n"), expected.join("\n"),
            reader.server_mismatch(false)
        );
    }

    // A server's second terminal shows its stderr too, which must be what the page shows. A
    // server that died mid-run is reported with the step that failed because of it.
    let trouble = reader.server_mismatch(true);
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

#[test]
fn team_runs_as_written() {
    run_page("team.md");
}

#[test]
fn cache_runs_as_written() {
    run_page("cache.md");
}

#[test]
fn leaving_runs_as_written() {
    run_page("leaving.md");
}

#[test]
fn leaked_secret_runs_as_written() {
    run_page("leaked-secret.md");
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
    let matches = |pattern: &str, actual: &str| line_matches(pattern, actual, &mut HashMap::new());

    let stacked = "Stacked parcel … on pallet \"main\".";
    assert!(matches(stacked, "Stacked parcel 8faf on pallet \"main\"."));
    assert!(matches("…", ""));
    assert!(matches("a … b … c", "a 1 b 2 c"));
    assert!(matches("exact", "exact"));

    assert!(!matches(stacked, "Stacked parcel 8faf on pallet \"shout\"."));
    assert!(!matches("exact", "exact "));
    assert!(!matches("a…a", "a"));
    assert!(!matches("a … b … c", "a 1 c 2 b"));
}

#[test]
fn a_placeholder_stands_for_one_word_and_binds_it() {
    let mut bound = HashMap::new();
    assert!(line_matches("Key id: <key>.", "Key id: 0c80c7.", &mut bound));
    assert_eq!(bound["key"], "0c80c7");

    // A second use, in the same line or a later one, is the word the first one bound.
    assert!(line_matches("key <key> again", "key 0c80c7 again", &mut bound));
    assert!(!line_matches("key <key>", "key 0c80c8", &mut bound));
    assert!(!line_matches("key <key>…", "key 0c80c7d", &mut bound));
    let mut short = HashMap::from([("key".to_string(), "0c80".to_string())]);
    assert!(!line_matches("key <key>…", "key 0c80c7", &mut short));
    assert!(line_matches("key <key>…", "key 0c80, then", &mut short));
    assert!(!line_matches("key …<key>", "key dead0c80", &mut short));
    assert!(!line_matches("key x<key>", "key x0c80", &mut short));
    assert!(line_matches("key …<key>", "key (0c80", &mut short));
    assert!(line_matches("<a> and <a>", "x and x", &mut HashMap::new()));
    assert!(!line_matches("<a> and <a>", "x and y", &mut HashMap::new()));

    // One word: never empty, never across a space.
    assert!(!line_matches("id: <id>", "id: ", &mut HashMap::new()));
    assert!(!line_matches("id: <id>", "id: two words", &mut HashMap::new()));
    assert!(line_matches("id: <id> …", "id: two words", &mut HashMap::new()));
    assert!(!line_matches("id: <id>c", "id: abc", &mut HashMap::new()));

    // A failed match binds nothing, and text that is not a name is plain text.
    let mut untouched = HashMap::new();
    assert!(!line_matches("<a> b", "x c", &mut untouched));
    assert!(untouched.is_empty());
    let usage = "Usage: forklift <COMMAND> < x>";
    assert!(line_matches(usage, usage, &mut untouched));
    assert!(!line_matches("Usage: forklift <COMMAND>", "Usage: forklift prepare", &mut untouched));
}

#[test]
fn output_must_match_line_for_line() {
    let lines = |text: &str| text.lines().map(str::to_string).collect::<Vec<_>>();
    let matches = |expected: &[String], actual: &[String]| {
        output_matches(expected, actual, &mut HashMap::new())
    };

    assert!(matches(&lines("…\nb"), &lines("a\nb")));
    assert!(matches(&[], &[]));

    assert!(!matches(&[], &lines("unexpected")));
    assert!(!matches(&lines("…"), &lines("a\nb")));
    assert!(!matches(&lines("a\nb"), &lines("a")));

    // A step that does not match keeps none of the bindings its first lines made.
    let mut bound = HashMap::new();
    assert!(!output_matches(&lines("<a>\nb"), &lines("x\nc"), &mut bound));
    assert!(bound.is_empty());
    assert!(output_matches(&lines("<a>\n<a>"), &lines("x\nx"), &mut bound));
    assert_eq!(bound["a"], "x");
}

#[test]
fn a_command_uses_what_its_placeholders_bound() {
    let bound = HashMap::from([
        ("key".to_string(), "0c80".to_string()),
        ("pop".to_string(), "b82c".to_string()),
    ]);

    assert_eq!(
        substitute("forklift office admit bob <key> <pop>", &bound).unwrap(),
        "forklift office admit bob 0c80 b82c"
    );
    assert_eq!(substitute("forklift prepare", &bound).unwrap(), "forklift prepare");
    assert!(substitute("forklift admit <key> <genesis>", &bound).unwrap_err().contains("binds"));

    // Inside quotes a shell passes `<key>` on as it is, so the test cannot replace it.
    assert!(substitute("echo 'id <key>' > notes", &bound).unwrap_err().contains("quotes"));
    assert!(substitute("echo \"id <key>\" > notes", &bound).unwrap_err().contains("quotes"));
    assert_eq!(substitute("echo 'a' <key> > notes", &bound).unwrap(), "echo 'a' 0c80 > notes");

    // Text that is not a name stays, for the shell check to refuse.
    assert_eq!(substitute("cat <Notes>", &bound).unwrap(), "cat <Notes>");
}

#[test]
fn machine_markers_name_the_machine_each_block_runs_on() {
    let page = "<!-- machine: ada -->\n```console\n$ a\n```\n\
        <!-- machine: bob-2 -->\n```console\n$ b\n$ c\n```\n";
    let steps = parse_steps(page).unwrap();
    let machines: Vec<Option<&str>> = steps.iter().map(|step| step.machine.as_deref()).collect();
    assert_eq!(machines, [Some("ada"), Some("bob-2"), Some("bob-2")]);

    let steps = parse_steps("```console\n$ a\n```\n<!-- a comment about machines -->\n").unwrap();
    assert_eq!(steps[0].machine, None);

    let refused = |page: &str| parse_steps(page).is_err();
    assert!(refused("```console\n$ a\n```\n<!-- machine: ada -->\n```console\n$ b\n```\n"));
    assert!(refused("<!-- machine: Ada -->\n"));
    assert!(refused("<!-- machine:ada -->\n"));
    assert!(refused("<!--machine: ada-->\n"));
    assert!(refused("<!-- Machine: ada -->\n"));
    assert!(refused("<!-- machine: ada bob -->\n"));
    assert!(refused("  <!-- machine: ada -->\n"));
    assert!(refused("<!---machine: ada -->\n"));
    assert!(refused("<!--\nmachine: ada\n-->\n"));
    assert!(refused("<!-- machine: ada\n-->\n"));
    assert!(refused("<!-- machines: bob -->\n"));
    assert!(refused("<!-- machine bob -->\n"));

    // A comment that never closes hides the rest of the page; one that closes and reopens on a
    // line stays open; one indented as code is not a comment.
    assert!(refused("<!-- a note -\n```console\n$ a\n```\n"));
    let hidden = "<!--\na\n--> <!-- b\n```console\n$ hidden\n```\n-->\n";
    assert!(parse_steps(hidden).unwrap().is_empty());
    assert!(refused("text\n\n    <!-- example\n```console\n$ a\n```\n-->\n"));

    // Other comments are prose, on one line or several, and an indented line after a comment
    // starts a code block as it would after a blank line.
    assert!(parse_steps("<!-- machine-readable output -->\n<!-- A machine differs -->\n").is_ok());
    assert!(parse_steps("<!--\nA note about the machine.\n-->\n").is_ok());
    assert!(refused("<!-- machine: ada -->\n    forklift lower\n"));
}

#[test]
fn a_placeholder_used_once_is_refused() {
    let page = "```console\n$ a\nid <id>\n$ b <id>\nrecheck <idd>\n```\n";
    let error = parse_steps(page).unwrap_err();
    assert!(error.contains("line 5: <idd> is used only once"), "{}", error);

    assert!(parse_steps("```console\n$ a\nid <id>\n$ b <id>\n```\n").is_ok());
}

#[test]
fn printed_lines_are_what_a_reader_sees() {
    assert_eq!(printed_lines(b"parcel 1  \r\nnext\n\n"), vec!["parcel 1", "next"]);

    // A colour sequence stays, so leaked colour fails the page instead of being hidden.
    assert_eq!(printed_lines(b"\x1b[33mparcel 1\x1b[0m"), vec!["\x1b[33mparcel 1\x1b[0m"]);
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

    assert!(runs("forklift stack -m \"First parcel\""));
    assert!(runs("echo 'Hello, world!' > readme.md"));
    assert!(runs("echo \"text\" >> notes.md"));
    assert!(runs("echo 'build/' > .gitignore"));
    assert!(runs("cd ../hello"));

    assert!(!runs("echo \"Hello, world!\" > readme.md"));
    assert!(!runs("forklift load *.md"));
    assert!(!runs("cd ~"));
    assert!(!runs("echo $HOME > f"));
    assert!(!runs("echo \"$HOME\" > f"));
    assert!(!runs("forklift load a && forklift stack -m m"));
    assert!(!runs("forklift history | head"));
    assert!(!runs("forklift load a; forklift stack -m m"));
    assert!(!runs("echo -n x > f"));
    assert!(!runs("echo a>f"));
    assert!(!runs("echo a > f > g"));
    assert!(!runs("forklift stack -m \"unclosed"));
    assert!(!runs("forklift stack -m m # a comment"));
    assert!(!runs("echo 'a\\nb' > f"));
    assert!(!runs("forklift config set operator.identifier =ada"));
    assert!(runs("forklift config set remote.url --opt=a=b"));
    assert!(!runs("  "));
}
