//! A materialization from a tree holding two spellings of one name, on a volume that folds them,
//! never loses the spelling the target keeps: removing the other spelling unlinks the one file
//! both name, so the kept spelling is written back from the target.
//!
//! These drive `shift_utils::diff_trees` and `apply_ops` directly, from a working directory
//! holding one file for the pair, a state no command on a folding volume would build. They live in
//! their own test binary because materialization resolves paths against the process's current
//! directory, which `in_working_tree` below changes and serializes.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::StorageRootScope;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::shift_utils;

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

/// Materialize `target` over `source` where the disk holds one file for the pair, spelled
/// `on_disk`, and return the working tree.
fn materialize(name: &str, source: &[(&str, &str)], target: &[(&str, &str)], on_disk: &str) -> Option<PathBuf> {
    let root = working_tree(name);
    if cfg!(any(target_os = "macos", windows)) {
        assert!(folds_case(&root), "the temporary directory is expected to fold case on this OS");
    }
    if !folds_case(&root) {
        std::fs::remove_dir_all(&root).ok();
        return None;
    }

    let path = root.join(on_disk);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, "kept\n").unwrap();

    in_working_tree(&root, || {
        let (source, target) = (build_tree(source), build_tree(target));
        let (ops, removed_dirs) = shift_utils::diff_trees(Some(&source), &target).unwrap();
        shift_utils::apply_ops(&ops, &removed_dirs).unwrap();
    });

    Some(root)
}

/// `README` and `readme` hold one content, the disk one file spelled `readme`, and the target keeps
/// `README` unchanged, so it has no operation of its own. Removing `readme` unlinks it.
#[test]
fn a_removal_through_an_alias_writes_the_kept_file_back() {
    let Some(root) = materialize("file",
                                 &[("README", "kept\n"), ("readme", "kept\n")],
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
                                 &[("Docs/a", "kept\n"), ("docs/a", "kept\n")],
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
                                 &[("Docs/sub/a", "kept\n"), ("docs/sub/a", "kept\n")],
                                 &[("Docs/sub/a", "kept\n")],
                                 "docs/sub/a") else { return };

    assert_eq!(listed(&root), BTreeSet::from(["Docs".to_string()]));
    assert_eq!(std::fs::read_to_string(root.join("Docs/sub/a")).unwrap(), "kept\n");
    std::fs::remove_dir_all(&root).ok();
}
