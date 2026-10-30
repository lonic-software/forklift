//! The command-surface drift guard: three checks that hold the clap tree in `cli.rs` to files a
//! person edits on purpose, so a new command, a renamed one or a dropped one is a reviewed decision
//! and never a side effect.
//!
//! - **The surface spec** (`surface/spec.txt`) lists every command path the binary accepts, and
//!   three lists drawn by written criteria: the daily loop, the destructive operations and the
//!   commands that record text. The binary and the spec must name the same commands.
//! - **The coverage map** (`surface/coverage.txt`) records every command path the binary had when
//!   the map was frozen, with its parameters, and what each one became. Every row must still be
//!   reachable with exactly the parameters it says, and every command the binary has must be some
//!   row's destination, so no operation is dropped by accident while the surface is reshaped.
//! - **The hint check** parses every next-step hint that names a `forklift` command, in every
//!   string literal of the crates' sources, against the real command tree.
//!
//! `docs/generated/commands.md` is generated from the same tree and checked by `bin/gen-docs
//! --check`; it follows the binary wherever it goes. These files do not: they change only when
//! someone edits them, which is what makes them a guard.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use clap::{CommandFactory, FromArgMatches};
use clap::error::{ContextKind, ContextValue, ErrorKind};
use proc_macro2::{Delimiter, TokenStream, TokenTree};
use crate::cli::Cli;

const SPEC: &str = include_str!("../surface/spec.txt");
const COVERAGE: &str = include_str!("../surface/coverage.txt");

/// The command tree as the binary parses it, built so that every subcommand carries the global
/// options and the generated `--help` flag.
fn built_cli() -> clap::Command {
    let mut cli = Cli::command();
    cli.build();
    cli
}

/// Every command in the tree, depth first in declaration order, as its path below the binary
/// (`["scope", "status"]`; the binary itself is `[]`) beside the command.
fn command_paths(cli: &clap::Command) -> Vec<(Vec<String>, &clap::Command)> {
    fn walk<'a>(command: &'a clap::Command, path: Vec<String>, out: &mut Vec<(Vec<String>, &'a clap::Command)>) {
        out.push((path.clone(), command));
        for sub in command.get_subcommands() {
            let mut sub_path = path.clone();
            sub_path.push(sub.get_name().to_string());
            walk(sub, sub_path, out);
        }
    }
    let mut out = Vec::new();
    walk(cli, Vec::new(), &mut out);
    out
}

/// The command at `path`, or `None` when the tree has no such command.
fn find_command<'a>(cli: &'a clap::Command, path: &[String]) -> Option<&'a clap::Command> {
    path.iter().try_fold(cli, |command, name| command.find_subcommand(name))
}

/// Lines of a surface file with `#` comments and surrounding blanks removed.
fn content_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    text.lines().enumerate()
        .map(|(index, line)| (index + 1, line.split('#').next().unwrap_or_default().trim()))
        .filter(|(_, line)| !line.is_empty())
}

/// The surface spec, parsed: the command paths, then each named list with its entries.
struct Spec {
    commands: Vec<Vec<String>>,
    lists: BTreeMap<String, Vec<(usize, Vec<String>)>>,
}

/// The three lists the spec must hold, in the order the file gives them.
const SPEC_LISTS: [&str; 3] = ["daily-loop", "destructive", "recorded-text"];

