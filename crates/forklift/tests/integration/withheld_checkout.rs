//! A file whose content the unavailable record lists, and the store lacks, is not checked out: the
//! head keeps naming it, no file stands at its path, and its inventory entry stays `Unavailable`
//! through every head move, load, unload and directory restore, so nothing turns it into a removal
//! unasked. A file put at its path is loaded as a change, and `forklift remove` stages a removal.

use std::path::PathBuf;
use std::process::Output;

use forklift_core::globals::StorageRootScope;
use forklift_core::util::{file_utils, object_utils, unavailable};

use crate::common;
use common::forklift_command;

/// The content `feature` changes `film.mkv` to, which the remote's host withholds.
const WITHHELD_FILM: &str = "the withheld film\n";

/// The content of `film.mkv` on `main`, which this warehouse holds.
const OLD_FILM: &str = "the old film\n";

/// One isolated, signed warehouse with its own home for global config + keys.
struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base = std::env::temp_dir().join(format!("forklift-withheld-checkout-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        let warehouse = Warehouse { root, home };
        warehouse.run_ok(&["prepare"]);
        warehouse.run_ok(&["config", "set", "operator.identifier", "withheld@forklift"]);
        warehouse.run_ok(&["office", "enroll"]);

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
            "`{}` failed: {}{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    fn write(&self, file: &str, content: &str) {
        let path = self.root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn stack_files(&self, files: &[(&str, &str)], message: &str) -> String {
        for (file, content) in files {
            self.write(file, content);
        }
        self.run_ok(&["load", "."]);
        self.run_ok(&["stack", "-m", message]);

        self.head()
    }

    fn head(&self) -> String {
        let pallet = std::fs::read_to_string(self.root.join(".forklift").join("position"))
            .unwrap()
            .lines()
            .nth(1)
            .unwrap()
            .to_string();

        std::fs::read_to_string(self.root.join(".forklift").join("pallets").join(pallet)).unwrap().trim().to_string()
    }

    fn delete_object(&self, hash: &str) {
        let objects = self.root.join(".forklift").join("objects").join(&hash[0..2]);
        file_utils::remove_file(objects.join(&hash[2..])).expect("the object existed");
    }

    fn scoped<T>(&self, work: impl FnOnce() -> T) -> T {
        let _scope = StorageRootScope::enter(&self.root);

        work()
    }

    fn exists(&self, file: &str) -> bool {
        self.root.join(file).symlink_metadata().is_ok()
    }

    /// `stocktake --json`'s data.
    fn stocktake(&self) -> serde_json::Value {
        let output = self.run_ok(&["stocktake", "--json"]);
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();

        report["data"].clone()
    }
}

/// The paths a stocktake list names, sorted.
fn paths(list: &serde_json::Value) -> Vec<String> {
    let mut paths: Vec<String> = list.as_array().map(|items| items.iter().map(|item| {
        item.as_str().map(str::to_string).unwrap_or_else(|| item["path"].as_str().unwrap().to_string())
    }).collect()).unwrap_or_default();
    paths.sort();

    paths
}

/// The `(kind, path)` pairs a stocktake list of changes names, sorted.
fn changes(list: &serde_json::Value) -> Vec<(String, String)> {
    let mut changes: Vec<(String, String)> = list.as_array().map(|items| items.iter().map(|item| {
        (item["kind"].as_str().unwrap().to_string(), item["path"].as_str().unwrap().to_string())
    }).collect()).unwrap_or_default();
    changes.sort();

    changes
}

fn file_hash(head: &str, path: &str) -> String {
    let root = object_utils::load_parcel(head).unwrap().tree_hash;
    object_utils::resolve_tree_file(&root, path).unwrap().expect("the path is a file").0
}

/// `main` holds `a.txt` and `film.mkv` at [`OLD_FILM`]; `feature` changes `film.mkv` to
/// [`WITHHELD_FILM`] and adds `extra/new.bin`, both recorded unavailable and absent from the store.
/// The bay is left on `main`. Returns the warehouse and `feature`'s head.
fn withheld_feature(name: &str) -> (Warehouse, String) {
    let warehouse = Warehouse::new(name);
    warehouse.stack_files(&[("a.txt", "a\n"), ("film.mkv", OLD_FILM)], "main");
    warehouse.run_ok(&["pallet", "create", "feature"]);
    let feature = warehouse.stack_files(&[("film.mkv", WITHHELD_FILM), ("extra/new.bin", "withheld new\n")], "feature");
    warehouse.run_ok(&["shift", "main"]);

    warehouse.scoped(|| {
        let withheld = [file_hash(&feature, "film.mkv"), file_hash(&feature, "extra/new.bin")];
        unavailable::record(withheld.iter().map(|hash| (hash.clone(), "Withheld by the remote's host.".to_string()))).unwrap();
        for hash in &withheld {
            warehouse.delete_object(hash);
        }
    });

    (warehouse, feature)
}

/// A head move writes no file whose content is unavailable: a new one is left out, and a changed
/// one is removed rather than left holding the old tree's bytes. Both stay staged at the head's
/// hash, and stocktake lists them apart from changes, with the bay clean: a shift away and back
/// is not refused, and brings the old bytes back and leaves them out again.
///
/// Revert: drop the skip's removal of the old file → `film.mkv` keeps the old bytes → red; take
/// an `Unavailable` entry as a change (`is_tracked_change` true for it) → the shift away is
/// refused → red; count it in `unstaged` → red.
#[test]
fn a_head_move_leaves_unavailable_content_out_and_the_bay_clean() {
    let (warehouse, _) = withheld_feature("shift");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), OLD_FILM);

    warehouse.run_ok(&["shift", "feature"]);
    assert!(!warehouse.exists("film.mkv"), "the changed file's old bytes are removed, not left under the new hash");
    assert!(!warehouse.exists("extra/new.bin"), "the new file is not written");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(), "a\n");

    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin", "film.mkv"], "{}", report);
    assert_eq!((&report["unavailable_count"], &report["staged_count"], &report["unstaged_count"]),
        (&2.into(), &0.into(), &0.into()), "{}", report);

    let human = warehouse.run_ok(&["stocktake"]);
    let human = String::from_utf8_lossy(&human.stdout);
    assert!(human.contains("Not checked out") && human.contains("film.mkv"), "{}", human);

    let summary: serde_json::Value = serde_json::from_slice(&warehouse.run_ok(&["stocktake", "--summary", "--json"]).stdout).unwrap();
    assert_eq!(summary["data"]["unavailable_count"], 2, "{}", summary);
    assert!(summary["data"].get("unavailable").is_none(), "--summary drops the list: {}", summary);

    warehouse.run_ok(&["shift", "main"]);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), OLD_FILM);
    assert!(warehouse.stocktake().get("unavailable_count").is_none(), "nothing is unavailable on main");

    warehouse.run_ok(&["shift", "feature"]);
    assert!(!warehouse.exists("film.mkv"));
    assert_eq!(warehouse.stocktake()["unavailable_count"], 2);
}

