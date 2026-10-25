//! Terminal colour. Human output is coloured only when a person is likely reading it, the way git
//! decides: `--color=auto` (the default) colours when stdout is a terminal, unless `NO_COLOR` is
//! set to a non-empty value (<https://no-color.org>) or `TERM` is `dumb`, or is unset outside
//! Windows (where a console sets no `TERM`). `--color=always` and `--color=never` override all of
//! these, so a flag on the command line beats the environment. clap's own help and usage errors
//! follow the same flag ([`from_args`]).
//!
//! The decision is made once, in `main`, before the pager is wired in: once it is, stdout is the
//! pager's pipe rather than the terminal, and paged output should keep its colour (`less` is run
//! with `R`, which shows it).

use std::fmt::{self, Display};
use std::sync::OnceLock;

/// When to colour human output (`--color`). The variants' doc comments are help text.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Color on a terminal, unless NO_COLOR is non-empty or TERM is unset or `dumb`.
    Auto,
    /// Always color, even into a pipe or a file.
    Always,
    /// Never color.
    Never,
}

impl ColorChoice {
    /// The same choice for clap, whose own help and usage errors it governs.
    pub fn for_clap(self) -> clap::ColorChoice {
        match self {
            ColorChoice::Auto => clap::ColorChoice::Auto,
            ColorChoice::Always => clap::ColorChoice::Always,
            ColorChoice::Never => clap::ColorChoice::Never,
        }
    }
}

/// The `--color` value on a raw command line, read before clap parses it, so clap's own help and
/// usage errors can follow it. Global, so it may sit anywhere before a `--` terminator, as
/// `--color WHEN` or `--color=WHEN`; the last one counts. `None` when it is absent or its value is
/// not one clap would accept (clap then reports that).
pub fn from_args(args: &[std::ffi::OsString]) -> Option<ColorChoice> {
    use clap::ValueEnum;

    let mut words = args.iter().skip(1).map(|arg| arg.to_string_lossy())
        .take_while(|arg| arg != "--");
    let mut chosen = None;
    while let Some(word) = words.next() {
        let value = match word.strip_prefix("--color") {
            Some("") => words.next(),
            Some(rest) => rest.strip_prefix('=').map(|value| value.to_string().into()),
            None => continue,
        };
        chosen = value.map(|value| ColorChoice::from_str(&value, false).ok());
    }
    chosen.flatten()
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

static CHOSEN: OnceLock<ColorChoice> = OnceLock::new();

/// Decide, once for the process, whether human output is coloured. Called from `main` before the
/// pager is set up; a second call is ignored. Until it runs, nothing is coloured.
pub fn init(choice: ColorChoice) {
    use std::io::IsTerminal;

    let enabled = enabled_from(choice, std::io::stdout().is_terminal(), cfg!(windows), |name| std::env::var_os(name));
    let _ = ENABLED.set(enabled);
    let _ = CHOSEN.set(choice);
}

/// The `--color` value [`init`] was given (`auto` before it runs), for help that clap renders
/// after parsing, as `forklift help` does.
pub fn chosen() -> ColorChoice {
    CHOSEN.get().copied().unwrap_or(ColorChoice::Auto)
}

/// [`enabled_when`], reading `NO_COLOR` and `TERM` through `var`.
fn enabled_from(choice: ColorChoice,
                terminal: bool,
                windows: bool,
                var: impl Fn(&str) -> Option<std::ffi::OsString>) -> bool {
    enabled_when(choice, terminal, windows, var("NO_COLOR").as_deref(), var("TERM").as_deref())
}

/// Whether the process colours its human output.
pub fn enabled() -> bool {
    *ENABLED.get().unwrap_or(&false)
}

/// The colour decision itself, from its inputs: whether stdout is a terminal, whether this is
/// Windows, and the values of `NO_COLOR` and `TERM`.
pub fn enabled_when(choice: ColorChoice,
                    terminal: bool,
                    windows: bool,
                    no_color: Option<&std::ffi::OsStr>,
                    term: Option<&std::ffi::OsStr>) -> bool {
    match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            terminal
                && no_color.is_none_or(|value| value.is_empty())
                && term.map_or(windows, |value| value != "dumb")
        }
    }
}

