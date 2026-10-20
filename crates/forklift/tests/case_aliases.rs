//! Two names a case-insensitive file system folds together (`README` and `readme`, `Docs` and
//! `docs`) name one file there. These tests drive every materializer across such pairs and check
//! that none of them overwrites one entry with the other: a case-only rename lands, two spellings
//! that would share one file are refused before anything is written, and an untracked occupant
//! reached through another spelling is refused under its own name.
//!
//! Each test asks the volume it runs on whether it folds case, and asserts the outcome that volume
//! must produce: on a case-sensitive one, both spellings simply materialize side by side.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Output;

use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::StorageRootScope;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::pallet_utils;

mod common;
use common::forklift_command;

struct Warehouse {
    base: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    /// A warehouse on `main` with `files` stacked as its first parcel.
    fn new(name: &str, files: &[(&str, &str)]) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-case-aliases-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.name", "spike@forklift"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        for (path, content) in files {
            warehouse.write(path, content);
        }
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "first"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.root, args)
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "`{}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }

    /// Point pallet `name` at a parcel on top of `base` whose root tree holds exactly `files`.
    /// The tree is built directly, so it may hold names no working directory on this volume
    /// could stage.
    fn craft(&self, name: &str, base: &str, files: &[(&str, &str)]) {
        let _scope = StorageRootScope::enter(&self.root);

        let head = pallet_utils::get_pallet_head(base).unwrap().unwrap();
        let entries: Vec<(String, String)> = files.iter()
            .map(|(path, content)| (path.to_string(), content.to_string()))
            .collect();

        let crafted = Parcel {
            tree_hash: build_tree(&entries),
            parents: vec![head],
            actions: Vec::new(),
            description: Some("crafted".to_string()),
        };
        let parcel_hash = store(LooseObjectBuilder::build_parcel(&crafted));

        pallet_utils::update_pallet_head(
            name,
            pallet_utils::get_pallet_head(name).unwrap().as_deref(),
            &parcel_hash,
        ).unwrap();
    }

    fn folds_case(&self) -> bool {
        folds_case(&self.base)
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

/// Store `object` in the scoped warehouse and return its hash.
fn store(mut object: LooseObject) -> String {
    object.store().unwrap();
    object.hash
}

/// Store a tree holding `entries` (`/`-separated paths and their contents), returning its hash.
fn build_tree(entries: &[(String, String)]) -> String {
    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut subdirectories: Vec<(String, Vec<(String, String)>)> = Vec::new();

    for (path, content) in entries {
        match path.split_once('/') {
            None => {
                let hash = store(LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() }));
                tree.add_child(TreeItem::new(path.clone(), hash, DirEntryType::Normal));
            }
            Some((directory, rest)) => {
                let entry = (rest.to_string(), content.clone());
                match subdirectories.iter_mut().find(|(name, _)| name == directory) {
                    Some((_, children)) => children.push(entry),
                    None => subdirectories.push((directory.to_string(), vec![entry])),
                }
            }
        }
    }

    for (name, children) in subdirectories {
        tree.add_child(TreeItem::new(name, build_tree(&children), DirEntryType::Tree));
    }

    store(LooseObjectBuilder::build_tree(&tree))
}

/// Whether the volume holding `dir` resolves a name spelled in another case to the same file.
fn folds_case(dir: &Path) -> bool {
    let probe = dir.join("case-probe");
    std::fs::create_dir_all(&probe).unwrap();
    std::fs::write(probe.join("probe"), "").unwrap();
    let folds = probe.join("PROBE").exists();
    std::fs::remove_dir_all(&probe).unwrap();
    folds
}

/// Whether the volume holding `dir` resolves an NFD spelling to the NFC one's file.
fn folds_normalization(dir: &Path) -> bool {
    let probe = dir.join("normalization-probe");
    std::fs::create_dir_all(&probe).unwrap();
    std::fs::write(probe.join("caf\u{e9}"), "").unwrap();
    let folds = probe.join("cafe\u{301}").exists();
    std::fs::remove_dir_all(&probe).unwrap();
    folds
}

/// The names `dir` lists, exactly as spelled on disk.
fn listed(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir).unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

const ALIAS_REFUSAL: &str = "would write two entries this file system treats as one file";
const UNTRACKED_REFUSAL: &str = "would overwrite these untracked files";

fn assert_refused_with(output: &Output, refusal: &str, names: &[&str]) {
    let stderr = stderr(output);
    assert!(!output.status.success(), "expected a refusal, got success: {}", stdout(output));
    assert!(stderr.contains(refusal), "expected \"{}\": {}", refusal, stderr);
    for name in names {
        assert!(stderr.contains(name), "the refusal must name \"{}\": {}", name, stderr);
    }
}

