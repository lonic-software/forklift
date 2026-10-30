//! The staged inventory keeps one shard per tracked directory, at an address hashed from the
//! directory's key. These tests drive the store end to end: keys a volume folds into one spelling
//! keep separate shards, keys of any length or depth stage, a store in another layout is refused
//! rather than read, and a reset leaves nothing a later key could read back.

use std::path::PathBuf;
use std::process::Output;

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
            .join(format!("forklift-shard-store-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { base, root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "operator.identifier", "spike@forklift"]);

        for (path, content) in files {
            warehouse.write(path, content);
        }
        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "first"]);

        warehouse
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(".", args)
    }

    fn run_in(&self, dir: &str, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(self.root.join(dir))
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

    fn write(&self, path: &str, content: &str) {
        let path = self.root.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }

    fn stocktake(&self) -> String {
        stdout(&self.run_ok(&["stocktake"]))
    }

    fn assert_clean(&self) {
        let status = self.stocktake();
        assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
        assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
    }

    fn folds_case(&self) -> bool {
        let probe = self.base.join("case-probe");
        std::fs::create_dir_all(&probe).unwrap();
        std::fs::write(probe.join("probe"), "").unwrap();
        let folds = probe.join("PROBE").exists();
        std::fs::remove_dir_all(&probe).unwrap();
        folds
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.base);
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// The volumes these tests must bite on fold case; a runner that does not would pass them vacuously.
fn assert_folds_where_expected(warehouse: &Warehouse) {
    if cfg!(any(target_os = "macos", windows)) {
        assert!(warehouse.folds_case(), "the temporary directory is expected to fold case on this OS");
    }
}

/// A case-only directory rename on a folding volume stages as one move: the old key's removal and
/// the new key's addition keep separate shards, so neither overwrites the other's, and a park
/// restores the head with nothing left over.
#[test]
fn a_case_only_directory_rename_stages_as_a_move_and_parks_clean() {
    let warehouse = Warehouse::new("case-rename", &[("Docs/a", "a\n"), ("other", "other\n")]);
    assert_folds_where_expected(&warehouse);
    if !warehouse.folds_case() {
        return;
    }

    warehouse.write("other", "edited\n");
    forklift_core::util::file_utils::rename(warehouse.root.join("Docs"), warehouse.root.join("docs")).unwrap();
    warehouse.run_ok(&["load", "."]);

    let status = warehouse.stocktake();
    assert!(status.contains("moved:     Docs/a -> docs/a"), "status: {}", status);
    assert!(status.contains("modified:  other"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);

    warehouse.run_ok(&["park"]);
    assert_eq!(warehouse.read("Docs/a"), "a\n");
    assert_eq!(warehouse.read("other"), "other\n");
    warehouse.assert_clean();
}

/// A shard's path is the same length whatever its key, so long names and deep trees stage like
/// any other.
#[test]
fn long_and_deep_directory_keys_load_and_stack() {
    let cjk_66_bytes = "漢".repeat(22);
    let cjk_21_bytes = "漢".repeat(7);
    let trees = [
        vec![cjk_66_bytes.as_str(); 5].join("/"),
        vec![cjk_21_bytes.as_str(); 15].join("/"),
        vec!["A"; 30].join("/"),
    ];

    for (index, tree) in trees.iter().enumerate() {
        let warehouse = Warehouse::new(&format!("long-keys-{}", index), &[("app.txt", "app\n")]);
        let file = format!("{}/file.txt", tree);
        warehouse.write(&file, "deep\n");

        warehouse.run_ok(&["load", "."]);
        warehouse.run_ok(&["stack", "deep"]);

        warehouse.assert_clean();
        assert_eq!(warehouse.read(&file), "deep\n");
    }
}

/// A store whose metadata carries another layout version is refused by every command that would
/// read or replace it, and nothing in it is touched.
#[test]
fn a_store_in_another_layout_is_refused_and_left_alone() {
    let warehouse = Warehouse::new("other-layout", &[("Docs/a", "a\n")]);
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.write("side.txt", "side\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "side"]);

    let inventory = warehouse.root.join(".forklift/inventory");
    std::fs::remove_dir_all(inventory.join("inv_")).unwrap();
    let old_shards = [inventory.join("inv_/data"), inventory.join("inv_/inv_Docs/data")];
    for shard in &old_shards {
        std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
        std::fs::write(shard, b"an older layout's shard").unwrap();
    }
    std::fs::write(inventory.join("metadata"), "/forklift-inventory-metadata 1\n./\nDocs\n").unwrap();

    // A git repository with one commit, so the import gets as far as staging its checkout.
    let git = warehouse.root.join("git-source");
    let git_run = |args: &[&str]| std::process::Command::new("git")
        .args(["-c", "user.name=spike", "-c", "user.email=spike@forklift"])
        .args(args).current_dir(&git).status()
        .is_ok_and(|status| status.success());
    std::fs::create_dir_all(&git).unwrap();
    std::fs::write(git.join("g.txt"), "g\n").unwrap();
    // git import is the one command here that resets the store without reading its metadata, so
    // only the layout gate refuses it: the case is required, never skipped.
    assert!(git_run(&["init", "-q"]) && git_run(&["add", "g.txt"]) && git_run(&["commit", "-qm", "g"]),
            "the git source repository could not be created");

    let commands = [vec!["stocktake"], vec!["load", "."], vec!["shift", "main"], vec!["git", "import", "git-source"]];

    for command in &commands {
        let output = warehouse.run(command);
        assert!(!output.status.success(), "`{}` read the other layout: {}", command.join(" "), stdout(&output));
        assert!(
            stderr(&output).contains("is in a format this version of Forklift does not know"),
            "`{}`: {}", command.join(" "), stderr(&output)
        );
    }

    for shard in &old_shards {
        assert_eq!(std::fs::read(shard).unwrap(), b"an older layout's shard", "{}", shard.display());
    }
}

/// A reset of the whole store leaves no shard behind: a directory re-created after the shift and
/// loaded narrowly stages only what it holds now.
#[test]
fn a_directory_recreated_after_a_shift_stages_only_what_it_holds() {
    let warehouse = Warehouse::new("reset-sweep", &[("app.txt", "app\n")]);
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.write("X/f", "f\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "with X"]);

    warehouse.run_ok(&["shift", "main"]);
    assert!(!warehouse.root.join("X").exists(), "the shift removed X");

    warehouse.write("X/g", "g\n");
    warehouse.run_ok(&["load", "X/g"]);

    let status = warehouse.stocktake();
    assert!(status.contains("added:     X/g"), "status: {}", status);
    assert!(!status.contains("X/f"), "a shard of X from the other pallet survived the reset: {}", status);
}

/// A process started in a directory spelled differently from how its parent lists it stages under
/// the listed key, never beside it.
#[test]
fn a_load_from_a_differently_spelled_current_directory_stages_under_the_listed_key() {
    let warehouse = Warehouse::new("cwd-spelling", &[("docs/a", "a\n")]);
    assert_folds_where_expected(&warehouse);
    if !warehouse.folds_case() {
        return;
    }

    warehouse.write("docs/a", "changed\n");
    let load = warehouse.run_in("DOCS", &["load", "."]);
    assert!(load.status.success(), "{}", stderr(&load));

    let status = warehouse.stocktake();
    assert!(status.contains("modified:  docs/a"), "status: {}", status);
    assert!(!status.contains("DOCS"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

/// A shard left in the store at a directory no longer registered is never read: restoring a file
/// it lists does not bring the file back.
#[test]
fn restore_never_reads_a_shard_of_an_unregistered_directory() {
    let warehouse = Warehouse::new("restore-unregistered", &[("kept.txt", "kept\n")]);
    warehouse.write("docs/a.txt", "staged\n");
    warehouse.run_ok(&["load", "docs/a.txt"]);

    let inventory = warehouse.root.join(".forklift/inventory");
    let shard = inventory.join(forklift_core::util::file_utils::shard_folder_relative("docs")).join("data");
    let saved = std::fs::read(&shard).unwrap();

    warehouse.run_ok(&["restore", "--staged", "."]);
    assert!(!shard.exists(), "the reset unregistered docs and removed its shard");

    std::fs::create_dir_all(shard.parent().unwrap()).unwrap();
    std::fs::write(&shard, saved).unwrap();
    std::fs::remove_file(warehouse.root.join("docs/a.txt")).unwrap();

    let output = warehouse.run(&["restore", "docs/a.txt"]);

    assert!(!warehouse.root.join("docs/a.txt").exists(),
        "restore read the shard of an unregistered directory: {}", stdout(&output));
    assert!(!output.status.success(), "restore of an untracked file must fail");
    assert!(stderr(&output).contains("\"docs/a.txt\" is not in the inventory."), "{}", stderr(&output));
}

/// A link in the store is refused rather than followed, even with a valid shard behind it, and the
/// way out the refusal names restores a clean staging area.
#[test]
fn a_linked_shard_folder_is_refused_and_the_named_way_out_works() {
    let warehouse = Warehouse::new("linked-slot", &[("docs/a.txt", "a\n")]);

    let inventory = warehouse.root.join(".forklift/inventory");
    let slot = inventory.join(forklift_core::util::file_utils::shard_folder_relative("docs"));
    let outside = warehouse.base.join("outside");
    forklift_core::util::file_utils::rename(&slot, &outside).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&outside, &slot).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&outside, &slot).unwrap();

    let output = warehouse.run(&["stocktake"]);
    assert!(!output.status.success(), "stocktake read through the link: {}", stdout(&output));
    assert!(stderr(&output).contains("a link is never followed") && stderr(&output).contains("restore --staged ."),
            "{}", stderr(&output));

    if cfg!(windows) {
        std::fs::remove_dir(&slot).unwrap();
    } else {
        std::fs::remove_file(&slot).unwrap();
    }
    warehouse.run_ok(&["restore", "--staged", "."]);

    warehouse.assert_clean();
    assert!(std::fs::symlink_metadata(&slot).unwrap().is_dir(), "the shard is back in a real folder");
}
