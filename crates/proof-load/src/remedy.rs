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

    let command = |span: &str, extracted: &mut Extracted| {
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

/// The path the harness may remove for a remove remedy, if any: only one inside one of the run's
/// own roots (the scratch root, and a work tree moved off it), each judged as [`removable_under`].
pub fn removable(path: &Path, run_roots: &[&Path]) -> Option<PathBuf> {
    run_roots.iter().find_map(|root| removable_under(path, root))
}

/// Carries out a remove remedy, the same in every lane: the path judged as [`removable`], and
/// removed when it is a file there. Returns the step as it is recorded in outside knowledge, saying
/// why when it was not carried out, and whether a file was removed.
pub fn remove(path: &Path, run_roots: &[&Path]) -> (String, bool) {
    let Some(judged) = removable(path, run_roots) else {
        return (format!("remove {} (outside the run root; not run)", path.display()), false);
    };
    match std::fs::symlink_metadata(&judged) {
        Err(_) => (format!("remove {} (nothing there; not run)", path.display()), false),
        Ok(metadata) if metadata.is_dir() => (format!("remove {} (a folder, and a remedy removes one file; not run)", path.display()), false),
        Ok(_) => {
            #[allow(clippy::disallowed_methods)] // a removal a refusal named, judged inside the run's roots; no retry is wanted
            let removed = std::fs::remove_file(&judged);
            match removed {
                Ok(()) => (format!("remove {}", path.display()), true),
                Err(error) => (format!("remove {} (failed: {})", path.display(), error), false),
            }
        }
    }
}

/// The path the harness may remove under one run root, if any: only one inside it, judged at
/// path-component boundaries, so a root of `/s` covers `/s/wh` but not `/srv/wh`.
/// A tool may print the path resolved (`/private/var/…` for `/var/…` on macOS, `\\?\C:\…` on
/// Windows), so a path that is not under the root as given is judged again with both resolved,
/// and then the resolved path is the one returned: what is removed is exactly what was judged,
/// never a link outside the root that resolves into it.
pub fn removable_under(path: &Path, run_root: &Path) -> Option<PathBuf> {
    let under = |path: &Path, root: &Path| path.is_absolute() && path.starts_with(root) && path != root
        && !path.components().any(|component| matches!(component, std::path::Component::ParentDir));
    if under(path, run_root) {
        return Some(path.to_path_buf());
    }
    // A path already gone is resolved through its folder, so a resolved spelling of it is still
    // judged inside the root it is in, and then found to be gone.
    let resolved = std::fs::canonicalize(path).ok()
        .or_else(|| Some(std::fs::canonicalize(path.parent()?).ok()?.join(path.file_name()?)))?;
    let root = std::fs::canonicalize(run_root).ok()?;
    under(&resolved, &root).then_some(resolved)
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
        assert!(extract(&["Run forklift heal, when done."]).remedies.is_empty());
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
    fn only_paths_under_the_run_root_are_removable_under() {
        let root = Path::new("/s");
        assert_eq!(removable_under(Path::new("/s/wh/.forklift/store.lock"), root), Some(PathBuf::from("/s/wh/.forklift/store.lock")));
        assert_eq!(removable_under(Path::new("/srv/wh/.forklift/store.lock"), root), None);
        assert_eq!(removable_under(Path::new("/s/../home/user/file"), root), None);
        assert_eq!(removable_under(Path::new("/s"), root), None);
        assert_eq!(removable_under(Path::new("wh/.forklift/store.lock"), root), None);
    }

    /// A run with its work tree moved off the scratch root may remove under either root, and
    /// nowhere else: not beside the moved work tree, not the moved folder itself.
    ///
    /// Revert: judge against the scratch root only → a lock in the moved work tree is refused, so a
    /// named working remedy grades as none; admit the moved folder's parent → a sibling passes.
    #[cfg(unix)]
    #[test]
    fn a_moved_work_tree_is_a_second_run_root() {
        let roots = [Path::new("/s"), Path::new("/fold/0001-x-work")];
        assert_eq!(removable(Path::new("/s/host/.forklift/serve.lock"), &roots), Some(PathBuf::from("/s/host/.forklift/serve.lock")));
        assert_eq!(removable(Path::new("/fold/0001-x-work/.forklift/store.lock"), &roots), Some(PathBuf::from("/fold/0001-x-work/.forklift/store.lock")));
        assert_eq!(removable(Path::new("/fold/0002-y-work/.forklift/store.lock"), &roots), None);
        assert_eq!(removable(Path::new("/fold/0001-x-work"), &roots), None);
        assert_eq!(removable(Path::new("/fold"), &roots), None);
        assert_eq!(removable(Path::new("/s/x"), &roots[..1]), Some(PathBuf::from("/s/x")));
        assert_eq!(removable(Path::new("/fold/0001-x-work/x"), &roots[..1]), None);
    }

    /// A path the tool printed resolved, through a link in the root as given, is still under it;
    /// one resolving outside the root is not; and a link outside the root that resolves into it is
    /// judged, and removed, as where it resolves, never as itself.
    ///
    /// Revert: compare only as given → the resolved path is refused, so a named working remedy
    /// grades as none; return the path as given → the outside link is what gets removed.
    #[cfg(unix)]
    #[test]
    fn a_resolved_path_under_the_root_is_removable_under() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let base = std::env::temp_dir().join(format!("proof-load-remedy-{}-{}", std::process::id(), nanos));
        let real = base.join("real");
        std::fs::create_dir_all(real.join("wh")).unwrap();
        std::fs::write(real.join("wh/store.lock"), b"").unwrap();
        std::fs::write(base.join("elsewhere"), b"").unwrap();
        std::os::unix::fs::symlink(&real, base.join("link")).unwrap();
        let root = base.join("link");
        let printed = std::fs::canonicalize(real.join("wh/store.lock")).unwrap();
        assert_eq!(removable_under(&printed, &root), Some(printed.clone()));
        assert_eq!(removable_under(&std::fs::canonicalize(base.join("elsewhere")).unwrap(), &root), None);
        let outside = base.join("outside-link");
        std::os::unix::fs::symlink(real.join("wh/store.lock"), &outside).unwrap();
        assert_eq!(removable_under(&outside, &root), Some(printed));
    }

    /// A removal is carried out only on a file inside a run root, and every one is recorded, with
    /// why when it was not carried out.
    ///
    /// Revert: record only removals carried out → the folder, the missing file and the outside
    /// path leave no step.
    #[test]
    fn every_removal_is_recorded() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let root = std::env::temp_dir().join(format!("proof-load-remove-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(root.join("folder")).unwrap();
        std::fs::write(root.join("serve.lock"), "").unwrap();
        let roots = [root.as_path()];
        let (step, done) = remove(&root.join("serve.lock"), &roots);
        assert_eq!((step, done), (format!("remove {}", root.join("serve.lock").display()), true));
        assert!(!root.join("serve.lock").exists());
        for (path, why) in [(root.join("folder"), "a folder"), (root.join("serve.lock"), "nothing there"), (PathBuf::from("/elsewhere/lock"), "outside the run root")] {
            let (step, done) = remove(&path, &roots);
            assert!(!done && step.starts_with(&format!("remove {} (", path.display())) && step.contains(why), "{}", step);
        }
        assert!(root.join("folder").is_dir());
        // Gone, and printed resolved (/private/var/… for /var/… on macOS): still inside the root.
        let resolved = std::fs::canonicalize(&root).unwrap().join("gone.lock");
        let (step, done) = remove(&resolved, &roots);
        assert!(!done && step.contains("nothing there"), "{}", step);
        #[allow(clippy::disallowed_methods)] // the test's own temp folder
        let _ = std::fs::remove_dir_all(&root);
    }
}
