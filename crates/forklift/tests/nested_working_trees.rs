//! A `.forklift` below the top level of a working directory is the root of another working tree:
//! warehouse discovery from inside it stops there. These tests check that no tree can plant one
//! (stack, git import and every materializer refuse the name at any depth, before anything is
//! written), that the walks leave a nested working tree to itself, and that neither command that
//! deletes a directory destroys one, whatever the ignore rules say.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
    /// A prepared warehouse in `<base>/warehouse` with nothing loaded.
    fn empty(name: &str) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-nested-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "nested@forklift"]);

        warehouse
    }

    /// [`Self::empty`], with `app.txt` stacked on `main`.
    fn new(name: &str) -> Warehouse {
        let warehouse = Warehouse::empty(name);
        warehouse.write("app.txt", "payload\n");
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "first"]);

        warehouse
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write(&self, relative: &str, content: &str) {
        let path = self.path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap()
    }

    fn run_at(&self, dir: &Path, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_at(&self.root, args)
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "`{}` failed: {}", args.join(" "), stderr(&output));
        output
    }

    /// Prepare another warehouse at `relative`, inside this one's working directory.
    fn prepare_nested(&self, relative: &str) {
        let dir = self.path(relative);
        std::fs::create_dir_all(&dir).unwrap();
        let prepared = self.run_at(&dir, &["prepare"]);
        assert!(prepared.status.success(), "prepare in {}: {}", relative, stderr(&prepared));
        assert!(dir.join(".forklift").is_dir(), "no nested warehouse at {}", relative);
    }

    /// The `(kind, path)` changes stocktake reports in `section` ("staged" or "unstaged"). An empty
    /// section is left out of the JSON.
    fn changes(&self, section: &str) -> Vec<(String, String)> {
        let output = self.run_ok(&["--json", "stocktake"]);
        let json: serde_json::Value = serde_json::from_str(&stdout(&output)).unwrap();

        json["data"][section].as_array().cloned().unwrap_or_default().iter()
            .map(|change| (change["kind"].as_str().unwrap().to_string(), change["path"].as_str().unwrap().to_string()))
            .collect()
    }

    fn head(&self, pallet: &str) -> Option<String> {
        let _scope = StorageRootScope::enter(&self.root);
        pallet_utils::get_pallet_head(pallet).unwrap()
    }

    /// Point the pallet `name` at a parcel on top of `main` whose root tree is `main`'s, with
    /// `app.txt` changed to `changed\n` (an entry that sorts before every crafted one) and the
    /// entries `extra` returns added.
    fn craft_pallet(&self, name: &str, extra: impl FnOnce(&str) -> Vec<TreeItem>) {
        let _scope = StorageRootScope::enter(&self.root);

        let head = pallet_utils::get_pallet_head("main").unwrap().unwrap();
        let parcel = object_utils::load_parcel(&head).unwrap();
        let mut tree = object_utils::load_tree(&parcel.tree_hash).unwrap();
        let changed = store(LooseObjectBuilder::build_blob(&Blob { content: b"changed\n".to_vec() }));
        tree.add_child(file("app.txt", &changed));

        for item in extra(&changed) {
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

        pallet_utils::update_pallet_head(name, pallet_utils::get_pallet_head(name).unwrap().as_deref(), &parcel_hash, &pallet_utils::RefPreconditions::NONE)
            .unwrap();
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Store `object` in the scoped warehouse and return its hash.
fn store(mut object: LooseObject) -> String {
    object.store().unwrap();
    object.hash
}

fn file(name: &str, blob: &str) -> TreeItem {
    TreeItem::new(name.to_string(), blob.to_string(), DirEntryType::Normal)
}

/// A subtree named `name` holding `children`.
fn tree(name: &str, children: Vec<TreeItem>) -> TreeItem {
    let mut child = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    for item in children {
        child.add_child(item);
    }
    let hash = store(LooseObjectBuilder::build_tree(&child));

    TreeItem::new(name.to_string(), hash, DirEntryType::Tree)
}

fn git_in(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git must be installed to run this test");
    assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
}

/// `None` when `output` refused naming `needle` and the working directory is exactly as it was:
/// `app.txt` (which every crafted pallet changes, and which sorts first) still holds `payload\n`,
/// and `absent` was never created. Otherwise what went wrong, for a test that collects arms.
fn refused_and_untouched(warehouse: &Warehouse, output: &Output, needle: &str, absent: &str) -> Option<String> {
    let mut problems = Vec::new();

    if output.status.success() {
        problems.push("it succeeded".to_string());
    } else if !stderr(output).contains(needle) {
        problems.push(format!("the refusal does not name {}: {}", needle, stderr(output)));
    }
    if warehouse.read("app.txt") != "payload\n" {
        problems.push("the earlier-sorting app.txt was rewritten".to_string());
    }
    if std::fs::symlink_metadata(warehouse.path(absent)).is_ok() {
        problems.push(format!("{} was written", absent));
    }

    (!problems.is_empty()).then(|| problems.join("; "))
}

/// Shift, fast-forward consolidate and merge consolidate all refuse a tree naming a metadata
/// folder below the top level, in each spelling, before writing anything.
#[test]
fn every_materializer_refuses_a_metadata_folder_below_the_top_level_and_writes_nothing() {
    let mut failures = Vec::new();

    for (index, name) in [".forklift", ".ForkLift", "FORKLI~1"].into_iter().enumerate() {
        let warehouse = Warehouse::new(&format!("materialize-{}", index));
        warehouse.craft_pallet("evil", |blob| vec![tree("sub", vec![tree(name, vec![file("config", blob)])])]);
        let needle = format!("{:?}", name);

        let shift = warehouse.run(&["shift", "evil"]);
        if let Some(problem) = refused_and_untouched(&warehouse, &shift, &needle, "sub") {
            failures.push(format!("shift, {:?}: {}", name, problem));
        }

        // `main` is an ancestor of `evil`: a fast-forward.
        let forward = warehouse.run(&["consolidate", "evil"]);
        if let Some(problem) = refused_and_untouched(&warehouse, &forward, &needle, "sub") {
            failures.push(format!("fast-forward consolidate, {:?}: {}", name, problem));
        }
    }

    // `main` moves on, so the consolidate is a real merge.
    let warehouse = Warehouse::new("materialize-merge");
    warehouse.craft_pallet("evil", |blob| vec![tree("sub", vec![tree(".forklift", vec![file("config", blob)])])]);
    warehouse.write("zzz.txt", "ours\n");
    warehouse.run_ok(&["load", "zzz.txt"]);
    warehouse.run_ok(&["stack", "ours"]);
    let merge = warehouse.run(&["consolidate", "evil"]);
    if let Some(problem) = refused_and_untouched(&warehouse, &merge, "\".forklift\"", "sub") {
        failures.push(format!("merge consolidate: {}", problem));
    }

    assert!(failures.is_empty(), "{:#?}", failures);
}

/// The positive twin: the same crafting path with an ordinary nested name shifts, so the refusals
/// above come from the name.
#[test]
fn a_crafted_tree_with_an_ordinary_nested_name_shifts() {
    let warehouse = Warehouse::new("materialize-ordinary");
    warehouse.craft_pallet("evil", |blob| vec![tree("sub", vec![tree("forklift", vec![file("config", blob)])])]);

    warehouse.run_ok(&["shift", "evil"]);
    assert_eq!(warehouse.read("app.txt"), "changed\n");
    assert_eq!(warehouse.read("sub/forklift/config"), "changed\n");
}

/// A name some file system opens as a metadata folder, below the top level, is not stacked: as a
/// file, and as a directory deeper down. Neither spelling is a metadata folder on this volume, so
/// the walk stages both. The way out the refusal names, unloading it and ignoring it, then stacks.
#[test]
fn stack_refuses_a_metadata_folder_name_below_the_top_level() {
    let mut cases = vec![("file", "sub/FORKLI~1", "sub/FORKLI~1")];
    // Windows drops the trailing dot, so the name is the metadata folder itself there.
    if !cfg!(windows) {
        cases.push(("directory", "deep/er/.forklift./x.txt", "deep/er/.forklift."));
    }

    for (kind, written, refused) in cases {
        let warehouse = Warehouse::new(&format!("stack-{}", kind));
        let head = warehouse.head("main");
        warehouse.write(written, "x\n");
        warehouse.write("zzz.txt", "also staged\n");
        warehouse.run_ok(&["load", "."]);

        let stack = warehouse.run(&["stack", "nested"]);
        assert!(!stack.status.success(), "stack accepted {}", written);
        assert!(stderr(&stack).contains(&format!("{:?}", refused)), "the refusal must name {}: {}", refused, stderr(&stack));
        assert!(stderr(&stack).contains("unload it and add it to .forkliftignore"), "{}", stderr(&stack));
        assert_eq!(warehouse.head("main"), head, "a parcel was stacked");

        warehouse.run_ok(&["unload", refused]);
        warehouse.write(".forkliftignore", &format!("^{}$\n", refused.replace('.', "\\.")));
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "without it"]);
        assert_ne!(warehouse.head("main"), head);
        assert_eq!(warehouse.changes("staged"), Vec::<(String, String)>::new());
    }
}