/// A load keeps an entry not checked out as it is, and the next parcel names the file at the
/// head's hash: `stack` neither drops it nor refuses its absent content. Audit and heal count none
/// of it as lost.
///
/// Revert: let the load's carry-over, or its pass over a directory gone from disk, stage it
/// `Deleted` → stocktake reports a removal and the stacked tree lacks it → red. The staged-content
/// check sees nothing here, since the head names the same hashes; see
/// [`a_soft_reset_restacks_a_file_not_checked_out`]. The audit walk's own boundary is pinned in
/// `recovery_utils`'s unit tests: plain `heal` runs it only over a torn taint.
#[test]
fn a_load_and_a_stack_keep_a_file_not_checked_out() {
    let (warehouse, feature) = withheld_feature("load");
    warehouse.run_ok(&["shift", "feature"]);

    warehouse.write("b.txt", "b\n");
    warehouse.run_ok(&["load", "."]);
    let report = warehouse.stocktake();
    assert_eq!(changes(&report["staged"]), [("added".to_string(), "b.txt".to_string())], "{}", report);
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin", "film.mkv"], "{}", report);

    let healed = warehouse.run_ok(&["heal", "--json"]);
    let healed: serde_json::Value = serde_json::from_slice(&healed.stdout).unwrap();
    assert!(!healed["data"].to_string().contains("missing"), "{}", healed);

    warehouse.run_ok(&["stack", "-m", "b"]);
    let head = warehouse.head();
    warehouse.scoped(|| {
        for path in ["film.mkv", "extra/new.bin"] {
            assert_eq!(file_hash(&head, path), file_hash(&feature, path), "{} is named as the head named it", path);
        }
    });
    warehouse.run_ok(&["audit"]);
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), ["extra/new.bin", "film.mkv"]);
}