/// Parse `surface/spec.txt`: a `[commands]` section, one top-level name per line, a family
/// written as `name: sub sub …`; then the [`SPEC_LISTS`] sections, one command path per line.
fn parse_spec(text: &str) -> Result<Spec, String> {
    let mut spec = Spec { commands: Vec::new(), lists: BTreeMap::new() };
    let mut section: Option<String> = None;
    for (number, line) in content_lines(text) {
        if let Some(name) = line.strip_prefix('[').and_then(|rest| rest.strip_suffix(']')) {
            if name != "commands" && !SPEC_LISTS.contains(&name) {
                return Err(format!("spec.txt:{number}: unknown section [{name}]"));
            }
            if name == "commands" && !spec.commands.is_empty() || spec.lists.contains_key(name) {
                return Err(format!("spec.txt:{number}: section [{name}] appears twice"));
            }
            if name != "commands" {
                spec.lists.insert(name.to_string(), Vec::new());
            }
            section = Some(name.to_string());
            continue;
        }
        match section.as_deref() {
            None => return Err(format!("spec.txt:{number}: a line before the first section")),
            Some("commands") => {
                let (name, subs) = match line.split_once(':') {
                    Some((name, subs)) => (name.trim(), Some(subs)),
                    None => (line, None),
                };
                if name.contains(char::is_whitespace) {
                    return Err(format!("spec.txt:{number}: `{name}` is not one command name"));
                }
                spec.commands.push(vec![name.to_string()]);
                for sub in subs.into_iter().flat_map(str::split_whitespace) {
                    spec.commands.push(vec![name.to_string(), sub.to_string()]);
                }
            }
            Some(list) => {
                let words = line.split_whitespace().map(str::to_string).collect();
                spec.lists.get_mut(list).expect("inserted with its header").push((number, words));
            }
        }
    }
    Ok(spec)
}

/// The spec and the binary name the same commands, and every list entry names one of them.
#[test]
fn the_surface_spec_names_exactly_the_binarys_commands() {
    let cli = built_cli();
    let spec = parse_spec(SPEC).unwrap();

    let binary: BTreeSet<Vec<String>> = command_paths(&cli).into_iter()
        .map(|(path, _)| path).filter(|path| !path.is_empty()).collect();
    let mut seen = BTreeSet::new();
    let doubled: Vec<String> = spec.commands.iter()
        .filter(|path| !seen.insert((*path).clone())).map(|path| path.join(" ")).collect();
    assert!(doubled.is_empty(), "surface/spec.txt lists these commands twice: {doubled:?}");

    let missing: Vec<String> = binary.difference(&seen).map(|path| path.join(" ")).collect();
    let stale: Vec<String> = seen.difference(&binary).map(|path| path.join(" ")).collect();
    assert!(
        missing.is_empty() && stale.is_empty(),
        "surface/spec.txt and the binary disagree. A new command is placed by adding it to the \
         spec, which classifies it against the lists there too; a removed one is taken out.\n\
         in the binary, not the spec: {missing:?}\nin the spec, not the binary: {stale:?}"
    );

    assert_eq!(spec.lists.keys().map(String::as_str).collect::<BTreeSet<_>>(),
        SPEC_LISTS.into_iter().collect(), "surface/spec.txt must hold each list section");
    for (list, entries) in &spec.lists {
        for (number, words) in entries {
            // A destructive entry may name the mode that makes it destructive: the flag that
            // selects it, then the value, as `lower --keep …=theirs`.
            let split = words.iter().position(|word| word.starts_with('-')).unwrap_or(words.len());
            let (path, mode) = words.split_at(split);
            let command = find_command(&cli, path).unwrap_or_else(|| panic!(
                "spec.txt:{number}: [{list}] names `{}`, which is not a command", path.join(" ")));
            if let Some(flag) = mode.first() {
                assert_eq!(list, "destructive", "spec.txt:{number}: only a destructive entry names a mode");
                assert!(mode.len() == 2, "spec.txt:{number}: a mode is one flag and one value");
                let long = flag.strip_prefix("--").unwrap_or_default();
                assert!(command.get_arguments().any(|arg| arg.get_long() == Some(long)),
                    "spec.txt:{number}: `{}` has no {flag} flag to select the mode", path.join(" "));
            }
        }
    }
}

