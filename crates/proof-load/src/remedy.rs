//! Remedies a refusal names, extracted in exactly two forms: a `forklift …` command, quoted or
//! standing as a whole text (a next_step that is nothing but the command), and a `remove "<path>"`
//! instruction. Anything else is recorded as unextractable; the harness never
//! guesses at a remedy.
//!
//! The text comes from a client's JSON error (its message and next_step), and for a restarted
//! server from its stderr and its 503 bodies.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Remedy {
    /// A forklift command, its arguments split on whitespace, without the leading `forklift`.
    Command { arguments: Vec<String> },
    /// Remove one file.
    Remove { path: PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Extracted {
    pub remedies: Vec<Remedy>,
    /// Quoted commands that could not be run as written, e.g. ones carrying a `<placeholder>`.
    pub unextractable: Vec<String>,
}

/// Every quoted span of `text`, in double quotes or backticks.
fn quoted(text: &str) -> Vec<(usize, &str)> {
    let mut spans = Vec::new();
    for quote in ['"', '`'] {
        let mut rest = text;
        let mut offset = 0;
        while let Some(open) = rest.find(quote) {
            let after = &rest[open + 1..];
            let Some(close) = after.find(quote) else { break };
            spans.push((offset + open, &after[..close]));
            let consumed = open + 1 + close + 1;
            offset += consumed;
            rest = &rest[consumed..];
        }
    }
    spans.sort();
    spans
}

/// Extracts the remedies `texts` name, in the order they appear.
pub fn extract(texts: &[&str]) -> Extracted {
    let mut extracted = Extracted { remedies: Vec::new(), unextractable: Vec::new() };

    let mut command = |span: &str, extracted: &mut Extracted| {
        let arguments: Vec<String> = span.trim_start_matches("forklift ").split_whitespace().map(str::to_string).collect();
        if arguments.is_empty() || arguments.iter().any(|argument| argument.contains('<') || argument.contains('{')) {
            extracted.unextractable.push(span.to_string());
        } else {
            extracted.remedies.push(Remedy::Command { arguments });
        }
    };

    for text in texts {
        // A whole text that is one command, as a next_step often is: its field delimits it the way
        // quotes delimit one inside prose.
        let whole = text.trim();
        if whole.starts_with("forklift ") && !whole.contains(['"', '`']) {
            command(whole, &mut extracted);
            continue;
        }
        for (start, span) in quoted(text) {
            if span.starts_with("forklift ") {
                command(span, &mut extracted);
            } else if text[..start].to_ascii_lowercase().trim_end().ends_with("remove") && !span.is_empty() {
                extracted.remedies.push(Remedy::Remove { path: PathBuf::from(span) });
            }
        }
    }

    extracted.remedies.dedup();
    extracted
}

/// Whether the harness may execute a remove remedy: only for a path inside the run's own root,
/// judged at path-component boundaries, so a root of `/s` covers `/s/wh` but not `/srv/wh`.
pub fn removable(path: &Path, run_root: &Path) -> bool {
    path.is_absolute() && path.starts_with(run_root) && path != run_root
        && !path.components().any(|component| matches!(component, std::path::Component::ParentDir))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two forms, as Forklift writes them, extracted in order.
    ///
    /// Revert: drop the backtick quote → the second command is lost; drop the `remove` form → the
    /// lock path is lost.
    #[test]
    fn both_forms_are_extracted() {
        let message = "the store is locked by process 42, which is no longer running, remove \"/s/wh/.forklift/store.lock\" and try again.";
        let next_step = "Run \"forklift heal\", then `forklift store compact --all`.";
        assert_eq!(extract(&[message, next_step]).remedies, vec![
            Remedy::Remove { path: PathBuf::from("/s/wh/.forklift/store.lock") },
            Remedy::Command { arguments: vec!["heal".to_string()] },
            Remedy::Command { arguments: vec!["store".to_string(), "compact".to_string(), "--all".to_string()] },
        ]);
    }

    /// Revert: run a templated command as written → `load -- <path>` would be executed.
    #[test]
    fn a_templated_command_is_unextractable_not_guessed() {
        let extracted = extract(&["Run \"forklift load -- <path>\" for each folder, or ask an admin."]);
        assert!(extracted.remedies.is_empty());
        assert_eq!(extracted.unextractable, vec!["forklift load -- <path>"]);
    }

    /// A next_step that is nothing but a command is that command; a prose sentence that merely
    /// starts with the word is not, and neither is an unquoted command inside a sentence.
    ///
    /// Revert: drop the whole-text form → `forklift shift main`, the remedy for a bay behind its
    /// pallet, is never run.
    #[test]
    fn a_next_step_that_is_a_command_is_extracted() {
        assert_eq!(extract(&["This bay is behind pallet \"main\".", "forklift shift main"]).remedies,
            vec![Remedy::Command { arguments: vec!["shift".to_string(), "main".to_string()] }]);
        assert!(extract(&["Run forklift heal when done."]).remedies.is_empty());
        assert_eq!(extract(&["forklift shift <pallet>"]).unextractable, vec!["forklift shift <pallet>"]);
    }

    /// A quoted path that no `remove` precedes is not a remedy.
    #[test]
    fn a_quoted_path_alone_is_not_a_remedy() {
        assert!(extract(&["could not read \"/s/wh/.forklift/trust\": damaged"]).remedies.is_empty());
    }

    /// Revert: compare as strings → `/srv/wh` passes under `/s`; allow `..` → an escape passes.
    #[cfg(unix)]
    #[test]
    fn only_paths_under_the_run_root_are_removable() {
        let root = Path::new("/s");
        assert!(removable(Path::new("/s/wh/.forklift/store.lock"), root));
        assert!(!removable(Path::new("/srv/wh/.forklift/store.lock"), root));
        assert!(!removable(Path::new("/s/../home/user/file"), root));
        assert!(!removable(Path::new("/s"), root));
        assert!(!removable(Path::new("wh/.forklift/store.lock"), root));
    }
}