fn assert_clean(warehouse: &Warehouse) {
    let status = stdout(&warehouse.run_ok(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

#[test]
fn a_tree_holding_two_spellings_of_one_name_is_refused_or_materialized_whole() {
    let warehouse = Warehouse::new("pair", &[("app.txt", "app\n")]);
    warehouse.craft("pair", "main", &[("app.txt", "app\n"), ("README", "upper\n"), ("readme", "lower\n")]);
    warehouse.craft("nested", "main", &[("app.txt", "app\n"), ("Docs/a", "a\n"), ("docs/b", "b\n")]);

    let pair = warehouse.run(&["shift", "pair"]);

    if warehouse.folds_case() {
        assert_refused_with(&pair, ALIAS_REFUSAL, &["README", "readme"]);
        let nested = warehouse.run(&["shift", "nested"]);
        assert_refused_with(&nested, ALIAS_REFUSAL, &["Docs", "docs"]);
        assert_eq!(listed(&warehouse.root), BTreeSet::from([".forklift".to_string(), "app.txt".to_string()]),
                   "a refused shift writes nothing");
        assert!(stdout(&warehouse.run_ok(&["stocktake"])).contains("On pallet \"main\""));
    } else {
        assert!(pair.status.success(), "{}", stderr(&pair));
        assert_eq!(warehouse.read("README"), "upper\n");
        assert_eq!(warehouse.read("readme"), "lower\n");
        warehouse.run_ok(&["shift", "nested"]);
        assert_eq!(warehouse.read("Docs/a"), "a\n");
        assert_eq!(warehouse.read("docs/b"), "b\n");
    }
}

/// `bay add` materializes through `materialize_tree`, which runs the same pair check.
#[test]
fn a_bay_of_a_tree_holding_two_spellings_is_refused_or_materialized_whole() {
    let warehouse = Warehouse::new("bay-pair", &[("app.txt", "app\n")]);
    warehouse.craft("main", "main", &[("app.txt", "app\n"), ("README", "upper\n"), ("readme", "lower\n")]);
    // A bay branches off the parcel the main tree stands on, so it records the crafted parcel as
    // its base, as stacking it there would have; its files stay as they are.
    {
        let _scope = StorageRootScope::enter(&warehouse.root);
        let head = pallet_utils::get_pallet_head("main").unwrap();
        pallet_utils::write_position(&pallet_utils::BayPosition::new("main", head)).unwrap();
    }

    let bay = warehouse.base.join("bay");
    let added = warehouse.run(&["bay", "add", "pairs", bay.to_str().unwrap()]);

    if warehouse.folds_case() {
        assert_refused_with(&added, ALIAS_REFUSAL, &["README", "readme"]);
        if bay.exists() {
            let names = listed(&bay);
            assert!(!names.contains("README") && !names.contains("readme"), "bay holds {:?}", names);
        }
    } else {
        assert!(added.status.success(), "{}", stderr(&added));
        assert_eq!(std::fs::read_to_string(bay.join("README")).unwrap(), "upper\n");
        assert_eq!(std::fs::read_to_string(bay.join("readme")).unwrap(), "lower\n");
    }
}

#[test]
fn a_shift_across_a_case_only_file_rename_keeps_the_file() {
    let warehouse = Warehouse::new("file-rename", &[("app.txt", "app\n")]);
    warehouse.craft("lower", "main", &[("app.txt", "app\n"), ("foo", "lower\n")]);
    warehouse.craft("upper", "main", &[("app.txt", "app\n"), ("Foo", "upper\n")]);

    warehouse.run_ok(&["shift", "lower"]);
    warehouse.run_ok(&["shift", "upper"]);
    assert!(listed(&warehouse.root).contains("Foo"));
    assert!(!listed(&warehouse.root).contains("foo"));
    assert_eq!(warehouse.read("Foo"), "upper\n");
    assert_clean(&warehouse);

    warehouse.run_ok(&["shift", "lower"]);
    assert!(listed(&warehouse.root).contains("foo"));
    assert!(!listed(&warehouse.root).contains("Foo"));
    assert_eq!(warehouse.read("foo"), "lower\n");
    assert_clean(&warehouse);
}

#[test]
fn a_shift_across_a_case_only_directory_rename_keeps_the_directory() {
    let warehouse = Warehouse::new("dir-rename", &[("app.txt", "app\n")]);
    warehouse.craft("lower", "main", &[("app.txt", "app\n"), ("Docs/a", "one\n")]);
    warehouse.craft("upper", "main", &[("app.txt", "app\n"), ("docs/a", "two\n")]);

    warehouse.run_ok(&["shift", "lower"]);
    warehouse.run_ok(&["shift", "upper"]);
    assert!(listed(&warehouse.root).contains("docs"));
    assert!(!listed(&warehouse.root).contains("Docs"));
    assert_eq!(warehouse.read("docs/a"), "two\n");
    assert_clean(&warehouse);
}

/// Anything the shift leaves under the old spelling keeps that directory, so the new spelling
/// would land inside it: an untracked file, an ignored one, or an empty untracked directory.
#[test]
fn a_case_only_directory_rename_is_refused_while_anything_keeps_the_old_directory() {
    let keepers: [(&str, fn(&Warehouse)); 3] = [
        ("untracked", |w| w.write("Docs/extra.txt", "mine\n")),
        ("ignored", |w| {
            w.write(".forkliftignore", "\\.log$\n");
            w.write("Docs/build.log", "mine\n");
        }),
        ("empty-dir", |w| std::fs::create_dir(w.root.join("Docs/empty")).unwrap()),
    ];

    for (label, keep) in keepers {
        let warehouse = Warehouse::new(&format!("dir-kept-{}", label), &[("app.txt", "app\n")]);
        warehouse.craft("lower", "main", &[("app.txt", "app\n"), ("Docs/a", "one\n")]);
        warehouse.craft("upper", "main", &[("app.txt", "app\n"), ("docs/a", "two\n")]);
        warehouse.run_ok(&["shift", "lower"]);
        keep(&warehouse);

        let shifted = warehouse.run(&["shift", "upper"]);

        if warehouse.folds_case() {
            assert_refused_with(&shifted, ALIAS_REFUSAL, &["Docs", "docs/a"]);
            assert_eq!(warehouse.read("Docs/a"), "one\n", "{}: the occupant is untouched", label);
            assert!(listed(&warehouse.root.join("Docs")).len() == 2, "{}: the keeper stays", label);
        } else {
            assert!(shifted.status.success(), "{}: {}", label, stderr(&shifted));
            assert_eq!(warehouse.read("docs/a"), "two\n");
        }
    }
}

#[test]
fn park_resets_a_staged_case_only_rename_and_pop_brings_it_back() {
    let warehouse = Warehouse::new("park-rename", &[("app.txt", "app\n"), ("foo", "old\n")]);
    std::fs::remove_file(warehouse.root.join("foo")).unwrap();
    warehouse.write("Foo", "new\n");
    warehouse.run_ok(&["load", "."]);

    warehouse.run_ok(&["park"]);
    assert!(listed(&warehouse.root).contains("foo"));
    assert!(!listed(&warehouse.root).contains("Foo"));
    assert_eq!(warehouse.read("foo"), "old\n");
    assert_clean(&warehouse);

    warehouse.run_ok(&["park", "pop"]);
    assert!(listed(&warehouse.root).contains("Foo"));
    assert!(!listed(&warehouse.root).contains("foo"));
    assert_eq!(warehouse.read("Foo"), "new\n");
}

/// Park's reset writes the staged-removed `foo` back; an untracked `FOO` is that file on a folding
/// volume, so park refuses before it records anything.
#[test]
fn park_refuses_an_untracked_occupant_of_a_path_its_reset_writes() {
    let warehouse = Warehouse::new("park-occupant", &[("app.txt", "app\n"), ("foo", "old\n")]);
    warehouse.run_ok(&["remove", "foo"]);
    let _ = std::fs::remove_file(warehouse.root.join("foo"));
    warehouse.write("FOO", "untracked\n");

    let parked = warehouse.run(&["park"]);

    if warehouse.folds_case() {
        assert_refused_with(&parked, UNTRACKED_REFUSAL, &["FOO"]);
        assert!(stdout(&warehouse.run_ok(&["park", "list"])).contains("There are no parked changes"));
        assert_eq!(warehouse.read("FOO"), "untracked\n");
    } else {
        assert!(parked.status.success(), "{}", stderr(&parked));
        assert_eq!(warehouse.read("foo"), "old\n");
        assert_eq!(warehouse.read("FOO"), "untracked\n");
    }
}

#[test]
fn a_merge_adding_a_second_spelling_beside_a_staying_entry_is_refused() {
    let warehouse = Warehouse::new("merge-alias", &[("app.txt", "app\n"), ("README", "ours\n"), ("Docs/a", "a\n")]);
    warehouse.craft("file", "main", &[("app.txt", "app\n"), ("README", "ours\n"), ("Docs/a", "a\n"),
                                      ("readme", "theirs\n")]);
    warehouse.craft("dir", "main", &[("app.txt", "app\n"), ("README", "ours\n"), ("Docs/a", "a\n"),
                                     ("docs/c", "theirs\n")]);
    warehouse.write("app.txt", "app, changed on main\n");
    warehouse.run_ok(&["load", "app.txt"]);
    warehouse.run_ok(&["stack", "diverge"]);

    let file = warehouse.run(&["consolidate", "file"]);

    if warehouse.folds_case() {
        // The staying tracked entry is named as the occupant, the new path after it.
        assert_refused_with(&file, ALIAS_REFUSAL, &["README and readme"]);
        assert_eq!(warehouse.read("README"), "ours\n");

        let dir = warehouse.run(&["consolidate", "dir"]);
        assert_refused_with(&dir, ALIAS_REFUSAL, &["Docs and docs/c"]);
        assert_eq!(listed(&warehouse.root.join("Docs")), BTreeSet::from(["a".to_string()]));
    } else {
        assert!(file.status.success(), "{}", stderr(&file));
        assert_eq!(warehouse.read("README"), "ours\n");
        assert_eq!(warehouse.read("readme"), "theirs\n");

        let dir = warehouse.run(&["consolidate", "dir"]);
        assert!(dir.status.success(), "{}", stderr(&dir));
        assert_eq!(warehouse.read("Docs/a"), "a\n");
        assert_eq!(warehouse.read("docs/c"), "theirs\n");
    }
}

/// `exists()` follows a link, so a dangling one reads as vacant; the check must not.
#[cfg(unix)]
#[test]
fn a_shift_refuses_a_dangling_symlink_reached_through_another_spelling() {
    let warehouse = Warehouse::new("dangling", &[("app.txt", "app\n")]);
    warehouse.craft("target", "main", &[("app.txt", "app\n"), ("readme", "tracked\n")]);
    std::os::unix::fs::symlink("nowhere", warehouse.root.join("README")).unwrap();

    let shifted = warehouse.run(&["shift", "target"]);

    if warehouse.folds_case() {
        assert_refused_with(&shifted, UNTRACKED_REFUSAL, &["README"]);
    } else {
        assert!(shifted.status.success(), "{}", stderr(&shifted));
        assert_eq!(warehouse.read("readme"), "tracked\n");
    }
    let link = std::fs::symlink_metadata(warehouse.root.join("README")).unwrap();
    assert!(link.file_type().is_symlink(), "the untracked link is left alone");
    assert_eq!(std::fs::read_link(warehouse.root.join("README")).unwrap(), Path::new("nowhere"));
}

/// A case-only directory rename on a folding volume stages as a move; restoring the old spelling
/// would write through the new one's file. While the removal is staged the restore is refused for
/// that, and once it is unstaged, for naming the same file.
#[test]
fn restore_refuses_a_path_that_names_another_spellings_file() {
    let warehouse = Warehouse::new("restore-alias", &[("app.txt", "app\n"), ("Docs/a", "old\n")]);
    if !warehouse.folds_case() {
        return;
    }

    std::fs::rename(warehouse.root.join("Docs"), warehouse.root.join("docs")).unwrap();
    warehouse.write("docs/a", "changed\n");
    warehouse.run_ok(&["load", "."]);

    let staged = warehouse.run(&["restore", "Docs/a"]);
    assert!(!staged.status.success());
    assert!(stderr(&staged).contains("The removal of \"Docs/a\" is staged"), "{}", stderr(&staged));
    assert_eq!(warehouse.read("docs/a"), "changed\n");

    warehouse.run_ok(&["unload", "Docs/a"]);

    let restored = warehouse.run(&["restore", "Docs/a"]);
    assert!(!restored.status.success());
    assert!(stderr(&restored).contains("names the same file as"), "{}", stderr(&restored));
    assert_eq!(warehouse.read("docs/a"), "changed\n");
}

/// A file where a new path needs a directory occupies the whole path: an untracked one is refused
/// by name on every volume, and on a folding one a staying tracked file reached through another
/// spelling is an alias collision.
#[test]
fn a_file_where_a_new_path_needs_a_directory_is_judged_as_its_occupant() {
    let warehouse = Warehouse::new("non-directory", &[("app.txt", "app\n"), ("Foo", "tracked\n")]);
    warehouse.craft("nested", "main", &[("app.txt", "app\n"), ("Foo", "tracked\n"), ("bar/a", "a\n")]);
    warehouse.write("bar", "untracked\n");

    let untracked = warehouse.run(&["shift", "nested"]);
    assert_refused_with(&untracked, UNTRACKED_REFUSAL, &["bar"]);
    assert_eq!(warehouse.read("bar"), "untracked\n");
    std::fs::remove_file(warehouse.root.join("bar")).unwrap();

    warehouse.craft("aliased", "main", &[("app.txt", "app\n"), ("Foo", "tracked\n"), ("foo/a", "a\n")]);
    let aliased = warehouse.run(&["shift", "aliased"]);

    if warehouse.folds_case() {
        assert_refused_with(&aliased, ALIAS_REFUSAL, &["Foo and foo/a"]);
        assert_eq!(warehouse.read("Foo"), "tracked\n");
    } else {
        assert!(aliased.status.success(), "{}", stderr(&aliased));
        assert_eq!(warehouse.read("foo/a"), "a\n");
    }
}

/// Two new spellings of which one is already on disk are judged against the disk alone, so the
/// refusal reports the untracked occupant once rather than also as a pair.
#[test]
fn a_new_pair_whose_one_name_is_on_disk_is_reported_once() {
    let warehouse = Warehouse::new("pair-on-disk", &[("app.txt", "app\n")]);
    warehouse.craft("pair", "main", &[("app.txt", "app\n"), ("a", "lower\n"), ("A", "upper\n")]);
    warehouse.write("a", "untracked\n");

    let shifted = warehouse.run(&["shift", "pair"]);
    assert_refused_with(&shifted, UNTRACKED_REFUSAL, &["a"]);
    assert!(!stderr(&shifted).contains(ALIAS_REFUSAL), "{}", stderr(&shifted));
    assert_eq!(warehouse.read("a"), "untracked\n");
}

/// A pair whose names resolve only to what the shift removes is still a pair: both are written,
/// so it is probed like any other rather than left to the per-path check.
#[test]
fn a_new_pair_replacing_a_removed_entry_is_refused_or_materialized_whole() {
    let warehouse = Warehouse::new("pair-replacing", &[("app.txt", "app\n"), ("README", "old\n"), ("Docs/a", "old\n")]);
    warehouse.craft("files", "main", &[("app.txt", "app\n"), ("Docs/a", "old\n"), ("readme", "lower\n"), ("ReadMe", "mixed\n")]);
    warehouse.craft("dirs", "main", &[("app.txt", "app\n"), ("README", "old\n"), ("docs/x", "x\n"), ("DOCS/y", "y\n")]);

    let files = warehouse.run(&["shift", "files"]);
    if warehouse.folds_case() {
        assert_refused_with(&files, ALIAS_REFUSAL, &["ReadMe and readme"]);
        assert_eq!(warehouse.read("README"), "old\n", "a refused shift removes nothing");
    } else {
        assert!(files.status.success(), "{}", stderr(&files));
        assert_eq!(warehouse.read("readme"), "lower\n");
        assert_eq!(warehouse.read("ReadMe"), "mixed\n");
        warehouse.run_ok(&["shift", "main"]);
    }

    let dirs = warehouse.run(&["shift", "dirs"]);
    if warehouse.folds_case() {
        assert_refused_with(&dirs, ALIAS_REFUSAL, &["DOCS and docs"]);
        assert_eq!(warehouse.read("Docs/a"), "old\n", "a refused shift removes nothing");
    } else {
        assert!(dirs.status.success(), "{}", stderr(&dirs));
        assert_eq!(warehouse.read("docs/x"), "x\n");
        assert_eq!(warehouse.read("DOCS/y"), "y\n");
    }
}

/// Two new directories spelled in NFC and NFD land on one directory where the volume normalizes,
/// and the second would be entered silently, merging their entries: the pair is refused up front.
#[test]
fn a_new_directory_pair_differing_only_in_normalization_is_refused_or_materialized_whole() {
    let (nfc, nfd) = ("caf\u{e9}", "cafe\u{301}");
    let warehouse = Warehouse::new("normalization", &[("app.txt", "app\n")]);
    warehouse.craft("pair", "main", &[("app.txt", "app\n"), (&format!("{}/a", nfc), "a\n"), (&format!("{}/b", nfd), "b\n")]);

    let shifted = warehouse.run(&["shift", "pair"]);

    if folds_normalization(&warehouse.base) {
        assert_refused_with(&shifted, ALIAS_REFUSAL, &[nfc, nfd]);
        assert_eq!(listed(&warehouse.root), BTreeSet::from([".forklift".to_string(), "app.txt".to_string()]),
                   "a refused shift writes nothing");
    } else {
        assert!(shifted.status.success(), "{}", stderr(&shifted));
        assert_eq!(warehouse.read(&format!("{}/a", nfc)), "a\n");
        assert_eq!(warehouse.read(&format!("{}/b", nfd)), "b\n");
    }
}

/// A file where the restored path needs a directory is refused by name, not left to fail inside
/// the write.
#[test]
fn restore_refuses_a_path_beneath_a_file() {
    let warehouse = Warehouse::new("restore-non-directory", &[("app.txt", "app\n"), ("foo/a.txt", "a\n")]);
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write("foo", "mine\n");

    for target in ["foo/a.txt", "foo"] {
        let restored = warehouse.run(&["restore", target]);
        assert!(!restored.status.success(), "restore {}", target);
        assert!(stderr(&restored).contains("\"foo\" is not a directory"), "restore {}: {}", target, stderr(&restored));
        assert_eq!(warehouse.read("foo"), "mine\n");
    }
}

/// A removed tracked symlink to a directory is a file the shift clears, so two new directory
/// spellings that resolve to it are probed as a pair rather than merged into one directory.
#[cfg(unix)]
#[test]
fn a_new_directory_pair_replacing_a_removed_symlink_is_refused_or_materialized_whole() {
    let warehouse = Warehouse::new("pair-replacing-link", &[("app.txt", "app\n"), ("real/x", "x\n")]);
    std::os::unix::fs::symlink("real", warehouse.root.join("lib")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "link"]);
    warehouse.craft("pair", "main", &[("app.txt", "app\n"), ("real/x", "x\n"), ("LIB/a", "a\n"), ("lib/b", "b\n")]);

    let shifted = warehouse.run(&["shift", "pair"]);

    if warehouse.folds_case() {
        assert_refused_with(&shifted, ALIAS_REFUSAL, &["LIB and lib"]);
        assert!(std::fs::symlink_metadata(warehouse.root.join("lib")).unwrap().file_type().is_symlink(),
                "a refused shift removes nothing");
        assert_eq!(listed(&warehouse.root.join("real")), BTreeSet::from(["x".to_string()]), "nothing written through the link");
    } else {
        assert!(shifted.status.success(), "{}", stderr(&shifted));
        assert_eq!(warehouse.read("LIB/a"), "a\n");
        assert_eq!(warehouse.read("lib/b"), "b\n");
    }
}

/// A pair beneath a symlink the shift replaces with a real directory is probed in the working
/// directory, never inside the link's target: an unwritable target must neither fail the shift
/// nor answer for the volume the directory is created on.
#[cfg(unix)]
#[test]
fn a_pair_beneath_a_replaced_symlink_is_probed_in_the_working_directory() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = Warehouse::new("probe-base-link", &[("app.txt", "app\n")]);
    let outside = warehouse.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, warehouse.root.join("lib")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "link"]);
    warehouse.craft("pair", "main", &[("app.txt", "app\n"), ("lib/d/x", "x\n"), ("lib/D/y", "y\n")]);

    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o555)).unwrap();
    let shifted = warehouse.run(&["shift", "pair"]);
    std::fs::set_permissions(&outside, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(!stderr(&shifted).contains("Could not tell"), "the probe ran in the link's target: {}", stderr(&shifted));
    assert!(listed(&outside).is_empty(), "nothing is written in the link's target");
    if warehouse.folds_case() {
        assert_refused_with(&shifted, ALIAS_REFUSAL, &["lib/D and lib/d"]);
    } else {
        assert!(shifted.status.success(), "{}", stderr(&shifted));
        assert_eq!(warehouse.read("lib/d/x"), "x\n");
        assert_eq!(warehouse.read("lib/D/y"), "y\n");
    }
}