/// One parameter, written so that a change to anything a caller depends on changes the text: a
/// positional's name, whether it is required, whether it repeats; a flag's spellings, whether it
/// takes a value and which values it accepts; whether it is hidden.
fn parameter(arg: &clap::Arg) -> String {
    let takes_value = arg.get_action().takes_values();
    let repeats = matches!(arg.get_action(), clap::ArgAction::Append)
        || arg.get_num_args().is_some_and(|range| range.max_values() > 1);
    let name = arg.get_value_names()
        .map(|names| names.iter().map(|name| name.to_string()).collect::<Vec<_>>().join(" "))
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase());
    let mut out = if arg.is_positional() {
        if arg.is_required_set() { format!("<{name}>") } else { format!("[{name}]") }
    } else {
        let mut spellings = Vec::new();
        if let Some(short) = arg.get_short() {
            spellings.push(format!("-{short}"));
        }
        if let Some(long) = arg.get_long() {
            spellings.push(format!("--{long}"));
        }
        let mut flag = spellings.join("/");
        if takes_value {
            flag.push_str(&format!("=<{name}>"));
        }
        flag
    };
    if takes_value {
        let values: Vec<String> = arg.get_possible_values().iter()
            .map(|value| value.get_name().to_string()).collect();
        if !values.is_empty() {
            out.push_str(&format!("{{{}}}", values.join("|")));
        }
    }
    if repeats {
        out.push_str("...");
    }
    if arg.is_hide_set() {
        out.push_str("(hidden)");
    }
    out
}

/// A command's parameters in declaration order. Below the binary, the global options and the
/// generated `--help` are left out: they are the binary's parameters, recorded once on its row.
fn parameters(command: &clap::Command, is_root: bool) -> String {
    command.get_arguments()
        .filter(|arg| is_root || !(arg.is_global_set() || arg.get_id() == "help"))
        .map(parameter).collect::<Vec<_>>().join(" ")
}

/// One coverage row: an operation the binary had when the map was frozen, and where each part of
/// it lives now.
struct CoverageRow {
    line: usize,
    old: (Vec<String>, String),
    new: Vec<(usize, Vec<String>, String)>,
}

/// `forklift scope status: [PATHS]...` as the path below the binary and the parameter text.
fn parse_command_line(number: usize, text: &str) -> Result<(Vec<String>, String), String> {
    let (path, params) = text.split_once(':')
        .ok_or_else(|| format!("coverage.txt:{number}: `{text}` has no `:` after the command"))?;
    let mut words = path.split_whitespace();
    if words.next() != Some("forklift") {
        return Err(format!("coverage.txt:{number}: a command starts with `forklift`"));
    }
    Ok((words.map(str::to_string).collect(), params.split_whitespace().collect::<Vec<_>>().join(" ")))
}

/// Parse `surface/coverage.txt`. A row is an old command and its parameters; the indented
/// `-> forklift …: …` lines under it are what it became. A row with none still exists unchanged.
fn parse_coverage(text: &str) -> Result<Vec<CoverageRow>, String> {
    let mut rows: Vec<CoverageRow> = Vec::new();
    for (number, line) in content_lines(text) {
        if let Some(rest) = line.strip_prefix("->") {
            let row = rows.last_mut()
                .ok_or_else(|| format!("coverage.txt:{number}: `->` before any row"))?;
            let (path, params) = parse_command_line(number, rest)?;
            row.new.push((number, path, params));
        } else {
            rows.push(CoverageRow { line: number, old: parse_command_line(number, line)?, new: Vec::new() });
        }
    }
    Ok(rows)
}

