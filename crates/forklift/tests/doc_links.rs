//! Every relative link in the repository's Markdown pages resolves: the file it names exists, and
//! a `#fragment` names a heading of that page as GitHub renders it. A renamed heading or a moved
//! page fails this test instead of leaving a link that goes nowhere.
//!
//! The pages are every `.md` file git tracks, so a page added anywhere is checked without being
//! listed here.

#[allow(dead_code)] // The half of the module that launches binaries is not used here.
mod common;

use common::{prose_lines, repository_root, tracked_pages};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// The targets of the inline links `[text](target)` on a page, with their line numbers, outside
/// code. A code span may wrap onto the next line, so whether a backtick run opens or closes one
/// carries across lines, until the blank line that ends the paragraph.
fn link_targets(page: &str) -> Vec<(usize, String)> {
    let mut targets = Vec::new();
    let mut code_span: Option<usize> = None;
    for (index, line) in prose_lines(page).into_iter().enumerate() {
        if line.trim().is_empty() {
            code_span = None;
            continue;
        }

        let mut outside_code = String::new();
        let mut rest = line;
        while !rest.is_empty() {
            let ticks = rest.len() - rest.trim_start_matches('`').len();
            if ticks > 0 {
                code_span = match code_span {
                    None => Some(ticks),
                    Some(open) if open == ticks => None,
                    open => open,
                };
                outside_code.push(' ');
                rest = &rest[ticks..];
                continue;
            }
            let next = rest.find('`').unwrap_or(rest.len());
            if code_span.is_none() {
                outside_code.push_str(&rest[..next]);
            }
            rest = &rest[next..];
        }

        let mut rest = outside_code.as_str();
        while let Some(at) = rest.find("](") {
            let after = &rest[at + 2..];
            let end = after.find([')', ' ']).unwrap_or(after.len());
            targets.push((index + 1, after[..end].to_string()));
            rest = &after[end..];
        }
    }
    targets
}

/// The anchor GitHub gives a heading: lowercased, every character but letters, digits, spaces,
/// `-` and `_` dropped, and each space turned into `-`.
fn heading_anchor(heading: &str) -> String {
    heading
        .trim()
        .to_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || matches!(c, ' ' | '-' | '_'))
        .map(|c| if c == ' ' { '-' } else { c })
        .collect()
}

/// The anchors a page offers: one per heading, with `-1`, `-2` and so on after repeats, as
/// GitHub numbers them; for an HTML page, its `id` attributes.
fn anchors(path: &Path) -> HashSet<String> {
    let page = std::fs::read_to_string(path).unwrap();
    let mut anchors = HashSet::new();

    if path.extension().is_some_and(|extension| extension == "html") {
        for piece in page.split(" id=\"").skip(1) {
            if let Some(end) = piece.find('"') {
                anchors.insert(piece[..end].to_string());
            }
        }
        return anchors;
    }

    let mut seen: HashMap<String, usize> = HashMap::new();
    for line in prose_lines(&page) {
        let level = line.chars().take_while(|&c| c == '#').count();
        if !(1..=6).contains(&level) || !line[level..].starts_with(' ') {
            continue;
        }
        let anchor = heading_anchor(&line[level..]);
        let repeats = seen.entry(anchor.clone()).or_insert(0);
        anchors.insert(if *repeats == 0 { anchor.clone() } else { format!("{}-{}", anchor, repeats) });
        *repeats += 1;
    }
    anchors
}

/// What is wrong with one link on `page`, if anything.
fn broken(page: &Path, target: &str) -> Option<String> {
    if target.is_empty() {
        return Some("an empty target; a link must not wrap after its opening parenthesis".to_string());
    }
    if target.contains(':') && !target.starts_with('#') && !target.starts_with('.') {
        return None; // A URL (https:, mailto:), which this test does not fetch.
    }

    let (path, fragment) = match target.split_once('#') {
        Some((path, fragment)) => (path, Some(fragment)),
        None => (target, None),
    };
    let file = if path.is_empty() { page.to_path_buf() } else { page.parent().unwrap().join(path) };

    if !file.exists() {
        return Some(format!("{} does not exist", file.display()));
    }

    let fragment = fragment?;
    let checkable = file.extension().is_some_and(|extension| extension == "md" || extension == "html");
    if !checkable {
        return Some(format!("a #{} fragment on a file that has no headings", fragment));
    }
    if !anchors(&file).contains(fragment) {
        return Some(format!("{} has no heading with the anchor #{}", file.display(), fragment));
    }
    None
}