/// `unload`'s reset, a directory restore and a park and its pop keep an entry not checked out:
/// the restore leaves it out and says how many it left, and writes nothing at its path.
///
/// Revert: let unload's reset start it `Normal` → stocktake reports it removed → red; let a
/// directory restore plan it → the restore fails over its absent content → red; drop its report
/// → red; let park's refresh of tracked entries stage a missing one's removal → red.
#[test]
fn unload_restore_and_park_keep_a_file_not_checked_out() {
    let (warehouse, _) = withheld_feature("unload");
    warehouse.run_ok(&["shift", "feature"]);
    let unavailable = ["extra/new.bin", "film.mkv"];

    warehouse.write("a.txt", "changed\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["unload", "."]);
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), unavailable, "{}", report);
    assert_eq!(changes(&report["unstaged"]), [("modified".to_string(), "a.txt".to_string())], "{}", report);

    let restored = warehouse.run_ok(&["restore", ".", "--confirm"]);
    let restored = String::from_utf8_lossy(&restored.stdout);
    assert!(restored.contains("Left out 2 file(s) not checked out"), "{}", restored);
    assert!(!warehouse.exists("film.mkv") && !warehouse.exists("extra/new.bin"));
    assert_eq!(std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(), "a\n");
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), unavailable);

    warehouse.write("a.txt", "parked\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["park"]);
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), unavailable);
    warehouse.run_ok(&["park", "pop"]);
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), unavailable, "{}", report);
    assert_eq!(changes(&report["staged"]), [("modified".to_string(), "a.txt".to_string())], "{}", report);
}

/// A file put at a path not checked out is loaded as a change at its own hash, never taken for the
/// withheld content: bytes equal to it are stored, which makes the entry an ordinary one and drops
/// the record's entry, and other bytes are a modification. `forklift remove` stages a removal on
/// purpose, and a directory standing at the path stages the file's removal.
///
/// Revert: compare a file at an `Unavailable` entry by stat → the equal bytes are never stored
/// and the record keeps the entry → red; keep an `Unavailable` entry over a directory at its path
/// → no removal is staged → red.
#[test]
fn a_file_put_back_a_removal_and_a_directory_end_a_file_not_checked_out() {
    let (warehouse, feature) = withheld_feature("put-back");
    warehouse.run_ok(&["shift", "feature"]);

    warehouse.write("film.mkv", WITHHELD_FILM);
    warehouse.write("extra/new.bin", "other bytes\n");
    warehouse.run_ok(&["load", "."]);
    let report = warehouse.stocktake();
    assert!(report.get("unavailable").is_none(), "{}", report);
    assert_eq!(changes(&report["staged"]), [("modified".to_string(), "extra/new.bin".to_string())], "{}", report);

    warehouse.scoped(|| {
        let film = file_hash(&feature, "film.mkv");
        assert!(file_utils::does_object_exist(&film).unwrap(), "the put-back bytes are stored");
        let record = unavailable::read_unavailable().unwrap();
        assert!(!record.contains(&film), "the record drops content the store holds again");
        assert!(record.contains(&file_hash(&feature, "extra/new.bin")), "other bytes drop nothing");
    });

    let (warehouse, _) = withheld_feature("remove");
    warehouse.run_ok(&["shift", "feature"]);
    warehouse.run_ok(&["remove", "film.mkv"]);
    warehouse.write("extra/new.bin/inner.txt", "a directory now\n");
    let report = warehouse.stocktake();
    assert!(changes(&report["unstaged"]).contains(&("removed".to_string(), "extra/new.bin".to_string())),
        "stocktake shows the removal the load will stage: {}", report);
    warehouse.run_ok(&["load", "."]);
    let report = warehouse.stocktake();
    assert!(report.get("unavailable").is_none(), "{}", report);
    assert_eq!(changes(&report["staged"]), [
        ("added".to_string(), "extra/new.bin/inner.txt".to_string()),
        ("removed".to_string(), "extra/new.bin".to_string()),
        ("removed".to_string(), "film.mkv".to_string()),
    ], "{}", report);
}

/// A soft reset keeps an entry not checked out staged while the head moves back past the parcel
/// that named it, so the next `stack` stages content this warehouse lacks: the record holds it,
/// and the stack names it at the same hash rather than refusing.
///
/// Revert: leave the record out of the staged-content check → `stack` refuses → red.
#[test]
fn a_soft_reset_restacks_a_file_not_checked_out() {
    let (warehouse, feature) = withheld_feature("undo");

    warehouse.run_ok(&["undo"]);
    assert!(!warehouse.exists("film.mkv"), "undoing the shift back to main leaves the file out again");
    warehouse.run_ok(&["undo"]);
    assert_ne!(warehouse.head(), feature, "the stack that named the withheld content is undone");
    assert!(!warehouse.exists("film.mkv") && !warehouse.exists("extra/new.bin"));

    warehouse.run_ok(&["stack", "-m", "again"]);
    let head = warehouse.head();
    warehouse.scoped(|| {
        for path in ["film.mkv", "extra/new.bin"] {
            assert_eq!(file_hash(&head, path), file_hash(&feature, path), "{} is named as before", path);
        }
    });
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), ["extra/new.bin", "film.mkv"]);
}