/// Every operation the coverage map records is still reachable with the parameters it states,
/// and every command the binary has is accounted for by some row.
#[test]
fn every_covered_operation_is_still_reachable() {
    let cli = built_cli();
    let rows = parse_coverage(COVERAGE).unwrap();

    let mut olds = BTreeSet::new();
    let mut destinations = BTreeSet::new();
    let mut failures = Vec::new();
    for row in &rows {
        if !olds.insert(row.old.0.clone()) {
            failures.push(format!("coverage.txt:{}: `{}` has two rows", row.line, row.old.0.join(" ")));
        }
        let unchanged = [(row.line, row.old.0.clone(), row.old.1.clone())];
        let new = if row.new.is_empty() { &unchanged[..] } else { &row.new[..] };
        for (number, path, expected) in new {
            destinations.insert(path.clone());
            let shown = std::iter::once("forklift").chain(path.iter().map(String::as_str))
                .collect::<Vec<_>>().join(" ");
            match find_command(&cli, path) {
                None => failures.push(format!("coverage.txt:{number}: `{shown}` is not a command")),
                Some(command) => {
                    let actual = parameters(command, path.is_empty());
                    if &actual != expected {
                        failures.push(format!(
                            "coverage.txt:{number}: `{shown}` takes different parameters\n  \
                             map:    {expected}\n  binary: {actual}"
                        ));
                    }
                }
            }
        }
    }
    for (path, _) in command_paths(&cli) {
        if !destinations.contains(&path) {
            failures.push(format!("`forklift {}` is a command no coverage row leads to", path.join(" ")));
        }
    }
    assert!(failures.is_empty(),
        "surface/coverage.txt and the binary disagree. A changed operation keeps its row and \
         gains `-> forklift …: …` lines saying what it became, with any change to its \
         parameters written there:\n{}", failures.join("\n"));
}

// ---- The hint check ----

/// The words that follow "forklift " without naming a command: prose ("a newer forklift does not
/// know", "another forklift process"), the domain tags signed records and MACs begin with
/// ("forklift key endorsement v1", "forklift hook protocol …"), a record header ("forklift git mirror
/// roots 1"), and a test's made-up command ("dollar"). A mention is exempt when its leading words
/// are one of these. Every other mention of `forklift <word>` is a hint and is parsed, so a hint
/// whose command is renamed fails rather than passing as prose. No entry may be a command path,
/// or a hint naming that command could hide behind it ([`no_exemption_is_a_command`]).
const NOT_COMMANDS: &[&str] = &[
    "binary", "bundle", "cannot", "created", "does", "dollar", "hook", "is", "key", "knows", "left",
    "manages", "mirror roots", "piped", "process", "records", "recognizes", "reversible", "root",
    "server", "should", "tree", "warehouse", "was", "with", "writer", "wrote",
];

/// The words of `text` an exemption is matched against: runs of letters, digits and `-`.
fn leading_words(text: &str) -> Vec<&str> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).filter(|word| !word.is_empty()).collect()
}

/// Rust sources under `dir`, sorted, skipping build output and integration tests. An integration
/// test's literals are expected output, held to the production strings by its own assertions, or
/// deliberately malformed input (the guide-page interpreter's); the strings it expects are
/// checked here where they are written.
fn rust_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap()
        .map(|entry| entry.unwrap().path()).collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            if !path.file_name().is_some_and(|name| name == "target" || name == "tests") {
                rust_sources(&path, out);
            }
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            out.push(path);
        }
    }
}

/// Every string literal's value in `stream`, escapes resolved, skipping doc comments (which
/// arrive as `#[doc = "…"]` attributes and are prose for maintainers).
fn string_literals(stream: TokenStream, out: &mut Vec<String>) {
    let mut after_hash = false;
    for tree in stream {
        match tree {
            TokenTree::Group(group) => {
                let is_doc = after_hash && group.delimiter() == Delimiter::Bracket
                    && group.stream().into_iter().next().is_some_and(|first| first.to_string() == "doc");
                if !is_doc {
                    string_literals(group.stream(), out);
                }
                after_hash = false;
            }
            TokenTree::Punct(punct) => after_hash = punct.as_char() == '#' || after_hash && punct.as_char() == '!',
            TokenTree::Literal(literal) => {
                after_hash = false;
                if let Ok(string) = syn::parse_str::<syn::LitStr>(&literal.to_string()) {
                    out.push(string.value());
                }
            }
            TokenTree::Ident(_) => after_hash = false,
        }
    }
}

