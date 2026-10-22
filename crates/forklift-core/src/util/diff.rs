//! The display diff. This is the third iteration of the "ladder" family (which started as
//! a zig-zag scan and grew a line-occurrence index); it is now a histogram-style diff:
//!
//! 1. Lines are compared on their **exact bytes**. Whitespace-only changes are real
//!    changes — the object store hashes them, so the diff must show them. They are,
//!    however, *classified*: every reported line carries an `is_whitespace_only` flag, so
//!    presentation layers can dim or filter the noise without the engine lying about
//!    line identity.
//! 2. At each mismatched region, the algorithm anchors on the **rarest** line shared by
//!    both sides (bounded by an occurrence cap), extends the anchor to the maximal common
//!    run, and recurses left and right of it. Rare lines are trustworthy anchors; braces
//!    and blank lines are not — this is the histogram idea (JGit's HistogramDiff), which
//!    descends from Heckel's occurrence table and patience diff's unique-line anchors.
//! 3. Regions where every shared line is too common to anchor fall back to an **exact
//!    LCS** when small enough, and to a plain replacement when not.

use std::collections::HashMap;
use std::ops::Range;
use crate::enums::diff_type::DiffType;
use crate::model::diff::Diff;
use crate::util::lcs;


/// The maximum number of occurrences (within the current region, on either side) a line
/// may have and still serve as an anchor. Lines more common than this — think braces and
/// blank lines — make untrustworthy anchors and are skipped.
const MAX_ANCHOR_OCCURRENCES: usize = 64;

/// The maximum region size (lines, per side) for the exact-LCS fallback. The LCS table is
/// quadratic; larger anchor-less regions are reported as a plain replacement instead.
const LCS_FALLBACK_LIMIT: usize = 2_000;

/// Get the differences between two byte arrays, line by line.
///
/// # Arguments
/// * `old`     - The old content.
/// * `new`     - The new content.
/// * `verbose` - Whether to include unchanged lines in the output (as `NoOp` entries).
///
/// # Returns
/// * `Vec<Diff>` - The differences, in file order.
pub fn lines(old: &[u8], new: &[u8], verbose: bool) -> Vec<Diff> {
    let old_lines = split_lines(old);
    let new_lines = split_lines(new);

    // Intern every distinct line as an id, so all further comparisons are integer
    // comparisons. The final line of a file that does not end with a new line byte is
    // interned separately from an identical line that does (`has_newline` is part of the
    // key), so a missing trailing new line shows up as a (whitespace-only) change.
    let mut ids: HashMap<(&[u8], bool), u32> = HashMap::new();
    let mut old_positions: Vec<Vec<u32>> = Vec::new();
    let mut new_positions: Vec<Vec<u32>> = Vec::new();

    let old_ids = intern_side(&old_lines, &mut ids, &mut old_positions, &mut new_positions, true);
    let new_ids = intern_side(&new_lines, &mut ids, &mut old_positions, &mut new_positions, false);

    let mut state = DiffState {
        old_lines: &old_lines,
        new_lines: &new_lines,
        old_ids: &old_ids,
        new_ids: &new_ids,
        old_positions: &old_positions,
        new_positions: &new_positions,
        verbose,
        diffs: Vec::new(),
    };

    state.run(0..old_lines.len(), 0..new_lines.len());

    state.diffs
}

/// A line of a file: its exact content (without the trailing new line byte) and whether
/// the new line byte was present (it can only be absent on the final line).
struct Line<'a> {
    content: &'a [u8],
    has_newline: bool,
}

/// Split content into lines. The trailing new line byte is not part of a line's content,
/// but its presence is recorded (see `Line`).
fn split_lines(content: &[u8]) -> Vec<Line<'_>> {
    let mut lines: Vec<Line<'_>> = Vec::new();
    let mut start = 0usize;

    for (index, byte) in content.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(Line { content: &content[start..index], has_newline: true });
            start = index + 1;
        }
    }

    if start < content.len() {
        lines.push(Line { content: &content[start..], has_newline: false });
    }

    lines
}

