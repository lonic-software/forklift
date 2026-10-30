//! The command-surface drift guard: two checks that hold the clap tree in `cli.rs` to files a
//! person edits on purpose, so a new command, a renamed one or a dropped one is a reviewed decision
//! and never a side effect.
//!
//! - **The surface spec** (`surface/spec.txt`) lists every command path the binary accepts, and
//!   three lists drawn by written criteria: the daily loop, the destructive operations and the
//!   commands that record text. The binary and the spec must name the same commands.
//! - **The hint check** parses every next-step hint that names a `forklift` command, in every
//!   string literal of the crates' sources, against the real command tree.
//!
//! Beside them, the shape lints hold the tree to the surface's rules that a parse can test:
//!
//! - no boolean flag is declared to conflict with a positional argument, because a flag never
//!   switches a command into a different operation;
//! - a visible hyphenated name sits only below the top level;
//! - recorded text is always `-m/--message`, on exactly the commands of the `[recorded-text]` list;
//! - a visible alias sits only on a command of the `[daily-loop]` list, at most one each;
//! - `--confirm` sits exactly on the commands of the `[destructive]` list, none of which has
//!   `--dry-run`, and each mode the list names (destructive only in one mode) names the
//!   behaviour test that holds its preview to that mode.
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
/// written as `name: sub sub …` and a family inside a family as its full path after its parent's
/// line (`self alias: install …`); then the [`SPEC_LISTS`] sections, one command path per line.
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
                let path: Vec<String> = name.split_whitespace().map(str::to_string).collect();
                if path.len() > 1 {
                    // A nested family: its parent's line has already listed the family itself.
                    if subs.is_none() || !spec.commands.contains(&path) {
                        return Err(format!(
                            "spec.txt:{number}: `{name}` is a path, which is written only as a family \
                             (`{name}: sub …`) after the line that lists it"
                        ));
                    }
                    let given = spec.commands.iter().any(|command| command.len() > path.len() && command.starts_with(&path));
                    if given || subs.is_some_and(|subs| subs.trim().is_empty()) {
                        return Err(format!(
                            "spec.txt:{number}: the family `{name}` is written once, on one line \
                             that lists every subcommand"
                        ));
                    }
                } else {
                    spec.commands.push(path.clone());
                }
                for sub in subs.into_iter().flat_map(str::split_whitespace) {
                    let mut sub_path = path.clone();
                    sub_path.push(sub.to_string());
                    spec.commands.push(sub_path);
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

/// A family inside a family is listed by its parent's line and gives its own members on a line
/// of its own; written anywhere else, the path is refused rather than read as something else.
#[test]
fn a_nested_family_is_written_after_its_parent() {
    let paths = |text: &str| parse_spec(text).map(|spec| {
        spec.commands.into_iter().map(|path| path.join(" ")).collect::<Vec<_>>()
    });
    assert_eq!(
        paths("[commands]\nfam: one inner\nfam inner: deep\n").unwrap(),
        ["fam", "fam one", "fam inner", "fam inner deep"]
    );
    for refused in ["[commands]\nfam inner: deep\nfam: one inner\n", "[commands]\nfam: one inner\nfam inner\n"] {
        let error = paths(refused).err().unwrap_or_else(|| panic!("accepted {refused:?}"));
        assert!(error.contains("`fam inner` is a path"), "{error}");
    }
    for refused in ["[commands]\nfam: one inner\nfam inner: deep\nfam inner: deeper\n", "[commands]\nfam: one inner\nfam inner:\n"] {
        let error = paths(refused).err().unwrap_or_else(|| panic!("accepted {refused:?}"));
        assert!(error.contains("`fam inner` is written once"), "{error}");
    }
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

// ---- The hint check ----

/// The words that follow "forklift " without naming a command: prose ("a newer forklift does not
/// know", "another forklift process"), the domain tags signed records and MACs begin with
/// ("forklift key endorsement v1", "forklift hook protocol …"), a record header ("forklift mirror
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
/// as a shell would and parsed by clap with the binary's own tree, so the hint is accepted exactly
/// when clap accepts a user's copy of it. Prefix matching plays no part: clap is not asked to infer subcommands. A hint that leaves
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
            Ok(_) => return None,
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

/// Every way a command declares a boolean flag to conflict with one of its positional arguments,
/// and every pair the check could not test, one line each. A boolean flag that excludes a
/// positional is a flag selecting a different operation (`config --unset KEY`, the old
/// `palletize --delete NAME`), and such an operation is a subcommand of its own.
///
/// The check parses rather than reads the declarations, so a conflict declared on either
/// argument, through a group or by an exclusive argument is found alike: for each pair it parses
/// the positionals up to that one, then the same line with the flag added, and only a declared
/// conflict turns the first parse's success into an `ArgumentConflict`. Clap checks conflicts
/// before required arguments, so the line need not carry them; it must carry values a parser
/// accepts, and a pair whose first parse fails any other way is reported as untested.
///
/// It is a necessary check, not a complete one: a flag that changes the operation without
/// declaring a conflict (as `restore --staged` once did) passes it, and is left to the spec review.
fn boolean_positional_conflicts(cli: &clap::Command) -> Vec<String> {
    fn value(arg: &clap::Arg) -> String {
        arg.get_possible_values().first().map(|value| value.get_name().to_string()).unwrap_or_else(|| "1".to_string())
    }
    let mut failures = Vec::new();
    for (path, command) in command_paths(cli) {
        let name = std::iter::once(cli.get_name().to_string()).chain(path).collect::<Vec<_>>().join(" ");
        let positionals: Vec<&clap::Arg> = command.get_positionals().collect();
        let flags = command.get_arguments().filter(|arg| !arg.is_positional() && matches!(
            arg.get_action(), clap::ArgAction::SetTrue | clap::ArgAction::SetFalse | clap::ArgAction::Count));
        for flag in flags {
            let spelling = match (flag.get_long(), flag.get_short()) {
                (Some(long), _) => format!("--{long}"),
                (None, Some(short)) => format!("-{short}"),
                (None, None) => continue,
            };
            for (index, positional) in positionals.iter().enumerate() {
                let mut line = vec![command.get_name().to_string()];
                for earlier in &positionals[..=index] {
                    if earlier.is_last_set() {
                        line.push("--".to_string());
                    }
                    line.push(value(earlier));
                }
                let pair = format!("`{name}`: {spelling} and <{}>", positional.get_id());
                match command.clone().try_get_matches_from(&line) {
                    Ok(_) => {}
                    Err(error) if error.kind() == ErrorKind::MissingRequiredArgument => {}
                    Err(error) => {
                        failures.push(format!("{pair}: untested, {line:?} does not parse ({:?})", error.kind()));
                        continue;
                    }
                }
                line.insert(1, spelling.clone());
                if let Err(error) = command.clone().try_get_matches_from(&line) {
                    if error.kind() == ErrorKind::ArgumentConflict {
                        failures.push(format!("{pair} are declared to conflict"));
                    }
                }
            }
        }
    }
    failures
}

/// No boolean flag is declared to conflict with a positional argument.
#[test]
fn no_boolean_flag_conflicts_with_a_positional() {
    let failures = boolean_positional_conflicts(&built_cli());
    assert!(failures.is_empty(),
        "a boolean flag that excludes a positional selects another operation; make it a subcommand:\n{}",
        failures.join("\n"));
}

/// The conflict check finds a conflict however it is declared, and nothing else.
#[test]
fn the_conflict_check_finds_every_declaration() {
    use clap::{Arg, ArgAction, ArgGroup, Command};
    let flag = || Arg::new("unset").long("unset").action(ArgAction::SetTrue);
    let key = || Arg::new("key");
    let probe = |sub: Command| {
        let mut root = Command::new("probe").subcommand(sub);
        root.build();
        boolean_positional_conflicts(&root)
    };
    let conflicting = [
        Command::new("on-flag").arg(flag().conflicts_with("key")).arg(key()),
        Command::new("on-positional").arg(flag()).arg(key().conflicts_with("unset")),
        Command::new("by-group").arg(flag()).arg(key()).group(ArgGroup::new("one").args(["unset", "key"])),
        Command::new("exclusive").arg(flag().exclusive(true)).arg(key()),
        Command::new("second").arg(flag().conflicts_with("value")).arg(key().required(true)).arg(Arg::new("value")),
        Command::new("counted").arg(Arg::new("unset").short('u').action(ArgAction::Count).conflicts_with("key")).arg(key()),
    ];
    for sub in conflicting {
        let name = sub.get_name().to_string();
        let failures = probe(sub);
        assert_eq!(failures.len(), 1, "{name}: {failures:?}");
        assert!(failures[0].ends_with("are declared to conflict"), "{name}: {failures:?}");
    }
    let clean = [
        Command::new("clean").arg(flag()).arg(key().required(true)),
        Command::new("valued").arg(Arg::new("into").long("into").conflicts_with("key")).arg(key()),
        Command::new("chosen").arg(flag()).arg(key().value_parser(["remote.url"])),
    ];
    for sub in clean {
        let name = sub.get_name().to_string();
        assert_eq!(probe(sub), Vec::<String>::new(), "{name}");
    }
    // A positional no placeholder value satisfies is reported, never passed silently.
    let untested = probe(Command::new("typed").arg(flag()).arg(key().value_parser(clap::value_parser!(u8).range(5..))));
    assert_eq!(untested.len(), 1, "{untested:?}");
    assert!(untested[0].contains("untested"), "{untested:?}");
}

/// Every top-level name and visible alias with a hyphen in it. A hyphenated top-level name is
/// a family written as one word (`scope-prune` beside `scope`), so the operation belongs inside
/// the family. Hidden aliases are exempt: they keep git's own words, and git's `cherry-pick` is
/// hyphenated. Names below the top level may be hyphenated (`office accept-regenesis`).
fn hyphenated_top_level_names(cli: &clap::Command) -> Vec<String> {
    cli.get_subcommands()
        .flat_map(|command| std::iter::once(command.get_name()).chain(command.get_visible_aliases())
            .filter(|name| name.contains('-'))
            .map(move |name| match name == command.get_name() {
                true => format!("`{name}`"),
                false => format!("`{name}` (a visible alias of `{}`)", command.get_name()),
            }))
        .collect()
}

/// No top-level name or visible alias is hyphenated.
#[test]
fn no_top_level_name_is_hyphenated() {
    let failures = hyphenated_top_level_names(&built_cli());
    assert!(failures.is_empty(),
        "a hyphenated top-level name is a family written as one word; make it a subcommand:\n{}",
        failures.join("\n"));
}

/// The hyphen check finds a hyphenated name or visible alias at the top level, and nothing else.
#[test]
fn the_hyphen_check_finds_only_visible_top_level_hyphens() {
    use clap::Command;
    let probe = |sub: Command| {
        let mut root = Command::new("probe").subcommand(sub);
        root.build();
        hyphenated_top_level_names(&root)
    };
    assert_eq!(probe(Command::new("scope-prune")), vec!["`scope-prune`"]);
    assert_eq!(probe(Command::new("prune").visible_alias("scope-prune")),
        vec!["`scope-prune` (a visible alias of `prune`)"]);
    assert_eq!(probe(Command::new("pick").alias("cherry-pick")), Vec::<String>::new());
    assert_eq!(probe(Command::new("office").subcommand(Command::new("accept-regenesis"))), Vec::<String>::new());
}

/// The paths a list of the spec names. A destructive entry's mode is not part of its path.
fn spec_list(spec: &Spec, list: &str) -> BTreeSet<Vec<String>> {
    spec.lists[list].iter()
        .map(|(_, words)| words.iter().take_while(|word| !word.starts_with('-')).cloned().collect())
        .collect()
}

/// Every command that breaks the recorded-text rule, given the paths of the [recorded-text] list.
/// A listed command takes its text as `-m/--message`, an option with one value, and has no
/// positional named for that text (`description` or `message`: the shape `stack` had); a command
/// off the list has neither `--message` nor `-m`, so recorded text is never taken unlisted. Which
/// of a command's other positionals hold prose is left to the spec review: the check reads names.
fn recorded_text_violations(cli: &clap::Command, listed: &BTreeSet<Vec<String>>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, command) in command_paths(cli) {
        if path.is_empty() {
            continue;
        }
        let name = path.join(" ");
        let message = command.get_arguments().find(|arg| arg.get_long() == Some("message"));
        let takes_m = |arg: &&clap::Arg| arg.get_short() == Some('m')
            || arg.get_all_short_aliases().is_some_and(|aliases| aliases.contains(&'m'));
        let short_m = command.get_arguments().find(takes_m);
        let other_m = command.get_arguments().filter(takes_m).find(|arg| arg.get_long() != Some("message"));
        if !listed.contains(&path) {
            if let Some(arg) = message.or(short_m) {
                failures.push(format!("`{name}` is not on the [recorded-text] list but has `{}`", arg.get_id()));
            }
            continue;
        }
        match message {
            None => failures.push(format!("`{name}` records text but has no --message")),
            Some(arg) => {
                if arg.get_short() != Some('m') {
                    failures.push(format!("`{name}`: --message is not also -m"));
                }
                if !matches!(arg.get_action(), clap::ArgAction::Set) || arg.get_num_args().is_some_and(|range| range.max_values() != 1) {
                    failures.push(format!("`{name}`: --message does not take exactly one value"));
                }
            }
        }
        if other_m.is_some() {
            failures.push(format!("`{name}`: -m is not --message"));
        }
        for positional in command.get_positionals().filter(|arg| ["description", "message"].contains(&arg.get_id().as_str())) {
            failures.push(format!("`{name}` takes its text as the positional <{}> too", positional.get_id()));
        }
    }
    failures
}

/// Every command on the [recorded-text] list takes `-m/--message`, and no command off it does.
#[test]
fn recorded_text_is_always_the_message_option() {
    let listed = spec_list(&parse_spec(SPEC).unwrap(), "recorded-text");
    let failures = recorded_text_violations(&built_cli(), &listed);
    assert!(failures.is_empty(),
        "recorded text is always -m/--message, on exactly the commands surface/spec.txt lists under \
         [recorded-text]:\n{}", failures.join("\n"));
}

/// The recorded-text check finds every way a command can break the rule, and nothing else.
#[test]
fn the_recorded_text_check_finds_each_violation() {
    use clap::{Arg, ArgAction, Command};
    let message = || Arg::new("message").short('m').long("message");
    let probe = |sub: Command, listed: bool| {
        let mut root = Command::new("probe").subcommand(Command::new("haul").subcommand(sub));
        root.build();
        let name = root.find_subcommand("haul").unwrap().get_subcommands().next().unwrap().get_name().to_string();
        let listed = match listed {
            true => BTreeSet::from([vec!["haul".to_string(), name]]),
            false => BTreeSet::new(),
        };
        recorded_text_violations(&root, &listed)
    };
    let broken = [
        (Command::new("positional").arg(Arg::new("description")), true, "has no --message"),
        (Command::new("long-only").arg(Arg::new("message").long("message")), true, "is not also -m"),
        (Command::new("switch").arg(message().action(ArgAction::SetTrue)), true, "exactly one value"),
        (Command::new("many").arg(message().num_args(1..)), true, "exactly one value"),
        (Command::new("both").arg(message()).arg(Arg::new("description")), true, "positional <description>"),
        (Command::new("other-m").arg(Arg::new("message").long("message")).arg(Arg::new("mode").short('m')), true, "-m is not --message"),
        (Command::new("unlisted").arg(message()), false, "is not on the [recorded-text] list"),
        (Command::new("unlisted-m").arg(Arg::new("mode").short('m')), false, "has `mode`"),
        (Command::new("aliased-m").arg(Arg::new("mode").short('o').short_alias('m')), false, "has `mode`"),
    ];
    for (sub, listed, expected) in broken {
        let name = sub.get_name().to_string();
        let failures = probe(sub, listed);
        assert!(failures.iter().any(|failure| failure.contains(expected)), "{name}: {failures:?}");
    }
    assert_eq!(probe(Command::new("review").arg(Arg::new("id")).arg(message()), true), Vec::<String>::new());
    assert_eq!(probe(Command::new("show").arg(Arg::new("id")), false), Vec::<String>::new());
}

/// Every command that breaks the alias rule, given the [daily-loop] list: a visible alias (a name,
/// a short-flag or a long-flag alias) sits only on a top-level command of the daily loop, and on
/// it at most once. Hidden aliases are exempt: Rule 6 keeps git's words, which are invisible.
fn visible_alias_violations(cli: &clap::Command, daily: &BTreeSet<Vec<String>>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, command) in command_paths(cli) {
        let aliases: Vec<String> = command.get_visible_aliases().map(str::to_string)
            .chain(command.get_visible_short_flag_aliases().map(|alias| format!("-{alias}")))
            .chain(command.get_visible_long_flag_aliases().map(|alias| format!("--{alias}")))
            .collect();
        if aliases.is_empty() {
            continue;
        }
        let name = path.join(" ");
        if path.len() > 1 {
            failures.push(format!("`{name}` is below the top level but has the visible alias {aliases:?}"));
        } else if !daily.contains(&path) {
            failures.push(format!("`{name}` is not on the [daily-loop] list but has the visible alias {aliases:?}"));
        } else if aliases.len() > 1 {
            failures.push(format!("`{name}` has more than one visible alias: {aliases:?}"));
        }
    }
    failures
}