/// A warehouse with `files` stacked beside `other` and `third`, then `other` edited and loaded,
/// `third` edited and left unstaged, and an untracked `stray`: the unrelated work a named load's
/// remedy is checked against.
fn with_unrelated_work(name: &str, files: &[(&str, &str)]) -> Warehouse {
    let mut stacked = files.to_vec();
    stacked.extend([("other", "o\n"), ("third", "t\n")]);

    let warehouse = Warehouse::new(name, &stacked);
    warehouse.write("other", "o2\n");
    warehouse.run_ok(&["load", "other"]);
    warehouse.write("third", "t2\n");
    warehouse.write("stray", "s\n");

    warehouse
}

impl Warehouse {
    fn rename(&self, from: &str, to: &str) {
        std::fs::rename(self.root.join(from), self.root.join(to)).unwrap();
    }

    /// The staged lines and the lines not in the inventory a stocktake lists, whitespace collapsed
    /// ("moved: README -> readme").
    fn stocktake(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        self.stocktake_in(&self.root)
    }

    /// [`stocktake`](Warehouse::stocktake) of the working tree at `dir`.
    fn stocktake_in(&self, dir: &Path) -> (BTreeSet<String>, BTreeSet<String>) {
        let output = self.run_in(dir, &["stocktake"]);
        assert!(output.status.success(), "{}", stderr(&output));
        let output = stdout(&output);
        let section = |heading: &str| -> BTreeSet<String> {
            output.lines()
                .skip_while(|line| !line.starts_with(heading))
                .skip(1)
                .take_while(|line| !line.trim().is_empty())
                .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
                .collect()
        };

        (section("Staged changes"), section("Changes not in the inventory"))
    }