/// A file put at a path not checked out is a change before any load: a shift away refuses rather
/// than remove the only copy of it, even when its bytes are the withheld content. A file standing
/// where a directory of entries not checked out was stages their removal.
///
/// Stocktake shows each removal before the load stages it, and an ignored directory at the path is
/// as much in the way as a walked one.
///
/// Revert: compare the file by its entry's hash → the shift takes it for the head's content and
/// overwrites it → red; take a file at the directory's path for no obstacle, in the load or in
/// stocktake → no removal is shown or staged → red; count only walked directories → the ignored
/// one stages nothing → red.
#[test]
fn a_file_put_back_is_a_change_and_a_file_over_a_directory_ends_its_entries() {
    let (warehouse, _) = withheld_feature("put-back-unloaded");
    warehouse.run_ok(&["shift", "feature"]);

    warehouse.write("film.mkv", WITHHELD_FILM);
    let refused = warehouse.run(&["shift", "main"]);
    assert!(!refused.status.success(), "the shift must refuse over the put-back file");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), WITHHELD_FILM);

    let (warehouse, _) = withheld_feature("file-over-dir");
    warehouse.run_ok(&["shift", "feature"]);
    warehouse.write("extra", "a file now\n");
    let report = warehouse.stocktake();
    assert!(changes(&report["unstaged"]).contains(&("removed".to_string(), "extra/new.bin".to_string())),
        "stocktake shows the removal the load will stage: {}", report);
    warehouse.run_ok(&["load", "."]);
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["film.mkv"], "{}", report);
    assert_eq!(changes(&report["staged"]), [
        ("added".to_string(), "extra".to_string()),
        ("removed".to_string(), "extra/new.bin".to_string()),
    ], "{}", report);

    let (warehouse, _) = withheld_feature("ignored-dir");
    warehouse.run_ok(&["shift", "feature"]);
    warehouse.write(".forkliftignore", "film.mkv/\n");
    warehouse.write("film.mkv/build.log", "ignored\n");
    let report = warehouse.stocktake();
    assert!(changes(&report["unstaged"]).contains(&("removed".to_string(), "film.mkv".to_string())), "{}", report);
    warehouse.run_ok(&["load", "."]);
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin"], "{}", report);
    assert_eq!(changes(&report["staged"]), [
        ("added".to_string(), ".forkliftignore".to_string()),
        ("removed".to_string(), "film.mkv".to_string()),
    ], "{}", report);
}

/// A directory restore writes a file not checked out once the store holds its content again, and
/// leaves out only what is still withheld.
///
/// Revert: skip every `Unavailable` entry on its state alone → the film is never written and the
/// restore says it left out two → red.
#[test]
fn a_restore_writes_a_file_whose_content_came_back() {
    let (warehouse, _) = withheld_feature("came-back");
    warehouse.run_ok(&["shift", "feature"]);

    warehouse.write("copy.txt", WITHHELD_FILM);
    warehouse.run_ok(&["load", "copy.txt"]);
    warehouse.run_ok(&["unload", "copy.txt"]);
    file_utils::remove_file(warehouse.root.join("copy.txt")).unwrap();

    let restored = warehouse.run_ok(&["restore", ".", "--confirm"]);
    let restored = String::from_utf8_lossy(&restored.stdout);
    assert!(restored.contains("Left out 1 file(s) not checked out"), "{}", restored);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), WITHHELD_FILM);
    assert!(!warehouse.exists("extra/new.bin"));
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), ["extra/new.bin"]);
}

/// A checkout that cannot read the unavailable record refuses before it touches the working
/// directory: which writes it leaves out is decided before the first removal.
///
/// Revert: read the record after the removals → `side.txt` is already gone when the shift fails
/// → red.
#[test]
fn an_unreadable_record_refuses_a_shift_before_any_removal() {
    let warehouse = Warehouse::new("unreadable-record");
    warehouse.stack_files(&[("a.txt", "a\n")], "main");
    warehouse.run_ok(&["pallet", "create", "side"]);
    warehouse.stack_files(&[("side.txt", "side\n")], "side");
    std::fs::write(warehouse.root.join(".forklift").join("unavailable"), "not a record\n").unwrap();

    let refused = warehouse.run(&["shift", "main"]);
    assert!(!refused.status.success(), "the shift must refuse over an unreadable record");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("side.txt")).unwrap(), "side\n");
}

