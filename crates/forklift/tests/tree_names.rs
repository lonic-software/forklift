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
        Warehouse::new_after(name, |_| {})
    }

    /// [`Self::new`], running `before_prepare` on the empty working directory first.
    fn new_after(name: &str, before_prepare: impl FnOnce(&std::path::Path)) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-tree-names-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        before_prepare(&root);

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "spike@forklift"]);

        std::fs::write(warehouse.root.join("app.txt"), "payload\n").unwrap();
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "-m", "first"]);

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

        // Straight to the ref file: the check every real ref write runs would refuse a tree it
        // cannot walk, and these trees are crafted to be unwalkable.
        std::fs::write(pallet_utils::get_pallets_folder().join(name), format!("{}\n", parcel_hash)).unwrap();
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
    assert!(!output.status.success(), "shift accepted the tree with {}; stderr: {}", quoted, stderr);
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

#[test]
fn a_root_entry_some_file_system_opens_as_the_metadata_folder_is_refused() {
    // Each is an ordinary name here, but opens `.forklift` somewhere: a data stream of it and an
    // 8.3 short name on NTFS (listed or hashed), a name holding a code point HFS+ ignores.
    for (index, name) in [".forklift::$INDEX_ALLOCATION", ".forklift:x", ".fork\u{200C}lift", "\u{FEFF}.forklift",
        "FORKLI~1", "forkli~4.", ".ForkLift\u{206F} ", "FOCBB9~1", "focbb9~7", "Focbb~10", "foc~1234"].into_iter().enumerate()
    {
        let warehouse = Warehouse::new(&format!("metadata-alias-{}", index));
        warehouse.craft_pallet(|blob| vec![subtree(name, blob)]);

        assert_refused(&warehouse, &warehouse.root.join(".forklift").join("escaped.txt"), &format!("{:?}", name));
    }
}

#[test]
fn a_root_entry_that_only_resembles_a_metadata_alias_still_shifts() {
    // The positive twin: names one step from an alias are ordinary on every file system.
    for (index, name) in ["forkli~5", "forkli~0", "forkli~12", "FORKLIF~1", "forklift", ".forklifts",
        ".fork-lift", "x:.forklift", "focbb8~1", "focbb9~0", "focbb9~123", "~1", "f~1", "foc~1",
        "focbb~1"].into_iter().enumerate()
    {
        #[cfg(windows)]
        if name.contains(':') {
            continue;
        }

        let warehouse = Warehouse::new(&format!("metadata-near-alias-{}", index));
        warehouse.craft_pallet(|blob| vec![subtree(name, blob)]);

        warehouse.run_ok(&["shift", "evil"]);
        assert!(warehouse.root.join(name).join("escaped.txt").exists(), "{:?} was not written", name);
    }
}

/// The 8.3 short name NTFS gave `path`'s last component, if it gave one.
#[cfg(windows)]
fn short_name(path: &std::path::Path) -> Option<String> {
    use std::os::windows::ffi::{OsStrExt, OsStringExt};
    use windows_sys::Win32::Storage::FileSystem::GetShortPathNameW;

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut buffer = vec![0u16; 1024];
    // SAFETY: `wide` is NUL-terminated, and `buffer` is writable for the length passed.
    let length = unsafe { GetShortPathNameW(wide.as_ptr(), buffer.as_mut_ptr(), buffer.len() as u32) } as usize;
    assert!(length > 0 && length < buffer.len(), "GetShortPathNameW failed for {}", path.display());

    let short = std::path::PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length]));
    let name = short.file_name()?.to_string_lossy().into_owned();
    (name != ".forklift").then_some(name)
}

#[cfg(windows)]
#[test]
fn a_hashed_short_name_of_the_metadata_folder_is_refused() {
    // Four siblings take `FORKLI~1` to `~4` before `.forklift` exists, so NTFS gives it a hashed
    // short name. The name rule lists the hash git-style; this pins it to what NTFS really gives.
    let warehouse = Warehouse::new_after("metadata-short-name", |root| {
        for index in 1..=4 {
            std::fs::create_dir(root.join(format!("forklift-sibling-{}", index))).unwrap();
        }
    });
    let Some(short) = short_name(&warehouse.root.join(".forklift")) else {
        assert!(std::env::var_os("CI").is_none(), "this volume gives no 8.3 short names, so the test proves nothing");
        return;
    };
    assert!(!short.to_lowercase().starts_with("forkli~"), "{:?} is not a hashed short name", short);
    assert!(forklift_core::util::office_utils::is_metadata_folder_name(&short),
        "NTFS hashed `.forklift` to {:?}, which the name rule does not list", short);

    let loaded = warehouse.run(&["load", &format!("{}/config", short)]);
    assert!(!loaded.status.success(), "the alias {:?} was loaded", short);

    warehouse.craft_pallet(|blob| vec![subtree(&short, blob)]);
    assert_refused(&warehouse, &warehouse.root.join(".forklift").join("escaped.txt"), &format!("{:?}", short));
}