/// `value` in `style`: wrapped in the style's escape sequence and a reset when colour is on, and
/// printed bare when it is off. A width or fill applies to the value, inside the escapes.
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
            return self.value.fmt(f);
        }

        let faint = match (code.is_empty(), faint) {
            (_, false) => "",
            (true, true) => "2",
            (false, true) => ";2",
        };
        write!(f, "\x1b[{}{}m", code, faint)?;
        self.value.fmt(f)?;
        f.write_str("\x1b[0m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    #[test]
    fn auto_colours_only_a_terminal_without_no_color_or_a_dumb_term() {
        let auto = |terminal, windows, no_color: Option<&str>, term: Option<&str>| {
            enabled_when(ColorChoice::Auto, terminal, windows, no_color.map(OsStr::new), term.map(OsStr::new))
        };

        for windows in [false, true] {
            assert!(auto(true, windows, None, Some("xterm-256color")));
            assert!(auto(true, windows, Some(""), Some("xterm")), "an empty NO_COLOR does not count as set");

            assert!(!auto(false, windows, None, Some("xterm")), "a pipe or a file is not coloured");
            assert!(!auto(true, windows, Some("1"), Some("xterm")));
            assert!(!auto(true, windows, Some("0"), Some("xterm")), "any non-empty NO_COLOR disables colour");
            assert!(!auto(true, windows, None, Some("dumb")));
        }

        // A Windows console sets no TERM; elsewhere an unset TERM is a dumb one, as git reads it.
        assert!(auto(true, true, None, None));
        assert!(!auto(true, false, None, None));
    }

    #[test]
    fn always_and_never_override_the_terminal_and_the_environment() {
        for (terminal, windows) in [(false, false), (true, false), (true, true)] {
            for no_color in [None, Some(OsStr::new("1"))] {
                for term in [None, Some(OsStr::new("dumb"))] {
                    assert!(enabled_when(ColorChoice::Always, terminal, windows, no_color, term));
                    assert!(!enabled_when(ColorChoice::Never, terminal, windows, no_color, term));
                }
            }
        }
    }

    /// `init` reads NO_COLOR and TERM by those names, each into its own place.
    #[test]
    fn the_decision_reads_no_color_and_term_by_name() {
        let with = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(key, _)| *key == name).map(|(_, value)| value.into())
        };

        assert!(enabled_from(ColorChoice::Auto, true, false, with(&[("TERM", "xterm")])));
        assert!(!enabled_from(ColorChoice::Auto, true, false, with(&[("TERM", "xterm"), ("NO_COLOR", "1")])));
        assert!(!enabled_from(ColorChoice::Auto, true, true, with(&[("TERM", "dumb")])));
        assert!(!enabled_from(ColorChoice::Auto, true, false, with(&[("NOCOLOR", "1"), ("COLORTERM", "dumb")])));
        assert!(enabled_from(ColorChoice::Auto, true, true, with(&[("NOCOLOR", "1"), ("COLORTERM", "dumb")])));
    }

    #[test]
    fn from_args_reads_the_last_color_before_a_terminator() {
        let read = |args: &[&str]| {
            from_args(&std::iter::once("forklift").chain(args.iter().copied()).map(Into::into).collect::<Vec<_>>())
        };

        assert_eq!(read(&["diff"]), None);
        assert_eq!(read(&["--color", "never", "diff"]), Some(ColorChoice::Never));
        assert_eq!(read(&["diff", "--color=always"]), Some(ColorChoice::Always));
        assert_eq!(read(&["--color=never", "diff", "--color", "auto"]), Some(ColorChoice::Auto));
        assert_eq!(read(&["--color", "sometimes"]), None, "clap reports a bad value");
        assert_eq!(read(&["--color"]), None);
        assert_eq!(read(&["--colorful", "x"]), None);
        assert_eq!(read(&["diff", "--", "--color", "never"]), None);
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

        let padded = |enabled| format!("[{:<3}]", Painted { style: YELLOW, value: "x", enabled });
        assert_eq!(padded(false), "[x  ]");
        assert_eq!(padded(true), "[\x1b[33mx  \x1b[0m]");
    }

    /// The lines of `source` outside its test modules that spell ESC or CSI, by number. A test
    /// module is a `mod tests {` line under a `#[cfg(...test...)]` one, through the next line that
    /// is a lone `}` (house style keeps its body indented).
    fn escapes_outside_tests(source: &str) -> Vec<usize> {
        const SPELLINGS: [&str; 8] = ["\\x1b", "\\033", "\\x9b", "0x1b", "0o33", "27u8", "27_u8", "\u{1b}"];
        // `\u{1b}` or `\u{9b}`, padded with any number of zeros.
        let unicode = |line: &str| line.match_indices("\\u{").any(|(at, _)| {
            let digits = line[at + 3..].trim_start_matches('0');
            digits.starts_with("1b}") || digits.starts_with("9b}")
        });

        let lines: Vec<&str> = source.lines().collect();
        let mut in_tests = false;
        let mut found = Vec::new();
        for (at, line) in lines.iter().enumerate() {
            let item = line.trim_start_matches("pub(crate) ");
            if item == "mod tests {" && at > 0 && lines[at - 1].starts_with("#[cfg(") && lines[at - 1].contains("test") {
                in_tests = true;
            } else if in_tests {
                in_tests = *line != "}";
            } else {
                let line = line.to_ascii_lowercase();
                let spelt = SPELLINGS.iter().any(|spelling| line.contains(spelling)) || unicode(&line)
                    || line.contains("27 as char") || line.contains("char::from(27");
                if spelt {
                    found.push(at + 1);
                }
            }
        }
        found
    }

    /// Every escape the CLI prints goes through this module, so `--color` and `NO_COLOR` govern
    /// all of them: no other source file of the CLI spells ESC or CSI outside its test modules, in
    /// any spelling [`escapes_outside_tests`] knows (a value computed at run time escapes it).
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
    fn the_escape_scan_skips_test_modules_only() {
        let source = "fn a() {}\nconst X: &str = \"\\u{1b}[1m\";\n#[cfg(test)]\nmod tests {\n    \"\\x1b[2J\"\n}\nconst Y: u8 = 0x1B;\n";
        assert_eq!(escapes_outside_tests(source), vec![2, 7], "the code after a test module is scanned");

        let gated = "#[cfg(all(test, unix))]\npub(crate) mod tests {\n    \"\\033[0m\"\n}\n";
        assert_eq!(escapes_outside_tests(gated), Vec::<usize>::new());
        assert_eq!(escapes_outside_tests("\"\\033[0m\"\n#[cfg(test)]\nmod test_support;\n\"\\x1B\""), vec![1, 4]);

        let spellings = ["\\u{1b}[1m", "\\u{001B}[1m", "\\u{00001b}", "\\x9b1m", "\\u{9B}", "\\u{009b}31m",
                         "27 as char", "char::from(27)", "27u8", "27_u8", "0o33", "\u{1b}[1m"];
        for spelling in spellings {
            assert_eq!(escapes_outside_tests(&format!("let x = \"{}\";", spelling)), vec![1], "{:?}", spelling);
        }
        assert_eq!(escapes_outside_tests("let x = \"\\u{2b}\\u{10b}\";"), Vec::<usize>::new());
    }
}
