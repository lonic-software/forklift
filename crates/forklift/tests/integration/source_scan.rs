//! Reading the workspace's production sources for the exact-set tests: which functions outside
//! test code hold a line a test names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub(crate) fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Whether `line` calls `primitive`: the name, not part of a longer identifier, followed by `(`.
pub(crate) fn calls(line: &str, primitive: &str) -> bool {
    let call = format!("{}(", primitive);

    line.match_indices(&call).any(|(at, _)| {
        !line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') && !line[..at].ends_with("fn ")
    })
}

/// The name of the function a line at `index` sits in: the nearest `fn` above it.
fn enclosing_fn(lines: &[&str], index: usize) -> String {
    lines[..=index].iter().rev()
        .find_map(|line| {
            let (_, rest) = line.split_once("fn ")?;
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            (!name.is_empty() && line.trim_start().split("fn ").next().unwrap().split_whitespace()
                .all(|word| ["pub", "pub(crate)", "async", "unsafe", "const"].contains(&word))).then_some(name)
        })
        .unwrap_or_default()
}

/// Every line outside test code and comments, under the workspace-relative `dirs`, that `matches`
/// counts, as (file, function) once per count.
pub(crate) fn production_sites(dirs: &[&str], matches: impl Fn(&str) -> usize) -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in dirs {
        rust_files(&root.join(dir), &mut files);
    }

    let mut sites = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        let relative = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        let test_lines = test_module_lines(&lines);
        for index in 0..lines.len() {
            let line = lines[index];
            if test_lines[index] || line.trim_start().starts_with("//") {
                continue;
            }

            for _ in 0..matches(line) {
                sites.push((relative.clone(), enclosing_fn(&lines, index)));
            }
        }
    }

    sites.sort();
    sites
}

pub(crate) fn counted<'a>(sites: impl Iterator<Item = (&'a str, &'a str)>) -> BTreeMap<(String, String), usize> {
    let mut counts = BTreeMap::new();
    for (file, function) in sites {
        *counts.entry((file.to_string(), function.to_string())).or_insert(0) += 1;
    }
    counts
}

/// The sites `found` and `expected` hold differently: found but not listed, then listed but not
/// found, each naming its count.
pub(crate) fn differences(found: &[(String, String)], expected: &[(&str, &str)]) -> (Vec<String>, Vec<String>) {
    let found = counted(found.iter().map(|(file, function)| (file.as_str(), function.as_str())));
    let expected = counted(expected.iter().copied());

    let unlisted = found.iter()
        .filter(|(key, count)| expected.get(*key) != Some(count))
        .map(|((file, function), count)| format!("{} {} ({} found)", file, function, count))
        .collect();
    let gone = expected.iter()
        .filter(|(key, count)| found.get(*key) != Some(count))
        .map(|((file, function), count)| format!("{} {} ({} listed)", file, function, count))
        .collect();

    (unlisted, gone)
}

/// The scan finds a call however it is spelled, and skips definitions, comments and test code.
#[test]
fn the_scan_finds_calls_and_skips_definitions() {
    assert!(calls("    pallet_utils::update_pallet_head(name, None, &head, &none)?;", "update_pallet_head"));
    assert!(calls("let x = update_pallet_head(a)", "update_pallet_head"));
    assert!(!calls("pub fn update_pallet_head(name: &str,", "update_pallet_head"));
    assert!(!calls("    update_meta_pallet_head(name)", "update_pallet_head"));
    assert!(!calls("    update_pallet_head_in(name)", "update_pallet_head"));

    let lines = ["pub(crate) async fn outer() {", "    let f = |x| x;", "    update_pallet_head(a);", "}"];
    assert_eq!(enclosing_fn(&lines, 2), "outer");
}

/// Which of a file's lines belong to an inline test module (`#[cfg(test)]` over `mod name {`): from
/// the attribute to the module's closing brace, the first line after it that is `}` at the left
/// margin, as every top-level item here closes. A module in its own file (`mod name;`) marks
/// nothing: that file is scanned like any other. Production code after a test module is still
/// scanned.
pub(crate) fn test_module_lines(lines: &[&str]) -> Vec<bool> {
    let mut marked = vec![false; lines.len()];
    let mut index = 0;
    while index < lines.len() {
        let opens = lines[index].trim() == "#[cfg(test)]" && lines.get(index + 1).is_some_and(|next| {
            let next = next.trim_start();
            (next.starts_with("mod ") || next.starts_with("pub mod ") || next.starts_with("pub(crate) mod "))
                && next.trim_end().ends_with('{')
        });
        if !opens {
            index += 1;
            continue;
        }
        let close = (index + 2..lines.len()).find(|&i| lines[i] == "}").unwrap_or(lines.len() - 1);
        for line in &mut marked[index..=close] {
            *line = true;
        }
        index = close + 1;
    }
    marked
}