    /// Run each command a refusal offers, from the root, in order.
    fn run_offered(&self, offered: &[Vec<String>]) {
        self.run_offered_in(&self.root, offered);
    }

    /// Run each command a refusal offers, from the working tree at `dir`, in order.
    fn run_offered_in(&self, dir: &Path, offered: &[Vec<String>]) {
        for command in offered {
            let args: Vec<&str> = command.iter().map(String::as_str).collect();
            let output = self.run_in(dir, &args);
            assert!(output.status.success(), "`{}` failed: {}", args.join(" "), stderr(&output));
        }
    }

    /// Park, then pop, both succeeding.
    fn park_and_pop(&self) {
        self.run_ok(&["park"]);
        self.run_ok(&["park", "pop"]);
    }
}

const NAMED_SPELLING_REFUSAL: &str = "this file system opens";
const STAGES_EVERYTHING: &str = "That stages every change in the working tree, not only this one, unstaged \
    edits and untracked files included: check \"forklift stocktake\" before stacking.";

/// The commands a refusal offers ("From the working tree's root, run "forklift a", then
/// "forklift b"."), each split into its arguments as a POSIX shell reads them. "forklift
/// stocktake" is a check, not a remedy.
fn offered_commands(output: &Output) -> Vec<Vec<String>> {
    let stderr = stderr(output);
    let Some((_, offer)) = stderr.split_once("From the working tree's root, run ")
        .or_else(|| stderr.split_once("To load it, run ")) else {
        return Vec::new();
    };

    offer.split('"')
        .skip(1)
        .step_by(2)
        .take_while(|command| command.starts_with("forklift ") && *command != "forklift stocktake")
        .map(|command| shell_words(command).into_iter().skip(1).collect())
        .collect()
}

