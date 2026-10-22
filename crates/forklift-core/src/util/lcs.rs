/// For each element of `a`, find the index of the element it maps to in `b` under the
/// longest common subsequence of the two slices (`None` for unmatched elements).
///
/// Classic O(n·m) dynamic program — exact, no heuristics. Used by the merge (diff3) and
/// as the fallback of the display diff for regions without a usable anchor. Callers are
/// responsible for keeping the inputs small enough for the quadratic table.
///
/// # Arguments
/// * `a` - The first slice (the "base" side; the returned vector has one entry per element).
/// * `b` - The second slice.
///
/// # Returns
/// * `Vec<Option<usize>>` - One entry per element of `a`.
pub fn lcs_matches<T: Eq>(a: &[T], b: &[T]) -> Vec<Option<usize>> {
    let n = a.len();
    let m = b.len();

    // table[i][j] = LCS length of a[i..] and b[j..].
    let mut table = vec![vec![0u32; m + 1]; n + 1];

    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[i][j] = if a[i] == b[j] {
                table[i + 1][j + 1] + 1
            } else {
                table[i + 1][j].max(table[i][j + 1])
            };
        }
    }

    let mut matches = vec![None; n];
    let mut i = 0usize;
    let mut j = 0usize;

    while i < n && j < m {
        if a[i] == b[j] {
            matches[i] = Some(j);
            i += 1;
            j += 1;
        } else if table[i + 1][j] >= table[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }

    matches
}

/// For each element of `a`, the index of the element it maps to in `b` under **a** longest
/// common subsequence of the two slices (`None` for unmatched elements): as many matches as
/// [`lcs_matches`] finds, though where several subsequences are equally long it may pick a
/// different one.
///
/// Myers' O((n+m)·D) algorithm in linear space (D being the number of elements only one side
/// holds), so two long, similar inputs cost about their length rather than the product of their
/// lengths. The common prefix and suffix are matched first; what remains is split at the middle
/// snake of a shortest edit script, which halves D, and each half is solved the same way.
///
/// # Arguments
/// * `a` - The first slice (the returned vector has one entry per element).
/// * `b` - The second slice.
///
/// # Returns
/// * `Vec<Option<usize>>` - One entry per element of `a`; matched indices increase.
pub fn myers_matches<T: Eq>(a: &[T], b: &[T]) -> Vec<Option<usize>> {
    myers_matches_within(a, b, u64::MAX).expect("an unbounded search always finishes")
}

/// [`myers_matches`], giving up once the search has taken `budget` steps: each diagonal visited
/// and each equal pair walked along one costs a step, so the count is the work done. Two long inputs with little in common cost
/// about the product of their lengths even under Myers; a caller with a fallback for that case
/// bounds it here instead of by the inputs' lengths, which would also refuse a long input
/// edited lightly.
///
/// # Arguments
/// * `a` - The first slice (the returned vector has one entry per element).
/// * `b` - The second slice.
/// * `budget` - The most steps to take.
///
/// # Returns
/// * `Some(Vec<Option<usize>>)` - As [`myers_matches`] returns.
/// * `None` - The budget ran out first.
pub fn myers_matches_within<T: Eq>(a: &[T], b: &[T], budget: u64) -> Option<Vec<Option<usize>>> {
    let mut budget = budget;
    let mut matches = vec![None; a.len()];
    let mut work = vec![(0..a.len(), 0..b.len())];

    while let Some((mut old, mut new)) = work.pop() {
        while old.start < old.end && new.start < new.end && a[old.start] == b[new.start] {
            matches[old.start] = Some(new.start);
            old.start += 1;
            new.start += 1;
        }
        while old.start < old.end && new.start < new.end && a[old.end - 1] == b[new.end - 1] {
            old.end -= 1;
            new.end -= 1;
            matches[old.end] = Some(new.end);
        }
        // One side used up: the rest of the other is unmatched. Otherwise both sides hold an
        // element the other does not (the ends differ), so D is at least 2 and both halves
        // around the middle snake have a smaller D than this region.
        if old.is_empty() || new.is_empty() {
            continue;
        }

        let (x_start, y_start, x_end, y_end) =
            middle_snake(&a[old.clone()], &b[new.clone()], &mut budget)?;
        for offset in 0..x_end - x_start {
            matches[old.start + x_start + offset] = Some(new.start + y_start + offset);
        }
        work.push((old.start..old.start + x_start, new.start..new.start + y_start));
        work.push((old.start + x_end..old.end, new.start + y_end..new.end));
    }

    Some(matches)
}

