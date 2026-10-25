//! Two live entries a case-folding volume holds as one (`README` beside `readme`, `Docs/a` beside
//! `docs/a`) are never recorded in a tree from that volume: stack and park refuse them, naming the
//! shallowest pair and the way to clear it, and the way they name works from the state that
//! produced the refusal.
//!
//! Each state is reached on a working tree that also carries unrelated work: an edit staged
//! before the pair arose, an edit left unstaged, and an untracked file. The tests that need a
//! folding volume return early on one that does not fold; the macOS and Windows runners must fold.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Output;

use forklift_core::globals::StorageRootScope;
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
    /// A warehouse on `main` with `files` and the unrelated files stacked as its first parcel.
    fn new(name: &str, files: &[(&str, &str)]) -> Warehouse {
        let base = std::env::temp_dir()
            .join(format!("forklift-alias-pairs-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        for (path, content) in files.iter().chain(&[("staged.txt", "staged\n"), ("unstaged.txt", "unstaged\n")]) {
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
        assert!(output.status.success(), "`{}` failed: {}", args.join(" "), stderr(&output));
        output
    }

    fn path(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn write(&self, path: &str, content: &str) {
        let path = self.path(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn rename(&self, from: &str, to: &str) {
        forklift_core::util::file_utils::rename(self.path(from), self.path(to)).unwrap();
    }

    fn stocktake(&self) -> String {
        stdout(&self.run_ok(&["stocktake"]))
    }

    fn parked(&self) -> String {
        stdout(&self.run_ok(&["park", "list"]))
    }

    fn head(&self) -> String {
        let _scope = StorageRootScope::enter(&self.root);
        pallet_utils::get_pallet_head("main").unwrap().unwrap()
    }

    /// The file paths the head tree holds.
    fn head_files(&self) -> BTreeSet<String> {
        let _scope = StorageRootScope::enter(&self.root);
        let parcel = object_utils::load_parcel(&self.head()).unwrap();
        let mut files = BTreeSet::new();
        collect_files(&object_utils::load_tree(&parcel.tree_hash).unwrap(), "", &mut files);
        files
    }

    /// Every entry of the working tree outside the metadata folder, as listed, with each file's
    /// content.
    fn disk(&self) -> BTreeMap<String, Option<String>> {
        let mut entries = BTreeMap::new();
        walk(&self.root, "", &mut entries);
        entries
    }

    fn folds_case(&self) -> bool {
        let probe = self.base.join("case-probe");
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("probe"), "").unwrap();
        let folds = probe.join("PROBE").exists();
        std::fs::remove_dir_all(&probe).unwrap();
        folds
    }

    /// Whether this test can bite here: the volume folds case. The macOS and Windows runners'
    /// temporary directories must, or every test here would pass without checking anything.
    fn bites(&self) -> bool {
        if cfg!(any(target_os = "macos", windows)) {
            assert!(self.folds_case(), "the temporary directory is expected to fold case on this OS");
        }
        self.folds_case()
    }

    /// The unrelated work every state carries: a staged edit, made before the pair arises.
    fn stage_other_work(&self) {
        self.write("staged.txt", "staged, edited\n");
        self.run_ok(&["load", "staged.txt"]);
    }

    /// The unrelated work a remedy must leave alone: an unstaged edit and an untracked file.
    fn leave_other_work(&self) {
        self.write("unstaged.txt", "unstaged, edited\n");
        self.write("stray.txt", "stray\n");
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn collect_files(tree: &TreeItem, prefix: &str, files: &mut BTreeSet<String>) {
    let join = |name: &str| if prefix.is_empty() { name.to_string() } else { format!("{}/{}", prefix, name) };

    for (name, _) in tree.get_files() {
        files.insert(join(name));
    }
    for (name, subtree) in tree.get_subtrees() {
        collect_files(&object_utils::load_tree(&subtree.hash).unwrap(), &join(name), files);
    }
}

fn walk(dir: &Path, prefix: &str, entries: &mut BTreeMap<String, Option<String>>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if prefix.is_empty() && name == ".forklift" {
            continue;
        }
        let key = if prefix.is_empty() { name } else { format!("{}/{}", prefix, name) };

        if entry.file_type().unwrap().is_dir() {
            entries.insert(key.clone(), None);
            walk(&entry.path(), &key, entries);
        } else {
            entries.insert(key, Some(std::fs::read_to_string(entry.path()).unwrap()));
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The lines of the stocktake section headed by `heading`.
fn section<'a>(stocktake: &'a str, heading: &str) -> Vec<&'a str> {
    stocktake.lines()
        .skip_while(|line| !line.starts_with(heading))
        .skip(1)
        .take_while(|line| !line.is_empty())
        .map(str::trim)
        .collect()
}

const STAGED: &str = "Staged changes";
const NOT_STAGED: &str = "Changes not in the inventory";

/// The words every directory-wide remedy owes the user.
const LOAD_ALL_WARNING: &str = "That stages every change in the working tree, not only this one, unstaged edits and \
    untracked files included: check \"forklift stocktake\" before stacking.";

/// Assert `output` is the pair refusal, naming `pair` and offering `remedy` (each command's
/// arguments, in order), and return its message.
fn assert_pair_refusal(output: &Output, pair: (&str, &str), remedy: &[&[&str]]) -> String {
    let message = stderr(output);
    assert_eq!(output.status.code(), Some(1), "expected the pair refusal: {}{}", stdout(output), message);

    let named = format!("The inventory holds both \"{}\" and \"{}\", which this file system treats as one entry",
                        pair.0, pair.1);
    assert!(message.contains(&named), "the refusal must name the pair {:?}: {}", pair, message);

    let commands: Vec<String> = remedy.iter().map(|args| format!("\"forklift {}\"", args.join(" "))).collect();
    let instructions = format!("From the working tree's root, run {}", commands.join(", then "));
    assert!(message.contains(&instructions), "the refusal must offer {}: {}", instructions, message);
    assert!(message.contains("Then run this command again."), "{}", message);

    message
}

/// One state the refusal must catch.
struct State {
    name: &'static str,
    /// What the first parcel holds besides the unrelated files.
    tracked: &'static [(&'static str, &'static str)],
    /// The steps that reach the pair, after the staged unrelated edit.
    reach: fn(&Warehouse),
    /// The shallowest pair, as the refusal names it.
    pair: (&'static str, &'static str),
    /// The remedy the refusal offers, one command's arguments each.
    remedy: &'static [&'static [&'static str]],
    /// The tracked file that must be on disk after a park, and the move a pop re-stages.
    parked_file: &'static str,
    moved: &'static str,
}

const LOAD_ALL: &[&[&str]] = &[&["load", "."]];

fn states() -> Vec<State> {
    vec![
        State {
            name: "loadnew",
            tracked: &[("README", "readme\n")],
            reach: |w| { w.rename("README", "readme"); w.run_ok(&["load", "readme"]); },
            pair: ("README", "readme"),
            remedy: &[&["remove", "README"]],
            parked_file: "README",
            moved: "moved:     README -> readme",
        },
        State {
            name: "restore",
            tracked: &[("README", "readme\n")],
            reach: |w| {
                w.rename("README", "readme");
                w.run_ok(&["load", "."]);
                w.run_ok(&["restore", "--staged", "README"]);
            },
            pair: ("README", "readme"),
            remedy: &[&["remove", "README"]],
            parked_file: "README",
            moved: "moved:     README -> readme",
        },
        State {
            name: "loaddir",
            tracked: &[("Docs/a", "a\n")],
            reach: |w| { w.rename("Docs", "docs"); w.run_ok(&["load", "docs"]); },
            pair: ("Docs", "docs"),
            remedy: LOAD_ALL,
            parked_file: "Docs/a",
            moved: "moved:     Docs/a -> docs/a",
        },
        State {
            name: "nested",
            tracked: &[("Docs/sub/a", "a\n")],
            reach: |w| { w.rename("Docs", "docs"); w.run_ok(&["load", "docs"]); },
            pair: ("Docs", "docs"),
            remedy: LOAD_ALL,
            parked_file: "Docs/sub/a",
            moved: "moved:     Docs/sub/a -> docs/sub/a",
        },
        State {
            name: "aliasparent",
            tracked: &[("Docs/README", "readme\n")],
            reach: |w| {
                w.rename("Docs/README", "Docs/readme");
                w.run_ok(&["load", "Docs/readme"]);
                w.rename("Docs", "docs");
            },
            pair: ("Docs/README", "Docs/readme"),
            remedy: LOAD_ALL,
            parked_file: "Docs/README",
            moved: "moved:     Docs/README -> docs/readme",
        },
        State {
            name: "thirdspelling",
            tracked: &[("README", "readme\n")],
            reach: |w| {
                w.rename("README", "Readme");
                w.run_ok(&["load", "Readme"]);
                w.rename("Readme", "readme");
            },
            pair: ("README", "Readme"),
            remedy: &[&["remove", "README"], &["remove", "Readme"], &["load", "readme"]],
            parked_file: "README",
            moved: "moved:     README -> readme",
        },
        State {
            name: "thirddir",
            tracked: &[("Docs/a", "a\n")],
            reach: |w| {
                w.rename("Docs", "DOcs");
                w.run_ok(&["load", "DOcs"]);
                w.rename("DOcs", "docs");
            },
            pair: ("DOcs", "Docs"),
            remedy: LOAD_ALL,
            parked_file: "Docs/a",
            moved: "moved:     Docs/a -> docs/a",
        },
    ]
}

/// From each state, stack and park are refused with nothing changed, the remedy they name clears
/// the pair, and the work then parks and pops back as the move it was.
#[test]
fn stack_and_park_refuse_a_pair_and_its_remedy_clears_it() {
    for state in states() {
        let warehouse = Warehouse::new(state.name, state.tracked);
        if !warehouse.bites() {
            return;
        }

        warehouse.stage_other_work();
        (state.reach)(&warehouse);

        let (head, parked, stocktake, disk) =
            (warehouse.head(), warehouse.parked(), warehouse.stocktake(), warehouse.disk());

        for command in [&["stack", "pair"][..], &["park"][..]] {
            let output = warehouse.run(command);
            assert_pair_refusal(&output, state.pair, state.remedy);

            assert_eq!(warehouse.head(), head, "{}: a refused {} moves no head", state.name, command[0]);
            assert_eq!(warehouse.parked(), parked, "{}: a refused {} parks nothing", state.name, command[0]);
            assert_eq!(warehouse.stocktake(), stocktake, "{}: a refused {} stages nothing", state.name, command[0]);
            assert_eq!(warehouse.disk(), disk, "{}: a refused {} touches no file", state.name, command[0]);
        }

        for args in state.remedy {
            warehouse.run_ok(args);
        }

        warehouse.run_ok(&["park"]);
        assert!(warehouse.path(state.parked_file).is_file(), "{}: the parked file is gone: {:?}",
                state.name, warehouse.disk());
        warehouse.run_ok(&["park", "pop"]);

        let staged = warehouse.stocktake();
        assert!(section(&staged, STAGED).contains(&state.moved), "{}: the pop must re-stage the move: {}",
                state.name, staged);
    }
}

/// A remedy confined to the pair leaves unrelated work where it was: the unstaged edit stays
/// unstaged and the untracked file untracked. Where the remedy loads a whole directory, the
/// refusal warns that it stages everything there, and it does.
#[test]
fn a_pair_remedy_stages_only_what_it_says() {
    for state in states() {
        let warehouse = Warehouse::new(&format!("reach-{}", state.name), state.tracked);
        if !warehouse.bites() {
            return;
        }

        warehouse.stage_other_work();
        (state.reach)(&warehouse);
        warehouse.leave_other_work();

        let message = assert_pair_refusal(&warehouse.run(&["stack", "pair"]), state.pair, state.remedy);
        for args in state.remedy {
            warehouse.run_ok(args);
        }

        let stocktake = warehouse.stocktake();
        let (staged, not_staged) = (section(&stocktake, STAGED), section(&stocktake, NOT_STAGED));
        assert!(staged.contains(&state.moved), "{}: the remedy must stage the move: {}", state.name, stocktake);

        if state.remedy == LOAD_ALL {
            assert!(message.contains(LOAD_ALL_WARNING), "{}: {}", state.name, message);
            assert!(staged.contains(&"modified:  unstaged.txt"), "{}: {}", state.name, stocktake);
            assert!(staged.contains(&"added:     stray.txt"), "{}: {}", state.name, stocktake);
        } else {
            assert!(!message.contains(LOAD_ALL_WARNING), "{}: {}", state.name, message);
            assert!(not_staged.contains(&"modified:  unstaged.txt"), "{}: {}", state.name, stocktake);
            assert!(not_staged.contains(&"untracked: stray.txt"), "{}: {}", state.name, stocktake);
        }

        warehouse.run_ok(&["stack", "cleared"]);
    }
}

/// Both spellings live and neither on disk: stack is refused, and removing each spelling clears
/// the pair, leaving a head that holds neither.
#[test]
fn a_pair_with_neither_spelling_on_disk_is_refused_and_removed() {
    let warehouse = Warehouse::new("absent", &[("README", "readme\n")]);
    if !warehouse.bites() {
        return;
    }

    warehouse.stage_other_work();
    warehouse.rename("README", "readme");
    warehouse.run_ok(&["load", "readme"]);
    std::fs::remove_file(warehouse.path("readme")).unwrap();
    warehouse.leave_other_work();

    let head = warehouse.head();
    assert_pair_refusal(&warehouse.run(&["stack", "pair"]), ("README", "readme"),
                        &[&["remove", "README"], &["remove", "readme"]]);
    assert_eq!(warehouse.head(), head);

    warehouse.run_ok(&["remove", "README"]);
    warehouse.run_ok(&["remove", "readme"]);

    let stocktake = warehouse.stocktake();
    assert!(section(&stocktake, NOT_STAGED).contains(&"modified:  unstaged.txt"), "{}", stocktake);
    assert!(section(&stocktake, NOT_STAGED).contains(&"untracked: stray.txt"), "{}", stocktake);

    warehouse.run_ok(&["stack", "cleared"]);
    let files = warehouse.head_files();
    assert!(!files.contains("README") && !files.contains("readme"), "{:?}", files);
    assert!(files.contains("staged.txt"), "{:?}", files);
}

/// Both spellings of a directory live, and the listed one now a file: every path beneath them
/// meets "not a directory", which is the pair's absence, not a read error.
#[test]
fn a_pair_beneath_a_spelling_that_became_a_file_is_refused_not_unreadable() {
    let warehouse = Warehouse::new("notdir", &[("Docs/a", "a\n")]);
    if !warehouse.bites() {
        return;
    }

    warehouse.stage_other_work();
    warehouse.rename("Docs", "docs");
    warehouse.run_ok(&["load", "docs/a"]);
    std::fs::remove_dir_all(warehouse.path("docs")).unwrap();
    warehouse.write("docs", "x\n");

    let head = warehouse.head();
    let message = assert_pair_refusal(&warehouse.run(&["stack", "pair"]), ("Docs", "docs"), LOAD_ALL);
    assert!(!message.contains("Error while"), "{}", message);
    assert_eq!(warehouse.head(), head);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "cleared"]);
}

/// The listed occupant of a file pair is a directory: removing the unlisted spelling would mark
/// that directory's key instead of the file entry, so the remedy loads the whole tree.
#[test]
fn a_file_pair_whose_listed_occupant_is_a_directory_is_cleared_by_loading_the_tree() {
    let warehouse = Warehouse::new("occupant", &[("README/x", "x\n")]);
    if !warehouse.bites() {
        return;
    }

    std::fs::remove_dir_all(warehouse.path("README")).unwrap();
    warehouse.write("README", "f\n");
    warehouse.run_ok(&["load", "README"]);
    warehouse.rename("README", "Readme");
    warehouse.run_ok(&["load", "Readme"]);
    std::fs::remove_file(warehouse.path("Readme")).unwrap();
    std::fs::create_dir(warehouse.path("readme")).unwrap();

    let message = assert_pair_refusal(&warehouse.run(&["stack", "pair"]), ("README", "Readme"), LOAD_ALL);
    assert!(message.contains(LOAD_ALL_WARNING), "{}", message);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "cleared"]);
}

/// Two names both listed under their own spelling are two entries, even when they are one file:
/// hard links on a case-sensitive volume stack, park and pop with both names kept.
#[cfg(unix)]
#[test]
fn hard_links_listed_under_both_spellings_are_not_a_pair() {
    let warehouse = Warehouse::new("hardlinks", &[]);
    if warehouse.folds_case() {
        return;
    }

    warehouse.write("README", "readme\n");
    std::fs::hard_link(warehouse.path("README"), warehouse.path("readme")).unwrap();
    warehouse.run_ok(&["load", "README"]);
    warehouse.run_ok(&["load", "readme"]);
    warehouse.run_ok(&["stack", "links"]);

    warehouse.write("staged.txt", "edited\n");
    warehouse.run_ok(&["load", "staged.txt"]);
    warehouse.run_ok(&["park"]);
    warehouse.run_ok(&["park", "pop"]);

    assert!(warehouse.path("README").is_file() && warehouse.path("readme").is_file(), "{:?}", warehouse.disk());
    let files = warehouse.head_files();
    assert!(files.contains("README") && files.contains("readme"), "{:?}", files);
}