/// `command` split into arguments as a POSIX shell reads it: single quotes keep their content
/// literal, and a backslash outside them keeps the next character.
fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word: Option<String> = None;
    let mut chars = command.chars();

    while let Some(c) = chars.next() {
        match c {
            ' ' => words.extend(word.take()),
            '\'' => {
                let current = word.get_or_insert_with(String::new);
                current.extend(chars.by_ref().take_while(|&c| c != '\''));
            }
            '\\' => word.get_or_insert_with(String::new).extend(chars.next()),
            c => word.get_or_insert_with(String::new).push(c),
        }
    }
    words.extend(word);

    words
}

/// A named load refused for its spelling: it fails naming the path the volume opens, and offers
/// exactly `offered`.
fn assert_spelling_refused(output: &Output, typed: &str, listed: &str, offered: &[&str]) {
    let stderr = stderr(output);
    assert!(!output.status.success(), "`load {}` was taken: {}", typed, stdout(output));
    assert!(stderr.contains(&format!("Nothing is listed as \"{}\": this file system opens \"{}\"", typed, listed)),
        "the refusal must name \"{}\": {}", listed, stderr);

    let offered_now: Vec<String> = offered_commands(output).iter().map(|command| command.join(" ")).collect();
    assert_eq!(offered_now, offered, "the remedy offered for `load {}`: {}", typed, stderr);
}