/// Shift each run of unmatched elements of `matches` (from `a` to `b`) along equal neighbours to
/// where git's diff puts it, keeping the number of matches: as far down as it slides, unless it
/// can sit across from a run on the other side, where it is aligned with that run. Equally long
/// subsequences differ only in which of several equal elements they match, and this picks the
/// one `git diff` shows with its indent heuristic off (`xdl_change_compact`), so `blame`
/// mostly credits the parcels git's does.
///
/// # Arguments
/// * `a` - The first slice.
/// * `b` - The second slice.
/// * `matches` - A common subsequence of `a` and `b`, one entry per element of `a`.
///
/// # Returns
/// * `Vec<Option<usize>>` - The slid subsequence, as long as `matches`.
pub fn slide_changes<T: Eq>(a: &[T], b: &[T], matches: &[Option<usize>]) -> Vec<Option<usize>> {
    let mut a_changed = vec![true; a.len()];
    let mut b_changed = vec![true; b.len()];
    for (i, matched) in matches.iter().enumerate() {
        if let Some(j) = *matched {
            a_changed[i] = false;
            b_changed[j] = false;
        }
    }

    slide_side(a, &mut a_changed, &b_changed);
    slide_side(b, &mut b_changed, &a_changed);

    // Sliding keeps each side's unchanged elements equal in number and in order of content,
    // so pairing them in order rebuilds the subsequence.
    let mut slid = vec![None; a.len()];
    let mut j = 0usize;
    for (i, changed) in a_changed.iter().enumerate() {
        if !changed {
            while b_changed[j] {
                j += 1;
            }
            slid[i] = Some(j);
            j += 1;
        }
    }
    slid
}

/// A run of changed elements, `start..end` (possibly empty, between two unchanged ones).
struct Group {
    start: usize,
    end: usize,
}

impl Group {
    fn first(changed: &[bool]) -> Group {
        let mut end = 0;
        while end < changed.len() && changed[end] {
            end += 1;
        }
        Group { start: 0, end }
    }

    /// Move to the next group, past one unchanged element. False at the last group.
    fn next(&mut self, changed: &[bool]) -> bool {
        if self.end == changed.len() {
            return false;
        }
        self.start = self.end + 1;
        self.end = self.start;
        while self.end < changed.len() && changed[self.end] {
            self.end += 1;
        }
        true
    }

    /// Move to the previous group, before one unchanged element. False at the first group.
    fn previous(&mut self, changed: &[bool]) -> bool {
        if self.start == 0 {
            return false;
        }
        self.end = self.start - 1;
        self.start = self.end;
        while self.start > 0 && changed[self.start - 1] {
            self.start -= 1;
        }
        true
    }

    /// Slide down one element if the one after the group equals its first, absorbing any group
    /// it then touches. False if it cannot slide.
    fn slide_down<T: Eq>(&mut self, lines: &[T], changed: &mut [bool]) -> bool {
        if self.end == lines.len() || lines[self.start] != lines[self.end] {
            return false;
        }
        changed[self.start] = false;
        changed[self.end] = true;
        self.start += 1;
        self.end += 1;
        while self.end < changed.len() && changed[self.end] {
            self.end += 1;
        }
        true
    }

    /// Slide up one element if the one before the group equals its last, absorbing any group it
    /// then touches. False if it cannot slide.
    fn slide_up<T: Eq>(&mut self, lines: &[T], changed: &mut [bool]) -> bool {
        if self.start == 0 || lines[self.start - 1] != lines[self.end - 1] {
            return false;
        }
        self.start -= 1;
        self.end -= 1;
        changed[self.start] = true;
        changed[self.end] = false;
        while self.start > 0 && changed[self.start - 1] {
            self.start -= 1;
        }
        true
    }
}

/// [`slide_changes`] for one side: `changed` is that side's, `other` the other side's (read
/// only, walked in step so each group knows the group across from it).
fn slide_side<T: Eq>(lines: &[T], changed: &mut [bool], other: &[bool]) {
    let mut group = Group::first(changed);
    let mut across = Group::first(other);

    loop {
        if group.end > group.start {
            let mut aligned_end;
            let mut earliest_end;
            loop {
                let size = group.end - group.start;
                aligned_end = None;

                while group.slide_up(lines, changed) {
                    across.previous(other);
                }
                earliest_end = group.end;
                if across.end > across.start {
                    aligned_end = Some(group.end);
                }

                while group.slide_down(lines, changed) {
                    across.next(other);
                    if across.end > across.start {
                        aligned_end = Some(group.end);
                    }
                }

                // Sliding absorbed a neighbouring group: slide the larger one again.
                if group.end - group.start == size {
                    break;
                }
            }

            if group.end != earliest_end && aligned_end.is_some() {
                while across.end == across.start {
                    group.slide_up(lines, changed);
                    across.previous(other);
                }
            }
        }

        if !group.next(changed) {
            break;
        }
        across.next(other);
    }
}

