//! What holds a warehouse format still (docs/format/WAREHOUSE_FORMAT.md): the census of every name
//! Forklift gives a file, the snapshot of the versions a format writes, and the frozen fixtures each
//! format leaves behind.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Output, Stdio};
use common::{forklift_command, repository_root, server_binary, test_command};

/// The prefixes of a constant that names a file or folder.
const NAME_PREFIXES: [&str; 4] = ["FILE_NAME", "FOLDER_NAME", "FILE_SUFFIX", "FILE_EXTENSION"];

/// Constants the version scan would take that are not a warehouse format's: the bound past which a
/// header is read as damage rather than as a newer version, and the key file's magic, which is
/// home-folder state outside the warehouse format that no upgrade touches.
const NOT_VERSIONS: [&str; 3] = [
    "pack_utils::PLAUSIBLE_VERSION_LIMIT", "signature_table::PLAUSIBLE_VERSION_LIMIT", "sign_utils::KEY_FILE_MAGIC",
];

/// The modes every format freezes a fixture of.
const MODES: [&str; 3] = ["full", "sparse", "cache"];

/// The fixture holding the operator every other fixture was made by: their global configuration
/// and keys, so a fixture's pending work can be finished and signed.
const OPERATOR: &str = "operator";

/// The first line of a fixture archive.
const ARCHIVE_HEADER: &str = "forklift-fixture 1";

/// One row of the census.
struct Row {
    paths: Vec<String>,
    /// The row names a place other than `.forklift/`, so a walk never matches it.
    elsewhere: bool,
    names: Vec<String>,
}

fn backticked(cell: &str) -> Vec<String> {
    cell.split('`').skip(1).step_by(2).map(str::to_string).collect()
}

fn census() -> Vec<Row> {
    let doc = std::fs::read_to_string(repository_root().join("docs/format/WAREHOUSE_FORMAT.md")).unwrap();
    let start = doc.find("<!-- census:start -->").expect("the census has a start marker");
    let end = doc.find("<!-- census:end -->").expect("the census has an end marker");

    doc[start..end].lines().skip(3).map(|line| {
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(cells.len(), 3, "a census row has three cells: {}", line);
        assert!(!cells[1].is_empty(), "a census row has a class: {}", line);

        Row { paths: backticked(cells[0]), elsewhere: !cells[0].ends_with('`'), names: backticked(cells[2]) }
    }).collect()
}

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Every `const` outside a test module of the sources under `crates/<crate>/src`, as
/// (`module::NAME`, its type and value as written).
fn source_constants(crates: &[&str]) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for name in crates {
        rust_files(&repository_root().join("crates").join(name).join("src"), &mut files);
    }

    let mut constants = Vec::new();
    for file in files {
        let module = file.file_stem().unwrap().to_string_lossy().into_owned();
        let mut after_cfg_test = false;

        for line in std::fs::read_to_string(&file).unwrap().lines() {
            let line = line.trim_start();
            if after_cfg_test && line.contains("mod ") {
                break;
            }
            after_cfg_test = line.starts_with("#[cfg(test)]");

            let line = line.strip_prefix("pub(crate) ").or_else(|| line.strip_prefix("pub ")).unwrap_or(line);
            let Some(rest) = line.strip_prefix("const ") else { continue };
            let Some((name, typed)) = rest.split_once(':') else { continue };
            constants.push((format!("{}::{}", module, name.trim()), typed.trim().to_string()));
        }
    }

    constants
}

