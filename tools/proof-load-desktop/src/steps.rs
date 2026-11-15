//! A row's steps as the step screen prints them: each command the scenario ran, then what it
//! printed, then how it ended. The steps are the library's (`Row::steps`); this only lays them out
//! as lines, so it is tested without egui.

use proof_load::tool::Outcome;

use crate::widgets::{Ink, ScreenLine};

/// The most lines one stream of one step shows; the rest are counted, not printed.
pub const STREAM_LINES: usize = 40;

/// The step screen's lines for `steps`, each a forklift command, whichever binding the row
/// faulted (`Row::binding`). A refusal prints its message red and its
/// next step after a dim label, in place of the JSON document it came in; anything else on stdout
/// prints plain, stderr amber, and every step ends with a dim line saying how it exited.
pub fn lines(steps: &[Outcome]) -> Vec<ScreenLine> {
    let mut lines = Vec::new();
    if steps.is_empty() {
        lines.push(ScreenLine::new(Ink::Dim, "the row ran no commands"));
    }
    for step in steps {
        let mut command = "forklift".to_string();
        for argument in &step.arguments {
            command.push(' ');
            command.push_str(&quoted(argument));
        }
        lines.push(ScreenLine::command(&command));
        match &step.failure {
            Some(failure) => {
                lines.push(ScreenLine::new(Ink::Red, format!("error: {}", failure.message)));
                if let Some(next) = &failure.next_step {
                    lines.push(ScreenLine::new(Ink::Dim, "  next step:").then(Ink::Plain, format!(" {next}")));
                }
            }
            None => stream(&mut lines, &step.stdout, Ink::Plain),
        }
        stream(&mut lines, &step.stderr, Ink::Amber);
        lines.push(ScreenLine::new(Ink::Dim, match step.exit {
            Some(code) => format!("  exit {code}"),
            None => "  ended on a signal".to_string(),
        }));
    }
    lines
}

/// `bytes` as lines in `ink`, at most `STREAM_LINES` of them, then how many more there were.
fn stream(lines: &mut Vec<ScreenLine>, bytes: &[u8], ink: Ink) {
    let text = String::from_utf8_lossy(bytes);
    let all: Vec<&str> = text.lines().collect();
    for line in all.iter().take(STREAM_LINES) {
        lines.push(ScreenLine::new(ink, *line));
    }
    if all.len() > STREAM_LINES {
        lines.push(ScreenLine::new(Ink::Dim, format!("… {} more lines", all.len() - STREAM_LINES)));
    }
}

/// An argument as a shell would need it typed: as it is when that is unambiguous, else in single
/// quotes.
fn quoted(argument: &str) -> String {
    let plain = !argument.is_empty()
        && argument.chars().all(|c| c.is_alphanumeric() || "-_./=:,+@%^".contains(c));
    if plain { argument.to_string() } else { format!("'{}'", argument.replace('\'', "'\\''")) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proof_load::tool::Failure;

    fn step(arguments: &[&str], exit: Option<i32>, stdout: &str, stderr: &str, failure: Option<Failure>) -> Outcome {
        Outcome {
            arguments: arguments.iter().map(|argument| argument.to_string()).collect(),
            exit, stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec(), failure,
        }
    }

    fn texts(lines: &[ScreenLine]) -> Vec<String> {
        lines.iter().map(|line| line.0.iter().map(|(_, text)| text.as_str()).collect()).collect()
    }

    /// A refusal prints its message and next step, not its JSON; a success prints its output.
    ///
    /// Revert: print stdout for a refusal too → the JSON document shows under the command.
    #[test]
    fn a_refusal_prints_its_message_and_next_step() {
        let failure = Failure { code: "damaged".into(), message: "The trust file is damaged.".into(), next_step: Some("forklift heal".into()) };
        let lines = lines(&[
            step(&["--json", "stack", "-m", "a message"], Some(1), "{\"error\": {}}", "", Some(failure)),
            step(&["heal"], Some(0), "rebuilt the trust file\n", "warning: one kept\n", None),
        ]);
        assert_eq!(texts(&lines), [
            "$ forklift --json stack -m 'a message'",
            "error: The trust file is damaged.",
            "  next step: forklift heal",
            "  exit 1",
            "$ forklift heal",
            "rebuilt the trust file",
            "warning: one kept",
            "  exit 0",
        ]);
        assert_eq!(lines[1].0[0].0, Ink::Red);
        assert_eq!(lines[2].0.iter().map(|(ink, _)| *ink).collect::<Vec<_>>(), [Ink::Dim, Ink::Plain]);
        assert_eq!(lines[5].0[0].0, Ink::Plain);
        assert_eq!(lines[6].0[0].0, Ink::Amber);
    }

    /// A long stream is cut at `STREAM_LINES` and says how much it left out; a signal is named.
    ///
    /// Revert: print every line → the screen holds all 45.
    #[test]
    fn a_long_stream_is_cut_and_counted() {
        let out: String = (0..45).map(|index| format!("line {index}\n")).collect();
        let lines = lines(&[step(&["fsck"], None, &out, "", None)]);
        let texts = texts(&lines);
        assert_eq!(texts.len(), 1 + STREAM_LINES + 2);
        assert_eq!(texts[STREAM_LINES], format!("line {}", STREAM_LINES - 1));
        assert_eq!(texts[STREAM_LINES + 1], "… 5 more lines");
        assert_eq!(texts.last().unwrap(), "  ended on a signal");
        assert_eq!(texts[0], "$ forklift fsck");
    }

    #[test]
    fn arguments_are_quoted_only_when_they_need_it() {
        assert_eq!(quoted("--out=./x"), "--out=./x");
        assert_eq!(quoted(""), "''");
        assert_eq!(quoted("it's"), "'it'\\''s'");
        assert_eq!(texts(&lines(&[])), ["the row ran no commands"]);
    }
}
