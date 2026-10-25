//! Every relative link in the repository's Markdown pages resolves: the file it names exists, and
//! a `#fragment` names a heading of that page as GitHub renders it. A renamed heading or a moved
//! page fails this test instead of leaving a link that goes nowhere.
//!
//! The pages are found by listing the folders, not from a list kept here: every `.md` file at the
//! repository root and anywhere under `docs/`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Every Markdown page this test checks.
fn pages() -> Vec<PathBuf> {
    fn walk(folder: &Path, pages: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, pages);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                pages.push(path);
            }
        }
    }

    let root = repository_root();
    let mut pages: Vec<PathBuf> = std::fs::read_dir(&root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.is_file() && path.extension().is_some_and(|extension| extension == "md"))
        .collect();
    walk(&root.join("docs"), &mut pages);
    pages.sort();
    pages
}

/// The page's lines with fenced code blocks blanked, so a link shown inside an example is not
/// read as one. Line numbers are kept.
fn prose_lines(page: &str) -> Vec<&str> {
    let mut in_fence = false;
    page.lines()
        .map(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
                in_fence = !in_fence;
                ""
            } else if in_fence {
                ""
            } else {
                line
            }
        })
        .collect()
}

/// The targets of the inline links `[text](target)` on one line, outside code spans.
fn link_targets(line: &str) -> Vec<String> {
    let mut outside_code = String::new();
    for (index, piece) in line.split('`').enumerate() {
        if index % 2 == 0 {
            outside_code.push_str(piece);
        }
        outside_code.push(' ');
    }

    let mut targets = Vec::new();
    let mut rest = outside_code.as_str();
    while let Some(at) = rest.find("](") {
        let after = &rest[at + 2..];
        let end = after.find([')', ' ']).unwrap_or(after.len());
        targets.push(after[..end].to_string());
        rest = &after[end..];
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

    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
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
    let pages = pages();
    assert!(pages.iter().any(|page| page.ends_with("docs/guide/cli.md")), "no guide pages found");

    let mut problems = Vec::new();
    let mut checked = 0;
    for page in &pages {
        let text = std::fs::read_to_string(page).unwrap();
        for (index, line) in prose_lines(&text).into_iter().enumerate() {
            for target in link_targets(line) {
                checked += 1;
                if let Some(problem) = broken(page, &target) {
                    problems.push(format!("{}:{}: ({}) {}", page.display(), index + 1, target, problem));
                }
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
    assert_eq!(link_targets("see [a](cli.md#x) and [b](../SERVER.md)"), ["cli.md#x", "../SERVER.md"]);
    assert_eq!(link_targets("[a](x.md \"title\")"), ["x.md"]);
    assert!(link_targets("`[not](a link)` and plain text").is_empty());
    assert_eq!(prose_lines("a\n```sh\n[x](y)\n```\nb"), ["a", "", "", "", "b"]);
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

    assert!(broken(&page, "#title-2").is_some());
    assert!(broken(&page, "#not-a-heading").is_some());
    assert!(broken(&page, "missing.md").is_some());
    assert!(broken(&page, "notes.txt#x").is_some());

    let _ = std::fs::remove_dir_all(&area);
}
