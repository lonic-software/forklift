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
//! successive versions are matched line by line under a longest common subsequence, so a line
//! unchanged from the previous version keeps its older attribution and a genuinely new line
//! is attributed to the version that introduced it. Lines a merge parcel brought in from a
//! side line (its second parent) are attributed to the merge — the honest limit of a
//! first-parent walk, and the natural reading of "where did this line enter this pallet".

use std::collections::HashMap;
use crate::util::{graph_utils, lcs, object_utils};

/// The most steps the match of one version may take (see [`lcs::myers_matches_within`]), a
/// fraction of a second. A version whose match would take more (two long versions with many
/// shared lines in a thoroughly different order) skips the line-level attribution, and its
/// whole content is attributed to the introducing parcel. Its length alone never does: a long
/// file edited lightly costs about its length.
const MAX_BLAME_STEPS: u64 = 50_000_000;

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

    // Fill the filters the walk below queries in one batched pass, so a cold graph is written a
    // shard at a time per batch rather than once per parcel. Best-effort: each query still
    // computes whatever this left missing. Past the parcels it could record (typically a merge
    // whose other parent is missing from a partial or damaged store, but any failure to record
    // counts), a query would walk the same ancestry to the same failure, once per parcel above
    // it; those parcels go straight to the tree check, the answer a failed query falls back to.
    let recorded = graph_utils::ensure_path_filters(chain.get(1..).unwrap_or(&[]))
        .map_or(chain.len(), |count| count + 1);

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
        if index > 0 && index < recorded && !graph_utils::path_maybe_changed(parcel, path).unwrap_or(true) {
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

/// The id of `line` in `ids`, giving it the next unused one if it has none yet.
fn intern<'a>(ids: &mut HashMap<&'a [u8], u32>, line: &'a [u8]) -> u32 {
    let next = ids.len() as u32;
    *ids.entry(line).or_insert(next)
}

/// Which ids, of `count`, occur in `line_ids`.
fn present(line_ids: &[u32], count: usize) -> Vec<bool> {
    let mut seen = vec![false; count];
    for &id in line_ids {
        seen[id as usize] = true;
    }
    seen
}

