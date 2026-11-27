//! A damaged record of the merge in progress — the consolidation state, its out-of-scope skeleton
//! or the pick state — is written back by `heal` from the copy every write of it leaves beside it,
//! once that copy reads and agrees with the bay. Without a usable copy, the record's way out is
//! named instead: `abort --keep-files` for a state, `abort` for the skeleton. Under an interrupted
//! merge or pick, whose recovery clears the records whatever its outcome, heal removes a record no
//! copy can restore rather than refuse; under any other interrupted operation it refuses only once
//! that operation's recovery has lowered its marker.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;

const MERGE_STATE_DAMAGED_EXIT: i32 = 68;

struct Area {
    base: PathBuf,
    home: PathBuf,
    root: PathBuf,
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&self.base);
    }
}

impl Area {
    /// A prepared warehouse at `wh` with `f.txt` changed on both "main" and "other" since their
    /// common parcel, standing on "main".
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-merge-state-heal-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("wh")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let area = Area { home: base.join("home"), root: base.join("wh"), base };

        area.ok(&["prepare"]);
        area.ok(&["config", "set", "operator.identifier", "merge-state-heal@forklift"]);
        area.stack("f.txt", "base");
        area.ok(&["pallet", "create", "other"]);
        area.ok(&["shift", "other"]);
        area.stack("f.txt", "theirs");
        area.ok(&["shift", "main"]);
        area.stack("f.txt", "ours");
        area
    }

    /// [`Area::new`] with "other" consolidated into "main", conflicted on `f.txt`.
    fn conflicted(name: &str) -> Area {
        let area = Area::new(name);
        let merge = area.ok(&["consolidate", "other"]);
        assert!(text(&merge.stdout).contains("conflict: f.txt"), "{}", text(&merge.stdout));
        area
    }

    /// [`Area::new`] with "other"'s head picked onto "main", conflicted on `f.txt`.
    fn picking(name: &str) -> Area {
        let area = Area::new(name);
        let source = area.head("other");
        let pick = area.run(&["pick", &source]);
        assert!(pick.status.success(), "{}{}", text(&pick.stdout), text(&pick.stderr));
        assert!(area.state("cherry-pick").exists());
        area
    }

    fn command(&self, args: &[&str]) -> std::process::Command {
        let mut command = forklift_command();
        command.args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(), "{:?} failed: {}{}", args, text(&output.stdout), text(&output.stderr));
        output
    }

    fn stack(&self, file: &str, message: &str) {
        std::fs::write(self.root.join(file), format!("{}\n", message)).unwrap();
        self.ok(&["load", "."]);
        self.ok(&["stack", "-m", message]);
    }

    /// A file of the warehouse's own working tree's bay-local state.
    fn state(&self, name: &str) -> PathBuf {
        self.root.join(".forklift").join(name)
    }

    fn head(&self, pallet: &str) -> String {
        std::fs::read_to_string(self.state("pallets").join(pallet)).unwrap().trim_end().to_string()
    }

    /// `heal --json`, asserting it succeeded: its data.
    fn healed(&self) -> serde_json::Value {
        parsed(&self.ok(&["--json", "heal"]))["data"].clone()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parsed(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({}): {}{}", e, text(&output.stdout), text(&output.stderr)))
}

/// The `--json` refusal of `output`, asserting it is `merge_state_damaged` with exit 68: its error.
fn merge_state_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(MERGE_STATE_DAMAGED_EXIT), "expected merge_state_damaged: {}{}",
        text(&output.stdout), text(&output.stderr));
    let value = parsed(output);
    assert_eq!(value["error"]["code"], "merge_state_damaged", "{}", value);
    value["error"].clone()
}

/// Flip a byte of the file at `path` under its digest line, so it still looks like a record.
fn flip(path: &Path) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = bytes.len() / 3;
    bytes[at] ^= 0x01;
    std::fs::write(path, bytes).unwrap();
}

/// The records of a merge in progress and their copies, by file name.
const MERGE_FILES: [&str; 6] = [
    "consolidation", "consolidation-copy", "consolidation-skeleton", "consolidation-skeleton-copy", "cherry-pick", "cherry-pick-copy",
];