/// Intern one file's lines: assign each distinct line an id (shared across both files)
/// and record the line's positions on this file's side.
///
/// # Arguments
/// * `lines`         - The file's lines.
/// * `ids`           - Line (content, has-newline) → id, shared across both files.
/// * `old_positions` - Per id: the (sorted) positions in the old file.
/// * `new_positions` - Per id: the (sorted) positions in the new file.
/// * `is_old`        - Which side's positions to record.
///
/// # Returns
/// * `Vec<u32>` - The file's lines as ids.
fn intern_side<'a>(lines: &[Line<'a>],
                   ids: &mut HashMap<(&'a [u8], bool), u32>,
                   old_positions: &mut Vec<Vec<u32>>,
                   new_positions: &mut Vec<Vec<u32>>,
                   is_old: bool) -> Vec<u32> {
    let mut line_ids: Vec<u32> = Vec::with_capacity(lines.len());

    for (index, line) in lines.iter().enumerate() {
        let next_id = old_positions.len() as u32;
        let id = *ids.entry((line.content, line.has_newline)).or_insert(next_id);

        if id == next_id {
            old_positions.push(Vec::new());
            new_positions.push(Vec::new());
        }

        let positions = if is_old { &mut *old_positions } else { &mut *new_positions };
        positions[id as usize].push(index as u32);

        line_ids.push(id);
    }

    line_ids
}

/// One unit of pending work for the (explicit, stack-based) region walk.
/// An explicit stack keeps adversarial inputs from overflowing the call stack.
enum Task {
    /// Diff the given regions of the two files.
    Diff { old: Range<usize>, new: Range<usize> },

    /// The regions were found equal; emit them (as `NoOp` lines when verbose).
    Common { old_start: usize, new_start: usize, len: usize },
}

/// The working state of one diff run.
struct DiffState<'a> {
    old_lines: &'a [Line<'a>],
    new_lines: &'a [Line<'a>],
    old_ids: &'a [u32],
    new_ids: &'a [u32],
    old_positions: &'a [Vec<u32>],
    new_positions: &'a [Vec<u32>],
    verbose: bool,
    diffs: Vec<Diff>,
}