/// Attribute the lines of a new version of the file: a line that matches one of the
/// previous version's lines (under a longest common subsequence) inherits its attribution;
/// every other line is attributed to `parcel`, which introduced it.
///
/// The subsequence comes from Myers' algorithm, whose cost grows with the lines that changed
/// rather than with the product of the two versions' lengths: the exact table it replaced
/// spent nearly all of a 13 s blame of git.git's 4,000-line Makefile. Where several
/// subsequences are equally long, the changed runs are then slid to where git's diff puts them,
/// so a repeated line (a blank, a closing brace) mostly keeps the parcel git's blame credits
/// it to. The display diff's histogram match was measured too and rejected: on that file it credited
/// one parcel with 312 lines where git and the longest subsequence credit it with 9.
fn attribute_version(previous: &[BlameLine], content: &[u8], parcel: &str) -> Vec<BlameLine> {
    let new_lines = split_lines(content);

    // Lines as small integer ids, so the matcher compares integers, not line bytes.
    let mut ids: HashMap<&[u8], u32> = HashMap::new();
    let old_ids: Vec<u32> = previous.iter().map(|line| intern(&mut ids, &line.content)).collect();
    let new_ids: Vec<u32> = new_lines.iter().map(|line| intern(&mut ids, line)).collect();

    // A line only one side holds can never match, so the search runs without them: the
    // longest subsequence is the same, and a version that rewrote most of the file leaves
    // little to search. `old_kept[i]` is the old line the i-th searched one stands for.
    let in_old = present(&old_ids, ids.len());
    let in_new = present(&new_ids, ids.len());
    let old_kept: Vec<usize> = (0..old_ids.len()).filter(|&i| in_new[old_ids[i] as usize]).collect();
    let new_kept: Vec<usize> = (0..new_ids.len()).filter(|&j| in_old[new_ids[j] as usize]).collect();
    let old_searched: Vec<u32> = old_kept.iter().map(|&i| old_ids[i]).collect();
    let new_searched: Vec<u32> = new_kept.iter().map(|&j| new_ids[j]).collect();

    // Past the budget, skip the match and attribute the whole version to the introducing
    // parcel: coarse, but bounded.
    let Some(searched) = lcs::myers_matches_within(&old_searched, &new_searched, MAX_BLAME_STEPS) else {
        return new_lines.into_iter()
            .map(|line| BlameLine { content: line.to_vec(), parcel: parcel.to_string() })
            .collect();
    };
    let mut found = vec![None; old_ids.len()];
    for (index, matched) in searched.iter().enumerate() {
        if let Some(new_index) = matched {
            found[old_kept[index]] = Some(new_kept[*new_index]);
        }
    }

    // For each old line, the new line it maps to (or none). A new line so matched inherits
    // the old line's attribution.
    let matches = lcs::slide_changes(&old_ids, &new_ids, &found);

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
    fn a_repeated_line_keeps_its_attribution_where_git_shows_it_unchanged() {
        // `x z` becomes `z z y`. Either new `z` can be the old one. Git's diff shows the first
        // unchanged and the second added (the added run slides down to join `y`), so the first
        // keeps P1, not the second.
        let previous = vec![line("x\n", "P1"), line("z\n", "P1")];

        let result = attribute_version(&previous, b"z\nz\ny\n", "P2");
        let parcels: Vec<&str> = result.iter().map(|blame| blame.parcel.as_str()).collect();

        assert_eq!(parcels, vec!["P1", "P2", "P2"]);
    }

    #[test]
    fn a_long_file_edited_lightly_keeps_its_line_level_attribution() {
        // 30,000 lines, one of them changed: the match costs about the file's length, so its
        // length alone must not send the version to the whole-file fallback.
        let previous: Vec<BlameLine> = (0..30_000).map(|i| line(&format!("line {i}\n"), "P1")).collect();
        let mut content: String = (0..30_000).map(|i| format!("line {i}\n")).collect();
        content = content.replacen("line 15000\n", "changed\n", 1);

        let result = attribute_version(&previous, content.as_bytes(), "P2");

        assert_eq!(result.iter().filter(|blame| blame.parcel == "P2").count(), 1);
        assert_eq!(result[15_000].parcel, "P2");
    }

    #[test]
    fn a_version_past_the_step_budget_is_attributed_whole() {
        // The two halves of 40,000 lines interleaved: every line is shared but no two neighbours
        // stay together, so the shortest edit script is about 40,000 long and the search costs
        // about the product of the lengths, past the budget. The version falls back to the
        // introducing parcel rather than taking that long.
        let previous: Vec<BlameLine> = (0..40_000).map(|i| line(&format!("{i}\n"), "P1")).collect();
        let content: String = (0..40_000)
            .map(|i| format!("{}\n", if i % 2 == 0 { i / 2 } else { 20_000 + i / 2 }))
            .collect();

        let result = attribute_version(&previous, content.as_bytes(), "P2");

        assert!(result.iter().all(|blame| blame.parcel == "P2"));
    }

    #[test]
    fn a_blame_above_an_unreadable_merge_parent_walks_its_ancestry_a_bounded_number_of_times() {
        // A merge whose second parent is missing from the store: no parcel from it up can get a
        // commit-graph record, so a filter query on any of them walks the ancestry back to the
        // missing parcel and fails. The blame must not pay that walk once per parcel above the
        // merge (quadratic in the chain), and its attribution must still be right.
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType::{Normal, Tree};
        use crate::globals::StorageRootScope;
        use crate::model::blob::Blob;
        use crate::model::parcel::Parcel;
        use crate::model::tree_item::TreeItem;

        let dir = std::env::temp_dir()
            .join(format!("forklift-blame-unresolvable-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scope = StorageRootScope::enter(&dir);

        let store_file_tree = |content: &str| -> String {
            let mut blob = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
            blob.store().unwrap();
            let mut tree = TreeItem::new(String::new(), String::new(), Tree);
            tree.add_child(TreeItem::new("notes.txt".to_string(), blob.hash.clone(), Normal));
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            object.hash
        };
        let store_parcel = |tree_hash: &str, parents: Vec<String>, tag: &str| -> String {
            let parcel = Parcel {
                tree_hash: tree_hash.to_string(),
                parents,
                actions: vec![],
                description: Some(tag.to_string()),
            };
            let mut object = LooseObjectBuilder::build_parcel(&parcel);
            object.store().unwrap();
            object.hash
        };

        let first = store_file_tree("one\n");
        let second = store_file_tree("one\ntwo\n");
        let third = store_file_tree("one\ntwo\nthree\n");

        const BELOW: usize = 10;
        const ABOVE: usize = 40;
        let root = store_parcel(&first, vec![], "root");
        let mut tip = root.clone();
        for i in 0..BELOW {
            tip = store_parcel(&first, vec![tip], &format!("below {i}"));
        }
        let merge = store_parcel(&second, vec![tip, "f".repeat(64)], "merge");
        tip = merge.clone();
        for i in 0..ABOVE {
            tip = store_parcel(&second, vec![tip], &format!("above {i}"));
        }
        let last = store_parcel(&third, vec![tip], "last");

        graph_utils::ENSURE_CALLS.with(|calls| calls.set(0));
        let result = blame(&last, "notes.txt");
        let walks = graph_utils::ENSURE_CALLS.with(|calls| calls.get());
        drop(scope);
        let _ = std::fs::remove_dir_all(&dir);

        let lines = result.expect("an unreadable merge parent must not fail a first-parent blame");
        let parcels: Vec<&str> = lines.lines.iter().map(|line| line.parcel.as_str()).collect();
        assert_eq!(parcels, vec![root.as_str(), merge.as_str(), last.as_str()],
            "each line must be attributed to the parcel that introduced it");
        // The filter pass: one failed walk from the newest parcel and a binary search over the
        // chain (about log2(52)). One more walk per parcel above the merge would be 41 more.
        assert!(walks <= 1 + 7,
            "the blame must not walk the unresolvable ancestry once per parcel above it; it made {walks}");
    }

    #[test]
    fn attribution_keeps_the_most_lines_a_rare_line_anchor_would_drop() {
        // A rare-line anchor (the display diff's) keeps the moved unique line and calls the
        // three braces new. The longest common subsequence keeps the three braces: only the
        // moved line is the second parcel's.
        let previous = vec![line("unique\n", "P1"), line("}\n", "P1"), line("}\n", "P1"), line("}\n", "P1")];

        let result = attribute_version(&previous, b"}\n}\n}\nunique\n", "P2");
        let parcels: Vec<&str> = result.iter().map(|blame| blame.parcel.as_str()).collect();

        assert_eq!(parcels, vec!["P1", "P1", "P1", "P2"]);
    }

    #[test]
    fn files_without_a_trailing_newline_split_into_all_their_lines() {
        assert_eq!(split_lines(b"one\ntwo").len(), 2);
        assert_eq!(split_lines(b"").len(), 0);
        assert_eq!(split_lines(b"one\n").len(), 1);
    }
}