/// A warehouse prepared in an untracked subdirectory is another working tree: `load .` stages
/// none of it, stocktake lists it as one untracked folder, and a load naming it, or a path in it,
/// refuses.
#[test]
fn an_untracked_nested_working_tree_is_left_to_itself() {
    let warehouse = Warehouse::new("walk-untracked");
    warehouse.prepare_nested("sub");
    warehouse.write("sub/x.txt", "inner\n");
    warehouse.write("other.txt", "outer\n");

    warehouse.run_ok(&["load", "."]);
    let staged = warehouse.changes("staged");
    assert_eq!(staged, vec![("added".to_string(), "other.txt".to_string())], "only other.txt may be staged");

    let unstaged = warehouse.changes("unstaged");
    assert!(unstaged.contains(&("untracked".to_string(), "sub/".to_string())), "{:?}", unstaged);
    assert!(!unstaged.iter().any(|(_, path)| path.starts_with("sub/") && path != "sub/"),
        "nothing below sub/ may be listed: {:?}", unstaged);

    for target in ["sub", "sub/x.txt", "sub/.forklift"] {
        let load = warehouse.run(&["load", target]);
        assert!(!load.status.success(), "load {} must refuse", target);
        assert!(stderr(&load).contains("\"sub\"") || stderr(&load).contains("metadata folder"),
            "load {} must say why: {}", target, stderr(&load));
    }
    assert_eq!(warehouse.changes("staged"), staged, "a refused load staged something");

    // The nested working tree is its own: a command run inside it answers from it.
    let inner = warehouse.run_at(&warehouse.path("sub"), &["--json", "stocktake"]);
    assert!(inner.status.success(), "{}", stderr(&inner));
    assert!(stdout(&inner).contains("x.txt"), "the nested warehouse must see its own file: {}", stdout(&inner));
}

