//! Terminal colour. Human output is coloured only when a person is likely reading it, the way git
//! decides: `--color=auto` (the default) colours when stdout is a terminal, unless `NO_COLOR` is
//! set to a non-empty value (<https://no-color.org>) or `TERM` is `dumb`. `--color=always` and
//! `--color=never` override all three, so a flag on the command line beats the environment.
//!
//! The decision is made once, in `main`, before the pager is wired in: once it is, stdout is the
//! pager's pipe rather than the terminal, and paged output should keep its colour (`less` is run
//! with `R`, which shows it).

use std::fmt::{self, Display};
use std::sync::OnceLock;

/// When to colour human output (`--color`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour when stdout is a terminal, `NO_COLOR` is unset or empty, and `TERM` is not `dumb`.
    Auto,
    /// Always colour, even into a pipe or a file.
    Always,
    /// Never colour.
    Never,
}

/// Bold, for a block's header line.
pub const BOLD: Style = Style::new("1");

/// Yellow, for a parcel or haul id that leads an entry.
pub const YELLOW: Style = Style::new("33");

/// Green, for an added line.
pub const GREEN: Style = Style::new("32");

/// Red, for a removed line.
pub const RED: Style = Style::new("31");

/// No styling: the value prints as it is, colour on or off.
pub const PLAIN: Style = Style::new("");

/// How a value is rendered when colour is on: an SGR parameter (`33`, what goes between `ESC [`
/// and `m`), optionally made faint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    code: &'static str,
    faint: bool,
}

impl Style {
    const fn new(code: &'static str) -> Style {
        Style { code, faint: false }
    }

    /// The same style, rendered faint (for a change that is whitespace only).
    pub fn faint(self) -> Style {
        Style { faint: true, ..self }
    }
}

static ENABLED: OnceLock<bool> = OnceLock::new();

/// Decide, once for the process, whether human output is coloured. Called from `main` before the
/// pager is set up; a second call is ignored. Until it runs, nothing is coloured.
pub fn init(choice: ColorChoice) {
    use std::io::IsTerminal;

    let enabled = enabled_when(
        choice,
        std::io::stdout().is_terminal(),
        std::env::var_os("NO_COLOR").as_deref(),
        std::env::var_os("TERM").as_deref(),
    );
    let _ = ENABLED.set(enabled);
}

/// Whether the process colours its human output.
pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

/// The colour decision itself, from its inputs: whether stdout is a terminal and the values of
/// `NO_COLOR` and `TERM`.
pub fn enabled_when(choice: ColorChoice,
                    terminal: bool,
                    no_color: Option<&std::ffi::OsStr>,
                    term: Option<&std::ffi::OsStr>) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            terminal
                && no_color.is_none_or(|value| value.is_empty())
                && term.is_none_or(|value| value != "dumb")
        }
    }
}

/// `value` in `style`: wrapped in the style's escape sequence and a reset when colour is on, and
/// printed bare when it is off.
pub fn paint<T: Display>(style: Style, value: T) -> Painted<T> {
    Painted { style, value, enabled: enabled() }
}

/// A value [`paint`] has styled, ready to format.
pub struct Painted<T> {
    style: Style,
    value: T,
    enabled: bool,
}

