//! Cryptographic blame (§9.4a): attribute every line of a file to the parcel that
//! introduced it.
//!
//! An ordinary blame answers "who last touched this line". Forklift's answers more,
//! because authorship is signed and carries an identity class, a supervisor and
//! provenance (§7.1): the caller can turn the per-line parcel this module resolves into
//! "was this line written by a human or an agent, under whose supervision" — offline and
//! forge-proof, blame git structurally cannot express. This module does the history walk;
//! the head resolves the parcel it returns to that signed metadata (core never prints).
//!
//! The walk follows the **first-parent chain** from the revision back to the root, exactly
//! git's `blame --first-parent`. Each parcel where the file's blob changed is a *version*;
//! successive versions are matched line by line with the same LCS the merge uses, so a line
//! unchanged from the previous version keeps its older attribution and a genuinely new line
//! is attributed to the version that introduced it. Lines a merge parcel brought in from a
//! side line (its second parent) are attributed to the merge — the honest limit of a
//! first-parent walk, and the natural reading of "where did this line enter this pallet".

use crate::util::{graph_utils, lcs, object_utils};

/// Above this many lines (per side), the line-level attribution is skipped for a version
/// and its whole content is attributed to the introducing parcel. The quadratic LCS table
/// is the cost being bounded — the same guard the merge applies (`merge_utils`).
const MAX_BLAME_LINES: usize = 20_000;

/// One attributed line of the blamed file.
pub struct BlameLine {
    /// The line's bytes, including its trailing newline (the last line may lack one).
    pub content: Vec<u8>,

    /// The hash of the parcel that introduced the line (its content did not exist, at this
    /// path, in the previous version).
    pub parcel: String,
}

/// The blame of one file at one revision: every line, attributed to a parcel.
pub struct FileBlame {
    /// The warehouse path that was blamed.
    pub path: String,

    /// The revision the blame was taken at (the resolved head parcel hash).
    pub revision: String,

    /// The file's lines, in order, each attributed to the parcel that introduced it.
    pub lines: Vec<BlameLine>,
}

/// Blame a file at a revision: attribute each of its lines to the parcel that introduced
/// it, walking the first-parent chain from `head`.
///
/// # Arguments
/// * `head` - The parcel hash the blame is taken at (already resolved from a revision).
/// * `path` - The warehouse path of the file to blame.
///
/// # Returns
/// * `Ok(FileBlame)` - The per-line attribution (empty `lines` for an empty file).
/// * `Err(String)`   - If the path is not a file at `head`, or an object could not be read.
pub fn blame(head: &str, path: &str) -> Result<FileBlame, String> {
    // The file must exist at the revision — blame is over a file that is there to read.
    let head_tree = object_utils::load_parcel(head)?.tree_hash;

    match object_utils::resolve_tree_file(&head_tree, path)? {
        None => return Err(format!(
            "There is no file \"{}\" at that revision (it may be a directory, or not exist there).",
            path
        )),
        // A chunked file is an opaque binary — there are no lines to attribute. Refuse loudly
        // from the tree entry type alone (no object load), the way `diff` reports it as binary.
        Some((_, item_type)) if item_type.is_chunked() => return Err(format!(
            "\"{}\" is a large binary file (stored in chunks); blame is only available for text files.",
            path
        )),
        Some(_) => {}
    }

    // The first-parent chain, oldest first: the linear history the blame is computed over.
    let chain = first_parent_chain(head)?;

    // Walk the versions oldest to newest, carrying the current attribution forward. A line
    // unchanged from the previous version (an LCS match) keeps its attribution; a new line
    // is attributed to the parcel that introduced it.
    let mut prev_blob: Option<String> = None;
    let mut current: Vec<BlameLine> = Vec::new();

    for (index, parcel) in chain.iter().enumerate() {
        // Skip parcels that did not touch this path, using the commit-graph's changed-path
        // filter — the walk's whole point on a long history, since a file changes in a tiny
        // fraction of its ancestors. Each chain entry's in-chain parent is its first parent
        // (the chain follows first parents), so the filter's "changed vs first parent" is
        // exactly this walk's "changed vs the previous version". A `false` is definitive (no
        // false negatives); the oldest parcel has no in-chain parent, so it is always examined;
        // and any graph hiccup falls back to the real tree check below — never a wrong answer.
        if index > 0 && !graph_utils::path_maybe_changed(parcel, path).unwrap_or(true) {
            continue;
        }

        let parcel_tree = object_utils::load_parcel(parcel)?.tree_hash;
        let file = object_utils::resolve_tree_file(&parcel_tree, path)?;
        let blob = file.as_ref().map(|(hash, _)| hash.clone());

        // No change to the file at this parcel: the attribution carries forward untouched.
        if blob == prev_blob {
            continue;
        }

        match &file {
            // The file does not exist at this parcel (not yet added, or removed). A later
            // re-add is treated as wholly new content there.
            None => current.clear(),

            // A chunked (binary) version is an opaque boundary the first-parent walk cannot
            // see through: there is no text to load or line-match against. Clear the carried
            // state exactly like the file-absent arm, so lines from before this version drop;
            // if the file later becomes plain text again, that version's lines have nothing to
            // inherit from (an empty `current`) and are attributed fresh to the parcel that
            // (re)introduced them — the same treatment a first version gets. This is what keeps
            // a plain-at-head file with a chunked ancestor blaming successfully instead of the
            // internal "is a recipe, not a blob" error a bare `load_blob` would otherwise leak.
            Some((_, item_type)) if item_type.is_chunked() => current.clear(),

            Some((blob_hash, _)) => {
                let content = object_utils::load_blob(blob_hash)?.content;
                current = attribute_version(&current, &content, parcel);
            }
        }

        prev_blob = blob;
    }

    Ok(FileBlame { path: path.to_string(), revision: head.to_string(), lines: current })
}

