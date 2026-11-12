//! Remedies a refusal names, extracted in exactly two forms: a quoted `forklift …` command, and a
//! `remove "<path>"` instruction. Anything else is recorded as unextractable; the harness never
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

    for text in texts {
        for (start, span) in quoted(text) {
            if let Some(command) = span.strip_prefix("forklift ") {
                let arguments: Vec<String> = command.split_whitespace().map(str::to_string).collect();
                if arguments.is_empty() || arguments.iter().any(|argument| argument.contains('<') || argument.contains('{')) {
                    extracted.unextractable.push(span.to_string());
                } else {
                    extracted.remedies.push(Remedy::Command { arguments });
                }
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