/// `withheld_feature` with `main` moved on by a change to `a.txt`, so `feature` diverges from it.
fn diverged_feature(name: &str) -> (Warehouse, String) {
    let (warehouse, feature) = withheld_feature(name);
    warehouse.stack_files(&[("a.txt", "a on main\n")], "main moves on");

    (warehouse, feature)
}

/// After a merge of `feature` into `main` that took both its files: neither is written, the old
/// film is gone rather than left under the new hash, both are staged `Unavailable`, and the bay
/// is clean at a head naming their hashes.
fn assert_merged_without_the_withheld_files(warehouse: &Warehouse, feature: &str) {
    assert!(!warehouse.exists("film.mkv"), "the changed file's old bytes are removed, not left under the new hash");
    assert!(!warehouse.exists("extra/new.bin"), "the new file is not written");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(), "a on main\n");

    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin", "film.mkv"], "{}", report);
    assert_eq!((&report["staged_count"], &report["unstaged_count"]), (&0.into(), &0.into()), "{}", report);

    let head = warehouse.head();
    warehouse.scoped(|| for path in ["film.mkv", "extra/new.bin"] {
        assert_eq!(file_hash(&head, path), file_hash(feature, path), "the merged head names their {}", path);
    });
}

/// A diverged consolidate and a pick take a file whose content is left out without reading it,
/// as a checkout does.
///
/// Revert: let the merge's take write such a file (no skip) → it dies reading the absent blob →
/// red; skip it but keep the old film → it stands under the new hash → red; stage it `Normal` →
/// stocktake reports it removed → red.
#[test]
fn a_merge_and_a_pick_leave_their_unavailable_files_out() {
    let (warehouse, feature) = diverged_feature("merge");
    warehouse.run_ok(&["consolidate", "feature"]);
    assert_merged_without_the_withheld_files(&warehouse, &feature);

    let (warehouse, feature) = diverged_feature("pick");
    warehouse.run_ok(&["pick", &feature]);
    assert_merged_without_the_withheld_files(&warehouse, &feature);
}

/// A merge or pick whose line merge needs content left out refuses `content_withheld` before it
/// changes anything, naming the file and the host's statement, and the way out: taking this
/// pallet's own change to the file out, after which the merge leaves the file out when their
/// version is withheld too, and takes their version when only ours is.
///
/// Revert: drop the refusal → the merge dies reading the absent blob, refused by the read with
/// no path → red.
#[test]
fn a_merge_that_must_read_withheld_content_refuses_content_withheld() {
    let (warehouse, feature) = withheld_feature("refuse");
    warehouse.stack_files(&[("film.mkv", "the film on main\n")], "main changes the film");

    for args in [vec!["consolidate", "feature"], vec!["pick", feature.as_str()]] {
        let output = warehouse.run(&[args.as_slice(), &["--json"]].concat());
        assert_eq!(output.status.code(), Some(51), "{}", String::from_utf8_lossy(&output.stderr));
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["error"]["code"], "content_withheld", "{}", report);
        assert_eq!(report["error"]["details"]["path"], "film.mkv", "{}", report);
        assert_eq!(report["error"]["details"]["statement"], "Withheld by the remote's host.", "{}", report);
        let next_step = report["error"]["next_step"].as_str().unwrap_or_default();
        assert!(next_step.contains("this pallet's own change to it taken out") && next_step.contains("the merge leaves the file out"), "{}", report);

        assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), "the film on main\n");
        let report = warehouse.stocktake();
        assert_eq!((&report["staged_count"], &report["unstaged_count"]), (&0.into(), &0.into()), "{}", report);
        assert!(!warehouse.run(&["abort"]).status.success(), "nothing is in progress to abort");
    }

    // The way out it names: with main's own change to the film taken out, the merge leaves it out.
    warehouse.stack_files(&[("film.mkv", OLD_FILM)], "main takes its change out");
    warehouse.run_ok(&["consolidate", "feature"]);
    assert!(!warehouse.exists("film.mkv"), "the merge left their film out");
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), ["extra/new.bin", "film.mkv"]);

    // Our own version withheld and theirs held: taking our change out takes their version.
    let (warehouse, _) = withheld_feature("refuse-ours");
    warehouse.stack_files(&[("film.mkv", "the film on main\n")], "main changes the film");
    warehouse.run_ok(&["shift", "feature"]);
    let output = warehouse.run(&["consolidate", "main", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!((output.status.code(), &report["error"]["code"]), (Some(51), &"content_withheld".into()), "{}", report);
    let next_step = report["error"]["next_step"].as_str().unwrap_or_default();
    assert!(next_step.contains("the merge takes the other side's version"), "{}", report);
    warehouse.stack_files(&[("film.mkv", OLD_FILM)], "feature takes its change out");
    warehouse.run_ok(&["consolidate", "main"]);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), "the film on main\n");
}