/// Visible aliases sit only on the daily loop's commands, at most one each.
#[test]
fn only_the_daily_loop_has_visible_aliases() {
    let daily = spec_list(&parse_spec(SPEC).unwrap(), "daily-loop");
    let failures = visible_alias_violations(&built_cli(), &daily);
    assert!(failures.is_empty(),
        "a visible short alias is something to remember, kept for the commands of the [daily-loop] \
         list in surface/spec.txt, one each; unique prefixes shorten the rest:\n{}", failures.join("\n"));
}

/// The alias check finds a visible alias off the daily loop, or a second one on it, and nothing
/// else.
#[test]
fn the_alias_check_finds_each_violation() {
    use clap::Command;
    let daily = BTreeSet::from([vec!["stack".to_string()], vec!["group".to_string(), "fetch".to_string()]]);
    let probe = |sub: Command| {
        let mut root = Command::new("probe").subcommand(sub);
        root.build();
        visible_alias_violations(&root, &daily)
    };
    let broken = [
        (Command::new("query").visible_alias("q"), "is not on the [daily-loop] list"),
        (Command::new("query").visible_short_flag_alias('q'), "is not on the [daily-loop] list"),
        (Command::new("query").visible_long_flag_alias("q"), "is not on the [daily-loop] list"),
        (Command::new("stack").visible_aliases(["s", "st"]), "more than one visible alias"),
        (Command::new("stack").visible_alias("s").visible_short_flag_alias('s'), "more than one visible alias"),
        (Command::new("group").subcommand(Command::new("fetch").visible_alias("gf")), "`group fetch` is below the top level"),
        (Command::new("group").subcommand(Command::new("stack").visible_alias("s")), "`group stack` is below the top level"),
    ];
    for (sub, expected) in broken {
        let failures = probe(sub);
        assert!(failures.len() == 1 && failures[0].contains(expected), "{expected}: {failures:?}");
    }
    assert_eq!(probe(Command::new("stack").visible_alias("s").aliases(["commit", "ci"])), Vec::<String>::new());
    assert_eq!(probe(Command::new("pick").alias("cherry-pick")), Vec::<String>::new());
}