/// The middle snake of a shortest edit script from `a` to `b` (both non-empty): a run of equal
/// elements, `a[x_start..x_end]` = `b[y_start..y_end]`, lying on some shortest script, found by
/// running the forward search from the start and the reverse one from the end until they meet.
/// The reverse search is the forward one over the reversed slices: reversed diagonal `delta - k`
/// is forward diagonal `k`, and a reversed x of `r` is a forward x of `n - r`.
///
/// # Returns
/// * `Some((usize, usize, usize, usize))` - `(x_start, y_start, x_end, y_end)`.
/// * `None` - `budget` ran out first (each diagonal visited and each equal pair walked costs one).
fn middle_snake<T: Eq>(a: &[T], b: &[T], budget: &mut u64) -> Option<(usize, usize, usize, usize)> {
    let n = a.len() as isize;
    let m = b.len() as isize;
    let delta = n - m;
    let odd = delta % 2 != 0;
    let max = (n + m + 1) / 2;
    // Diagonals run from -(max + 1) to max + 1; `offset` maps diagonal k to index k + offset.
    let offset = max + 1;
    let mut forward = vec![0isize; (2 * max + 3) as usize];
    let mut reverse = vec![0isize; (2 * max + 3) as usize];

    // The furthest x on diagonal k after one more step: from k + 1 (an insertion keeps x) or
    // from k - 1 (a deletion advances x), whichever reaches further.
    let step = |v: &[isize], k: isize, d: isize| -> isize {
        if k == -d || (k != d && v[(k - 1 + offset) as usize] < v[(k + 1 + offset) as usize]) {
            v[(k + 1 + offset) as usize]
        } else {
            v[(k - 1 + offset) as usize] + 1
        }
    };

    for d in 0..=max {
        let mut k = -d;
        while k <= d {
            let x_start = step(&forward, k, d);
            let y_start = x_start - k;
            let (mut x, mut y) = (x_start, y_start);
            while x < n && y < m && a[x as usize] == b[y as usize] {
                x += 1;
                y += 1;
            }
            forward[(k + offset) as usize] = x;
            *budget = budget.checked_sub(1 + (x - x_start) as u64)?;

            // An odd delta meets a reverse path of d - 1 steps on this diagonal.
            let reverse_k = delta - k;
            if odd && reverse_k.abs() < d && x + reverse[(reverse_k + offset) as usize] >= n {
                return Some((x_start as usize, y_start as usize, x as usize, y as usize));
            }
            k += 2;
        }

        let mut k = -d;
        while k <= d {
            let r_start = step(&reverse, k, d);
            let s_start = r_start - k;
            let (mut r, mut s) = (r_start, s_start);
            while r < n && s < m && a[(n - 1 - r) as usize] == b[(m - 1 - s) as usize] {
                r += 1;
                s += 1;
            }
            reverse[(k + offset) as usize] = r;
            *budget = budget.checked_sub(1 + (r - r_start) as u64)?;

            // An even delta meets a forward path of d steps on this diagonal.
            let forward_k = delta - k;
            if !odd && forward_k.abs() <= d && r + forward[(forward_k + offset) as usize] >= n {
                let (x_start, y_start) = ((n - r) as usize, (m - s) as usize);
                return Some((x_start, y_start, (n - r_start) as usize, (m - s_start) as usize));
            }
            k += 2;
        }
    }

    unreachable!("a forward and a reverse search of (n + m + 1) / 2 steps each always meet")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_follow_the_longest_common_subsequence() {
        let a = [1, 2, 3, 4];
        let b = [2, 4, 5];

        assert_eq!(lcs_matches(&a, &b), vec![None, Some(0), None, Some(1)]);
    }

    #[test]
    fn disjoint_slices_have_no_matches() {
        assert_eq!(lcs_matches(&[1, 2], &[3, 4]), vec![None, None]);
        assert_eq!(myers_matches(&[1, 2], &[3, 4]), vec![None, None]);
    }

    /// Check `matches` is a common subsequence of `a` and `b`: matched elements are equal and
    /// matched indices strictly increase. Returns its length.
    fn common_subsequence_length(a: &[u8], b: &[u8], matches: &[Option<usize>]) -> usize {
        assert_eq!(matches.len(), a.len());
        let mut last: Option<usize> = None;
        let mut length = 0;
        for (i, matched) in matches.iter().enumerate() {
            if let Some(j) = *matched {
                assert_eq!(a[i], b[j], "a[{i}] is matched with an unequal b[{j}]: {a:?} / {b:?}");
                assert!(last.map_or(true, |previous| j > previous),
                    "matches must increase: {matches:?} for {a:?} / {b:?}");
                last = Some(j);
                length += 1;
            }
        }
        length
    }

    #[test]
    fn myers_finds_a_longest_common_subsequence_on_every_input() {
        // Against the exact table as the oracle, over inputs small enough to enumerate many of:
        // a common subsequence, and as long as the longest. A small alphabet makes repeated
        // elements (and so many equally long subsequences) the common case.
        let mut state: u64 = 0x2545_f491_4f6c_dd1d;
        let mut next = |bound: u64| -> u64 {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state % bound
        };
        for case in 0..20_000 {
            let alphabet = 1 + next(4) as u8;
            let a: Vec<u8> = (0..next(12)).map(|_| next(alphabet as u64) as u8).collect();
            let b: Vec<u8> = (0..next(12)).map(|_| next(alphabet as u64) as u8).collect();

            let exact = common_subsequence_length(&a, &b, &lcs_matches(&a, &b));
            let fast_matches = myers_matches(&a, &b);
            let fast = common_subsequence_length(&a, &b, &fast_matches);
            let slid = common_subsequence_length(&a, &b, &slide_changes(&a, &b, &fast_matches));

            assert_eq!(fast, exact,
                "case {case}: myers matched {fast}, the longest is {exact}: {a:?} / {b:?}");
            assert_eq!(slid, exact, "case {case}: sliding left {slid} of {exact} matches: {a:?} / {b:?}");
        }
    }

    #[test]
    fn an_added_run_slides_down_past_an_equal_line_as_git_shows_it() {
        // `a }` becomes `a } b }`: the added run is either `} b` or `b }`. Git's diff shows
        // `b }` (slid as far down as it goes), so the old `}` keeps matching the first one.
        let a = ["a", "}"];
        let b = ["a", "}", "b", "}"];

        assert_eq!(slide_changes(&a, &b, &[Some(0), Some(3)]), vec![Some(0), Some(1)]);
        assert_eq!(slide_changes(&a, &b, &[Some(0), Some(1)]), vec![Some(0), Some(1)]);
    }

    #[test]
    fn a_removed_run_stays_across_from_the_run_that_replaced_it() {
        // `z z` becomes `x z`: the removed `z` could be either one. Git's diff shows the first
        // replaced by `x` (a run that can sit across from a run on the other side stays there)
        // rather than sliding it down past the equal second one.
        let a = ["z", "z"];
        let b = ["x", "z"];

        assert_eq!(slide_changes(&a, &b, &[None, Some(1)]), vec![None, Some(1)]);
        assert_eq!(slide_changes(&a, &b, &[Some(1), None]), vec![None, Some(1)]);
    }

    #[test]
    fn a_bounded_search_gives_up_on_unrelated_inputs_but_not_on_similar_ones() {
        // Two unrelated 2,000-element inputs need D = 4,000 rounds; one edit needs one.
        let a: Vec<u32> = (0..2_000).collect();
        let unrelated: Vec<u32> = (2_000..4_000).rev().collect();
        let mut edited = a.clone();
        edited[1_000] = u32::MAX;

        assert_eq!(myers_matches_within(&a, &unrelated, 1_000_000), None);
        assert_eq!(myers_matches_within(&a, &edited, 1_000_000), Some(myers_matches(&a, &edited)));
    }

    #[test]
    fn myers_matches_long_similar_inputs_without_a_quadratic_table() {
        // 200,000 elements a side: the exact table would be 40 billion cells. A few scattered
        // edits leave every other element matched.
        let a: Vec<u32> = (0..200_000).collect();
        let mut b = a.clone();
        b.remove(150_000);
        b.insert(100_000, u32::MAX);
        b[50_000] = u32::MAX - 1;

        let matches = myers_matches(&a, &b);

        assert_eq!(matches.iter().filter(|m| m.is_some()).count(), 200_000 - 2);
    }
}