fn names(restored: &serde_json::Value) -> Vec<(String, bool)> {
    restored["records"].as_array().unwrap_or_else(|| panic!("no records: {}", restored)).iter()
        .map(|record| (record["record"].as_str().unwrap().to_string(), record["removed"].as_bool().unwrap()))
        .collect()
}

/// T1. Each record, damaged with no marker standing, is written back byte for byte from its copy
/// and reported, and the merge it records then completes: the consolidation's stack records both
/// parents.
///
/// Revert: drop the copy write from the record primitive → no copy to restore from, heal exits 68.
#[test]
fn heal_writes_a_damaged_merge_record_back_from_its_copy() {
    for (file, record) in [("consolidation", "consolidation state"), ("consolidation-skeleton", "consolidation skeleton")] {
        let area = Area::conflicted(&format!("restore-{}", file));
        let original = std::fs::read(area.state(file)).unwrap();
        assert_eq!(std::fs::read(area.state(&format!("{}-copy", file))).unwrap(), original, "{}: the copy holds the record's bytes", file);

        flip(&area.state(file));
        let data = area.healed();
        assert_eq!(names(&data["merge_state_restored"]), vec![(record.to_string(), false)], "{}: {}", file, data);
        assert_eq!(std::fs::read(area.state(file)).unwrap(), original, "{}: the record is back exactly", file);

        std::fs::write(area.root.join("f.txt"), "resolved\n").unwrap();
        area.ok(&["load", "f.txt"]);
        let stacked = parsed(&area.ok(&["--json", "stack", "-m", "merged"]));
        let merge = stacked["data"]["parcel"].as_str().unwrap_or_else(|| panic!("{}", stacked)).to_string();
        let peek = text(&area.ok(&["store", "peek", &merge]).stdout);
        for pallet in ["other"] {
            assert!(peek.contains(&format!("parent {}", area.head(pallet))), "{}: the merge records {}: {}", file, pallet, peek);
        }
        assert_eq!(peek.matches("parent ").count(), 2, "{}: {}", file, peek);
        for name in MERGE_FILES {
            assert!(!area.state(name).exists(), "{}: the stack consumed {}", file, name);
        }
    }

    let area = Area::picking("restore-pick");
    let original = std::fs::read(area.state("cherry-pick")).unwrap();
    assert_eq!(std::fs::read(area.state("cherry-pick-copy")).unwrap(), original, "the copy holds the pick state's bytes");
    flip(&area.state("cherry-pick"));
    let data = area.healed();
    assert_eq!(names(&data["merge_state_restored"]), vec![("pick state".to_string(), false)], "{}", data);
    assert_eq!(std::fs::read(area.state("cherry-pick")).unwrap(), original);
    area.ok(&["abort"]);

    // A folder in the record's place goes first, or the write back would fail on every heal.
    let area = Area::conflicted("restore-over-folder");
    let original = std::fs::read(area.state("consolidation")).unwrap();
    forklift_core::util::file_utils::remove_file(area.state("consolidation")).unwrap();
    std::fs::create_dir(area.state("consolidation")).unwrap();
    let data = area.healed();
    assert_eq!(names(&data["merge_state_restored"]), vec![("consolidation state".to_string(), false)], "{}", data);
    assert_eq!(std::fs::read(area.state("consolidation")).unwrap(), original, "the folder is replaced by the record");
}

