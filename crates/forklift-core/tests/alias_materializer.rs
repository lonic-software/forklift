//! A materialization from a tree holding two spellings of one name, on a volume that folds them,
//! never loses the spelling the target keeps: removing the other spelling unlinks the one file
//! both name, so the kept spelling is written back from the target.
//!
//! These drive `shift_utils::diff_trees` and `apply_ops` (and `kept_through_aliases`, which the
//! merges behind `consolidate` and `cherry-pick` call) directly, from a working directory holding
//! one file for the pair, a state no command on a folding volume would build. They live in
//! their own test binary because materialization resolves paths against the process's current
//! directory, which `in_working_tree` below changes and serializes.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::StorageRootScope;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::scope_utils::MaterializationScope;
use forklift_core::util::shift_utils::{self, FileOp};

/// Run `body` with the current directory at `root` and the warehouse there in scope, one test at a
/// time.
fn in_working_tree<T>(root: &Path, body: impl FnOnce() -> T) -> T {
    static CWD: Mutex<()> = Mutex::new(());
    let _guard = CWD.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let previous = std::env::current_dir().unwrap();
    std::env::set_current_dir(root).unwrap();
    let scope = StorageRootScope::enter(root);

    let result = body();

    drop(scope);
    std::env::set_current_dir(previous).unwrap();
    result
}

/// A fresh working tree with an empty metadata folder.
fn working_tree(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("forklift-alias-materializer-{}-{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join(".forklift")).unwrap();
    // A full warehouse, so the main tree checks out every path.
    std::fs::write(root.join(".forklift").join("warehouse"), "/forklift-warehouse 2\nfull\n").unwrap();
    root
}

fn folds_case(root: &Path) -> bool {
    let probe = root.join("case-probe");
    std::fs::create_dir_all(&probe).unwrap();
    std::fs::write(probe.join("probe"), "").unwrap();
    let folds = probe.join("PROBE").exists();
    std::fs::remove_dir_all(&probe).unwrap();
    folds
}

fn store(mut object: LooseObject) -> String {
    object.store().unwrap();
    object.hash
}

/// Store a tree holding `entries` (`/`-separated paths and their contents), returning its hash.
fn build_tree(entries: &[(&str, &str)]) -> String {
    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut subdirectories: Vec<(String, Vec<(&str, &str)>)> = Vec::new();

    for (path, content) in entries {
        match path.split_once('/') {
            None => {
                let hash = store(LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() }));
                tree.add_child(TreeItem::new(path.to_string(), hash, DirEntryType::Normal));
            }
            Some((directory, rest)) => match subdirectories.iter_mut().find(|(name, _)| name == directory) {
                Some((_, children)) => children.push((rest, content)),
                None => subdirectories.push((directory.to_string(), vec![(rest, content)])),
            },
        }
    }

    for (name, children) in subdirectories {
        tree.add_child(TreeItem::new(name, build_tree(&children), DirEntryType::Tree));
    }

    store(LooseObjectBuilder::build_tree(&tree))
}

/// The names `dir` lists, exactly as spelled on disk.
fn listed(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir).unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != ".forklift")
        .collect()
}

/// A working tree holding `on_disk` (paths and contents), or `None` when the temporary directory
/// does not fold case and `folding` asks that it does, or folds it and `folding` asks that it
/// does not.
fn prepared(name: &str, on_disk: &[(&str, &str)], folding: bool) -> Option<PathBuf> {
    let root = working_tree(name);
    if folding && cfg!(any(target_os = "macos", windows)) {
        assert!(folds_case(&root), "the temporary directory is expected to fold case on this OS");
    }
    if folds_case(&root) != folding {
        std::fs::remove_dir_all(&root).ok();
        return None;
    }

    for (path, content) in on_disk {
        let path = root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
    }

    Some(root)
}

/// Materialize `target` over `source` in `root`, returning every path the plan writes.
fn shift(root: &Path, source: &[(&str, &str)], target: &[(&str, &str)]) -> Vec<String> {
    in_working_tree(root, || {
        let (source, target) = (build_tree(source), build_tree(target));
        let (ops, removed_dirs) = shift_utils::diff_trees(Some(&source), &target).unwrap();
        shift_utils::apply_ops(&ops, &removed_dirs).unwrap();

        ops.iter()
            .filter_map(|op| match op {
                FileOp::Write { path, .. } => Some(path.clone()),
                _ => None,
            })
            .collect()
    })
}

/// Materialize `target` over `source` where the disk holds one file for the pair, spelled
/// `on_disk` and holding `kept\n`, and return the working tree.
fn materialize(name: &str, source: &[(&str, &str)], target: &[(&str, &str)], on_disk: &str) -> Option<PathBuf> {
    let root = prepared(name, &[(on_disk, "kept\n")], true)?;
    shift(&root, source, target);
    Some(root)
}