/// Every command that breaks the confirmation rule, given the paths of the [destructive] list: a
/// listed command has `--confirm` and no `--dry-run`, because its preview is the default, and a
/// command off the list has no `--confirm`. A preview on a command that destroys nothing is
/// spelled `--dry-run`. The check sees that the flag exists, not when it is needed: a listed
/// mode's behaviour is pinned by the test [`MODE_TESTS`] names.
fn destructive_violations(cli: &clap::Command, listed: &BTreeSet<Vec<String>>) -> Vec<String> {
    let mut failures = Vec::new();
    for (path, command) in command_paths(cli) {
        if path.is_empty() {
            continue;
        }
        let name = path.join(" ");
        let has = |long: &str| command.get_arguments().any(|arg| arg.get_long() == Some(long)
            || arg.get_all_aliases().is_some_and(|aliases| aliases.contains(&long)));
        if listed.contains(&path) {
            if !has("confirm") {
                failures.push(format!("`{name}` is destructive but has no --confirm"));
            }
            if has("dry-run") {
                failures.push(format!("`{name}` is destructive, so its preview is the default, but it has --dry-run"));
            }
        } else if has("confirm") {
            failures.push(format!("`{name}` is not on the [destructive] list but has --confirm"));
        }
    }
    failures
}

/// Every command on the [destructive] list confirms with `--confirm` and previews by default, and
/// no command off it has `--confirm`.
#[test]
fn destructive_commands_confirm_and_nothing_else_does() {
    let listed = spec_list(&parse_spec(SPEC).unwrap(), "destructive");
    let failures = destructive_violations(&built_cli(), &listed);
    assert!(failures.is_empty(),
        "a destructive operation shows what it would destroy unless --confirm is given, and only the \
         commands surface/spec.txt lists under [destructive] take --confirm:\n{}", failures.join("\n"));
}