impl DiffState<'_> {
    /// Walk the two files, emitting diffs in file order.
    fn run(&mut self, old: Range<usize>, new: Range<usize>) {
        let mut stack: Vec<Task> = vec![Task::Diff { old, new }];

        // Tasks are pushed in reverse order (right, middle, left), so popping yields the
        // file order.
        while let Some(task) = stack.pop() {
            match task {
                Task::Common { old_start, new_start, len } => {
                    self.emit_common(old_start, new_start, len);
                }
                Task::Diff { old, new } => self.diff_region(old, new, &mut stack),
            }
        }
    }

    /// Diff one region pair: trim the common prefix and suffix, then split on the best
    /// anchor (or fall back for anchor-less regions).
    fn diff_region(&mut self, mut old: Range<usize>, mut new: Range<usize>, stack: &mut Vec<Task>) {
        // Common prefix (emitted now — it precedes everything else this task produces).
        let mut prefix = 0usize;

        while old.start + prefix < old.end
            && new.start + prefix < new.end
            && self.old_ids[old.start + prefix] == self.new_ids[new.start + prefix] {
            prefix += 1;
        }

        if prefix > 0 {
            self.emit_common(old.start, new.start, prefix);
            old.start += prefix;
            new.start += prefix;
        }

        // Common suffix (emitted after the middle: pushed below everything else).
        let mut suffix = 0usize;

        while old.end > old.start
            && new.end > new.start
            && self.old_ids[old.end - 1] == self.new_ids[new.end - 1] {
            suffix += 1;
            old.end -= 1;
            new.end -= 1;
        }

        if suffix > 0 {
            stack.push(Task::Common { old_start: old.end, new_start: new.end, len: suffix });
        }

        if old.is_empty() && new.is_empty() {
            return;
        }

        if old.is_empty() || new.is_empty() {
            self.emit_change(old, new);
            return;
        }

        match self.find_anchor(&old, &new) {
            Some((anchor_old, anchor_new, len)) => {
                stack.push(Task::Diff {
                    old: anchor_old + len..old.end,
                    new: anchor_new + len..new.end,
                });
                stack.push(Task::Common { old_start: anchor_old, new_start: anchor_new, len });
                stack.push(Task::Diff {
                    old: old.start..anchor_old,
                    new: new.start..anchor_new,
                });
            }
            None => {
                if old.len() <= LCS_FALLBACK_LIMIT && new.len() <= LCS_FALLBACK_LIMIT {
                    self.diff_region_by_lcs(old, new);
                } else {
                    self.emit_change(old, new);
                }
            }
        }
    }

    /// Find the best anchor of a region pair: the maximal common run around the rarest
    /// shared line. Returns the run as (old start, new start, length).
    ///
    /// A line's rarity is its occurrence count within the region (the larger of the two
    /// sides); lines above `MAX_ANCHOR_OCCURRENCES` never anchor. A run is as rare as its
    /// rarest line. Ties are broken by run length. After a candidate run is measured, the scan skips past it (trying every
    /// line inside a run it is part of would re-measure the same run).
    fn find_anchor(&self, old: &Range<usize>, new: &Range<usize>) -> Option<(usize, usize, usize)> {
        let mut best: Option<(usize, usize, usize, usize)> = None; // (cost, old, new, len)

        let mut j = new.start;

        while j < new.end {
            let mut next_j = j + 1;
            let id = self.new_ids[j] as usize;

            let old_occurrences = occurrences_in_range(&self.old_positions[id], old);
            let cost = self.line_cost(id, old, new);

            let is_anchor_material = !old_occurrences.is_empty()
                && cost <= MAX_ANCHOR_OCCURRENCES
                && best.map(|(best_cost, ..)| cost <= best_cost).unwrap_or(true);

            if is_anchor_material {
                for &occurrence in old_occurrences {
                    let i = occurrence as usize;

                    // Extend the match around (i, j) to the maximal common run.
                    let mut run_old = i;
                    let mut run_new = j;

                    while run_old > old.start
                        && run_new > new.start
                        && self.old_ids[run_old - 1] == self.new_ids[run_new - 1] {
                        run_old -= 1;
                        run_new -= 1;
                    }

                    let mut end_old = i + 1;
                    let mut end_new = j + 1;

                    while end_old < old.end
                        && end_new < new.end
                        && self.old_ids[end_old] == self.new_ids[end_new] {
                        end_old += 1;
                        end_new += 1;
                    }

                    let len = end_old - run_old;

                    // The run is as trustworthy as its rarest line, not the line that found
                    // it: the scan skips past this run below, so a rarer line inside it is
                    // never scanned on its own, and charging the run this line's cost would
                    // let a stray common line elsewhere outrank it.
                    let mut run_cost = cost;

                    for offset in 0..len {
                        if run_cost == 1 {
                            break;
                        }

                        run_cost = run_cost.min(self.line_cost(self.new_ids[run_new + offset] as usize, old, new));
                    }

                    let is_better = match best {
                        None => true,
                        Some((best_cost, _, _, best_len)) =>
                            run_cost < best_cost || (run_cost == best_cost && len > best_len),
                    };

                    if is_better {
                        best = Some((run_cost, run_old, run_new, len));
                    }

                    next_j = next_j.max(end_new);
                }
            }

            j = next_j;
        }

        best.map(|(_, anchor_old, anchor_new, len)| (anchor_old, anchor_new, len))
    }

    /// A line's occurrence count within a region pair: the larger of its counts on the two
    /// sides.
    fn line_cost(&self, id: usize, old: &Range<usize>, new: &Range<usize>) -> usize {
        occurrences_in_range(&self.old_positions[id], old).len()
            .max(occurrences_in_range(&self.new_positions[id], new).len())
    }

    /// Diff an anchor-less region pair exactly, with the shared LCS.
    fn diff_region_by_lcs(&mut self, old: Range<usize>, new: Range<usize>) {
        let matches = lcs::lcs_matches(&self.old_ids[old.clone()], &self.new_ids[new.clone()]);

        let old_len = old.len();
        let new_len = new.len();

        let mut i = 0usize;
        let mut j = 0usize;

        loop {
            // The next matched pair at or after the cursor.
            let mut match_old = i;

            while match_old < old_len && matches[match_old].is_none() {
                match_old += 1;
            }

            let (match_old, match_new) = match matches.get(match_old).copied().flatten() {
                Some(match_new) => (match_old, match_new),
                None => (old_len, new_len),
            };

            if i < match_old || j < match_new {
                self.emit_change(
                    old.start + i..old.start + match_old,
                    new.start + j..new.start + match_new,
                );
            }

            if match_old >= old_len {
                break;
            }

            // The matched run: consecutive aligned matches.
            let mut run = 1usize;

            while match_old + run < old_len && matches[match_old + run] == Some(match_new + run) {
                run += 1;
            }

            self.emit_common(old.start + match_old, new.start + match_new, run);

            i = match_old + run;
            j = match_new + run;
        }
    }

    /// Emit one changed region: the old lines as removals, the new lines as additions,
    /// all flagged when the change is whitespace-only.
    fn emit_change(&mut self, old: Range<usize>, new: Range<usize>) {
        let is_whitespace_only = is_whitespace_only_change(
            &self.old_lines[old.clone()],
            &self.new_lines[new.clone()],
        );

        for index in old {
            self.diffs.push(Diff {
                line_number_new: None,
                line_number_old: Some(index as u32 + 1),
                diff_type: DiffType::Remove,
                is_whitespace_only,
                line: self.old_lines[index].content.to_vec(),
            });
        }

        for index in new {
            self.diffs.push(Diff {
                line_number_new: Some(index as u32 + 1),
                line_number_old: None,
                diff_type: DiffType::Add,
                is_whitespace_only,
                line: self.new_lines[index].content.to_vec(),
            });
        }
    }

    /// Emit one common (unchanged) region — visible only in verbose mode.
    fn emit_common(&mut self, old_start: usize, new_start: usize, len: usize) {
        if !self.verbose {
            return;
        }

        for offset in 0..len {
            self.diffs.push(Diff {
                line_number_new: Some((new_start + offset) as u32 + 1),
                line_number_old: Some((old_start + offset) as u32 + 1),
                diff_type: DiffType::NoOp,
                is_whitespace_only: false,
                line: self.new_lines[new_start + offset].content.to_vec(),
            });
        }
    }
}

