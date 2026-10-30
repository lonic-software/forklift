//! Unique-prefix matching of command names: `ha` is `haul`, `haul rev` is `haul review`. At every
//! level of the command tree, a word that is not an exact name or alias is taken as the one
//! visible command whose name starts with it. An exact name or alias always wins, so `s` stays
//! stack's alias although it starts seven commands; a prefix of two or more names is an error
//! listing them, never a guess. Hidden aliases (git's words) match only when spelled out: they are
//! there for typing git's word, and a prefix of one would be a short form nobody can see. Visible
//! aliases are typed whole too: they are already short, and a prefix is of a command's name. Flags
//! are never expanded.
//!
//! Prefixes are for typing. Adding a command can make a working prefix ambiguous, so every machine
//! surface spells full names, and the checks that hold hints and MCP argv to the tree parse with
//! clap alone, where no prefix resolves.
//!
//! clap's own `infer_subcommands` is not used: it lets hidden aliases take part, and it reports an
//! ambiguous prefix as an unknown command with a fuzzy list of similar names rather than the
//! commands the prefix starts.

use std::ffi::OsString;
use clap::Command;
use clap::error::ErrorKind;

/// What a word names among one command's subcommands.
pub enum Resolved<'a> {
    /// The word is this subcommand's name or alias, or a prefix of its name and of no other.
    Command(&'a Command),
    /// The word starts the names of all of these subcommands, and is no name or alias itself.
    Ambiguous(Vec<&'a str>),
    /// No subcommand has this name or alias, and no visible one starts with it.
    Unknown,
}

/// The subcommand of `parent` that `word` names, exactly or by a unique prefix of a visible name.
pub fn subcommand<'a>(parent: &'a Command, word: &str) -> Resolved<'a> {
    if let Some(exact) = parent.find_subcommand(word) {
        return Resolved::Command(exact);
    }
    if word.is_empty() {
        return Resolved::Unknown;
    }
    let mut starts: Vec<&Command> = parent.get_subcommands()
        .filter(|command| !command.is_hide_set() && command.get_name().starts_with(word))
        .collect();
    match starts.len() {
        0 => Resolved::Unknown,
        1 => Resolved::Command(starts.remove(0)),
        _ => Resolved::Ambiguous(starts.iter().map(|command| command.get_name()).collect()),
    }
}

/// The message for a word that starts more than one command's name, naming every one of them in
/// full: `parent` is the path of the command whose subcommands they are (`["haul"]`, or empty).
pub fn ambiguity(word: &str, parent: &[&str], names: &[&str]) -> String {
    let full: Vec<String> = names.iter()
        .map(|name| format!("\"{}\"", std::iter::once("forklift").chain(parent.iter().copied())
            .chain(std::iter::once(*name)).collect::<Vec<_>>().join(" ")))
        .collect();
    let (last, rest) = full.split_last().expect("an ambiguous prefix starts at least two names");
    format!("\"{word}\" could be {} or {last}; type more of the name", rest.join(", "))
}

/// Replace every command word in `args` that is a unique prefix with the full name, level by
/// level, so clap parses the command the user meant. `command` must be built, so that every
/// subcommand carries the global options: the walk reads each level's options to step over the
/// value an option takes as its own word (`--color never`). The walk stops at `--`, at a command
/// with no subcommands, and at a word no subcommand matches, which clap then reports as it always
/// has. An ambiguous prefix is a usage error raised on the command whose subcommands it starts;
/// the words before it are already expanded then, so they still name the command they reached.
pub fn expand(command: &Command, args: &mut [OsString]) -> Result<(), clap::Error> {
    let mut level = command;
    let mut path: Vec<&str> = Vec::new();
    let mut index = 1;
    while index < args.len() && level.has_subcommands() {
        let Some(word) = args[index].to_str().map(str::to_string) else {
            break;
        };
        if word == "--" {
            break;
        }
        if word.starts_with('-') && word != "-" {
            index += if takes_next_word(level, &word) { 2 } else { 1 };
            continue;
        }
        match subcommand(level, &word) {
            Resolved::Command(sub) => {
                args[index] = OsString::from(sub.get_name());
                path.push(sub.get_name());
                level = sub;
            }
            Resolved::Ambiguous(names) => {
                return Err(level.clone().error(ErrorKind::InvalidSubcommand, ambiguity(&word, &path, &names)));
            }
            Resolved::Unknown => break,
        }
        index += 1;
    }
    Ok(())
}

