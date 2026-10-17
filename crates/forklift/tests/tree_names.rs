//! A tree's entry names arrive from whoever wrote the tree, a remote included, and checkout joins
//! them into filesystem paths. These tests craft trees the builder would never produce from a
//! working directory and check that `shift` refuses them before writing anything, so no name can
//! reach outside the working directory.

use std::path::PathBuf;
use std::process::Output;

use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::globals::StorageRootScope;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::{object_utils, pallet_utils};

mod common;
use common::forklift_command;

struct Warehouse {
    base: PathBuf,
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-tree-names-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.name", "spike@forklift"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        std::fs::write(warehouse.root.join("app.txt"), "payload\n").unwrap();
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

    /// Point a new pallet `evil` at a parcel on top of `main` whose root tree is `main`'s root
    /// tree plus the entries `extra` returns, built from the blob hash of `app.txt`.
    fn craft_pallet(&self, extra: impl FnOnce(&str) -> Vec<TreeItem>) {
        self.craft_pallet_named("evil", "main", extra);
    }

    /// [`Self::craft_pallet`], naming the new pallet and the pallet it builds on.
    fn craft_pallet_named(&self, name: &str, base: &str, extra: impl FnOnce(&str) -> Vec<TreeItem>) {
        let _scope = StorageRootScope::enter(&self.root);

        let head = pallet_utils::get_pallet_head(base).unwrap().unwrap();
        let parcel = object_utils::load_parcel(&head).unwrap();
        let mut tree = object_utils::load_tree(&parcel.tree_hash).unwrap();
        let blob = tree.get_files()
            .find(|(name, _)| name.as_str() == "app.txt")
            .map(|(_, item)| item.hash.clone())
            .unwrap();

        for item in extra(&blob) {
            tree.add_child(item);
        }
        let tree_hash = store(LooseObjectBuilder::build_tree(&tree));

        let crafted = Parcel {
            tree_hash,
            parents: vec![head],
            actions: Vec::new(),
            description: Some("crafted".to_string()),
        };
        let parcel_hash = store(LooseObjectBuilder::build_parcel(&crafted));

        pallet_utils::set_pallet_head(name, &parcel_hash).unwrap();
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

fn file(name: &str, blob: &str) -> TreeItem {
    TreeItem::new(name.to_string(), blob.to_string(), DirEntryType::Normal)
}

/// A symlink entry named `name` pointing at `target`.
fn symlink(name: &str, target: &str) -> TreeItem {
    let hash = store(LooseObjectBuilder::build_blob(&Blob { content: target.as_bytes().to_vec() }));

    TreeItem::new(name.to_string(), hash, DirEntryType::SymbolicLink)
}

/// A subtree named `name` holding one file, `escaped.txt`.
fn subtree(name: &str, blob: &str) -> TreeItem {
    let mut child = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    child.add_child(file("escaped.txt", blob));
    let hash = store(LooseObjectBuilder::build_tree(&child));

    TreeItem::new(name.to_string(), hash, DirEntryType::Tree)
}

/// `shift evil` must fail, write nothing at `escape`, and name the offending entry.
fn assert_refused(warehouse: &Warehouse, escape: &PathBuf, quoted: &str) {
    let output = warehouse.run(&["shift", "evil"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!escape.exists(), "the tree wrote outside the working directory: {}", escape.display());
    assert!(!output.status.success(), "shift accepted the tree; stderr: {}", stderr);
    assert!(stderr.contains(quoted), "the refusal must name the entry {}; stderr: {}", quoted, stderr);
}

#[test]
fn a_dot_dot_subtree_is_refused_and_writes_nothing_outside() {
    let warehouse = Warehouse::new("dotdot");
    warehouse.craft_pallet(|blob| vec![subtree("..", blob)]);

    assert_refused(&warehouse, &warehouse.base.join("escaped.txt"), "\"..\"");
}

#[test]
fn a_name_with_a_separator_is_refused_and_writes_nothing_outside() {
    let warehouse = Warehouse::new("separator");
    warehouse.craft_pallet(|blob| vec![file("../escaped.txt", blob)]);

    assert_refused(&warehouse, &warehouse.base.join("escaped.txt"), "\"../escaped.txt\"");
}

#[test]
fn an_absolute_name_is_refused_and_writes_nothing_outside() {
    let warehouse = Warehouse::new("absolute");
    let target = warehouse.base.join("absolute.txt");
    let name = target.to_str().unwrap().to_string();
    warehouse.craft_pallet(|blob| vec![file(&name, blob)]);

    assert_refused(&warehouse, &target, &format!("{:?}", name));
}

#[test]
fn an_ordinary_crafted_name_still_shifts() {
    // The positive twin: the same crafting path with a valid name is accepted and written, so the
    // refusals above come from the name and not from the crafted parcel.
    let warehouse = Warehouse::new("ordinary");
    warehouse.craft_pallet(|blob| vec![subtree("nested", blob)]);

    warehouse.run_ok(&["shift", "evil"]);

    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("nested").join("escaped.txt")).unwrap(),
        "payload\n"
    );
}

#[test]
fn a_root_forklift_entry_is_refused_and_leaves_the_metadata_alone() {
    let warehouse = Warehouse::new("metadata");
    warehouse.craft_pallet(|blob| vec![subtree(".forklift", blob)]);

    assert_refused(&warehouse, &warehouse.root.join(".forklift").join("escaped.txt"), "\".forklift\"");
}

#[test]
fn a_root_forklift_entry_in_another_case_is_refused() {
    let warehouse = Warehouse::new("metadata-case");
    warehouse.craft_pallet(|blob| vec![subtree(".ForkLift", blob)]);

    assert_refused(&warehouse, &warehouse.root.join(".forklift").join("escaped.txt"), "\".ForkLift\"");
}

/// Whether the warehouse's file system treats names differently by case.
fn is_case_insensitive(warehouse: &Warehouse) -> bool {
    warehouse.root.join("APP.TXT").exists()
}

#[cfg(unix)]
#[test]
fn a_subtree_cased_like_a_symlink_does_not_write_through_it() {
    let warehouse = Warehouse::new("symlink-case");
    if !is_case_insensitive(&warehouse) {
        return;
    }

    let outside = warehouse.base.join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    let target = outside.to_str().unwrap().to_string();

    warehouse.craft_pallet(|blob| vec![symlink("lib", &target), subtree("LIB", blob)]);

    assert_refused(&warehouse, &outside.join("escaped.txt"), "\"LIB");
}

#[cfg(unix)]
#[test]
fn a_symlink_replaced_by_a_file_is_not_written_through() {
    // Legitimate history shape: `link` is a symlink in one parcel and a regular file in the
    // next. Shifting across that change must replace the link, not write into its target.
    let warehouse = Warehouse::new("symlink-to-file");
    let outside = warehouse.base.join("outside.txt");
    std::fs::write(&outside, "untouched\n").unwrap();
    let target = outside.to_str().unwrap().to_string();

    warehouse.craft_pallet(|_| vec![symlink("link", &target)]);
    warehouse.run_ok(&["shift", "evil"]);
    assert!(std::fs::symlink_metadata(warehouse.root.join("link")).unwrap().file_type().is_symlink());

    warehouse.run_ok(&["shift", "main"]);
    warehouse.run_ok(&["shift", "evil"]);
    warehouse.craft_pallet_named("next", "evil", |blob| vec![file("link", blob)]);
    warehouse.run_ok(&["shift", "next"]);

    assert_eq!(std::fs::read_to_string(&outside).unwrap(), "untouched\n", "the shift wrote through the old symlink");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("link")).unwrap(), "payload\n");
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_chunked_assembly_temp_path_is_not_written_through() {
    // A chunked file is assembled into `.<name>.forklift-assemble.tmp` and renamed into place. A
    // tree that plants a symlink under that name, sorted before the chunked entry, must not get
    // the assembly written into the symlink's target.
    let warehouse = Warehouse::new("assemble-temp");
    let first = {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::get_pallet_head("main").unwrap().unwrap()
    };

    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(warehouse.root.join("big.bin"), &big).unwrap();
    warehouse.run_ok(&["load", "big.bin"]);
    warehouse.run_ok(&["stack", "big"]);

    let outside = warehouse.base.join("outside.txt");
    std::fs::write(&outside, "untouched\n").unwrap();
    let target = outside.to_str().unwrap().to_string();
    warehouse.craft_pallet(|_| vec![symlink(".big.bin.forklift-assemble.tmp", &target)]);

    {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::set_pallet_head("before", &first).unwrap();
    }
    warehouse.run_ok(&["shift", "before"]);
    assert!(!warehouse.root.join("big.bin").exists());

    let _ = warehouse.run(&["shift", "evil"]);

    assert!(std::fs::read(&outside).unwrap() == b"untouched\n", "the assembly wrote through the symlink");
}

#[cfg(unix)]
#[test]
fn stacking_a_top_level_alias_of_the_metadata_folder_is_refused() {
    // `.forklift.` is an ordinary directory here, but Windows opens `.forklift` for it, so a
    // parcel carrying it could never be checked out there: refuse it when stacking, not later.
    let warehouse = Warehouse::new("stack-alias");
    std::fs::create_dir_all(warehouse.root.join(".forklift.")).unwrap();
    std::fs::write(warehouse.root.join(".forklift.").join("notes.txt"), "notes\n").unwrap();
    warehouse.run_ok(&["load", "."]);

    let output = warehouse.run(&["stack", "alias"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "stack accepted the alias; stderr: {}", stderr);
    assert!(stderr.contains("\".forklift.\""), "the refusal must name the entry; stderr: {}", stderr);
}