/// Each command `text` names after "forklift ", as the words to parse. A mention is "forklift "
/// at the start of a word, followed by a lower-case letter or a `-` (`forklift --json audit`).
/// Opened by a quote or a backtick, it runs to the matching one; otherwise to the first `;`, `,`,
/// `(`, `)`, line break, or `.` that ends a sentence, outside single quotes (`forklift bay remove
/// 'a;b'` is one hint). An apostrophe inside a word ("the remote's") opens no quote. The crate
/// name of a `cargo install` is not a mention: one with only options and a URL between it and
/// `cargo install` (`cargo install --git <url> forklift --force`). Mentions whose
/// leading words are in [`NOT_COMMANDS`] are not hints. A command formatted in at run time
/// (`forklift {}`) cannot be read here and is not a mention.
fn hints(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (start, _) in text.match_indices("forklift ") {
        let before = text[..start].chars().next_back();
        if before.is_some_and(|c| c.is_alphanumeric() || "-_./@".contains(c)) {
            continue;
        }
        let line = &text[text[..start].rfind('\n').map_or(0, |i| i + 1)..start];
        if line.rfind("cargo install ").is_some_and(|install| line[install + "cargo install ".len()..]
            .split_whitespace().all(|word| word.starts_with('-') || word.contains("://")))
        {
            continue;
        }
        let rest = &text[start + "forklift ".len()..];
        if !rest.starts_with(|c: char| c.is_ascii_lowercase() || c == '-') {
            continue;
        }
        let end = match before {
            // Inside JSON written as a Rust literal, the hint's quotes are escaped too.
            Some(quote @ ('"' | '`' | '\'')) if text[..start - 1].ends_with('\\') =>
                rest.find(&format!("\\{quote}")).unwrap_or(rest.len()),
            Some(quote @ ('"' | '`' | '\'')) => rest.find(quote).unwrap_or(rest.len()),
            _ => {
                let mut quoted = false;
                rest.char_indices()
                    .find(|&(i, c)| {
                        if c == '\'' {
                            let inside_word = rest[..i].ends_with(char::is_alphanumeric)
                                && rest[i + 1..].starts_with(char::is_alphanumeric);
                            quoted ^= !inside_word;
                        }
                        !quoted && (";,()\n".contains(c)
                            || c == '.' && rest[i + 1..].starts_with(char::is_whitespace)
                                && !rest[..i].ends_with(' '))
                    })
                    .map_or(rest.len(), |(i, _)| i)
            }
        };
        let mut hint = rest[..end].trim_end();
        if !matches!(before, Some('"' | '`' | '\'')) {
            hint = hint.strip_suffix('.').filter(|h| !h.ends_with(' ')).unwrap_or(hint);
        }
        let words = leading_words(hint);
        if !NOT_COMMANDS.iter().any(|exempt| words.starts_with(&leading_words(exempt))) {
            out.push(hint.to_string());
        }
    }
    out
}