#[test]
fn every_relative_link_resolves() {
    let root = repository_root();
    let pages = tracked_pages();
    for expected in ["docs/guide/cli.md", "infra/aws-serverless/README.md"] {
        assert!(pages.contains(&root.join(expected)), "{} is not among the pages read", expected);
    }

    let mut problems = Vec::new();
    let mut checked = 0;
    for page in &pages {
        let text = std::fs::read_to_string(page).unwrap();
        for (line, target) in link_targets(&text) {
            checked += 1;
            if let Some(problem) = broken(page, &target) {
                problems.push(format!("{}:{}: ({}) {}", page.display(), line, target, problem));
            }
        }
    }

    assert!(checked > 100, "only {} links found; is the link reader broken?", checked);
    assert!(problems.is_empty(), "broken links:\n{}", problems.join("\n"));
}

#[test]
fn anchors_are_github_heading_anchors() {
    assert_eq!(heading_anchor(" 8. Machine & agent output"), "8-machine--agent-output");
    assert_eq!(heading_anchor(" `park` — stash (`pa`)"), "park--stash-pa");
    assert_eq!(heading_anchor(" `bay add --scope` — a scoped (sparse) bay"), "bay-add---scope--a-scoped-sparse-bay");
    assert_eq!(heading_anchor(" Ünïcode_names stay"), "ünïcode_names-stay");
}

#[test]
fn links_are_read_outside_code_only() {
    let targets = |page: &str| -> Vec<String> { link_targets(page).into_iter().map(|(_, target)| target).collect() };

    assert_eq!(targets("see [a](cli.md#x) and [b](../SERVER.md)"), ["cli.md#x", "../SERVER.md"]);
    assert_eq!(targets("[a](x.md \"title\")"), ["x.md"]);
    assert_eq!(targets("[`code` text](x.md)"), ["x.md"]);
    assert!(targets("`[not](a link)` and plain text").is_empty());
    assert!(targets("``a ` [not](a link)`` and plain text").is_empty());

    // A code span that wraps: the link after it on the next line is still read.
    assert_eq!(link_targets("ends with `a span\nthat wraps` then [a](x.md)."), [(2, "x.md".to_string())]);
    // A blank line ends a paragraph, so an unclosed backtick does not hide the next one's links.
    assert_eq!(targets("a stray ` backtick\n\n[a](x.md)"), ["x.md"]);
    // A target wrapped onto the next line is read as empty, which is reported, not skipped.
    assert_eq!(targets("[a](\nx.md)"), [""]);
}

#[test]
fn a_fence_closes_only_on_its_own_kind() {
    assert_eq!(prose_lines("a\n```sh\n[x](y)\n```\nb"), ["a", "", "", "", "b"]);
    assert_eq!(prose_lines("~~~\n```\n[x](y)\n~~~\nb"), ["", "", "", "", "b"]);
    assert_eq!(prose_lines("````\n```\n[x](y)\n````\nb"), ["", "", "", "", "b"]);
    assert_eq!(prose_lines("```\n```sh\nc\n```\nb"), ["", "", "", "", "b"]);
    assert_eq!(prose_lines("    ```\na"), ["    ```", "a"]);
}

#[test]
fn a_link_to_a_missing_page_or_heading_is_broken() {
    let area = std::env::temp_dir().join(format!("forklift-doc-links-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&area);
    std::fs::create_dir_all(&area).unwrap();
    let page = area.join("page.md");
    std::fs::write(&page, "# Title\n\n## Title\n\n```sh\n# not a heading\n```\n").unwrap();
    std::fs::write(area.join("notes.txt"), "text").unwrap();

    assert_eq!(broken(&page, "#title"), None);
    assert_eq!(broken(&page, "#title-1"), None);
    assert_eq!(broken(&page, "page.md#title"), None);
    assert_eq!(broken(&page, "notes.txt"), None);
    assert_eq!(broken(&page, "https://example.com/a#b"), None);

    assert!(broken(&page, "").is_some());
    assert!(broken(&page, "#title-2").is_some());
    assert!(broken(&page, "#not-a-heading").is_some());
    assert!(broken(&page, "missing.md").is_some());
    assert!(broken(&page, "notes.txt#x").is_some());

    let _ = std::fs::remove_dir_all(&area);
}