/// The slice of a line's (sorted) occurrence positions that falls inside a region.
fn occurrences_in_range<'a>(positions: &'a [u32], range: &Range<usize>) -> &'a [u32] {
    let lo = positions.partition_point(|&p| (p as usize) < range.start);
    let hi = positions.partition_point(|&p| (p as usize) < range.end);

    &positions[lo..hi]
}

/// Check whether a change is whitespace-only: the removed and added lines are equal once
/// every ASCII whitespace byte is dropped (lines that then become empty — blank lines —
/// are dropped entirely). This classifies reindentation, trailing-whitespace edits,
/// blank-line insertions and missing-final-newline changes.
fn is_whitespace_only_change(removed: &[Line<'_>], added: &[Line<'_>]) -> bool {
    let normalize = |lines: &[Line<'_>]| -> Vec<Vec<u8>> {
        lines.iter()
            .map(|line| {
                line.content.iter()
                    .copied()
                    .filter(|byte| !byte.is_ascii_whitespace())
                    .collect::<Vec<u8>>()
            })
            .filter(|normalized| !normalized.is_empty())
            .collect()
    };

    normalize(removed) == normalize(added)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed_lines(diffs: &[Diff], diff_type: DiffType) -> Vec<String> {
        diffs.iter()
            .filter(|d| d.diff_type == diff_type)
            .map(|d| String::from_utf8(d.line.clone()).unwrap())
            .collect()
    }

    #[test]
    fn identical_files_produce_no_diffs() {
        let content = b"line one\nline two\n";

        assert!(lines(content, content, false).is_empty());
    }

    #[test]
    fn a_changed_line_produces_a_remove_and_an_add() {
        let old = b"line one\nline two\nline three\n";
        let new = b"line one\nline 2\nline three\n";

        let diffs = lines(old, new, false);

        assert_eq!(changed_lines(&diffs, DiffType::Remove), vec!["line two"]);
        assert_eq!(changed_lines(&diffs, DiffType::Add), vec!["line 2"]);
        assert!(diffs.iter().all(|d| !d.is_whitespace_only));

        let remove = diffs.iter().find(|d| d.diff_type == DiffType::Remove).unwrap();
        assert_eq!(remove.line_number_old, Some(2));
        let add = diffs.iter().find(|d| d.diff_type == DiffType::Add).unwrap();
        assert_eq!(add.line_number_new, Some(2));
    }

    #[test]
    fn whitespace_only_changes_are_reported_and_flagged() {
        // The old readers treated whitespace-only lines as empty and produced no diff at
        // all; the object store disagrees (the blob hash changes), so the diff must
        // report the change — flagged, so presentation can dim or filter it.
        let old = b"line one\n \nline three\n";
        let new = b"line one\n\t\nline three\n";

        let diffs = lines(old, new, false);

        assert_eq!(diffs.len(), 2);
        assert!(diffs.iter().all(|d| d.is_whitespace_only));
    }

    #[test]
    fn reindentation_is_flagged_as_whitespace_only() {
        let old = b"fn a() {\nx();\n}\n";
        let new = b"fn a() {\n    x();\n}\n";

        let diffs = lines(old, new, false);

        assert_eq!(diffs.len(), 2);
        assert!(diffs.iter().all(|d| d.is_whitespace_only));
    }

    #[test]
    fn a_removed_trailing_newline_is_a_flagged_change() {
        let old = b"one\ntwo\n";
        let new = b"one\ntwo";

        let diffs = lines(old, new, false);

        assert_eq!(changed_lines(&diffs, DiffType::Remove), vec!["two"]);
        assert_eq!(changed_lines(&diffs, DiffType::Add), vec!["two"]);
        assert!(diffs.iter().all(|d| d.is_whitespace_only));
    }

    #[test]
    fn an_inserted_block_produces_only_additions() {
        let old = b"fn alpha() {\n    one();\n}\n\nfn omega() {\n    two();\n}\n";
        let new = b"fn alpha() {\n    one();\n}\n\nfn beta() {\n    three();\n}\n\nfn omega() {\n    two();\n}\n";

        let diffs = lines(old, new, false);

        // The inserted block shares its "}" and blank lines with the surroundings; those
        // must not be used as anchors in a way that turns the insertion into a
        // remove/add churn.
        assert!(changed_lines(&diffs, DiffType::Remove).is_empty(), "nothing was removed");
        assert_eq!(
            changed_lines(&diffs, DiffType::Add),
            vec!["fn beta() {", "    three();", "}", ""],
        );
    }

    #[test]
    fn a_run_found_through_a_common_line_keeps_the_rarity_of_its_rarest_line() {
        // The scan reaches "}" first: it is common, and the run it finds (which holds the
        // unique "one", "two", "three") is skipped past afterwards, so those lines are never
        // scanned on their own. Charged the cost of "}", that run lost to the moved line —
        // rarer than "}" — and anchoring on the moved line turned every other line into
        // churn (git.git's Makefile at 58d1772c85: +382/-371 for a +12/-1 change).
        let old = b"moved\n}\none\ntwo\nthree\n}\n}\nold tail\n";
        let new = b"}\none\ntwo\nthree\n}\n}\nmoved\nnew tail\n";

        let diffs = lines(old, new, false);

        assert_eq!(changed_lines(&diffs, DiffType::Remove), vec!["moved", "old tail"]);
        assert_eq!(changed_lines(&diffs, DiffType::Add), vec!["moved", "new tail"]);
    }

    #[test]
    fn repeated_lines_fall_back_to_the_exact_lcs() {
        // Every shared line occurs far above the anchor cap, so the histogram pass finds
        // no anchor and the exact LCS takes over.
        let old = "x\n".repeat(100);
        let new = format!("y\n{}y\n", "x\n".repeat(99));

        let diffs = lines(old.as_bytes(), new.as_bytes(), false);

        assert_eq!(changed_lines(&diffs, DiffType::Remove), vec!["x"]);
        assert_eq!(changed_lines(&diffs, DiffType::Add), vec!["y", "y"]);
    }

    #[test]
    fn verbose_mode_includes_unchanged_lines_with_both_line_numbers() {
        let old = b"one\ntwo\nthree\n";
        let new = b"one\nTWO\nthree\n";

        let diffs = lines(old, new, true);

        let noops: Vec<&Diff> = diffs.iter().filter(|d| d.diff_type == DiffType::NoOp).collect();
        assert_eq!(noops.len(), 2);
        assert!(noops.iter().all(|d| d.line_number_new.is_some() && d.line_number_old.is_some()));

        // Verbose output is in file order: "one" first, "three" last.
        assert_eq!(diffs.first().unwrap().line, b"one");
        assert_eq!(diffs.last().unwrap().line, b"three");
    }

    #[test]
    fn changes_in_multiple_regions_are_reported_in_file_order() {
        let old = b"a\ncommon one\nb\ncommon two\nc\n";
        let new = b"A\ncommon one\nB\ncommon two\nC\n";

        let diffs = lines(old, new, false);

        assert_eq!(changed_lines(&diffs, DiffType::Remove), vec!["a", "b", "c"]);
        assert_eq!(changed_lines(&diffs, DiffType::Add), vec!["A", "B", "C"]);

        // File order: the remove/add pairs appear region by region.
        let sequence: Vec<&[u8]> = diffs.iter().map(|d| d.line.as_slice()).collect();
        assert_eq!(sequence, vec![b"a" as &[u8], b"A", b"b", b"B", b"c", b"C"]);
    }

    #[test]
    fn empty_files_are_pure_additions_or_removals() {
        let content = b"one\ntwo\n";

        let additions = lines(b"", content, false);
        assert_eq!(changed_lines(&additions, DiffType::Add), vec!["one", "two"]);
        assert!(changed_lines(&additions, DiffType::Remove).is_empty());

        let removals = lines(content, b"", false);
        assert_eq!(changed_lines(&removals, DiffType::Remove), vec!["one", "two"]);
        assert!(changed_lines(&removals, DiffType::Add).is_empty());
    }
}