/// A conflict over content left out refuses too: putting their withheld version back where we
/// deleted the file (the way out named, putting the file back, then leaves it out), and keeping
/// our withheld version where they deleted it (the way out named, taking our change out, then
/// takes their deletion).
///
/// Revert: drop the put-back arm, or the conflict arm, of the refusal → the first merges by
/// reading the absent blob and is refused by the read with no path, the second stages a conflict
/// at a path holding no file → red.
#[test]
fn a_conflict_over_withheld_content_refuses_content_withheld() {
    let refused = |warehouse: &Warehouse, args: &[&str]| -> String {
        let output = warehouse.run(&[args, &["--json"]].concat());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!((output.status.code(), &report["error"]["code"]), (Some(51), &"content_withheld".into()), "{}", report);
        assert_eq!(report["error"]["details"]["path"], "film.mkv", "{}", report);
        report["error"]["next_step"].as_str().unwrap_or_default().to_string()
    };

    let (warehouse, _) = withheld_feature("refuse-put-back");
    warehouse.run_ok(&["remove", "film.mkv"]);
    warehouse.run_ok(&["stack", "-m", "main deletes the film"]);
    let before = warehouse.stocktake();
    let next_step = refused(&warehouse, &["consolidate", "feature"]);
    assert!(next_step.contains("this pallet's deletion of it taken out") && next_step.contains("the merge leaves the file out"), "{}", next_step);
    assert_eq!(warehouse.stocktake(), before, "the merge changed nothing");
    // The way out it names: with the film put back as it was, the merge leaves their film out.
    warehouse.stack_files(&[("film.mkv", OLD_FILM)], "main puts the film back");
    warehouse.run_ok(&["consolidate", "feature"]);
    assert!(!warehouse.exists("film.mkv"), "the merge left their film out");

    let (warehouse, _) = withheld_feature("refuse-keep-ours");
    warehouse.run_ok(&["remove", "film.mkv"]);
    warehouse.run_ok(&["stack", "-m", "main deletes the film"]);
    file_utils::remove_file(warehouse.root.join("film.mkv")).unwrap();
    warehouse.run_ok(&["shift", "feature"]);
    let next_step = refused(&warehouse, &["consolidate", "main"]);
    assert!(next_step.contains("the merge takes the other side's deletion of it"), "{}", next_step);
    assert_eq!(warehouse.stocktake()["unavailable_count"], 2, "the merge changed nothing");
    // The way out it names: with our change taken out (the film as it was), their deletion goes through.
    warehouse.stack_files(&[("film.mkv", OLD_FILM)], "feature takes its change out");
    warehouse.run_ok(&["consolidate", "main"]);
    assert!(!warehouse.exists("film.mkv"), "the merge took their deletion");
    assert_eq!(paths(&warehouse.stocktake()["unavailable"]), ["extra/new.bin"]);
}

/// A merge whose base version is left out cannot take one's own change out (that needs the file
/// as it was before), so it names the way out that reads nothing: making this pallet's file the
/// same as the other side's version, which this warehouse holds. Following it, the merge goes
/// through.
///
/// Revert: drop the base check in `withheld_way_out` → the next step names taking the change out
/// → red.
#[test]
fn a_merge_whose_base_is_withheld_names_matching_theirs() {
    let warehouse = Warehouse::new("refuse-base");
    let base = warehouse.stack_files(&[("a.txt", "a\n"), ("film.mkv", OLD_FILM)], "main");
    warehouse.run_ok(&["pallet", "create", "feature"]);
    warehouse.stack_files(&[("film.mkv", "the film on feature\n")], "feature");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.stack_files(&[("film.mkv", "the film on main\n")], "main changes the film");

    warehouse.scoped(|| {
        let withheld = file_hash(&base, "film.mkv");
        unavailable::record([(withheld.clone(), "Withheld by the remote's host.".to_string())]).unwrap();
        warehouse.delete_object(&withheld);
    });

    let output = warehouse.run(&["consolidate", "feature", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!((output.status.code(), &report["error"]["code"]), (Some(51), &"content_withheld".into()), "{}", report);
    let next_step = report["error"]["next_step"].as_str().unwrap_or_default();
    assert!(next_step.contains("made the same as the other side's version"), "{}", report);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), "the film on main\n");

    // The way out it names: main's film made the same as feature's, and the merge goes through.
    let theirs = warehouse.run_ok(&["show", "feature:film.mkv"]);
    warehouse.stack_files(&[("film.mkv", &String::from_utf8_lossy(&theirs.stdout))], "main takes feature's film");
    warehouse.run_ok(&["consolidate", "feature"]);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), "the film on feature\n");
}