/// `hint` with each format placeholder (`{}`, `{name}`, `{:?}`) replaced by a plain word and each
/// escaped brace (`{{`, `}}`) by the brace it prints, so a formatted hint parses as the command a
/// user would be shown.
fn fill_placeholders(hint: &str) -> String {
    let mut out = String::new();
    let mut rest = hint;
    while let Some(open) = rest.find(['{', '}']) {
        out.push_str(&rest[..open]);
        if rest[open..].starts_with("{{") || rest[open..].starts_with("}}") {
            out.push_str(&rest[open..open + 1]);
            rest = &rest[open + 2..];
            continue;
        }
        if rest[open..].starts_with('}') {
            out.push('}');
            rest = &rest[open + 1..];
            continue;
        }
        match rest[open..].find('}') {
            Some(close) => {
                out.push('x');
                rest = &rest[open + close + 1..];
            }
            None => {
                rest = &rest[open..];
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// Why `hint` does not parse as a command the binary accepts, or `None` when it does. It is split
/// as a shell would and parsed by clap with the binary's own tree, then held to the cross-argument
/// rule `main` applies after clap, so the hint is accepted exactly when a user's copy of it would
/// be. Prefix matching plays no part: clap is not asked to infer subcommands. A hint that leaves
/// out a required argument (`forklift remote move`) or a flag's value (`forklift undo --as`) still
/// names a real command and passes; one naming a command, flag or value that does not exist does
/// not. `forklift help` takes any words, so the command a help hint names is looked up in the tree
/// ([`help_path_error`]).
fn hint_error(hint: &str) -> Option<String> {
    let words = match shlex::split(&fill_placeholders(hint)) {
        Some(words) => words,
        None => return Some("its quoting does not close".to_string()),
    };
    if let Some(error) = shlex::split(hint).and_then(|raw| help_path_error(&raw)) {
        return Some(error);
    }
    let argv = std::iter::once("forklift".to_string()).chain(words);
    let error = match Cli::command().try_get_matches_from(argv) {
        Ok(matches) => match Cli::from_arg_matches(&matches) {
            Ok(cli) => return crate::cross_argument_error(&cli).map(|(_, message)| message.to_string()),
            Err(error) => error,
        },
        Err(error) => error,
    };
    match error.kind() {
        ErrorKind::MissingRequiredArgument | ErrorKind::MissingSubcommand
        | ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand | ErrorKind::DisplayHelp
        | ErrorKind::DisplayVersion => None,
        ErrorKind::InvalidValue if matches!(error.get(ContextKind::InvalidValue),
            Some(ContextValue::String(value)) if value.is_empty()) => None,
        _ => Some(error.render().to_string().lines().next().unwrap_or_default().to_string()),
    }
}

/// Why the command a help hint names (`forklift --json h office admit`) is not in the tree, or
/// `None` when it is or the hint is not a help hint. Options are skipped wherever they stand, with
/// the value a global option takes as its own word (`--color always`), and the path ends at the
/// first word that stands for a name rather than spelling one: a format placeholder (`{}`),
/// `<command>`, `[COMMAND]`, or a bare `COMMAND`. `raw` is the hint's words before
/// placeholders are filled. The tree is not built: only the root's own options and the names
/// below it are read.
fn help_path_error(raw: &[String]) -> Option<String> {
    let cli = Cli::command();
    let takes_value = |word: &str| word.strip_prefix("--").is_some_and(|long| cli.get_arguments()
        .any(|arg| arg.get_long() == Some(long) && arg.get_action().takes_values()));
    let mut words = Vec::new();
    let mut skip_value = false;
    for word in raw {
        if std::mem::take(&mut skip_value) {
            continue;
        }
        if word.starts_with('-') {
            skip_value = takes_value(word);
        } else {
            words.push(word);
        }
    }
    let (first, rest) = words.split_first()?;
    if cli.find_subcommand(first.as_str())?.get_name() != "help" {
        return None;
    }
    let names_a_name = |word: &&&String| word.contains(['{', '<', '[']) || word.as_str() == "COMMAND";
    let path: Vec<String> = rest.iter().take_while(|word| !names_a_name(word)).map(|word| word.to_string()).collect();
    match find_command(&cli, &path) {
        Some(_) => None,
        None => Some(format!("`forklift help` names no command `{}`", path.join(" "))),
    }
}

/// Every text a user can be shown, beside where it comes from: each string literal in the crates'
/// sources, and each help text of the binary's commands and arguments. The help comes from the
/// clap tree rather than the sources because most of it is written as doc comments, which the
/// literal scan skips: the rest of the doc comments are prose for maintainers.
fn shown_texts() -> Vec<(String, String)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut files = Vec::new();
    rust_sources(&crates, &mut files);
    let mut out = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let stream = TokenStream::from_str(&text)
            .unwrap_or_else(|error| panic!("{} does not lex: {error}", file.display()));
        let mut literals = Vec::new();
        string_literals(stream, &mut literals);
        let shown = file.strip_prefix(&crates).unwrap_or(&file).display().to_string();
        out.extend(literals.into_iter().map(|literal| (shown.clone(), literal)));
    }
    let cli = built_cli();
    for (path, command) in command_paths(&cli) {
        let shown = format!("the help of `{}`", std::iter::once("forklift")
            .chain(path.iter().map(String::as_str)).collect::<Vec<_>>().join(" "));
        let helps = [command.get_about(), command.get_long_about(), command.get_before_help(),
            command.get_before_long_help(), command.get_after_help(), command.get_after_long_help()]
            .into_iter()
            .chain(command.get_arguments().flat_map(|arg| [arg.get_help(), arg.get_long_help()]));
        out.extend(helps.flatten().map(|help| (shown.clone(), help.to_string())));
    }
    out
}

/// Every hint in every text a user can be shown, beside where it comes from.
fn source_hints() -> Vec<(String, String)> {
    shown_texts().into_iter()
        .flat_map(|(shown, text)| hints(&text).into_iter().map(move |hint| (shown.clone(), hint)))
        .collect()
}

/// Every hint a message gives names a command, flags and values the binary accepts.
#[test]
fn every_hint_names_a_real_command() {
    let hints = source_hints();
    assert!(hints.len() > 200, "the scan found only {} hints; it is not reading the sources", hints.len());
    let failures: Vec<String> = hints.iter()
        .filter_map(|(file, hint)| hint_error(hint).map(|error| format!("{file}: `forklift {hint}`: {error}")))
        .collect();
    assert!(failures.is_empty(),
        "these hints name a command the binary does not accept as written:\n{}", failures.join("\n"));
}

/// Hints are a machine surface, so they must spell full names: a prefix that works today breaks
/// when a command starting with the same letters is added. The hint check parses with
/// `Cli::command()` alone, where the binary's prefix expansion does not run; this holds it there,
/// and clap's own inference, switched on in the tree, would turn it red. The MCP argv check has
/// its own (`the_argv_check_parses_with_prefixes_off`).
#[test]
fn hints_parse_with_prefixes_off() {
    for line in [&["forklift", "ha", "list"][..], &["forklift", "haul", "li"][..]] {
        let error = Cli::command().try_get_matches_from(line).map(|_| ()).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand, "{line:?}");
    }
    assert_eq!(hint_error("haul list"), None);
    // A prefix the binary itself would expand (`ha`, `hel`) is not a command here.
    assert!(hint_error("ha list").is_some());
    assert!(hint_error("haul li").is_some());
    assert!(hint_error("help hau").is_some());
}

/// An exemption that was also a command would let a hint naming that command pass unparsed.
#[test]
fn no_exemption_is_a_command() {
    let cli = built_cli();
    for exempt in NOT_COMMANDS {
        let words: Vec<String> = leading_words(exempt).into_iter().map(String::from).collect();
        let mut command = Some(&cli);
        for word in &words {
            command = command.and_then(|command| command.find_subcommand(word));
        }
        assert!(command.is_none(), "\"{exempt}\" is a command, so it cannot be exempt from the hint check");
    }
}

/// Every command a message names in quotes or backticks within a sentence carries `forklift `,
/// so the hint check above sees it. A quoted span is a command when its first word is a top-level
/// command's name. Two kinds of span are not: a JSON key or value (a `:` against the quote),
/// and a name followed by the noun it is the name of — "the \"lower\" tool", "the reserved
/// \"office\" pallet", "the \"tag\" field".
#[test]
fn every_quoted_command_says_forklift() {
    let cli = built_cli();
    let names: BTreeSet<&str> = cli.get_subcommands().map(|command| command.get_name()).collect();
    let mut failures = Vec::new();
    for (shown, literal) in shown_texts() {
        {
            for quote in ['"', '`'] {
                let parts: Vec<&str> = literal.split(quote).collect();
                if parts.len() < 3 {
                    continue;
                }
                for i in (1..parts.len() - 1).step_by(2) {
                    let first = parts[i].split_whitespace().next().unwrap_or_default();
                    let json = parts[i - 1].ends_with(':') || parts[i + 1].starts_with(':');
                    let named = [" tool", " pallet", " field"].iter().any(|noun| parts[i + 1].starts_with(noun));
                    if names.contains(first) && !json && !named {
                        failures.push(format!("{shown}: {quote}{}{quote}", parts[i]));
                    }
                }
            }
        }
    }
    assert!(failures.is_empty(),
        "these name a command without `forklift `, so the hint check cannot read them:\n{}", failures.join("\n"));
}