/// On a case-sensitive volume an unlisted spelling opens nothing, so the load fails as it always
/// has, offering nothing.
fn assert_plain_failure(output: &Output) {
    assert!(!output.status.success(), "{}", stdout(output));
    assert!(!stderr(output).contains(NAMED_SPELLING_REFUSAL), "{}", stderr(output));
}

/// After an unloaded case-only rename, a load that types an unlisted spelling would stage a second
/// live entry for the one file, and a park from there loses it. The refusal offers the pair's own
/// remedy instead, which stages only the rename and leaves unrelated work as it was.
#[test]
fn a_named_load_of_an_unlisted_spelling_offers_the_pair_scoped_remedy() {
    let states: [(&str, &str, bool, &[&str]); 3] = [
        ("README", "unloaded", false, &["remove README", "load readme"]),
        ("README", "readme-loaded", true, &["remove README"]),
        ("Readme", "third-spelling", false, &["remove README", "load readme"]),
    ];

    for (typed, label, load_new_first, offered) in states {
        let warehouse = with_unrelated_work(&format!("reach-file-{}", label), &[("README", "r\n")]);
        warehouse.rename("README", "readme");
        if load_new_first {
            warehouse.run_ok(&["load", "readme"]);
        }

        let loaded = warehouse.run(&["load", typed]);

        if !warehouse.folds_case() {
            assert_plain_failure(&loaded);
            continue;
        }
        assert_spelling_refused(&loaded, typed, "readme", offered);

        warehouse.run_offered(&offered_commands(&loaded));
        let (staged, unstaged) = warehouse.stocktake();
        assert_eq!(staged, BTreeSet::from(["moved: README -> readme".to_string(), "modified: other".to_string()]),
            "{}: only the rename is newly staged", label);
        assert_eq!(unstaged, BTreeSet::from(["modified: third".to_string(), "untracked: stray".to_string()]),
            "{}: unrelated work stays as it was", label);

        warehouse.park_and_pop();
        assert!(listed(&warehouse.root).contains("readme"), "{}: the file survives park", label);
        assert!(!listed(&warehouse.root).contains("README"), "{}", label);
        assert_eq!(warehouse.read("readme"), "r\n", "{}", label);
    }
}

/// With the rename already staged as a move, the old spelling is no longer live, so there is no
/// pair: the refusal offers the listed spelling, and a park after the refused load keeps the file.
#[test]
fn a_named_load_of_an_old_spelling_after_a_staged_move_offers_the_listed_spelling() {
    let warehouse = with_unrelated_work("reach-staged-move", &[("README", "r\n")]);
    warehouse.rename("README", "readme");
    warehouse.run_ok(&["load", "."]);

    let loaded = warehouse.run(&["load", "README"]);

    if !warehouse.folds_case() {
        assert_plain_failure(&loaded);
        return;
    }
    assert_spelling_refused(&loaded, "README", "readme", &["load readme"]);

    warehouse.park_and_pop();
    assert!(listed(&warehouse.root).contains("readme"), "the file survives park");
    assert!(!listed(&warehouse.root).contains("README"));

    warehouse.run_offered(&offered_commands(&loaded));
    let (staged, _) = warehouse.stocktake();
    assert!(staged.contains("moved: README -> readme"), "the move stays staged: {:?}", staged);
}

/// With no rename, a typed spelling the directory does not list is a typo: the refusal offers the
/// listed spelling, which loads, instead of registering the typed one beside it.
#[test]
fn a_named_load_of_a_case_typo_offers_the_listed_spelling() {
    let nfd = "cafe\u{301}";
    let warehouse = Warehouse::new("reach-typo", &[("README", "r\n"), ("Docs/a", "a\n"), (nfd, "c\n")]);
    warehouse.write("README", "r2\n");
    warehouse.write("Docs/a", "a2\n");

    let typos: [(&str, &str); 2] = [("readme", "README"), ("DOCS/a", "Docs/a")];

    for (typed, listed) in typos {
        let loaded = warehouse.run(&["load", typed]);

        if !warehouse.folds_case() {
            assert_plain_failure(&loaded);
            continue;
        }
        let offered = format!("load {}", listed);
        assert_spelling_refused(&loaded, typed, listed, &[offered.as_str()]);
        warehouse.run_offered(&offered_commands(&loaded));
    }

    if warehouse.folds_case() {
        let (staged, _) = warehouse.stocktake();
        assert_eq!(staged, BTreeSet::from(["modified: README".to_string(), "modified: Docs/a".to_string()]));
    }

    let nfc = "caf\u{e9}";
    let loaded = warehouse.run(&["load", nfc]);
    if folds_normalization(&warehouse.base) {
        let offered = format!("load {}", nfd);
        assert_spelling_refused(&loaded, nfc, nfd, &[offered.as_str()]);
        warehouse.run_offered(&offered_commands(&loaded));
    } else {
        assert_plain_failure(&loaded);
    }
}