/// Every name constant in the source is in the census, and every name in the census is a constant,
/// so a new file name cannot ship unclassified and a row cannot outlive its name.
///
/// Revert: leave a constant out of the census → the first assertion; name one that does not exist →
/// the second.
#[test]
fn every_name_constant_is_in_the_census() {
    let in_source = unique_keys(source_constants(&["forklift-core", "forklift", "forklift-server", "forklift-aws-lambda"])
        .into_iter()
        .filter(|(key, typed)| {
            let name = key.rsplit("::").next().unwrap();
            NAME_PREFIXES.iter().any(|prefix| name.starts_with(prefix)) && typed.contains("str")
        })
        .map(|(key, _)| key));
    assert!(in_source.len() > 40, "the scan finds the constants: {:?}", in_source);

    let in_census: BTreeSet<String> = census().into_iter().flat_map(|row| row.names).collect();

    let missing: Vec<&String> = in_source.difference(&in_census).collect();
    assert!(missing.is_empty(), "name constants with no census row in docs/format/WAREHOUSE_FORMAT.md: {:?}", missing);

    let stale: Vec<&String> = in_census.difference(&in_source).collect();
    assert!(stale.is_empty(), "census names that are not constants in the source: {:?}", stale);
}

/// `keys` as a set, failing on one that appears twice: two files with one stem would otherwise
/// hide each other's constant.
fn unique_keys(keys: impl IntoIterator<Item = String>) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for key in keys {
        assert!(set.insert(key.clone()), "two constants are both {}: rename one, so each is checked", key);
    }
    set
}

/// The record and object versions in the source, and the names their headers carry, by
/// `module::NAME`, each as its value is written: integer constants named for a version or format;
/// string constants naming a schema, magic or header that carry a version number in their text; and
/// the record kinds and format names a header opens with, since renaming one makes every file
/// already written read as having no header. Protocol versions are not the warehouse's.
fn source_versions() -> BTreeMap<String, String> {
    let versions: Vec<(String, String)> = source_constants(&["forklift-core"]).into_iter()
        .filter(|(key, _)| !NOT_VERSIONS.contains(&key.as_str()))
        .filter_map(|(key, typed)| {
            let name = key.rsplit("::").next().unwrap();
            let (kind, value) = typed.split_once('=')?;
            let (kind, value) = (kind.trim(), value.trim().trim_end_matches(';').to_string());

            let integer = ["u8", "u16", "u32", "u64", "i64"].contains(&kind)
                && (name.contains("VERSION") || name.contains("FORMAT"));
            let text = kind == "&str"
                && ["SCHEMA", "MAGIC", "HEADER", "VERSION"].iter().any(|word| name.contains(word))
                && !name.contains("PROTOCOL")
                && value.chars().any(|c| c.is_ascii_digit());
            let header_name = kind == "&str"
                && (name.ends_with("RECORD") || (name.contains("FORMAT") && !name.starts_with("FILE_NAME")));

            (integer || text || header_name).then_some((key, value))
        })
        .collect();

    unique_keys(versions.iter().map(|(key, _)| key.clone()));
    versions.into_iter().collect()
}

fn current_format() -> u64 {
    source_versions()["warehouse_utils::WAREHOUSE_FORMAT"].parse().unwrap()
}

fn fixtures_root() -> PathBuf {
    repository_root().join("crates/forklift/tests/fixtures")
}

/// Each `format-<N>` folder of fixtures, by `N`.
fn fixture_formats() -> BTreeMap<u64, PathBuf> {
    std::fs::read_dir(fixtures_root()).unwrap()
        .map(|entry| entry.unwrap().path())
        .filter_map(|path| {
            let format = path.file_name()?.to_str()?.strip_prefix("format-")?.parse().ok()?;
            Some((format, path))
        })
        .collect()
}