/// A bay's merge records are its own: a conflicted consolidate and a conflicted pick in a bay write
/// their records and copies, and the journal its entries, in the bay's state, never in the
/// warehouse's own working tree's. Heal in the bay rewrites a damaged copy and restores a damaged
/// record there.
///
/// Revert: name the warehouse's own working tree in place of the checkout in scope at a record's
/// writer, the journal's or heal's → the bay's file is missing or left damaged, red. Re-sync a copy
/// into its record's place → the damaged copy stays, red.
#[test]
fn a_bays_merge_records_are_written_and_healed_in_the_bay() {
    let area = Area::new("bay");
    let bay = area.base.join("b");
    area.ok(&["bay", "add", "b", bay.to_str().unwrap()]);
    let in_bay = |args: &[&str]| -> Output {
        let output = area.command(args).current_dir(&bay).output().unwrap();
        assert!(output.status.success(), "{:?} in the bay failed: {}{}", args, text(&output.stdout), text(&output.stderr));
        output
    };
    let bay_state = |name: &str| area.state("bays").join("b").join(name);
    let own_journal = std::fs::read(area.state("journal.json")).ok();

    let merge = in_bay(&["consolidate", "other"]);
    assert!(text(&merge.stdout).contains("conflict: f.txt"), "{}", text(&merge.stdout));
    for name in ["consolidation", "consolidation-copy", "consolidation-skeleton", "consolidation-skeleton-copy", "journal.json"] {
        assert!(bay_state(name).is_file(), "the bay's {} is in the bay", name);
    }
    assert_eq!(std::fs::read(area.state("journal.json")).ok(), own_journal, "the bay's journal entry is not the working tree's");

    for (file, record) in [("consolidation", "consolidation state"), ("consolidation-skeleton", "consolidation skeleton")] {
        let original = std::fs::read(bay_state(file)).unwrap();
        let copy = format!("{}-copy", file);
        flip(&bay_state(&copy));
        in_bay(&["heal"]);
        assert_eq!(std::fs::read(bay_state(&copy)).unwrap(), original, "{}: heal rewrote the bay's copy", file);

        flip(&bay_state(file));
        let healed = parsed(&in_bay(&["--json", "heal"]))["data"].clone();
        assert_eq!(names(&healed["merge_state_restored"]), vec![(record.to_string(), false)], "{}: {}", file, healed);
        assert_eq!(std::fs::read(bay_state(file)).unwrap(), original, "{}: heal restored the bay's record", file);
    }

    in_bay(&["abort"]);

    let source = area.head("other");
    let bay_journal = std::fs::read(bay_state("journal.json")).unwrap();
    in_bay(&["pick", &source]);
    for name in ["cherry-pick", "cherry-pick-copy"] {
        assert!(bay_state(name).is_file(), "the bay's {} is in the bay", name);
    }
    assert_ne!(std::fs::read(bay_state("journal.json")).unwrap(), bay_journal, "the pick's journal entry is in the bay");

    let original = std::fs::read(bay_state("cherry-pick")).unwrap();
    flip(&bay_state("cherry-pick-copy"));
    in_bay(&["heal"]);
    assert_eq!(std::fs::read(bay_state("cherry-pick-copy")).unwrap(), original, "heal rewrote the bay's copy of the pick state");
    flip(&bay_state("cherry-pick"));
    let healed = parsed(&in_bay(&["--json", "heal"]))["data"].clone();
    assert_eq!(names(&healed["merge_state_restored"]), vec![("pick state".to_string(), false)], "{}", healed);
    assert_eq!(std::fs::read(bay_state("cherry-pick")).unwrap(), original, "heal restored the bay's pick state");
    in_bay(&["abort"]);

    assert_eq!(std::fs::read(area.state("journal.json")).ok(), own_journal, "no journal entry of the bay's is the working tree's");
    for name in MERGE_FILES {
        assert!(!area.state(name).exists(), "the working tree holds no {} of the bay's", name);
        assert!(!bay_state(name).exists(), "the bay's {} went with its merge", name);
    }
}

/// A heal stopped at any rename while it writes a record back leaves that record present, damaged
/// or restored, never absent: an absent state reads as no merge at all.
///
/// Revert: remove the damaged record before its write back → a stop at the write's rename leaves
/// no record, red.
#[test]
fn a_heal_stopped_while_writing_a_record_back_never_leaves_it_absent() {
    use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

    for at in 1..200 {
        let area = Area::conflicted(&format!("restore-stopped-{}", at));
        flip(&area.state("consolidation"));
        let stopped = area.command(&["heal"]).env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string()).output().unwrap();
        assert!(area.state("consolidation").exists(), "a heal stopped at rename {} left no consolidation state", at);
        if stopped.status.code() != Some(CRASH_POINT_EXIT_CODE) {
            return;
        }
    }
    panic!("heal never ran past every rename");
}