#[test]
fn a_bay_refuses_entries_that_name_its_own_metadata_folder() {
    // A bay's `.forklift` is a folder holding its redirect and its volume's probes: a parcel naming
    // it, in any case, is refused there and writes nothing into it, and nothing inside it loads.
    let warehouse = Warehouse::new("bay-metadata");
    let bay = warehouse.base.join("bay");
    warehouse.run_ok(&["bay", "add", "linked", bay.to_str().unwrap()]);
    let metadata = bay.join(".forklift");
    let run = |args: &[&str]| forklift_command()
        .args(args)
        .current_dir(&bay)
        .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", warehouse.home.join("keys"))
        .output()
        .unwrap();

    for name in [".forklift", ".ForkLift"] {
        warehouse.craft_pallet(|blob| vec![subtree(name, blob)]);
        let shifted = run(&["shift", "evil"]);
        let stderr = String::from_utf8_lossy(&shifted.stderr);
        assert!(!metadata.join("escaped.txt").exists(), "the parcel wrote into the bay's metadata folder");
        assert!(!shifted.status.success(), "shift accepted {:?}; stderr: {}", name, stderr);
        assert!(stderr.contains(&format!("{:?}", name)), "the refusal must name {:?}; stderr: {}", name, stderr);
    }

    let loaded = run(&["load", ".forklift/redirect"]);
    assert!(!loaded.status.success(), "the bay's redirect was loaded");
    assert!(metadata.join("redirect").is_file(), "the bay's redirect is untouched");
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

    // The two spellings land on one entry, so the up-front alias check refuses the pair by name.
    assert_refused(&warehouse, &outside.join("escaped.txt"), "LIB and lib");
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
    // A chunked file is assembled into a temp file beside it and renamed into place. A tree that
    // plants a symlink under a temp-like name, sorted before the chunked entry, must not get the
    // assembly written into the symlink's target.
    let warehouse = Warehouse::new("assemble-temp");
    let first = {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::get_pallet_head("main").unwrap().unwrap()
    };

    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(warehouse.root.join("big.bin"), &big).unwrap();
    warehouse.run_ok(&["load", "big.bin"]);
    warehouse.run_ok(&["stack", "-m", "big"]);

    let outside = warehouse.base.join("outside.txt");
    std::fs::write(&outside, "untouched\n").unwrap();
    let target = outside.to_str().unwrap().to_string();
    warehouse.craft_pallet(|_| vec![symlink(".big.bin.forklift-assemble.tmp", &target)]);

    {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::update_pallet_head(
            "before",
            pallet_utils::get_pallet_head("before").unwrap().as_deref(),
            &first,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
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

    let output = warehouse.run(&["stack", "-m", "alias"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(!output.status.success(), "stack accepted the alias; stderr: {}", stderr);
    assert!(stderr.contains("\".forklift.\""), "the refusal must name the entry; stderr: {}", stderr);
}

/// A warehouse whose `main` holds a chunked `big` beside `app.txt`, shifted to a pallet `before`
/// that lacks it, so the next `shift main` assembles it. Returns the chunked file's content.
fn with_chunked_file(warehouse: &Warehouse, big: &str) -> Vec<u8> {
    let first = {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::get_pallet_head("main").unwrap().unwrap()
    };

    let content: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(warehouse.root.join(big), &content).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "big"]);

    {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::update_pallet_head(
            "before",
            pallet_utils::get_pallet_head("before").unwrap().as_deref(),
            &first,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
    }
    warehouse.run_ok(&["shift", "before"]);
    content
}

#[test]
fn a_leftover_from_an_interrupted_assembly_is_cleared_and_never_reaches_the_working_directory() {
    // An interrupted assembly leaves its temp file behind. It lives in the metadata folder, so
    // it is never stacked, and a later run clears it instead of letting leftovers pile up.
    let warehouse = Warehouse::new("assemble-leftover");
    let big = with_chunked_file(&warehouse, "big.bin");
    let assembly = warehouse.root.join(".forklift").join("assembly");
    std::fs::create_dir_all(&assembly).unwrap();
    std::fs::write(assembly.join("1-0.tmp"), "half an assembly").unwrap();

    warehouse.run_ok(&["shift", "main"]);

    assert!(std::fs::read(warehouse.root.join("big.bin")).unwrap() == big);
    assert!(!assembly.join("1-0.tmp").exists(), "the leftover was not cleared");
    let strays: Vec<_> = std::fs::read_dir(&warehouse.root).unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("assemble"))
        .collect();
    assert!(strays.is_empty(), "assembly left files in the working directory: {:?}", strays);
}

#[test]
fn assembly_leftovers_are_cleared_by_any_command_that_takes_the_lock() {
    // A warehouse that never checks out another chunked file must not keep leftovers forever.
    // A read-only command does not hold the lock, so it may not clear them: an assembly could
    // be in flight.
    let warehouse = Warehouse::new("assemble-sweep");
    let assembly = warehouse.root.join(".forklift").join("assembly");
    let plant = || {
        std::fs::create_dir_all(&assembly).unwrap();
        std::fs::write(assembly.join("1-0.tmp"), "half an assembly").unwrap();
        std::fs::write(assembly.join("2-7.tmp"), "another").unwrap();
    };
    let left = || -> Vec<String> {
        std::fs::read_dir(&assembly).unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    };

    plant();
    warehouse.run_ok(&["stocktake"]);
    assert!(assembly.join("1-0.tmp").exists(), "a command without the lock cleared a leftover");

    for command in [&["load", "."][..], &["heal"][..]] {
        plant();
        warehouse.run_ok(command);
        assert!(left().is_empty(), "`{}` left assembly leftovers: {:?}", command.join(" "), left());
    }
}

#[test]
fn a_bay_clears_its_own_assembly_leftovers_and_no_other() {
    // The lock and the assembly folder are both the bay's own: taking a bay's lock clears that
    // bay's folder, and never the one a process holding another bay's lock may be assembling in.
    let warehouse = Warehouse::new("assemble-sweep-bay");
    let bay = warehouse.base.join("side");
    let main_assembly = warehouse.root.join(".forklift").join("assembly");
    let bay_assembly = warehouse.root.join(".forklift").join("bays").join("side").join("assembly");

    warehouse.run_ok(&["bay", "add", "side", bay.to_str().unwrap()]);

    std::fs::create_dir_all(&main_assembly).unwrap();
    std::fs::create_dir_all(&bay_assembly).unwrap();
    std::fs::write(main_assembly.join("1-0.tmp"), "the main tree's").unwrap();
    std::fs::write(bay_assembly.join("2-0.tmp"), "the bay's").unwrap();

    let output = forklift_command()
        .args(["load", "."])
        .current_dir(&bay)
        .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", warehouse.home.join("keys"))
        .output()
        .unwrap();
    assert!(output.status.success(), "load in the bay failed: {}", String::from_utf8_lossy(&output.stderr));

    assert!(!bay_assembly.join("2-0.tmp").exists(), "the bay's lock left the bay's leftover");
    assert!(main_assembly.join("1-0.tmp").exists(), "the bay's lock cleared the main tree's folder");
}

#[test]
fn a_chunked_file_with_a_name_near_the_length_limit_checks_out() {
    // The temp name must not grow with the file's name, or a name the file system took when the
    // file was stacked could never be checked out again.
    let warehouse = Warehouse::new("assemble-long-name");
    let name = format!("{}.bin", "n".repeat(250));
    let big = with_chunked_file(&warehouse, &name);

    warehouse.run_ok(&["shift", "main"]);

    assert!(std::fs::read(warehouse.root.join(&name)).unwrap() == big);
}

#[test]
fn a_tracked_file_named_like_an_assembly_temp_file_survives_checkout() {
    // The temp name sits in the user's namespace, so a tree may track a file that happens to
    // carry it. Assembling the chunked sibling must neither delete nor overwrite that file.
    let warehouse = Warehouse::new("assemble-namesake");
    let first = {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::get_pallet_head("main").unwrap().unwrap()
    };

    let big: Vec<u8> = (0..9 * 1024 * 1024u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 24) as u8).collect();
    std::fs::write(warehouse.root.join("big.bin"), &big).unwrap();
    std::fs::write(warehouse.root.join(".big.bin.forklift-assemble.tmp"), "tracked\n").unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "-m", "big"]);

    {
        let _scope = StorageRootScope::enter(&warehouse.root);
        pallet_utils::update_pallet_head(
            "before",
            pallet_utils::get_pallet_head("before").unwrap().as_deref(),
            &first,
            &pallet_utils::RefPreconditions::NONE,
        ).unwrap();
    }
    warehouse.run_ok(&["shift", "before"]);
    warehouse.run_ok(&["shift", "main"]);

    assert!(std::fs::read(warehouse.root.join("big.bin")).unwrap() == big);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join(".big.bin.forklift-assemble.tmp")).unwrap(),
        "tracked\n",
        "the assembly clobbered a tracked file"
    );
}