/// A folder this warehouse tracks that becomes another working tree's root makes the walks refuse
/// rather than guess, and the way out the message names leaves a clean walk: straight away when
/// only stocktake refused, and after re-running the load when a load did, as its own note says.
#[test]
fn a_tracked_folder_that_becomes_a_nested_working_tree_refuses_until_it_is_removed() {
    for after_load in [false, true] {
        let warehouse = Warehouse::new(&format!("walk-tracked-{}", after_load));
        warehouse.write("sub/x.txt", "tracked\n");
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "track sub"]);
        warehouse.prepare_nested("sub");

        let mut refusing: Vec<&[&str]> = vec![&["stocktake"], &["load", "sub/x.txt"]];
        if after_load {
            refusing.push(&["load", "."]);
        }
        for args in refusing {
            let refused = warehouse.run(args);
            assert!(!refused.status.success(), "`{}` must refuse", args.join(" "));
            assert!(stderr(&refused).contains("\"forklift remove sub\""),
                "`{}` must name the way out: {}", args.join(" "), stderr(&refused));
        }

        warehouse.run_ok(&["remove", "sub"]);
        let named = warehouse.run(&["load", "sub/x.txt"]);
        assert!(!named.status.success(), "a named load into the nested tree must still refuse");
        assert!(stderr(&named).contains("that working tree's to load"),
            "once removed, the named load must not name the way out again: {}", stderr(&named));
        if after_load {
            warehouse.run_ok(&["load", "."]);
        }
        warehouse.run_ok(&["stack", "leave sub"]);

        warehouse.run_ok(&["load", "."]);
        assert_eq!(warehouse.changes("staged"), Vec::<(String, String)>::new(), "after_load {}", after_load);
        let unstaged = warehouse.changes("unstaged");
        assert_eq!(unstaged, vec![("untracked".to_string(), "sub/".to_string())], "after_load {}", after_load);
        assert_eq!(warehouse.read("sub/x.txt"), "tracked\n", "the way out must keep the files");
    }
}