/// T2. A reader of a damaged record names `forklift heal` exactly when heal can restore it: with a
/// usable copy beside it. With the copy damaged too, it names the record's way out, which then
/// runs: heal refuses with it only after its other repairs, so a damaged inventory, which that way
/// out reads, is rebuilt first.
///
/// Revert: make the shared predicate answer that heal can run whatever the copy holds → the second
/// half names `forklift heal`, red. Refuse before the inventory's rebuild → the abort meets the
/// damaged inventory, red.
#[test]
fn a_damaged_record_names_heal_only_while_its_copy_can_be_used() {
    for (file, way_out) in [("consolidation", "forklift abort --keep-files"), ("consolidation-skeleton", "forklift abort")] {
        let area = Area::conflicted(&format!("next-step-{}", file));
        flip(&area.state(file));

        let error = merge_state_damaged(&area.run(&["--json", "undo"]));
        assert_eq!(error["next_step"], "forklift heal", "{}: {}", file, error);

        flip(&area.state(&format!("{}-copy", file)));
        let error = merge_state_damaged(&area.run(&["--json", "undo"]));
        assert_eq!(error["next_step"], way_out, "{}: {}", file, error);

        let inventory = area.state("inventory/metadata");
        let sound = std::fs::read(&inventory).unwrap();
        std::fs::write(&inventory, vec![0u8; sound.len()]).unwrap();
        let refused = area.run(&["--json", "heal"]);
        let error = merge_state_damaged(&refused);
        assert_eq!(error["next_step"], way_out, "{}: heal names the same way out: {}", file, error);
        assert!(!error["message"].as_str().unwrap().contains("Nothing was changed"), "{}: {}", file, error);
        assert_eq!(std::fs::read(&inventory).unwrap(), sound, "{}: the inventory was rebuilt before the refusal", file);

        let args: Vec<&str> = way_out.split(' ').skip(1).collect();
        area.ok(&args);
        for name in MERGE_FILES {
            assert!(!area.state(name).exists(), "{}: {} is gone with the merge", file, name);
        }
    }
}

/// T3. A skeleton that reads but is not the one written with the consolidation in progress is
/// refused as damage, and heal writes back the copy whose bytes match the digest the state records.
///
/// Revert: drop the digest comparison → undo reads the wrong skeleton without refusing, red.
#[test]
fn a_skeleton_of_another_merge_is_damage_and_heal_restores_this_ones() {
    let area = Area::conflicted("wrong-skeleton");
    let original = std::fs::read(area.state("consolidation-skeleton")).unwrap();
    let other = forklift_core::util::record_utils::to_bytes("consolidation-skeleton", 2, ["D src/elsewhere"]);
    assert_ne!(other, original);
    std::fs::write(area.state("consolidation-skeleton"), &other).unwrap();

    let error = merge_state_damaged(&area.run(&["--json", "undo"]));
    assert_eq!(error["next_step"], "forklift heal", "{}", error);

    let data = area.healed();
    assert_eq!(names(&data["merge_state_restored"]), vec![("consolidation skeleton".to_string(), false)], "{}", data);
    assert_eq!(std::fs::read(area.state("consolidation-skeleton")).unwrap(), original);
}

/// T4. A copy with no record beside it is how a clear interrupted between the two leaves the bay,
/// never a merge in progress: heal writes no record from it, and nothing reads a merge.
///
/// Revert: let heal write an absent state back from its copy → a consolidation reappears, red.
#[test]
fn a_copy_without_its_record_is_never_written_back() {
    let area = Area::conflicted("lone-copy");
    let copies: Vec<(String, Vec<u8>)> = ["consolidation-copy", "consolidation-skeleton-copy"].iter()
        .map(|name| (name.to_string(), std::fs::read(area.state(name)).unwrap())).collect();
    area.ok(&["abort"]);
    for (name, bytes) in &copies {
        std::fs::write(area.state(name), bytes).unwrap();
    }

    let data = area.healed();
    assert!(data.get("merge_state_restored").is_none(), "{}", data);
    assert!(!area.state("consolidation").exists() && !area.state("consolidation-skeleton").exists(), "{}", data);

    let nothing = area.run(&["abort", "--keep-files"]);
    assert_eq!(nothing.status.code(), Some(1), "{}", text(&nothing.stderr));
    assert!(text(&nothing.stderr).contains("No consolidation or pick is in progress"), "{}", text(&nothing.stderr));
}