/// The confirmation check finds every way a command can break the rule, and nothing else.
#[test]
fn the_destructive_check_finds_each_violation() {
    use clap::{Arg, ArgAction, Command};
    let confirm = || Arg::new("confirm").long("confirm").action(ArgAction::SetTrue);
    let dry_run = || Arg::new("dry_run").long("dry-run").action(ArgAction::SetTrue);
    let probe = |sub: Command, listed: bool| {
        let mut root = Command::new("probe").subcommand(Command::new("scope").subcommand(sub));
        root.build();
        let name = root.find_subcommand("scope").unwrap().get_subcommands().next().unwrap().get_name().to_string();
        let listed = match listed {
            true => BTreeSet::from([vec!["scope".to_string(), name]]),
            false => BTreeSet::new(),
        };
        destructive_violations(&root, &listed)
    };
    let broken = [
        (Command::new("prune").arg(Arg::new("path")), true, "has no --confirm"),
        (Command::new("prune").arg(confirm()).arg(dry_run()), true, "but it has --dry-run"),
        (Command::new("prune").arg(Arg::new("yes").long("yes").alias("confirm")).arg(dry_run()), true, "but it has --dry-run"),
        (Command::new("evict").arg(confirm()), false, "is not on the [destructive] list"),
        (Command::new("evict").arg(Arg::new("yes").long("yes").alias("confirm")), false, "is not on the [destructive] list"),
    ];
    for (sub, listed, expected) in broken {
        let name = sub.get_name().to_string();
        let failures = probe(sub, listed);
        assert!(failures.len() == 1 && failures[0].contains(expected), "{name} ({expected}): {failures:?}");
    }
    assert_eq!(probe(Command::new("prune").arg(confirm()), true), Vec::<String>::new());
    assert_eq!(probe(Command::new("evict").arg(dry_run()), false), Vec::<String>::new());
    assert_eq!(probe(Command::new("accept").arg(Arg::new("confirm").long("confirm").value_name("GENESIS")), true), Vec::<String>::new());
}