/// `README` and `readme` both in the source, the disk one file spelled `readme`, and the target
/// keeps `README` unchanged, so it has no operation of its own. Removing `readme` unlinks it. The
/// removed spelling's content differs, so only the kept file's own content passes.
#[test]
fn a_removal_through_an_alias_writes_the_kept_file_back() {
    let Some(root) = materialize("file",
                                 &[("README", "kept\n"), ("readme", "other\n")],
                                 &[("README", "kept\n")],
                                 "readme") else { return };

    assert_eq!(listed(&root), BTreeSet::from(["README".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("README")).unwrap(), "kept\n");
    std::fs::remove_dir_all(&root).ok();
}

/// The pair one level down: `Docs/a` beside `docs/a`, the disk holding `docs/a`, the target
/// keeping `Docs`.
#[test]
fn a_removal_through_an_aliased_directory_writes_the_kept_file_back() {
    let Some(root) = materialize("directory",
                                 &[("Docs/a", "kept\n"), ("docs/a", "other\n")],
                                 &[("Docs/a", "kept\n")],
                                 "docs/a") else { return };

    assert_eq!(listed(&root), BTreeSet::from(["Docs".to_string()]));
    assert_eq!(listed(&root.join("Docs")), BTreeSet::from(["a".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("Docs/a")).unwrap(), "kept\n");
    std::fs::remove_dir_all(&root).ok();
}

/// The pair two levels down, the directories differing at the top.
#[test]
fn a_removal_through_an_aliased_ancestor_writes_the_kept_file_back() {
    let Some(root) = materialize("nested",
                                 &[("Docs/sub/a", "kept\n"), ("docs/sub/a", "other\n")],
                                 &[("Docs/sub/a", "kept\n")],
                                 "docs/sub/a") else { return };

    assert_eq!(listed(&root), BTreeSet::from(["Docs".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("Docs/sub/a")).unwrap(), "kept\n");
    std::fs::remove_dir_all(&root).ok();
}

/// A kept file the target changes already has its own write: it is written once, with the
/// target's content.
#[test]
fn a_kept_file_with_its_own_write_is_written_once() {
    let Some(root) = prepared("changed", &[("readme", "kept\n")], true) else { return };

    let writes = shift(&root, &[("README", "kept\n"), ("readme", "other\n")], &[("README", "new\n")]);

    assert_eq!(writes, vec!["README".to_string()]);
    assert_eq!(listed(&root), BTreeSet::from(["README".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("README")).unwrap(), "new\n");
    std::fs::remove_dir_all(&root).ok();
}

/// Where the volume tells the spellings apart, removing one never reaches the other: nothing is
/// written back.
#[test]
fn a_volume_that_tells_the_spellings_apart_writes_nothing_back() {
    let Some(root) = prepared("apart", &[("README", "kept\n"), ("readme", "other\n")], false) else { return };

    let writes = shift(&root, &[("README", "kept\n"), ("readme", "other\n")], &[("README", "kept\n")]);

    assert_eq!(writes, Vec::<String>::new());
    assert_eq!(listed(&root), BTreeSet::from(["README".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("README")).unwrap(), "kept\n");
    std::fs::remove_dir_all(&root).ok();
}

/// Two hard links, each listed under its own name, are one inode but two entries: removing one
/// leaves the other, so nothing is written back.
#[cfg(unix)]
#[test]
fn hard_links_under_both_spellings_write_nothing_back() {
    let Some(root) = prepared("links", &[("README", "kept\n")], false) else { return };
    std::fs::hard_link(root.join("README"), root.join("readme")).unwrap();

    let writes = shift(&root, &[("README", "kept\n"), ("readme", "kept\n")], &[("README", "kept\n")]);

    assert_eq!(writes, Vec::<String>::new());
    assert_eq!(listed(&root), BTreeSet::from(["README".to_string()]));
    std::fs::remove_dir_all(&root).ok();
}

/// The entry the merges call: a deletion of `readme` where the tree left in place still holds
/// both spellings (it lists the deleted path too) keeps `README`, with its own entry, in scope
/// only.
#[test]
fn a_merge_deletion_keeps_the_aliased_file_in_scope_only() {
    let Some(root) = prepared("merge", &[("readme", "kept\n")], true) else { return };

    in_working_tree(&root, || {
        let ours = build_tree(&[("README", "kept\n"), ("readme", "other\n")]);
        let ours = forklift_core::util::object_utils::load_tree(&ours).unwrap();
        let removed = vec!["readme".to_string()];
        let planned = HashSet::from(["readme"]);

        let kept = shift_utils::kept_through_aliases(&ours, &removed, &planned, &MaterializationScope::full()).unwrap();
        let readme = ours.get_files().find(|(name, _)| *name == "README").unwrap().1.hash.clone();
        assert_eq!(kept.iter().map(|(path, item)| (path.as_str(), item.hash.as_str())).collect::<Vec<_>>(),
                   vec![("README", readme.as_str())]);

        let scoped = MaterializationScope::from_prefixes(["src"]);
        assert!(shift_utils::kept_through_aliases(&ours, &removed, &planned, &scoped).unwrap().is_empty());
    });

    std::fs::remove_dir_all(&root).ok();
}