impl<T: Display> Display for Painted<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Style { code, faint } = self.style;
        if !self.enabled || (code.is_empty() && !faint) {
            return write!(f, "{}", self.value);
        }

        let faint = match (code.is_empty(), faint) {
            (_, false) => "",
            (true, true) => "2",
            (false, true) => ";2",
        };
        write!(f, "\x1b[{}{}m{}\x1b[0m", code, faint, self.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn auto_colours_only_a_terminal_without_no_color_or_a_dumb_term() {
        let auto = |terminal, no_color: Option<&str>, term: Option<&str>| {
            enabled_when(ColorChoice::Auto, terminal, no_color.map(OsStr::new), term.map(OsStr::new))
        };

        assert!(auto(true, None, None));
        assert!(auto(true, None, Some("xterm-256color")));
        assert!(auto(true, Some(""), None), "an empty NO_COLOR does not count as set");

        assert!(!auto(false, None, None), "a pipe or a file is not coloured");
        assert!(!auto(true, Some("1"), None));
        assert!(!auto(true, Some("0"), None), "any non-empty NO_COLOR disables colour");
        assert!(!auto(true, None, Some("dumb")));
    }

    #[test]
    fn always_and_never_override_the_terminal_and_the_environment() {
        for terminal in [false, true] {
            for no_color in [None, Some(OsStr::new("1"))] {
                for term in [None, Some(OsStr::new("dumb"))] {
                    assert!(enabled_when(ColorChoice::Always, terminal, no_color, term));
                    assert!(!enabled_when(ColorChoice::Never, terminal, no_color, term));
                }
            }
        }
    }

    #[test]
    fn painted_wraps_only_when_enabled_and_styled() {
        let painted = |style, enabled| Painted { style, value: "x", enabled }.to_string();

        assert_eq!(painted(YELLOW, true), "\x1b[33mx\x1b[0m");
        assert_eq!(painted(GREEN.faint(), true), "\x1b[32;2mx\x1b[0m");
        assert_eq!(painted(PLAIN.faint(), true), "\x1b[2mx\x1b[0m");
        assert_eq!(painted(PLAIN, true), "x");
        assert_eq!(painted(YELLOW, false), "x");
        assert_eq!(painted(BOLD, true), "\x1b[1mx\x1b[0m");
        assert_eq!(painted(RED.faint(), false), "x");
    }

    /// The lines of `source` before its test module that spell an escape character, by number.
    fn escapes_outside_tests(source: &str) -> Vec<usize> {
        const SPELLINGS: [&str; 5] = ["\\x1b", "\\x1B", "\\u{1b}", "\\u{1B}", "\\033"];

        let lines: Vec<&str> = source.lines().collect();
        let tests = (1..lines.len()).find(|&at| {
            let item = lines[at].trim_start_matches("pub(crate) ");
            item == "mod tests {" && lines[at - 1].starts_with("#[cfg(") && lines[at - 1].contains("test")
        });

        lines[..tests.unwrap_or(lines.len())].iter().enumerate()
            .filter(|(_, line)| SPELLINGS.iter().any(|spelling| line.contains(spelling)))
            .map(|(at, _)| at + 1)
            .collect()
    }

    /// Every escape the CLI prints goes through this module, so `--color` and `NO_COLOR` govern
    /// all of them: no other source file of the CLI spells an escape outside its test module.
    #[test]
    fn no_other_cli_source_spells_an_escape() {
        let mut pending = vec![std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src")];
        let mut scanned = 0;
        let mut found = Vec::new();
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|entry| entry.unwrap().path()));
            } else if path.extension().is_some_and(|extension| extension == "rs")
                && !path.ends_with("src/color.rs") {
                let source = std::fs::read_to_string(&path).unwrap();
                scanned += 1;
                found.extend(escapes_outside_tests(&source).into_iter().map(|line| (path.clone(), line)));
            }
        }

        assert!(scanned > 20, "scanned only {} files", scanned);
        assert!(found.is_empty(), "print escapes through crate::color: {:?}", found);
    }

    #[test]
    fn the_escape_scan_reads_up_to_the_test_module_only() {
        let source = "fn a() {}\nconst X: &str = \"\\u{1b}[1m\";\n#[cfg(test)]\nmod tests {\n    \"\\x1b[2J\"\n}\n";
        assert_eq!(escapes_outside_tests(source), vec![2]);

        let gated = "#[cfg(all(test, unix))]\npub(crate) mod tests {\n    \"\\033[0m\"\n}\n";
        assert_eq!(escapes_outside_tests(gated), Vec::<usize>::new());
        assert_eq!(escapes_outside_tests("\"\\033[0m\"\n#[cfg(test)]\nmod test_support;\n\"\\x1B\""), vec![1, 4]);
    }
}