/// Under a directory renamed in case only, the old directory spelling is still live (as the
/// directory itself, as an ancestor of a live file, or as the parent of a live alias), so loading
/// the listed path alone would leave the pair. The refusal offers loading the working tree's root,
/// and says that this stages everything else too, which the stocktake after it shows.
#[test]
fn a_named_load_under_a_case_renamed_directory_offers_loading_the_root() {
    let states: [(&str, &[(&str, &str)], &[(&str, &str)], &str, &str, &str); 3] = [
        ("depth-one", &[("Docs/a", "a\n")], &[("Docs", "docs")], "DOCS/a", "docs/a", "moved: Docs/a -> docs/a"),
        ("ancestor", &[("Docs/sub/a", "a\n")], &[("Docs", "docs")], "Docs/sub/a", "docs/sub/a",
            "moved: Docs/sub/a -> docs/sub/a"),
        ("alias-under-parent", &[("Docs/README", "a\n")], &[("Docs/README", "Docs/readme"), ("Docs", "docs")],
            "docs/README", "docs/readme", "moved: Docs/README -> docs/readme"),
    ];

    for (label, files, renames, typed, opened, moved) in states {
        let warehouse = with_unrelated_work(&format!("reach-dir-{}", label), files);
        for (from, to) in renames {
            warehouse.rename(from, to);
        }

        let loaded = warehouse.run(&["load", typed]);

        if !warehouse.folds_case() {
            assert_plain_failure(&loaded);
            continue;
        }
        assert_spelling_refused(&loaded, typed, opened, &["load ."]);
        assert!(stderr(&loaded).contains(STAGES_EVERYTHING), "{}: {}", label, stderr(&loaded));

        warehouse.run_offered(&offered_commands(&loaded));
        let (staged, unstaged) = warehouse.stocktake();
        let expected: BTreeSet<String> = [moved, "modified: other", "modified: third", "added: stray"]
            .into_iter().map(str::to_string).collect();
        assert_eq!(staged, expected, "{}: the root's load stages the unrelated work too", label);
        assert!(unstaged.is_empty(), "{}: {:?}", label, unstaged);

        warehouse.park_and_pop();
        assert_eq!(warehouse.read(opened), "a\n", "{}: the file survives park", label);
        assert!(listed(&warehouse.root).contains("docs"), "{}", label);
        assert!(!listed(&warehouse.root).contains("Docs"), "{}", label);
    }
}

/// Park re-stages tracked entries through `lstat`, which opens an unloaded case-only rename's new
/// spelling: the entry stays live, so park succeeds and the rename is left as an unstaged move.
#[test]
fn park_over_an_unloaded_case_only_rename_leaves_it_an_unstaged_move() {
    let warehouse = Warehouse::new("park-unloaded-rename", &[("README", "r\n"), ("other", "o\n")]);
    if !warehouse.folds_case() {
        return;
    }
    warehouse.write("other", "o2\n");
    warehouse.run_ok(&["load", "other"]);
    warehouse.rename("README", "readme");

    warehouse.run_ok(&["park"]);
    let (staged, unstaged) = warehouse.stocktake();
    assert!(staged.is_empty(), "{:?}", staged);
    assert_eq!(unstaged, BTreeSet::from(["moved: README -> readme".to_string()]));
    assert!(listed(&warehouse.root).contains("readme"));
    assert_eq!(warehouse.read("readme"), "r\n");

    warehouse.run_ok(&["park", "pop"]);
    let (staged, _) = warehouse.stocktake();
    assert_eq!(staged, BTreeSet::from(["modified: other".to_string()]));
}

/// A remedy's paths are shell arguments: a name holding a space is offered quoted, and the
/// commands, split as a shell splits them, stage only the rename.
#[test]
fn a_named_load_remedy_quotes_each_path_for_the_shell() {
    let warehouse = with_unrelated_work("reach-quoted", &[("My Notes", "n\n")]);
    warehouse.rename("My Notes", "my notes");

    let loaded = warehouse.run(&["load", "My Notes"]);

    if !warehouse.folds_case() {
        assert_plain_failure(&loaded);
        return;
    }
    assert!(stderr(&loaded).contains("run \"forklift remove 'My Notes'\", then \"forklift load 'my notes'\"."),
        "{}", stderr(&loaded));
    assert_spelling_refused(&loaded, "My Notes", "my notes", &["remove My Notes", "load my notes"]);

    warehouse.run_offered(&offered_commands(&loaded));
    let (staged, _) = warehouse.stocktake();
    assert_eq!(staged, BTreeSet::from(["moved: My Notes -> my notes".to_string(), "modified: other".to_string()]));
}

/// A folder that can be entered but not listed cannot say how a name in it is spelled, so a named
/// load beneath it is taken as typed, as it was before the spelling was checked.
#[cfg(unix)]
#[test]
fn a_named_load_beneath_a_folder_that_cannot_be_listed_is_taken_as_typed() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = Warehouse::new("reach-unlistable", &[("secret/file", "s\n")]);
    warehouse.write("secret/file", "s2\n");
    let secret = warehouse.root.join("secret");

    // The owner may enter the folder but not list it.
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o311)).unwrap();
    if unsafe { libc::geteuid() } == 0 {
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();
        return;
    }
    assert!(std::fs::read_dir(&secret).is_err(), "the folder must not be listable");

    let loaded = warehouse.run(&["load", "secret/file"]);
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o755)).unwrap();

    assert!(loaded.status.success(), "{}", stderr(&loaded));
    let (staged, _) = warehouse.stocktake();
    assert_eq!(staged, BTreeSet::from(["modified: secret/file".to_string()]));
}