/// `consolidate feature` into `main`, stopped at a conflict over `a.txt` (both sides changed it),
/// having left their `film.mkv` and `extra/new.bin` out. Returns the warehouse and `feature`'s head.
fn merge_stopped_at_a_conflict(name: &str) -> (Warehouse, String) {
    let (warehouse, _) = withheld_feature(name);
    warehouse.run_ok(&["shift", "feature"]);
    let feature = warehouse.stack_files(&[("a.txt", "a on feature\n")], "feature changes a");
    warehouse.run_ok(&["shift", "main"]);
    warehouse.stack_files(&[("a.txt", "a on main\n")], "main changes a");

    let output = warehouse.run_ok(&["consolidate", "feature"]);
    assert!(String::from_utf8_lossy(&output.stdout).contains("conflict: a.txt"), "the merge stops at the conflict over a.txt");
    assert!(!warehouse.exists("film.mkv"), "the merge left their film out");

    (warehouse, feature)
}

/// After an abort of [`merge_stopped_at_a_conflict`]: our files are back, and nothing is staged.
fn assert_aborted_to_main(warehouse: &Warehouse) {
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), OLD_FILM);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(), "a on main\n");
    assert!(!warehouse.exists("extra/new.bin"));

    let report = warehouse.stocktake();
    assert!(report.get("unavailable_count").is_none(), "{}", report);
    assert_eq!((&report["staged_count"], &report["unstaged_count"]), (&0.into(), &0.into()), "{}", report);
}

/// Aborting a merge that stopped at a conflict puts back our version of a file it left out,
/// without counting the path as a change to discard: no file standing there is what it wrote. It
/// judges that from the staged entry, not the store: their film's content coming back since (a
/// load of the same bytes elsewhere) changes nothing.
///
/// Revert: judge the working file by the recomputed write alone (ignore the `Unavailable` entry) →
/// the second abort lists `film.mkv` as a discard and refuses without --confirm → red.
#[test]
fn an_abort_puts_back_a_file_the_merge_left_out() {
    let (warehouse, _) = merge_stopped_at_a_conflict("abort");
    warehouse.run_ok(&["abort"]);
    assert_aborted_to_main(&warehouse);

    let (warehouse, _) = merge_stopped_at_a_conflict("abort-came-back");
    warehouse.write("copy/film.mkv", WITHHELD_FILM);
    warehouse.run_ok(&["load", "copy"]);
    warehouse.run_ok(&["abort"]);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("film.mkv")).unwrap(), OLD_FILM);
    assert!(!warehouse.exists("extra/new.bin"));
    let report = warehouse.stocktake();
    assert!(report.get("unavailable_count").is_none(), "{}", report);
    assert_eq!(changes(&report["staged"]), [("added".to_string(), "copy/film.mkv".to_string())], "the load is carried: {}", report);
    assert_eq!(report["unstaged_count"], 0, "{}", report);

    let (warehouse, _) = merge_stopped_at_a_conflict("abort-ours-back");
    warehouse.write("film.mkv", OLD_FILM);
    warehouse.run_ok(&["abort"]);
    assert_aborted_to_main(&warehouse);
}

/// A merge's file whose output needs content left out since the merge ran (their `a.txt`,
/// line-merged into the conflict) cannot be told from a change made since: the abort does not
/// refuse `content_withheld` over it, but counts it as a discard, and `--confirm` puts ours back.
///
/// Revert: let the abort's recompute refuse as a merge does → the abort fails `content_withheld`
/// → red.
#[test]
fn an_abort_over_content_withheld_since_the_merge_asks_to_confirm() {
    let (warehouse, feature) = merge_stopped_at_a_conflict("abort-withheld-since");
    warehouse.scoped(|| {
        let their_a = file_hash(&feature, "a.txt");
        unavailable::record([(their_a.clone(), "Withheld by the remote's host.".to_string())]).unwrap();
        warehouse.delete_object(&their_a);
    });

    let output = warehouse.run(&["abort", "--json"]);
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["error"]["code"], "confirmation_required", "{}", report);
    assert_eq!(report["error"]["details"]["discards"], serde_json::json!(["a.txt"]), "{}", report);

    warehouse.run_ok(&["abort", "--confirm"]);
    assert_aborted_to_main(&warehouse);
}

