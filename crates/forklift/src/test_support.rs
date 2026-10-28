//! Shared fixtures for the binary crate's own unit tests: the in-process tests that reach the
//! `#[cfg(test)]` fault-injection seams in the commands.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::{StorageRootScope, FOLDER_NAME_FORKLIFT_ROOT};
use forklift_core::model::blob::Blob;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::tree_item::TreeItem;

/// The process working directory is shared by every test in this binary, and the working-tree
/// writers resolve paths against it: a test that materializes files holds this for its duration.
static WORKING_DIRECTORY: Mutex<()> = Mutex::new(());

/// A scratch warehouse entered on this thread (and, when it owns the working directory, as the
/// process working directory too). Removed on drop.
pub(crate) struct Warehouse {
    pub(crate) root: PathBuf,
    _scope: StorageRootScope,
    _cwd: Option<MutexGuard<'static, ()>>,
}

impl Warehouse {
    /// A warehouse for a test that only touches storage under `.forklift`.
    pub(crate) fn storage_only(name: &str) -> Warehouse {
        Warehouse::create(name, None)
    }

    /// A warehouse for a test that writes the working tree: it owns the process working
    /// directory until dropped.
    pub(crate) fn with_working_tree(name: &str) -> Warehouse {
        let guard = WORKING_DIRECTORY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        Warehouse::create(name, Some(guard))
    }

    fn create(name: &str, cwd: Option<MutexGuard<'static, ()>>) -> Warehouse {
        let root = std::env::temp_dir().join(format!("forklift-bin-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        // A bare full warehouse: its marker is what every ref write reads the mode from.
        let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        std::fs::write(forklift.join(forklift_core::util::warehouse_utils::FILE_NAME_WAREHOUSE_MARKER),
            b"/forklift-warehouse 2\nfull\n").unwrap();

        if cwd.is_some() {
            std::env::set_current_dir(&root).unwrap();
        }

        let scope = StorageRootScope::enter(&root);

        Warehouse { root, _scope: scope, _cwd: cwd }
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        if self._cwd.is_some() {
            let _ = std::env::set_current_dir(std::env::temp_dir());
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// Store a tree holding one file, `name`, with `content`; returns the tree's hash.
pub(crate) fn store_tree(name: &str, content: &[u8]) -> String {
    store_files(&[(name, content)])
}

/// Store a flat tree holding `files` (name, content); returns the tree's hash.
pub(crate) fn store_files(files: &[(&str, &[u8])]) -> String {
    let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

    for (name, content) in files {
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() });
        blob.store().unwrap();
        root.add_child(TreeItem::new(name.to_string(), blob.hash.clone(), DirEntryType::Normal));
    }

    let mut tree = LooseObjectBuilder::build_tree(&root);
    tree.store().unwrap();

    tree.hash
}

/// Configure an operator in the warehouse's own configuration, so a stack never mints one into
/// the global configuration of whoever runs the tests.
pub(crate) fn configure_operator() {
    use forklift_core::enums::config_scope::ConfigScope;
    use forklift_core::util::config_utils;

    config_utils::set_value(config_utils::KEY_OPERATOR_IDENTIFIER, "test-operator", ConfigScope::Warehouse).unwrap();
}

/// Store a parcel of `tree` on `parents`; returns its hash.
pub(crate) fn store_parcel(tree: &str, parents: &[&str]) -> String {
    let parcel = Parcel {
        tree_hash: tree.to_string(),
        parents: parents.iter().map(|parent| parent.to_string()).collect(),
        actions: Vec::new(),
        description: Some("test parcel".to_string()),
    };
    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store().unwrap();

    object.hash
}

/// Point pallet `name` at `head` by writing its ref file directly, past the closure check every real
/// ref write runs: for a fixture that models a store already missing history `head` reaches.
pub(crate) fn write_ref_unchecked(name: &str, head: &str) {
    let path = forklift_core::util::pallet_utils::get_pallets_folder().join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, format!("{}\n", head)).unwrap();
}

/// Check `tree` out into the empty working directory and staging, as a fixture: under an operation
/// marker of its own, since every wholesale rewrite of staging needs one.
pub(crate) fn check_out(tree: &str) {
    use forklift_core::util::operation_utils::{self, Operation, OperationKind};
    use forklift_core::util::{file_utils, pallet_utils::BayPosition, progress_utils::Progress, shift_utils};

    let position = BayPosition::new("main", None);
    operation_utils::raise(&Operation::new(OperationKind::Shift, position.clone(), None, position)).unwrap();
    shift_utils::materialize_tree(None, tree, "Test", file_utils::take_stat_anchor().unwrap(), &Progress::NONE).unwrap();
    operation_utils::finish().unwrap();
}

/// Run an async command to completion on this thread, so the thread's storage scope applies.
pub(crate) fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(future)
}

/// Whether `path` (relative to the warehouse) holds exactly `content`.
pub(crate) fn file_holds(root: &Path, path: &str, content: &[u8]) -> bool {
    std::fs::read(root.join(path)).map(|bytes| bytes == content).unwrap_or(false)
}