/// Sharing a fold key only proposes a member of the pair. A live name the volume keeps apart
/// (`straße` beside `strasse` on a volume that folds case but not `ß`) is another file, so the
/// pair's remedy leaves its unstaged deletion alone.
#[test]
fn a_pair_remedy_leaves_a_name_the_volume_keeps_apart() {
    let warehouse = with_unrelated_work("reach-eszett", &[("Strasse", "s\n"), ("straße", "e\n")]);
    let names = listed(&warehouse.root);
    if !warehouse.folds_case() || !names.contains("Strasse") || !names.contains("straße") {
        return;
    }
    std::fs::remove_file(warehouse.root.join("straße")).unwrap();
    warehouse.rename("Strasse", "strasse");

    let loaded = warehouse.run(&["load", "Strasse"]);

    assert_spelling_refused(&loaded, "Strasse", "strasse", &["remove Strasse", "load strasse"]);
    warehouse.run_offered(&offered_commands(&loaded));
    let (staged, unstaged) = warehouse.stocktake();
    assert_eq!(staged, BTreeSet::from(["moved: Strasse -> strasse".to_string(), "modified: other".to_string()]));
    assert!(unstaged.contains("removed: straße"), "the deletion stays unstaged: {:?}", unstaged);
}

/// A path inside another working tree is refused as that, under any spelling: the spelling
/// refusal never offers a load the nested-working-tree refusal would then turn down.
#[test]
fn a_named_load_into_another_working_tree_is_refused_as_that_under_any_spelling() {
    let warehouse = Warehouse::new("reach-nested", &[("app", "a\n")]);
    warehouse.write("sub/x", "x\n");
    std::fs::create_dir(warehouse.root.join("sub/.forklift")).unwrap();

    let loaded = warehouse.run(&["load", "Sub/x"]);

    if !warehouse.folds_case() {
        assert_plain_failure(&loaded);
        return;
    }
    assert!(!loaded.status.success(), "{}", stdout(&loaded));
    assert!(stderr(&loaded).contains("root of another working tree"), "{}", stderr(&loaded));
    assert!(!stderr(&loaded).contains(NAMED_SPELLING_REFUSAL), "{}", stderr(&loaded));
}

/// A tracked folder that became another working tree's root is found under the spelling it is
/// registered by, so a load typing another spelling gets the tracked folder's own way out, which
/// leaves the folder to the other working tree.
#[test]
fn a_named_load_into_a_tracked_folder_turned_working_tree_offers_its_way_out_under_any_spelling() {
    let warehouse = Warehouse::new("reach-nested-tracked", &[("app", "a\n"), ("sub/x", "x\n")]);
    std::fs::create_dir(warehouse.root.join("sub/.forklift")).unwrap();

    let loaded = warehouse.run(&["load", "Sub/x"]);

    if !warehouse.folds_case() {
        assert_plain_failure(&loaded);
        return;
    }
    assert!(!loaded.status.success(), "{}", stdout(&loaded));
    assert!(stderr(&loaded).contains("but this warehouse tracks it. To leave it to the other working tree, run \
        \"forklift remove sub\""), "{}", stderr(&loaded));

    warehouse.run_ok(&["remove", "sub"]);
    warehouse.run_ok(&["stack", "leave sub"]);
    assert!(warehouse.root.join("sub/x").exists(), "the folder's files stay on disk");
}

/// The warehouse never enters a symlink, so a path beneath one is not respelled inside the link's
/// target: a load through it goes as it would under its exact spelling, never offering a path
/// through the link.
#[cfg(unix)]
#[test]
fn a_named_load_through_a_symlink_is_not_respelled_inside_its_target() {
    let warehouse = Warehouse::new("reach-link", &[("app", "a\n")]);
    let outside = warehouse.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("file"), "f\n").unwrap();
    std::os::unix::fs::symlink(&outside, warehouse.root.join("link")).unwrap();

    let typed = warehouse.run(&["load", "link/FILE"]);
    let exact = warehouse.run(&["load", "link/file"]);

    assert!(!stderr(&typed).contains(NAMED_SPELLING_REFUSAL), "{}", stderr(&typed));
    assert_eq!(typed.status.success(), exact.status.success(), "{} / {}", stderr(&typed), stderr(&exact));
}

/// In a scoped bay the scope is judged by the path the volume opens, so a case typo of an in-scope
/// path meets the spelling refusal (naming the listed path, which loads) rather than being called
/// out of scope.
#[test]
fn a_named_load_of_an_in_scope_alias_in_a_scoped_bay_offers_the_listed_spelling() {
    let warehouse = Warehouse::new("reach-scoped", &[("Docs/a", "a\n"), ("app", "p\n")]);
    let bay = warehouse.base.join("bay");
    warehouse.run_ok(&["bay", "add", "scoped", bay.to_str().unwrap(), "--scope", "Docs"]);
    std::fs::write(bay.join("Docs/a"), "a2\n").unwrap();

    let loaded = warehouse.run_in(&bay, &["load", "DOCS/a"]);

    if !warehouse.folds_case() {
        assert_plain_failure(&loaded);
        return;
    }
    assert_spelling_refused(&loaded, "DOCS/a", "Docs/a", &["load Docs/a"]);

    warehouse.run_offered_in(&bay, &offered_commands(&loaded));
    let (staged, _) = warehouse.stocktake_in(&bay);
    assert_eq!(staged, BTreeSet::from(["modified: Docs/a".to_string()]));
}