/// The source's versions are exactly the current format's snapshot, so a version cannot change, and
/// a record cannot be added or dropped, without a format bump.
///
/// Revert: change any version constant, or add one → the assertion names it.
#[test]
fn the_versions_are_the_current_formats() {
    let versions = source_versions();
    assert!(versions.contains_key("park_utils::PARKED_VERSION"), "the scan finds the versions: {:?}", versions);

    let format = current_format();
    let path = fixtures_root().join(format!("format-{}", format)).join("versions");
    let snapshot: BTreeMap<String, String> = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("format {} has no version snapshot at {}: {}", format, path.display(), e))
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = line.split_once(" = ").unwrap_or_else(|| panic!("a snapshot line is NAME = N: {}", line));
            (key.to_string(), value.to_string())
        })
        .collect();

    let changed: Vec<String> = versions.keys().chain(snapshot.keys()).collect::<BTreeSet<_>>().into_iter()
        .filter(|key| versions.get(*key) != snapshot.get(*key))
        .map(|key| format!("{}: frozen {:?}, now {:?}", key, snapshot.get(key), versions.get(key)))
        .collect();

    assert!(changed.is_empty(),
        "versions differ from format {}'s snapshot, which never changes: {:?}. A changed version is a format \
        break: bump WAREHOUSE_FORMAT and ship what docs/format/WAREHOUSE_FORMAT.md lists.", format, changed);
}

/// Every file and folder under `dir`, relative to it, with "/" between names and after a folder.
fn walk(dir: &Path) -> Vec<String> {
    fn visit(root: &Path, dir: &Path, paths: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let relative: Vec<String> = path.strip_prefix(root).unwrap().components()
                .map(|component| component.as_os_str().to_string_lossy().into_owned())
                .collect();
            let relative = relative.join("/");

            if std::fs::symlink_metadata(&path).unwrap().is_dir() {
                paths.push(format!("{}/", relative));
                visit(root, &path, paths);
            } else {
                paths.push(relative);
            }
        }
    }

    let mut paths = Vec::new();
    visit(dir, dir, &mut paths);
    paths.sort();
    paths
}