/// Whether the option word `word` (`--color`, `-v`, a cluster `-vc`) takes the next word as its
/// value: a long option that takes values, written without `=VALUE`, or a short cluster whose last
/// letter takes values. An option `level` does not have takes nothing; clap reports it.
pub fn takes_next_word(level: &Command, word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        return !long.contains('=') && level.get_arguments().any(|arg| arg.get_action().takes_values()
            && (arg.get_long() == Some(long) || arg.get_all_aliases().is_some_and(|aliases| aliases.contains(&long))));
    }
    // In a cluster, the first letter that takes a value takes the rest of the word as that value,
    // and the next word only when nothing follows it.
    let letters: Vec<char> = word[1..].chars().collect();
    for (position, letter) in letters.iter().enumerate() {
        let takes = level.get_arguments().any(|arg| arg.get_action().takes_values()
            && (arg.get_short() == Some(*letter)
                || arg.get_all_short_aliases().is_some_and(|aliases| aliases.contains(letter))));
        if takes {
            return position == letters.len() - 1;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use clap::CommandFactory;
    use clap::error::ErrorKind;
    use crate::cli::Cli;

    fn built() -> clap::Command {
        let mut cli = Cli::command();
        cli.build();
        cli
    }

    /// The argv `line` expands to, as words.
    fn expand(line: &str) -> Result<Vec<String>, clap::Error> {
        let mut args: Vec<OsString> = std::iter::once("forklift").chain(line.split_whitespace()).map(OsString::from).collect();
        super::expand(&built(), &mut args)
            .map(|()| args.into_iter().skip(1).map(|arg| arg.into_string().unwrap()).collect())
    }

    fn expands_to(line: &str, expected: &str) {
        let got = expand(line).unwrap_or_else(|error| panic!("`{line}`: {error}"));
        assert_eq!(got.join(" "), expected, "`{line}`");
    }

    /// A unique prefix becomes the full name, at the top level and below it.
    #[test]
    fn a_unique_prefix_is_the_command_it_starts() {
        expands_to("ha", "haul");
        expands_to("ha rev", "haul review");
        expands_to("hau rev --approve", "haul review --approve");
        expands_to("off adm", "office admit");
    }

    /// An exact name or alias wins over the longer names it starts, hidden aliases included.
    #[test]
    fn an_exact_name_or_alias_wins() {
        // `s` starts stack, stocktake, store, show, shift, scope and more; it is stack's alias.
        expands_to("s", "stack");
        expands_to("st", "stocktake");
        // `sh` starts show and shift, and is shift's alias.
        expands_to("sh", "shift");
        // `scope` is a name and starts `scope-prune`.
        expands_to("scope st", "scope status");
        // Hidden git words, spelled out.
        expands_to("commit", "stack");
        expands_to("status", "stocktake");
    }

    /// A hidden alias takes no part in prefix matching: `comm` is no prefix of a visible name, so
    /// it is left for clap to report, although it starts the hidden `commit`.
    #[test]
    fn a_hidden_alias_matches_only_when_spelled_out() {
        expands_to("comm", "comm");
        expands_to("anno", "anno");
        // A hidden command is reached by its full name only.
        expands_to("__d", "__d");
        expands_to("__docgen", "__docgen");
    }

    /// A prefix of several names is a usage error naming exactly those names, on the command whose
    /// subcommands they are.
    #[test]
    fn an_ambiguous_prefix_names_every_candidate() {
        let error = expand("he").unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidSubcommand);
        let text = error.render().to_string();
        assert!(text.contains("\"he\" could be \"forklift heal\" or \"forklift help\";"), "{text}");

        let error = expand("haul re").unwrap_err();
        let text = error.render().to_string();
        assert!(text.contains("\"re\" could be \"forklift haul review\" or \"forklift haul reopen\";"), "{text}");
        assert!(text.contains("forklift haul"), "the usage line is haul's: {text}");
    }

    /// Flags, the values options take, and everything after `--` are left as they are.
    #[test]
    fn flags_and_values_are_never_expanded() {
        // `--js` is no flag, and `ha` after `--color` is the option's value, not a command.
        expands_to("--js ha", "--js haul");
        expands_to("--color ha", "--color ha");
        expands_to("--color=never ha", "--color=never haul");
        expands_to("-v ha l", "-v haul list");
        expands_to("-- ha", "-- ha");
        // A word past a command with no subcommands is an argument.
        expands_to("hist ha", "history ha");
        // Options between levels are stepped over.
        expands_to("ha --json l", "haul --json list");

        // The ambiguity error keeps the words before it expanded, so they name what they reached.
        let mut args: Vec<OsString> = ["forklift", "ha", "re"].map(OsString::from).to_vec();
        assert!(super::expand(&built(), &mut args).is_err());
        assert_eq!(args[1], "haul");
    }

    /// A short option that takes a value takes the next word only when it ends its cluster;
    /// anywhere earlier, the rest of the cluster is its value. No family has such an option yet, so
    /// a tree of its own holds the rule, its short alias included.
    #[test]
    fn a_short_option_takes_the_next_word_only_at_the_end_of_its_cluster() {
        let mut tree = clap::Command::new("t")
            .arg(clap::Arg::new("flag").short('v').action(clap::ArgAction::SetTrue))
            .arg(clap::Arg::new("value").short('c').short_alias('k').action(clap::ArgAction::Set))
            .subcommand(clap::Command::new("haul"));
        tree.build();
        let walk = |line: &[&str]| {
            let mut args: Vec<OsString> = line.iter().map(OsString::from).collect();
            super::expand(&tree, &mut args).unwrap();
            args.into_iter().map(|arg| arg.into_string().unwrap()).collect::<Vec<_>>().join(" ")
        };
        assert_eq!(walk(&["t", "-c", "ha"]), "t -c ha");
        assert_eq!(walk(&["t", "-vc", "ha"]), "t -vc ha");
        assert_eq!(walk(&["t", "-k", "ha"]), "t -k ha");
        assert_eq!(walk(&["t", "-cv", "ha"]), "t -cv haul");
        assert_eq!(walk(&["t", "-cx", "ha"]), "t -cx haul");
        assert_eq!(walk(&["t", "-v", "ha"]), "t -v haul");
    }

    /// The resolver steps over an option's value by reading whether the option takes one, so an
    /// option of a command with subcommands must take exactly one word, and such a command must
    /// take no positional argument a command word could be mistaken for.
    #[test]
    fn every_family_lets_the_walk_find_its_subcommand() {
        let cli = built();
        let mut families = vec![&cli];
        let mut seen = 0;
        while let Some(family) = families.pop() {
            seen += 1;
            for sub in family.get_subcommands() {
                if sub.has_subcommands() {
                    families.push(sub);
                }
            }
            for arg in family.get_arguments() {
                let name = family.get_name();
                assert!(!arg.is_positional(), "`{name}` has subcommands and the positional {}", arg.get_id());
                if arg.get_action().takes_values() {
                    assert!(!arg.is_require_equals_set(),
                        "`{name} --{}` needs `=`; the walk steps over the next word", arg.get_id());
                    let range = arg.get_num_args().expect("built");
                    assert!(range.min_values() == 1 && range.max_values() == 1,
                        "`{name} --{}` takes {range:?} values; the walk steps over exactly one",
                        arg.get_id());
                }
            }
        }
        assert!(seen > 10, "the walk saw only {seen} commands with subcommands");
    }
}
