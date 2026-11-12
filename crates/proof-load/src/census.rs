//! The census of the version under test: the table in that version's
//! `docs/format/WAREHOUSE_FORMAT.md` naming every path a warehouse holds and its class.
//!
//! It is read from the source tree the harness is given for the version under test, never from the
//! harness's own checkout: a borrowed census would judge one version's files by another's names.
//! A tree with no census is a result ("not run: no census for this version"), never a fallback;
//! a census that is present but cannot be read is an error, never a guess.

use std::fmt;
use std::path::{Path, PathBuf};

/// Where the census lives inside a source tree.
pub const CENSUS_DOCUMENT: &str = "docs/format/WAREHOUSE_FORMAT.md";
const START_MARKER: &str = "<!-- census:start -->";
const END_MARKER: &str = "<!-- census:end -->";

/// A census row's class. The set is closed: a class this harness does not know refuses the census.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Class {
    HistoryBorne,
    Rewritable,
    Derived,
    Lock,
    Probe,
    Transient,
    Foreign,
    Outside,
}

impl Class {
    fn parse(cell: &str) -> Option<Class> {
        Some(match cell {
            "history-borne" => Class::HistoryBorne,
            "rewritable" => Class::Rewritable,
            "derived" => Class::Derived,
            "lock" => Class::Lock,
            "probe" => Class::Probe,
            "transient" => Class::Transient,
            "foreign" => Class::Foreign,
            "outside" => Class::Outside,
            _ => return None,
        })
    }

    /// Whether every name of this class needs a damage binding or a named exemption.
    pub fn needs_damage(self) -> bool {
        matches!(self, Class::HistoryBorne | Class::Rewritable | Class::Derived | Class::Lock)
    }
}

/// One census row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// Each path the row lists, relative to `.forklift/` unless `elsewhere`.
    pub paths: Vec<String>,
    pub class: Class,
    /// The row names a place other than `.forklift/` (text follows its last path), so a walk of a
    /// warehouse never matches it.
    pub elsewhere: bool,
}

/// The census of one version.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Census {
    pub rows: Vec<Row>,
    /// The document it was read from, for the report.
    pub source: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
pub enum CensusError {
    Unreadable { path: PathBuf, reason: String },
    Malformed { path: PathBuf, reason: String },
}

impl fmt::Display for CensusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CensusError::Unreadable { path, reason } => write!(f, "cannot read the census at {}: {}", path.display(), reason),
            CensusError::Malformed { path, reason } => write!(f, "the census at {} cannot be read: {}", path.display(), reason),
        }
    }
}

impl std::error::Error for CensusError {}

/// The paths a Path cell lists: its leading comma-separated backticked items. A qualifier after
/// them ("in a bay's own `.forklift`") may quote a name of its own, which is not a listed path.
fn backticked(cell: &str) -> Vec<String> {
    cell.split(", ").map_while(|item| item.strip_prefix('`')?.strip_suffix('`').filter(|inner| !inner.contains('`')))
        .map(str::to_string).collect()
}

impl Census {
    /// Reads the census of the source tree at `tree`. `Ok(None)` means the version has no census:
    /// the document or its markers are absent. Anything present but unreadable is an error.
    pub fn load(tree: &Path) -> Result<Option<Census>, CensusError> {
        let path = tree.join(CENSUS_DOCUMENT);
        let document = match std::fs::read_to_string(&path) {
            Ok(document) => document,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(CensusError::Unreadable { path, reason: error.to_string() }),
        };

        Census::parse(&document, path)
    }