/// A shift that would write below another working tree's root refuses before anything is written.
#[test]
fn a_shift_into_an_untracked_nested_working_tree_refuses_and_writes_nothing() {
    let warehouse = Warehouse::new("shift-into");
    warehouse.run_ok(&["pallet", "create", "feature"]);
    warehouse.run_ok(&["shift", "feature"]);
    warehouse.write("app.txt", "changed\n");
    warehouse.write("sub/x.txt", "feature\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "feature"]);
    warehouse.run_ok(&["shift", "main"]);
    assert!(!warehouse.path("sub").exists());

    warehouse.prepare_nested("sub");
    let shift = warehouse.run(&["shift", "feature"]);

    if let Some(problem) = refused_and_untouched(&warehouse, &shift, "\"sub\"", "sub/x.txt") {
        panic!("{}", problem);
    }
}

/// The three places a nested working tree can sit relative to the ignore rules: no rule covers it,
/// a rule covers it, and a rule covers a directory above it. Each is `(ignore file, nested root)`.
fn ignore_variants(parent: &str) -> [(Option<String>, String); 3] {
    [
        (None, format!("{}/sub", parent)),
        (Some(format!("^{}/sub$\n", parent)), format!("{}/sub", parent)),
        (Some(format!("^{}/gen$\n", parent)), format!("{}/gen/sub", parent)),
    ]
}

/// A shift that replaces a tracked folder holding another working tree with a file refuses up
/// front and writes nothing, whether or not an ignore rule hides that working tree.
#[test]
fn replacing_a_folder_holding_a_nested_working_tree_refuses_whatever_the_rules_say() {
    let mut failures = Vec::new();

    for (index, (ignore, nested)) in ignore_variants("a").into_iter().enumerate() {
        let warehouse = Warehouse::empty(&format!("flip-nested-{}", index));
        warehouse.write("0.txt", "zero\n");
        warehouse.write("a/x", "tracked\n");
        warehouse.write("app.txt", "payload\n");
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "dir"]);
        warehouse.run_ok(&["pallet", "create", "flip"]);
        warehouse.run_ok(&["shift", "flip"]);
        warehouse.run_ok(&["remove", "a"]);
        std::fs::remove_dir_all(warehouse.path("a")).unwrap();
        warehouse.write("a", "now a file\n");
        warehouse.write("0.txt", "changed\n");
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "file"]);
        warehouse.run_ok(&["shift", "main"]);

        if let Some(ignore) = &ignore {
            warehouse.write(".forkliftignore", ignore);
        }
        warehouse.prepare_nested(&nested);

        let shift = warehouse.run(&["shift", "flip"]);
        let mut problems = Vec::new();
        if shift.status.success() {
            problems.push("it succeeded".to_string());
        }
        if warehouse.read("0.txt") != "zero\n" {
            problems.push("the earlier-sorting 0.txt was rewritten".to_string());
        }
        if !warehouse.path("a/x").is_file() {
            problems.push("the tracked a/x was removed".to_string());
        }
        if !warehouse.path(&nested).join(".forklift").is_dir() {
            problems.push("the nested warehouse is gone".to_string());
        }
        if !problems.is_empty() {
            failures.push(format!("ignore {:?}: {}", ignore, problems.join("; ")));
        }
    }

    assert!(failures.is_empty(), "{:#?}", failures);
}