/// T5. A copy of a state that names a parcel other than the one the bay stands on is no copy of
/// this bay's merge: heal does not write it back, and names the abort instead.
///
/// Revert: skip the agreement check → heal writes the foreign copy back and exits 0, red.
#[test]
fn a_copy_that_disagrees_with_the_bay_is_not_written_back() {
    let area = Area::conflicted("disagreeing-copy");
    let state = forklift_core::util::merge_utils::read_consolidation_state_in(&area.root.join(".forklift")).unwrap().unwrap();
    let foreign = forklift_core::util::record_utils::to_bytes("consolidation", 4, [
        state.their_head.clone(), state.their_pallet.clone(), state.their_head.clone(), state.our_pallet.clone(), state.skeleton_digest.clone(),
    ]);
    std::fs::write(area.state("consolidation-copy"), foreign).unwrap();
    flip(&area.state("consolidation"));

    let error = merge_state_damaged(&area.run(&["--json", "heal"]));
    assert_eq!(error["next_step"], "forklift abort --keep-files", "{}", error);
    area.ok(&["abort", "--keep-files"]);
}

/// Crash `args` at each rename in turn until one stops it with its marker standing beside a
/// consolidation state; `prepare` sets the bay up before each attempt. A consolidate lowers its
/// marker with no rename between its state's write and the lowering, so a crash that stops the
/// state's own rename is completed by hand: the rename is atomic, and the bay is then exactly as a
/// stop right after it leaves it.
fn crashed_with_a_marker(name: &str, prepare: &dyn Fn(&str) -> Area, args: &[&str], kind: &str) -> Area {
    use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

    for at in 1..200 {
        let area = prepare(&format!("{}-{}", name, at));
        let stopped = area.command(args).env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string()).output().unwrap();
        if stopped.status.code() != Some(CRASH_POINT_EXIT_CODE) {
            break;
        }
        let pending = std::fs::read_dir(area.state("")).unwrap().map(|entry| entry.unwrap().path())
            .find(|path| path.file_name().unwrap().to_string_lossy().starts_with("consolidation.tmp~"));
        if let Some(pending) = pending {
            forklift_core::util::file_utils::rename(pending, area.state("consolidation")).unwrap();
        }
        let marker = std::fs::read_to_string(area.state("operation")).unwrap_or_default();
        if marker.contains(kind) && area.state("consolidation").exists() {
            return area;
        }
    }

    panic!("no rename stops {:?} with its {} marker beside a consolidation state", args, kind);
}

/// T6. Under an interrupted merge, whose recovery clears every merge record whatever its outcome,
/// heal never refuses a damaged record: it writes it back from a usable copy, and removes it with
/// its copy when there is none. The recovery then undoes the merge.
///
/// A skeleton missing with its copy is no record to remove, and is reported as nothing.
///
/// Revert: refuse under a merge marker as with none → heal exits 68, red; remove the record even
/// when its copy can be used → the report names it removed, red; report a missing skeleton as
/// removed → the third case names it, red.
#[test]
fn under_an_interrupted_merge_heal_restores_or_removes_never_refuses() {
    for case in ["both", "state", "no-skeleton"] {
        let area = crashed_with_a_marker(&format!("merge-marker-{}", case), &|name| Area::new(name), &["consolidate", "other"], "merge");
        let ours = area.head("main");
        let expected = match case {
            "no-skeleton" => {
                for name in ["consolidation-skeleton", "consolidation-skeleton-copy"] {
                    forklift_core::util::file_utils::remove_file(area.state(name)).unwrap();
                }
                Vec::new()
            }
            _ => {
                flip(&area.state("consolidation"));
                if case == "both" {
                    flip(&area.state("consolidation-copy"));
                }
                vec![("consolidation state".to_string(), case == "both")]
            }
        };

        let data = area.healed();
        match expected.is_empty() {
            true => assert!(data.get("merge_state_restored").is_none(), "{}: {}", case, data),
            false => assert_eq!(names(&data["merge_state_restored"]), expected, "{}: {}", case, data),
        }
        assert!(data["operation"].is_object(), "the recovery ran: {}", data);
        assert!(!area.state("operation").exists(), "{}", data);
        for name in MERGE_FILES {
            assert!(!area.state(name).exists(), "{}: {} is gone with the undone merge", case, name);
        }
        assert_eq!(area.head("main"), ours, "the merge is undone");
    }
}

