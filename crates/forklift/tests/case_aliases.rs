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
        forklift_command()
            .args(args)
            .current_dir(&self.root)
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

        pallet_utils::set_pallet_head(name, &parcel_hash).unwrap();
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

/// Staging a case-only directory rename on a folding volume records both spellings (FORK-179);
/// restoring the old one would write through the new one's file.
#[test]
fn restore_refuses_a_path_that_names_another_spellings_file() {
    let warehouse = Warehouse::new("restore-alias", &[("app.txt", "app\n"), ("Docs/a", "old\n")]);
    if !warehouse.folds_case() {
        return;
    }

    std::fs::rename(warehouse.root.join("Docs"), warehouse.root.join("docs")).unwrap();
    warehouse.write("docs/a", "changed\n");
    warehouse.run_ok(&["load", "."]);

    let restored = warehouse.run(&["restore", "Docs/a"]);
    assert!(!restored.status.success());
    assert!(stderr(&restored).contains("names the same file as"), "{}", stderr(&restored));
    assert_eq!(warehouse.read("docs/a"), "changed\n");
}
