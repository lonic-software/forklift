//! The scanner behind the generator's refusal to publish references a reader cannot resolve
//! (see `docgen`). It lives outside the `docgen` module so its tests run under a plain
//! `cargo test`, which never compiles that feature.

/// Fails when `text`, rendered for `page`, cites something only the project's own trackers can
/// resolve: a `FORK-<n>` ticket, a `§<n>` section of the design document, or a review-round
/// label (`S<n>-F<n>`, "review round …", "round <n> finding"). Every description on a
/// generated page comes from a doc comment written for maintainers, and the page ships to
/// readers who can open none of those, so the reasoning must be stated in the sentence instead.
///
/// # Returns
/// * `Ok(())`      - No line holds such a reference.
/// * `Err(String)` - The page name and every offending line.
pub(crate) fn check_no_internal_refs(page: &str, text: &str) -> Result<(), String> {
    let hits: Vec<&str> = text.lines().filter(|line| has_internal_ref(line)).collect();
    if hits.is_empty() {
        return Ok(());
    }
    Err(format!(
        "docgen: {page} would cite references a reader cannot resolve (a FORK-<n> ticket, a \
         §<n> design section, or a review-round label). State the reasoning in the doc comment \
         the text comes from instead:\n{}",
        hits.join("\n")
    ))
}

/// Whether `line` holds a `FORK-<n>`, `§<n>`, `S<n>-F<n>`, "review round" or
/// "round <n> finding" reference, ignoring ASCII case. A rendered JSON description keeps a doc
/// comment's line breaks as `\n` escapes inside one physical line, so those escapes and every
/// whitespace run count as a single space: a label the source wrapped is still caught.
pub(crate) fn has_internal_ref(line: &str) -> bool {
    let upper = line.replace("\\n", " ").split_whitespace().collect::<Vec<_>>().join(" ")
        .to_ascii_uppercase();
    let bytes = upper.as_bytes();
    let digit_at = |i: usize| bytes.get(i).is_some_and(u8::is_ascii_digit);
    let skip = |mut i: usize, byte: u8| {
        while bytes.get(i) == Some(&byte) {
            i += 1;
        }
        i
    };

    let ticket = upper.match_indices("FORK-").any(|(i, _)| digit_at(i + 5));
    let section = upper.match_indices('§').any(|(i, s)| digit_at(skip(i + s.len(), b' ')));
    let starts_word = |i: usize| i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
    let digits_end = |i: usize| bytes[i..].iter().take_while(|b| b.is_ascii_digit()).count() + i;

    let review_round = upper.contains("REVIEW ROUND");
    let round_finding = upper.match_indices("ROUND ").any(|(i, s)| {
        let end = digits_end(i + s.len());
        starts_word(i) && end > i + s.len()
            && upper[skip(end, b',')..].strip_prefix(" FINDING")
                .is_some_and(|rest| !rest.starts_with(|c: char| c.is_ascii_alphanumeric()))
    });
    let label = upper.match_indices('S').any(|(i, _)| {
        let after_digits = digits_end(i + 1);
        starts_word(i) && after_digits > i + 1
            && upper[after_digits..].starts_with("-F") && digit_at(after_digits + 2)
    });
    ticket || section || review_round || round_finding || label
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flags_every_reference_shape() {
        for line in [
            "see FORK-49",
            "(fork-12) in lowercase",
            "design §7.7",
            "design § 8.12",
            "S2-F3",
            "multi-digit S12-F3 label",
            "(S2-F10)",
            "review round 5, finding 2",
            "Review Round 8",
            "(review round S2 fix hole)",
            "wrapped (review\\nround 5 finding 2)",
            "fix was unpinned (round 6 finding 1)",
            "round 6, finding 1",
            "stable across every review round",
        ] {
            assert!(has_internal_ref(line), "missed: {line:?}");
        }
    }

    #[test]
    fn passes_text_that_only_resembles_one() {
        for line in [
            "plain prose with no reference",
            "FORK- with no number",
            "forklift-core and FORK-x",
            "a lone § sign",
            "TLS1-F2 is mid-word, not a label",
            "S2-X3",
            "S-F1",
            "S2-F",
            "a round of review",
            "around 5 findings",
            "round 3 of 5",
            "in round 2 findings were batched",
            "",
        ] {
            assert!(!has_internal_ref(line), "false positive: {line:?}");
        }
    }

    #[test]
    fn the_check_names_every_offending_line_and_passes_a_clean_page() {
        assert_eq!(check_no_internal_refs("p.md", "clean\nalso clean\n"), Ok(()));

        let err = check_no_internal_refs("p.md", "clean\nsee FORK-1\nok\n(§3)\n").unwrap_err();
        assert!(err.starts_with("docgen: p.md "), "{err}");
        assert!(err.ends_with("\nsee FORK-1\n(§3)"), "{err}");
    }
}