    /// Parses a census document. `Ok(None)` when it has neither marker.
    pub fn parse(document: &str, source: PathBuf) -> Result<Option<Census>, CensusError> {
        let malformed = |reason: String| CensusError::Malformed { path: source.clone(), reason };

        let (start, end) = match (document.find(START_MARKER), document.find(END_MARKER)) {
            (None, None) => return Ok(None),
            (Some(start), Some(end)) if start < end => (start + START_MARKER.len(), end),
            _ => return Err(malformed("its start and end markers do not enclose a table".to_string())),
        };

        let mut lines = document[start..end].lines().map(str::trim).filter(|line| !line.is_empty());
        let header = lines.next().ok_or_else(|| malformed("the table is empty".to_string()))?;
        if header != "| Path | Class | Names |" {
            return Err(malformed(format!("unexpected header {:?}", header)));
        }
        lines.next().ok_or_else(|| malformed("the table has no separator row".to_string()))?;

        let mut rows = Vec::new();
        for line in lines {
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() != 3 {
                return Err(malformed(format!("a row has {} cells, not 3: {}", cells.len(), line)));
            }
            let class = Class::parse(cells[1]).ok_or_else(|| malformed(format!("unknown class {:?} in: {}", cells[1], line)))?;
            let paths = backticked(cells[0]);
            if paths.is_empty() {
                return Err(malformed(format!("a row names no path: {}", line)));
            }

            rows.push(Row { paths, class, elsewhere: !cells[0].ends_with('`') });
        }

        if rows.is_empty() {
            return Err(malformed("the table has no rows".to_string()));
        }

        Ok(Some(Census { rows, source }))
    }

    /// Every path name a damage binding must cover: each listed name of a row whose class needs
    /// damage, on its own, since a binding on one of a row's names says nothing about the others.
    pub fn damage_names(&self) -> Vec<DamageName> {
        let mut names: Vec<DamageName> = self.rows.iter()
            .filter(|row| row.class.needs_damage() && !row.elsewhere)
            .flat_map(|row| row.paths.iter().map(move |path| DamageName { path: path.clone(), class: row.class }))
            .collect();
        names.sort();
        names.dedup();
        names
    }

    /// The paths of a walk that no row of this census matches.
    pub fn unclassified<'a>(&self, walked: &'a [String]) -> Vec<&'a str> {
        let patterns: Vec<&str> = self.rows.iter().filter(|row| !row.elsewhere)
            .flat_map(|row| row.paths.iter().map(String::as_str)).collect();

        walked.iter().map(String::as_str)
            .filter(|path| !patterns.iter().any(|pattern| matches(pattern, path)))
            .collect()
    }
}

/// One name a damage binding must cover.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
pub struct DamageName {
    pub path: String,
    pub class: Class,
}

/// Whether `name` matches `pattern`, where each `<…>` stands for one or more characters.
fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.find('<') {
        None => pattern == name,
        Some(open) => {
            let Some(close) = pattern[open..].find('>').map(|close| open + close) else { return pattern == name };
            let (literal, rest) = (&pattern[..open], &pattern[close + 1..]);
            let Some(name) = name.strip_prefix(literal) else { return false };

            (1..=name.len()).filter(|&cut| name.is_char_boundary(cut))
                .any(|cut| name_matches(rest, &name[cut..]))
        }
    }
}

fn segments_match(pattern: &[&str], path: &[&str]) -> bool {
    match (pattern.first(), path.first()) {
        (None, None) => true,
        (Some(&"**"), _) => segments_match(&pattern[1..], path) || (!path.is_empty() && segments_match(pattern, &path[1..])),
        (Some(name), Some(segment)) => name_matches(name, segment) && segments_match(&pattern[1..], &path[1..]),
        _ => false,
    }
}

/// Whether a walked path (relative to `.forklift/`, `/`-separated, a folder ending in `/`) matches
/// a census path, `{bay}` standing for the main tree's state folder or a bay's.
pub fn matches(pattern: &str, path: &str) -> bool {
    let path: Vec<&str> = path.split('/').collect();

    let expanded = match pattern.strip_prefix("{bay}") {
        Some(rest) => vec![rest.to_string(), format!("bays/<name>/{}", rest)],
        None => vec![pattern.to_string()],
    };

    expanded.iter().any(|pattern| segments_match(&pattern.split('/').collect::<Vec<_>>(), &path))
}