/// The dir→file flip over an ignored plain file refuses up front: applying removes only tracked
/// files, so the ignored one would keep the folder and fail the write halfway.
#[test]
fn replacing_a_folder_holding_an_ignored_file_refuses_and_writes_nothing() {
    let warehouse = Warehouse::empty("flip-ignored");
    warehouse.write("0.txt", "zero\n");
    warehouse.write("a/x", "tracked\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "dir"]);
    warehouse.run_ok(&["pallet", "create", "flip"]);
    warehouse.run_ok(&["shift", "flip"]);
    warehouse.run_ok(&["remove", "a"]);
    std::fs::remove_dir_all(warehouse.path("a")).unwrap();
    warehouse.write("a", "now a file\n");
    warehouse.write("0.txt", "changed\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "file"]);
    warehouse.run_ok(&["shift", "main"]);

    warehouse.write(".forkliftignore", "^a/junk$\n");
    warehouse.write("a/junk", "ignored\n");

    let shift = warehouse.run(&["shift", "flip"]);
    assert!(!shift.status.success(), "shift replaced a folder holding an ignored file");
    assert!(stderr(&shift).contains("untracked files:\n  a\n"), "the refusal must name the folder: {}", stderr(&shift));
    assert_eq!(warehouse.read("0.txt"), "zero\n", "the earlier-sorting 0.txt was rewritten");
    assert_eq!(warehouse.read("a/x"), "tracked\n", "the tracked a/x was removed");
    assert_eq!(warehouse.read("a/junk"), "ignored\n");

    // Once the ignored file is out of the way, the same shift goes through.
    std::fs::remove_file(warehouse.path("a/junk")).unwrap();
    warehouse.run_ok(&["shift", "flip"]);
    assert_eq!(warehouse.read("a"), "now a file\n");
}

/// git import refuses a git history whose tree names a metadata folder below the top level.
#[test]
fn import_git_refuses_a_metadata_folder_below_the_top_level() {
    let base = std::env::temp_dir().join(format!("forklift-nested-import-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let root = base.join("repo");
    let home = base.join("home");
    std::fs::create_dir_all(root.join("sub/.forklift")).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(root.join("app.txt"), "payload\n").unwrap();
    std::fs::write(root.join("sub/.forklift/config"), "planted\n").unwrap();
    git_in(&root, &["init", "-q", "-b", "main"]);
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "planted"]);
    // The planted folder stays in git's history; it must not stay on disk, or it would be the
    // warehouse `prepare` finds.
    std::fs::remove_dir_all(root.join("sub")).unwrap();

    let run = |args: &[&str]| forklift_command()
        .args(args)
        .current_dir(&root)
        .env("FORKLIFT_GLOBAL_CONFIG", home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", home.join("keys"))
        .output()
        .unwrap();
    assert!(run(&["prepare"]).status.success());
    assert!(run(&["config", "set", "operator.identifier", "nested@forklift"]).status.success());

    let import = run(&["git", "import", "."]);
    let _ = std::fs::remove_dir_all(&base);
    assert!(!import.status.success(), "git import accepted the planted folder");
    assert!(stderr(&import).contains("\"sub\"") && stderr(&import).contains("\".forklift\""),
        "the refusal must name where: {}", stderr(&import));
}

/// narrow refuses to delete a dropped subtree holding another working tree, whatever the ignore
/// rules say, and leaves it in place.
#[test]
fn narrow_refuses_to_delete_a_nested_working_tree_whatever_the_rules_say() {
    let mut failures = Vec::new();

    for (index, (ignore, nested)) in ignore_variants("a").into_iter().enumerate() {
        let warehouse = Warehouse::empty(&format!("narrow-{}", index));
        warehouse.write("a/x.txt", "a\n");
        warehouse.write("b/y.txt", "b\n");
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "base"]);

        let bay = warehouse.base.join("scoped");
        warehouse.run_ok(&["bay", "add", "scoped", bay.to_str().unwrap(), "--scope", "a", "--scope", "b"]);
        if let Some(ignore) = &ignore {
            std::fs::write(bay.join(".forkliftignore"), ignore).unwrap();
        }
        let nested_dir = bay.join(&nested);
        std::fs::create_dir_all(&nested_dir).unwrap();
        let prepared = warehouse.run_at(&nested_dir, &["prepare"]);
        assert!(prepared.status.success(), "{}", stderr(&prepared));

        let narrow = warehouse.run_at(&bay, &["narrow", "a"]);
        let mut problems = Vec::new();
        if narrow.status.success() {
            problems.push("it succeeded".to_string());
        } else if !stderr(&narrow).contains(&format!("\"{}\"", nested)) {
            problems.push(format!("the refusal does not name {}: {}", nested, stderr(&narrow)));
        }
        if !nested_dir.join(".forklift").is_dir() {
            problems.push("the nested warehouse is gone".to_string());
        }
        if !bay.join("a/x.txt").is_file() {
            problems.push("a/x.txt was de-materialized".to_string());
        }
        if !problems.is_empty() {
            failures.push(format!("ignore {:?}: {}", ignore, problems.join("; ")));
        }
    }

    assert!(failures.is_empty(), "{:#?}", failures);
}