/// Attribute the lines of a new version of the file: a line that matches one of the
/// previous version's lines (under their LCS) inherits its attribution; every other line is
/// attributed to `parcel`, which introduced it.
fn attribute_version(previous: &[BlameLine], content: &[u8], parcel: &str) -> Vec<BlameLine> {
    let new_lines = split_lines(content);

    // Over the guard, skip the quadratic match and attribute the whole version to the
    // introducing parcel — coarse, but bounded (large generated/data files, rarely blamed).
    if previous.len() > MAX_BLAME_LINES || new_lines.len() > MAX_BLAME_LINES {
        return new_lines.into_iter()
            .map(|line| BlameLine { content: line.to_vec(), parcel: parcel.to_string() })
            .collect();
    }

    let old_lines: Vec<&[u8]> = previous.iter().map(|line| line.content.as_slice()).collect();

    // For each old line, the new line it maps to under the LCS (or none). A new line so
    // matched inherits the old line's attribution.
    let matches = lcs::lcs_matches(&old_lines, &new_lines);

    let mut inherited: Vec<Option<&str>> = vec![None; new_lines.len()];

    for (old_index, matched) in matches.iter().enumerate() {
        if let Some(new_index) = matched {
            inherited[*new_index] = Some(previous[old_index].parcel.as_str());
        }
    }

    new_lines.iter().enumerate()
        .map(|(index, line)| BlameLine {
            content: line.to_vec(),
            parcel: inherited[index].unwrap_or(parcel).to_string(),
        })
        .collect()
}

/// The first-parent chain from `head` back to the root, oldest first. Following only the
/// first parent keeps the walk linear (git's `blame --first-parent`); a cycle in the graph
/// (a corrupt warehouse) is broken defensively rather than looped forever.
///
/// Parent edges come from the commit-graph ([`graph_utils::parents`]) rather than by decoding
/// every ancestor parcel — on a long history that is the difference between a resident-index
/// lookup per parcel and reading tens of thousands of parcel objects. It falls back to decoding
/// any parcel whose graph record is missing, so the chain is correct on a cold graph too.
fn first_parent_chain(head: &str) -> Result<Vec<String>, String> {
    let mut chain: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut current = Some(head.to_string());

    while let Some(hash) = current {
        if !seen.insert(hash.clone()) {
            break;
        }

        current = graph_utils::parents(&hash)?.into_iter().next();
        chain.push(hash);
    }

    chain.reverse();

    Ok(chain)
}

/// Split content into lines, each including its trailing new line byte (the last line may
/// lack one). Exact bytes, matching the merge's line model (`merge_utils`).
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;

    for (index, byte) in content.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
        }
    }

    if start < content.len() {
        lines.push(&content[start..]);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(content: &str, parcel: &str) -> BlameLine {
        BlameLine { content: content.as_bytes().to_vec(), parcel: parcel.to_string() }
    }

    #[test]
    fn a_first_version_attributes_every_line_to_its_parcel() {
        let result = attribute_version(&[], b"one\ntwo\nthree\n", "P1");

        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|blame| blame.parcel == "P1"));
    }

    #[test]
    fn unchanged_lines_keep_their_older_attribution() {
        let previous = vec![line("one\n", "P1"), line("two\n", "P1")];

        // P2 changes the second line and appends a third; the first line is untouched.
        let result = attribute_version(&previous, b"one\nTWO\nthree\n", "P2");

        assert_eq!(result[0].parcel, "P1"); // unchanged — older attribution kept
        assert_eq!(result[1].parcel, "P2"); // changed — attributed to P2
        assert_eq!(result[2].parcel, "P2"); // new — attributed to P2
    }

    #[test]
    fn an_inserted_line_does_not_reattribute_its_neighbours() {
        let previous = vec![line("one\n", "P1"), line("two\n", "P1")];

        // A line inserted between the two originals: only the insertion is P2's.
        let result = attribute_version(&previous, b"one\nMIDDLE\ntwo\n", "P2");

        assert_eq!(result[0].parcel, "P1");
        assert_eq!(result[1].parcel, "P2");
        assert_eq!(result[2].parcel, "P1");
    }

    #[test]
    fn files_without_a_trailing_newline_split_into_all_their_lines() {
        assert_eq!(split_lines(b"one\ntwo").len(), 2);
        assert_eq!(split_lines(b"").len(), 0);
        assert_eq!(split_lines(b"one\n").len(), 1);
    }
}