/// A conflicted merge that left files out, resolved by a load and finished by a stack, names their
/// files at their hashes, and stocktake still lists them not checked out.
///
/// Revert: stage the left-out take `Deleted` (a removal) → the stacked head lacks the files → red.
#[test]
fn a_resolved_merge_keeps_the_files_it_left_out() {
    let (warehouse, feature) = merge_stopped_at_a_conflict("resolve");
    warehouse.stack_files(&[("a.txt", "a resolved\n")], "merge feature");

    let head = warehouse.head();
    warehouse.scoped(|| for path in ["film.mkv", "extra/new.bin"] {
        assert_eq!(file_hash(&head, path), file_hash(&feature, path), "the merged head names their {}", path);
    });
    assert!(!warehouse.exists("film.mkv") && !warehouse.exists("extra/new.bin"));
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin", "film.mkv"], "{}", report);
    assert_eq!((&report["staged_count"], &report["unstaged_count"]), (&0.into(), &0.into()), "{}", report);
}

/// A read that needs the bytes of a file not checked out refuses `content_withheld`, carrying the
/// hash and the host's statement, rather than reporting a missing object: `show`, `blame`, a `diff`
/// between revisions that prints its lines, and a `restore` of that file. A `diff --json` lists the
/// change without reading the bytes, and the same reads of content this warehouse holds succeed.
/// Loading the same bytes at the path checks the file out again, and the read then succeeds.
///
/// Revert: drop the read path's unavailable arm (`unavailable::absent_read_refusal`) → each read
/// fails with a raw missing-object error and no code → red.
#[test]
fn a_read_of_a_file_not_checked_out_refuses_content_withheld() {
    let (warehouse, feature) = withheld_feature("reads");
    let withheld = warehouse.scoped(|| file_hash(&feature, "film.mkv"));
    warehouse.run_ok(&["shift", "feature"]);

    let reads: [&[&str]; 3] = [
        &["show", "feature:film.mkv"],
        &["blame", "film.mkv"],
        &["restore", "film.mkv"],
    ];
    for args in reads {
        let output = warehouse.run(&[args, &["--json"]].concat());
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap_or_default();
        assert_eq!(output.status.code(), Some(51), "{:?}: {}{}", args, report, String::from_utf8_lossy(&output.stderr));
        assert_eq!(report["error"]["code"], "content_withheld", "{:?}: {}", args, report);
        assert_eq!(report["error"]["details"]["hash"], withheld.as_str(), "{:?}: {}", args, report);
        assert_eq!(report["error"]["details"]["statement"], "Withheld by the remote's host.", "{:?}: {}", args, report);
    }

    let diff = warehouse.run(&["diff", "main", "feature", "film.mkv"]);
    let stderr = String::from_utf8_lossy(&diff.stderr);
    assert_eq!(diff.status.code(), Some(51), "{}{}", String::from_utf8_lossy(&diff.stdout), stderr);
    assert!(stderr.contains("Withheld by the remote's host."), "{}", stderr);
    let listed = warehouse.run_ok(&["diff", "main", "feature", "--json"]);
    assert!(String::from_utf8_lossy(&listed.stdout).contains("film.mkv"));

    let shown = warehouse.run_ok(&["show", "main:film.mkv"]);
    assert_eq!(String::from_utf8_lossy(&shown.stdout), OLD_FILM);
    warehouse.run_ok(&["blame", "a.txt"]);

    // The way out the guide names: the same bytes, got some other way, put at the path and loaded.
    std::fs::write(warehouse.root.join("film.mkv"), WITHHELD_FILM).unwrap();
    warehouse.run_ok(&["load", "film.mkv"]);
    let report = warehouse.stocktake();
    assert_eq!(paths(&report["unavailable"]), ["extra/new.bin"], "the loaded file is checked out again: {}", report);
    assert_eq!((&report["staged_count"], &report["unstaged_count"]), (&0.into(), &0.into()), "{}", report);
    let shown = warehouse.run_ok(&["show", "feature:film.mkv"]);
    assert_eq!(String::from_utf8_lossy(&shown.stdout), WITHHELD_FILM);
}