/// Every path under `root`, relative to it, `/`-separated, folders ending in `/`, sorted. Links are
/// listed, never followed.
pub fn walk(root: &Path) -> std::io::Result<Vec<String>> {
    fn visit(root: &Path, dir: &Path, paths: &mut Vec<String>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let path = entry?.path();
            let relative: Vec<String> = path.strip_prefix(root).unwrap_or(&path).components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect();
            let relative = relative.join("/");

            if std::fs::symlink_metadata(&path)?.is_dir() {
                paths.push(format!("{}/", relative));
                visit(root, &path, paths)?;
            } else {
                paths.push(relative);
            }
        }
        Ok(())
    }

    let mut paths = Vec::new();
    visit(root, root, &mut paths)?;
    paths.sort();
    Ok(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "intro\n<!-- census:start -->\n| Path | Class | Names |\n|---|---|---|\n\
        | `warehouse` | rewritable | `a::B` |\n\
        | `store.lock`, `serve.lock` | lock | `a::C`, `a::D` |\n\
        | `objects/<xx>/<hash>` | history-borne | `a::E` |\n\
        | `clock` | probe | `a::F` |\n\
        | `redirect`, in a bay's own `.forklift` | rewritable | `a::H` |\n\
        | `~/.forkliftconfig`, in the home folder | outside | `a::G` |\n\
        <!-- census:end -->\n";

    fn sample() -> Census {
        Census::parse(SAMPLE, PathBuf::from("sample")).unwrap().unwrap()
    }

    /// Revert: count a row instead of each of its names → `serve.lock` goes missing; let a probe or
    /// an outside row need damage → `clock` or the home config appears; read every backticked span
    /// of a Path cell as a path → the qualifier's `.forklift` appears.
    #[test]
    fn damage_is_owed_per_listed_name_of_the_damageable_classes() {
        let names: Vec<String> = sample().damage_names().into_iter().map(|name| name.path).collect();
        assert_eq!(names, vec!["objects/<xx>/<hash>", "redirect", "serve.lock", "store.lock", "warehouse"]);
    }

    /// A tree with no census document, or a document with no markers, is a version with no census.
    #[test]
    fn a_missing_census_is_none_not_an_error() {
        assert_eq!(Census::parse("no table here", PathBuf::from("x")), Ok(None));
        let empty = std::env::temp_dir().join(format!("proof-load-no-census-{}", std::process::id()));
        assert_eq!(Census::load(&empty), Ok(None));
    }

    /// Revert: skip an unknown class, or accept one marker alone → these parse instead of refusing.
    #[test]
    fn a_census_it_cannot_read_is_refused() {
        let unknown = SAMPLE.replace("| probe |", "| ephemeral |");
        assert!(matches!(Census::parse(&unknown, PathBuf::from("x")), Err(CensusError::Malformed { .. })));
        let half = SAMPLE.replace("<!-- census:end -->", "");
        assert!(matches!(Census::parse(&half, PathBuf::from("x")), Err(CensusError::Malformed { .. })));
        let wide = SAMPLE.replace("| `clock` | probe | `a::F` |", "| `clock` | probe | `a::F` | extra |");
        assert!(matches!(Census::parse(&wide, PathBuf::from("x")), Err(CensusError::Malformed { .. })));
    }

    /// Revert: let the walk match rows that name another place → the home config row would classify
    /// a `.forklift/~/.forkliftconfig`; drop the per-segment match → `objects/ab/cd/ef` passes.
    #[test]
    fn the_walk_check_names_every_unmatched_path() {
        let walked: Vec<String> = ["warehouse", "objects/ab/cdef", "objects/ab/cd/ef", "~/.forkliftconfig", "stray"]
            .iter().map(|path| path.to_string()).collect();
        assert_eq!(sample().unclassified(&walked), vec!["objects/ab/cd/ef", "~/.forkliftconfig", "stray"]);
    }

    #[test]
    fn the_matcher_reads_the_census_shapes() {
        assert!(matches("objects/<xx>/<hash>.sig", "objects/ab/cdef.sig"));
        assert!(!matches("objects/<xx>/<hash>.sig", "objects/ab/.sig"));
        assert!(matches("objects/", "objects/"));
        assert!(!matches("objects/", "objects"));
        assert!(matches("{bay}parked", "parked"));
        assert!(matches("{bay}parked", "bays/review/parked"));
        assert!(!matches("{bay}parked", "bays/parked"));
        assert!(matches("**/<name>.tmp~<pid>-<n>", "pallets/main.tmp~12-3"));
        assert!(matches("**/<name>.tmp~<pid>-<n>/**", "inventory/x.tmp~1-2/a/b"));
    }

    /// The census of the tree this harness is built from reads, and is not empty. A version under
    /// test is read the same way from the tree it is given.
    #[test]
    fn this_trees_census_reads() {
        let tree = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let census = Census::load(&tree).unwrap().expect("this tree has a census");
        assert!(census.damage_names().iter().any(|name| name.path == "refs.lock"));
    }
}