/// Whether `name` matches `pattern`, where each `<…>` stands for one or more characters.
fn name_matches(pattern: &str, name: &str) -> bool {
    match pattern.find('<') {
        None => pattern == name,
        Some(open) => {
            let close = open + pattern[open..].find('>').expect("a placeholder is closed");
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

/// Whether a path a walk found matches a census path, `{bay}` standing for the main tree's state
/// folder or a bay's.
fn census_matches(pattern: &str, path: &str) -> bool {
    let path: Vec<&str> = path.split('/').collect();

    let expanded = match pattern.strip_prefix("{bay}") {
        Some(rest) => vec![rest.to_string(), format!("bays/<name>/{}", rest)],
        None => vec![pattern.to_string()],
    };

    expanded.iter().any(|pattern| segments_match(&pattern.split('/').collect::<Vec<_>>(), &path))
}

/// The paths under a warehouse's `.forklift` that no census row matches.
fn unclassified(warehouse: &Path) -> Vec<String> {
    let patterns: Vec<String> = census().into_iter().filter(|row| !row.elsewhere).flat_map(|row| row.paths).collect();

    walk(&warehouse.join(".forklift")).into_iter()
        .filter(|path| !patterns.iter().any(|pattern| census_matches(pattern, path)))
        .collect()
}

/// The matcher the walk relies on, on the shapes the census uses.
///
/// Revert: let `<…>` match nothing → the `.sig` case; drop the `{bay}` expansion → the bay case;
/// make `**` match only one folder → the nested temp case.
#[test]
fn the_census_matcher_reads_its_patterns() {
    assert!(census_matches("objects/<xx>/<hash>.sig", "objects/ab/cdef.sig"));
    assert!(!census_matches("objects/<xx>/<hash>.sig", "objects/ab/.sig"));
    assert!(!census_matches("objects/<xx>/<hash>", "objects/ab/cd/ef"));
    assert!(census_matches("objects/", "objects/"));
    assert!(!census_matches("objects/", "objects"));
    assert!(census_matches("{bay}parked", "parked"));
    assert!(census_matches("{bay}parked", "bays/review/parked"));
    assert!(!census_matches("{bay}parked", "bays/parked"));
    assert!(census_matches("**/<name>.tmp~<pid>-<n>", "pallets/main.tmp~12-0"));
    assert!(census_matches("**/<name>.tmp~<pid>-<n>/**", "a/b/c.tmp~1-2/lock"));
    assert!(census_matches("mirror.git/**", "mirror.git/"));
    assert!(census_matches("mirror.git/**", "mirror.git/refs/heads/main"));
}

/// Write `warehouse`, whole, to `archive`: each folder and each file with its bytes.
fn pack_fixture(warehouse: &Path, archive: &Path) {
    let mut bytes = format!("{}\n", ARCHIVE_HEADER).into_bytes();

    for path in walk(warehouse) {
        if let Some(folder) = path.strip_suffix('/') {
            bytes.extend(format!("D {}\n", folder).as_bytes());
        } else {
            let content = std::fs::read(warehouse.join(&path)).unwrap();
            bytes.extend(format!("F {} {}\n", path, content.len()).as_bytes());
            bytes.extend(content);
            bytes.push(b'\n');
        }
    }

    std::fs::write(archive, bytes).unwrap();
}

/// Recreate the warehouse `archive` holds at `into`.
fn unpack_fixture(archive: &Path, into: &Path) {
    let bytes = std::fs::read(archive).unwrap();
    let mut rest = bytes.strip_prefix(format!("{}\n", ARCHIVE_HEADER).as_bytes())
        .unwrap_or_else(|| panic!("{} is not a fixture archive", archive.display()));
    std::fs::create_dir_all(into).unwrap();

    while !rest.is_empty() {
        let end = rest.iter().position(|&byte| byte == b'\n').expect("an entry line ends");
        let line = std::str::from_utf8(&rest[..end]).unwrap();
        rest = &rest[end + 1..];

        if let Some(folder) = line.strip_prefix("D ") {
            std::fs::create_dir_all(into.join(folder)).unwrap();
        } else {
            let (path, length) = line.strip_prefix("F ").and_then(|entry| entry.rsplit_once(' '))
                .unwrap_or_else(|| panic!("not an archive entry: {}", line));
            let length: usize = length.parse().unwrap();
            std::fs::write(into.join(path), &rest[..length]).unwrap();
            assert_eq!(rest[length], b'\n', "a file's bytes end with a line break");
            rest = &rest[length + 1..];
        }
    }
}

fn copy_folder(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for path in walk(from) {
        match path.strip_suffix('/') {
            Some(folder) => std::fs::create_dir_all(to.join(folder)).unwrap(),
            None => { std::fs::copy(from.join(&path), to.join(&path)).unwrap(); }
        }
    }
}

/// A scratch folder, deleted when it goes.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = std::env::temp_dir().join(format!("forklift-format-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn forklift(scratch: &Scratch, dir: &Path, args: &[&str]) -> Output {
    forklift_command()
        .args(args)
        .current_dir(dir)
        .env("FORKLIFT_GLOBAL_CONFIG", scratch.0.join(OPERATOR).join("global.toml"))
        .env("FORKLIFT_KEYS_DIR", scratch.0.join(OPERATOR).join("keys"))
        .output()
        .unwrap()
}

fn succeeded(output: &Output, what: &str) {
    assert!(output.status.success(), "{} failed:\n{}{}", what,
        String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
}

/// The fixtures every format freezes, and the commands that open each: between them they read every
/// record a fixture holds, so a reader that refuses its own format's bytes fails here.
fn fixtures() -> Vec<(&'static str, Vec<Vec<&'static str>>)> {
    let reads = || vec![
        vec!["stocktake"], vec!["history"], vec!["pallet", "list"], vec!["park", "list"], vec!["audit"],
        vec!["tag"], vec!["office"], vec!["scope"], vec!["store", "status"], vec!["show", "main:src/one.txt"],
    ];
    let finish = |name| vec![vec!["load", "."], vec!["stack", "-m", name], vec!["history"]];

    let mut fixtures: Vec<_> = MODES.iter().map(|mode| (*mode, reads())).collect();
    fixtures.push(("consolidating", [vec![vec!["conflicts"]], finish("consolidated")].concat()));
    fixtures.push(("picking", finish("picked")));
    fixtures.push(("served", reads()));
    fixtures
}

/// Open the fixture at `archive` and walk it against the census, before and after.
fn open_and_walk(archive: &Path, commands: &[Vec<&str>], upgrade: bool) {
    let name = archive.file_stem().unwrap().to_string_lossy().into_owned();
    let scratch = Scratch::new(&format!("fixture-{}", name));
    let warehouse = scratch.0.join("w");
    unpack_fixture(archive, &warehouse);
    unpack_fixture(&archive.with_file_name(format!("{}.fixture", OPERATOR)), &scratch.0.join(OPERATOR));

    let before = unclassified(&warehouse);
    assert!(before.is_empty(), "{}: paths no census row matches: {:?}", archive.display(), before);

    if upgrade {
        succeeded(&forklift(&scratch, &warehouse, &["upgrade"]), &format!("{}: upgrade", archive.display()));
    }

    for command in commands {
        succeeded(&forklift(&scratch, &warehouse, command), &format!("{}: {}", archive.display(), command.join(" ")));
    }

    let after = unclassified(&warehouse);
    assert!(after.is_empty(), "{}: after opening, paths no census row matches: {:?}", archive.display(), after);
}

/// The current format has a fixture of every mode, and the current build opens each one, which
/// holds only paths the census classifies.
///
/// Revert: drop a census row a fixture needs → the walk names the path; make the build refuse the
/// format → the commands fail.
#[test]
fn the_current_format_fixtures_open() {
    let folder = fixtures_root().join(format!("format-{}", current_format()));

    for (name, commands) in fixtures() {
        let archive = folder.join(format!("{}.fixture", name));
        assert!(archive.is_file(), "format {} has no {} fixture: run freeze_the_current_format_fixtures", current_format(), name);
        open_and_walk(&archive, &commands, false);
    }
}

/// Every older format's fixtures upgrade to the current format and then open. Format 1 is the first,
/// so there is none yet.
#[test]
fn older_format_fixtures_upgrade_and_open() {
    let current = current_format();

    for (format, folder) in fixture_formats() {
        assert!(format <= current, "fixtures of format {} are newer than this build's {}", format, current);
        if format == current {
            continue;
        }

        for (name, commands) in fixtures() {
            let archive = folder.join(format!("{}.fixture", name));
            if archive.is_file() {
                open_and_walk(&archive, &commands, true);
            }
        }
    }
}

/// A `forklift-server` serving `root` until it is dropped.
struct Server {
    child: Child,
    url: String,
}

impl Server {
    fn start(root: &Path) -> Server {
        let mut child = test_command(server_binary())
            .args(["serve", "--root", root.to_str().unwrap(), "--addr", "127.0.0.1:0", "--open"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();

        let mut line = String::new();
        BufReader::new(child.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let url = line.trim().rsplit(' ').next().unwrap().to_string();
        assert!(url.starts_with("http://"), "unexpected startup line: {}", line);

        Server { child, url }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Make the current format's fixtures. Run it once per format, after a bump:
/// `cargo test -p forklift --test warehouse_format -- --ignored freeze`. It refuses a format that
/// already has fixtures, since those never change.
#[test]
#[ignore]
fn freeze_the_current_format_fixtures() {
    let folder = fixtures_root().join(format!("format-{}", current_format()));
    assert!(!folder.join("full.fixture").exists(), "{} already holds frozen fixtures", folder.display());
    std::fs::create_dir_all(&folder).unwrap();

    let scratch = Scratch::new("freeze");
    let at = |name: &str| scratch.0.join(name);
    let run = |dir: &Path, args: &[&str]| succeeded(&forklift(&scratch, dir, args), &args.join(" "));
    let write = |path: PathBuf, content: &str| {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    };

    let prepared = test_command(server_binary()).args(["prepare", "--root", at("served").to_str().unwrap()]).output().unwrap();
    succeeded(&prepared, "server prepare");
    let server = Server::start(&at("served"));

    // A signed full warehouse with two pallets, a tag, a pallet deleted on the remote, packed objects
    // and parked work.
    let full = at("full");
    std::fs::create_dir_all(&full).unwrap();
    run(&full, &["prepare"]);
    run(&full, &["config", "set", "--global", "operator.identifier", "fixture@forklift"]);
    run(&full, &["config", "set", "remote.url", &server.url]);
    write(full.join("src/one.txt"), "one\n");
    write(full.join("docs/a.md"), "a\n");
    write(full.join("shared.txt"), "base\n");
    run(&full, &["load", "."]);
    run(&full, &["stack", "-m", "first"]);
    run(&full, &["office", "enroll"]);
    write(full.join("src/one.txt"), "one\ntwo\n");
    run(&full, &["load", "."]);
    run(&full, &["stack", "-m", "signed"]);
    run(&full, &["tag", "create", "v1"]);
    run(&full, &["pallet", "create", "feature"]);
    run(&full, &["shift", "feature"]);
    write(full.join("shared.txt"), "feature\n");
    run(&full, &["load", "."]);
    run(&full, &["stack", "-m", "feature"]);
    run(&full, &["lift"]);
    run(&full, &["shift", "main"]);
    write(full.join("shared.txt"), "main\n");
    run(&full, &["load", "."]);
    run(&full, &["stack", "-m", "main"]);
    run(&full, &["lift"]);
    run(&full, &["pallet", "create", "gone"]);
    run(&full, &["shift", "gone"]);
    run(&full, &["lift"]);
    run(&full, &["shift", "main"]);
    run(&full, &["pallet", "delete", "--remote", "gone"]);
    run(&full, &["store", "compact"]);
    write(full.join("src/parked.txt"), "parked\n");
    run(&full, &["load", "."]);
    run(&full, &["park"]);

    let bundled = test_command(server_binary()).args(["bundle", "--root", at("served").to_str().unwrap()]).output().unwrap();
    succeeded(&bundled, "server bundle");

    run(&scratch.0, &["clone", &server.url, "sparse", "--only", "src"]);
    run(&scratch.0, &["clone", &server.url, "cache", "--cache"]);

    // A consolidation and a pick of `feature`, each left in conflict over `shared.txt`.
    for (name, args) in [("consolidating", ["consolidate", "feature"]), ("picking", ["pick", "feature"])] {
        let warehouse = at(name);
        copy_folder(&full, &warehouse);
        let output = forklift(&scratch, &warehouse, &args);
        assert!(warehouse.join(".forklift").join(if name == "picking" { "cherry-pick" } else { "consolidation" }).is_file(),
            "{} left no record in conflict:\n{}{}", name,
            String::from_utf8_lossy(&output.stdout), String::from_utf8_lossy(&output.stderr));
    }
    drop(server);

    for (name, _) in fixtures() {
        let warehouse = at(name);
        // A dead server's lock is the one file a stopped process leaves that no command would.
        let _ = std::fs::remove_file(warehouse.join(".forklift").join("serve.lock"));
        let left = unclassified(&warehouse);
        assert!(left.is_empty(), "{}: paths no census row matches: {:?}", name, left);
        pack_fixture(&warehouse, &folder.join(format!("{}.fixture", name)));
    }
    pack_fixture(&at(OPERATOR), &folder.join(format!("{}.fixture", OPERATOR)));

    let mut versions = format!("# The versions format {} writes. Frozen: never edit this file.\n", current_format());
    for (key, value) in source_versions() {
        versions.push_str(&format!("{} = {}\n", key, value));
    }
    std::fs::write(folder.join("versions"), versions).unwrap();
}