/// T7. An undo that puts a merge back in progress writes the skeleton through the same primitive,
/// copy first, so heal restores the skeleton it put back.
///
/// Revert: write the undo's skeleton raw, as before → no copy beside it, heal exits 68, red.
#[test]
fn a_merge_an_undo_put_back_is_restored_from_its_copy() {
    let area = Area::conflicted("undo-put-back");
    std::fs::write(area.root.join("f.txt"), "resolved\n").unwrap();
    area.ok(&["load", "f.txt"]);
    area.ok(&["stack", "-m", "merged"]);
    assert!(!area.state("consolidation-skeleton-copy").exists(), "the stack consumed the copy");

    area.ok(&["undo"]);
    assert!(area.state("consolidation").exists(), "the undo put the merge back");
    let original = std::fs::read(area.state("consolidation-skeleton")).unwrap();
    flip(&area.state("consolidation-skeleton"));

    let data = area.healed();
    assert_eq!(names(&data["merge_state_restored"]), vec![("consolidation skeleton".to_string(), false)], "{}", data);
    assert_eq!(std::fs::read(area.state("consolidation-skeleton")).unwrap(), original);
}

/// T8. A merge is never started over one in progress: `haul consolidate` refuses with the
/// consolidation left byte for byte as it was. Every merge — consolidate, haul, lift, lower — goes
/// through the one primitive the guard sits in.
///
/// Revert: drop the mid-merge guard from the merge primitive → haul consolidate writes a second
/// consolidation over the first, red.
#[test]
fn haul_consolidate_refuses_over_a_merge_in_progress() {
    let area = Area::new("haul-mid-merge");
    area.ok(&["pallet", "create", "feature"]);
    area.stack("g.txt", "feature work");
    area.ok(&["shift", "main"]);
    area.ok(&["haul", "open", "--target", "main", "--source", "feature", "--title", "Add feature"]);
    let id = parsed(&area.ok(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"].as_str().unwrap().to_string();

    let merge = area.ok(&["consolidate", "other"]);
    assert!(text(&merge.stdout).contains("conflict: f.txt"), "{}", text(&merge.stdout));
    // Resolved to our side and loaded, the tree holds no change a merge would overwrite: only the
    // merge in progress stands in the way.
    std::fs::write(area.root.join("f.txt"), "ours\n").unwrap();
    area.ok(&["load", "f.txt"]);
    let state = std::fs::read(area.state("consolidation")).unwrap();

    let refused = area.run(&["haul", "consolidate", &id]);
    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    assert!(text(&refused.stderr).contains("A consolidation is in progress"), "{}", text(&refused.stderr));
    assert_eq!(std::fs::read(area.state("consolidation")).unwrap(), state, "the merge in progress is untouched");
}

fn git(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git must be installed to run this test");
    assert!(output.status.success(), "git {:?}: {}", args, text(&output.stderr));
}

/// T9. Under an interrupted operation of another kind, a record no copy can restore is left for its
/// recovery, and refused once that has lowered its marker: the way out named is one that then
/// runs.
///
/// Revert: refuse before the recovery → the marker still stands, so the abort it names is refused
/// too, red.
#[test]
fn under_another_interrupted_operation_heal_refuses_after_its_recovery() {
    let prepare = |name: &str| {
        let area = Area::conflicted(name);
        git(&area.root, &["init", "-q", "-b", "imported"]);
        std::fs::write(area.root.join("g.txt"), "from git\n").unwrap();
        git(&area.root, &["add", "g.txt"]);
        git(&area.root, &["commit", "-qm", "first commit"]);
        area
    };
    let area = crashed_with_a_marker("import-marker", &prepare, &["git", "import", "."], "import");
    flip(&area.state("consolidation"));
    flip(&area.state("consolidation-copy"));

    let error = merge_state_damaged(&area.run(&["--json", "heal"]));
    assert_eq!(error["next_step"], "forklift abort --keep-files", "{}", error);
    assert!(!area.state("operation").exists(), "the import was recovered first");

    area.ok(&["abort", "--keep-files"]);
    for name in MERGE_FILES {
        assert!(!area.state(name).exists(), "{} is gone with the abandoned merge", name);
    }
}