/// The destructive entries that name a mode, each with the end-to-end test that runs the command
/// in that mode without `--confirm` (expecting the preview and a non-zero exit, nothing changed)
/// and outside it (expecting no `--confirm` to be needed), as `file::test`.
const MODE_TESTS: &[(&str, &str)] = &[
    ("lower --keep …=theirs", "tests/remote.rs::lower_needs_confirm_only_to_take_theirs"),
];

/// Every mode the [destructive] list names has a behaviour test, and the test exists: the flag
/// check above cannot tell a mode that previews from one that acts.
#[test]
fn every_destructive_mode_has_its_behaviour_test() {
    let spec = parse_spec(SPEC).unwrap();
    let modes: BTreeSet<String> = spec.lists["destructive"].iter()
        .filter(|(_, words)| words.iter().any(|word| word.starts_with('-')))
        .map(|(_, words)| words.join(" "))
        .collect();
    let tested: BTreeSet<String> = MODE_TESTS.iter().map(|(mode, _)| mode.to_string()).collect();
    assert_eq!(modes, tested, "each destructive mode in surface/spec.txt needs its row in MODE_TESTS, and only those");
    for (mode, test) in MODE_TESTS {
        let (file, name) = test.split_once("::").unwrap();
        let source = std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(file))
            .unwrap_or_else(|e| panic!("{mode}: cannot read {file}: {e}"));
        let declared = source.lines().any(|line| line.trim_start().starts_with(&format!("fn {name}()")));
        assert!(declared, "{mode}: {file} declares no `fn {name}()`");
    }
}
