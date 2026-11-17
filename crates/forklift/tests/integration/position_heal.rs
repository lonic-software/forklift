//! A lost or damaged bay position is refused as `bay_position_damaged` (exit 64) by every command
//! that reads it, and `heal` rebuilds it only from exact evidence: the copy every position write
//! leaves beside it, or a standing operation marker's source. Without either it refuses and lists
//! the pallets, and `heal --position <pallet>` puts the bay on that pallet's head. These tests
//! damage the position the ways Proof Load does (deleted, cut short, zeroed, a byte flipped), in
//! the warehouse's own working tree and in a bay.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;

const BAY_POSITION_DAMAGED_EXIT: i32 = 64;

struct Area {
    base: PathBuf,
    home: PathBuf,
}

impl Drop for Area {
    fn drop(&mut self) {
        let _ = forklift_core::util::file_utils::remove_dir_all(&self.base);
    }
}

impl Area {
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-position-heal-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let home = base.join("home");

        Area { base, home }
    }

    fn command(&self, dir: &Path, args: &[&str]) -> std::process::Command {
        let mut command = forklift_command();
        command.args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));
        command
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }

    fn run_ok(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.run(dir, args);
        assert!(output.status.success(), "{:?} in {:?} failed: {}{}", args, dir, text(&output.stdout), text(&output.stderr));
        output
    }

    /// A prepared warehouse with a parcel on `main`, and the working tree shifted onto pallet
    /// `feature` with a parcel of its own, so the position names neither the default pallet nor
    /// the newest journaled one... and `main` is behind nothing.
    fn warehouse(&self) -> PathBuf {
        let root = self.base.join("wh");
        std::fs::create_dir_all(&root).unwrap();
        self.run_ok(&root, &["prepare"]);
        self.run_ok(&root, &["config", "set", "operator.identifier", "position-heal@forklift"]);
        self.stack(&root, "a.txt", "one");
        self.run_ok(&root, &["pallet", "create", "feature"]);
        self.run_ok(&root, &["shift", "feature"]);
        self.stack(&root, "b.txt", "two");
        root
    }

    fn stack(&self, dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), format!("{}\n", message)).unwrap();
        self.run_ok(dir, &["load", "."]);
        self.run_ok(dir, &["stack", "-m", message]);
    }

    /// A bay at `name` beside the warehouse.
    fn bay(&self, root: &Path, name: &str) -> PathBuf {
        let dir = self.base.join(name);
        self.run_ok(root, &["bay", "add", name, dir.to_str().unwrap()]);
        dir
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parsed(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({}): {}{}", e, text(&output.stdout), text(&output.stderr)))
}

/// The folder holding the position of the working tree at `dir`: its own `.forklift` for the
/// warehouse's tree, the bay's state folder for a bay.
fn state_dir(root: &Path, bay: Option<&str>) -> PathBuf {
    match bay {
        None => root.join(".forklift"),
        Some(bay) => root.join(".forklift").join("bays").join(bay),
    }
}

fn head(root: &Path, pallet: &str) -> String {
    std::fs::read_to_string(root.join(".forklift").join("pallets").join(pallet)).unwrap().trim_end().to_string()
}

/// The `--json` refusal of `output`, asserting it is `bay_position_damaged` with exit 64.
fn position_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(BAY_POSITION_DAMAGED_EXIT), "expected bay_position_damaged: {}{}",
        text(&output.stdout), text(&output.stderr));
    let value = parsed(output);
    assert_eq!(value["error"]["code"], "bay_position_damaged", "{}", value);
    value
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Delete,
    Truncate,
    Zero,
    FlipByte,
}

const DAMAGES: [Damage; 4] = [Damage::Delete, Damage::Truncate, Damage::Zero, Damage::FlipByte];

fn damage(path: &Path, damage: Damage) {
    let bytes = std::fs::read(path).unwrap();

    match damage {
        Damage::Delete => forklift_core::util::file_utils::remove_file(path).unwrap(),
        Damage::Truncate => std::fs::write(path, &bytes[..bytes.len() / 2]).unwrap(),
        Damage::Zero => std::fs::write(path, vec![0u8; bytes.len()]).unwrap(),
        Damage::FlipByte => {
            let mut bytes = bytes;
            bytes[20] ^= 0x80;
            std::fs::write(path, bytes).unwrap()
        }
    }
}

/// The command that meets the damage refuses with `bay_position_damaged` and names `heal`; `heal`
/// rebuilds the position from its copy, byte for byte, and the refused command then succeeds.
fn heals_from_the_copy(area: &Area, root: &Path, dir: &Path, bay: Option<&str>, kind: Damage) {
    let position = state_dir(root, bay).join("position");
    let before = std::fs::read(&position).unwrap();
    damage(&position, kind);

    std::fs::write(dir.join("c.txt"), "three\n").unwrap();
    let refused = position_damaged(&area.run(dir, &["--json", "stack", "-m", "three"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{:?}: {}", kind, refused);

    let data = parsed(&area.run_ok(dir, &["--json", "heal"]))["data"].clone();
    assert_eq!(data["position_rebuilt"]["from"], "copy", "{:?}: {}", kind, data);
    assert_eq!(std::fs::read(&position).unwrap(), before, "{:?}: the position must be the one it held", kind);

    area.run_ok(dir, &["load", "."]);
    area.run_ok(dir, &["stack", "-m", "three"]);
}

// Revert: read_position_at mapping every failure to a plain String → the refusal is "error", not
// bay_position_damaged. Drop the copy write from write_position_in → heal finds no copy.
#[test]
fn a_damaged_position_in_the_working_tree_is_rebuilt_from_its_copy() {
    for kind in DAMAGES {
        let area = Area::new(&format!("tree-{:?}", kind));
        let root = area.warehouse();
        heals_from_the_copy(&area, &root, &root, None, kind);
    }
}

#[test]
fn a_damaged_position_in_a_bay_is_rebuilt_from_its_copy() {
    for kind in DAMAGES {
        let area = Area::new(&format!("bay-{:?}", kind));
        let root = area.warehouse();
        let bay = area.bay(&root, "b1");
        area.stack(&bay, "d.txt", "bay");
        heals_from_the_copy(&area, &root, &bay, Some("b1"), kind);
    }
}

// Revert: fall back to the undo journal's newest entry when there is no copy → heal rebuilds on
// "feature" instead of refusing. Drop the pallet list from the message → red.
#[test]
fn without_a_copy_or_a_marker_heal_refuses_and_position_puts_the_bay_on_a_head() {
    let area = Area::new("chosen");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    forklift_core::util::file_utils::remove_file(dir.join("position")).unwrap();
    forklift_core::util::file_utils::remove_file(dir.join("position-copy")).unwrap();

    let refused = position_damaged(&area.run(&root, &["--json", "heal"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal --position <pallet>", "{}", refused);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("\"feature\"") && message.contains("\"main\""), "the pallets are listed: {}", message);
    assert!(!dir.join("position").exists(), "a refused heal writes nothing");

    let unknown = area.run(&root, &["--json", "heal", "--position", "nope"]);
    assert!(!unknown.status.success(), "a pallet that does not exist is refused");
    assert!(!dir.join("position").exists(), "a refused heal writes nothing");

    let data = parsed(&area.run_ok(&root, &["--json", "heal", "--position", "main"]))["data"].clone();
    assert_eq!(data["position_rebuilt"]["from"], "chosen", "{}", data);
    assert_eq!(data["position_rebuilt"]["pallet"], "main", "{}", data);
    assert_eq!(data["position_rebuilt"]["base"], head(&root, "main"), "{}", data);
    assert_eq!(std::fs::read(dir.join("position")).unwrap(), std::fs::read(dir.join("position-copy")).unwrap(),
        "the chosen position is written with its copy");

    area.stack(&root, "c.txt", "three");
}

// Revert: drop keep's refusal under --position → the readable position is overwritten.
#[test]
fn position_is_refused_over_a_position_that_reads() {
    let area = Area::new("reads");
    let root = area.warehouse();
    let position = state_dir(&root, None).join("position");
    let before = std::fs::read(&position).unwrap();

    let refused = area.run(&root, &["--json", "heal", "--position", "main"]);
    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    assert!(parsed(&refused)["error"]["message"].as_str().unwrap().contains("forklift shift"), "{}", text(&refused.stdout));
    assert_eq!(std::fs::read(&position).unwrap(), before);
}

// Revert: keep's copy comparison → a stale or missing copy stays as it was.
#[test]
fn heal_rewrites_a_copy_that_does_not_match_the_position() {
    let area = Area::new("copy");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    let position = std::fs::read(dir.join("position")).unwrap();

    for stale in [None, Some(forklift_core::util::record_utils::to_bytes("position", 2, ["main", &head(&root, "main")]))] {
        match &stale {
            None => forklift_core::util::file_utils::remove_file(dir.join("position-copy")).unwrap(),
            Some(bytes) => std::fs::write(dir.join("position-copy"), bytes).unwrap(),
        }

        let data = parsed(&area.run_ok(&root, &["--json", "heal"]))["data"].clone();
        assert!(data.get("position_rebuilt").is_none(), "the position itself is not rebuilt: {}", data);
        assert_eq!(std::fs::read(dir.join("position-copy")).unwrap(), position, "{:?}", stale.is_some());
    }
}

// Revert: decode_position reading a newer build's record as damage → heal rewrites it from the
// copy, or --position overwrites it.
#[test]
fn a_position_a_newer_version_wrote_is_never_written_over() {
    let area = Area::new("newer");
    let root = area.warehouse();
    let position = state_dir(&root, None).join("position");
    let newer = forklift_core::util::record_utils::to_bytes("position", 99, ["feature", &head(&root, "feature")]);
    std::fs::write(&position, &newer).unwrap();

    let refused = area.run(&root, &["--json", "stack", "-m", "x"]);
    assert!(!refused.status.success());
    assert_ne!(parsed(&refused)["error"]["code"], "bay_position_damaged", "{}", text(&refused.stdout));

    let data = parsed(&area.run_ok(&root, &["--json", "heal"]))["data"].clone();
    assert!(data.get("position_rebuilt").is_none(), "{}", data);
    assert!(!area.run(&root, &["heal", "--position", "main"]).status.success());
    assert_eq!(std::fs::read(&position).unwrap(), newer, "a newer build's position is left as it was");
}

// Revert: read_position_in passing "forklift heal" as the next step → the listing in the main
// tree names a heal that would not reach the bay. Wrap the error in a plain String → no code.
#[test]
fn another_bays_damaged_position_is_refused_without_naming_a_heal_here() {
    let area = Area::new("other-bay");
    let root = area.warehouse();
    area.bay(&root, "b1");
    damage(&state_dir(&root, Some("b1")).join("position"), Damage::Truncate);

    let listed = area.run(&root, &["--json", "bay"]);
    assert!(!listed.status.success(), "{}", text(&listed.stdout));
    let value = position_damaged(&listed);
    assert!(value["error"]["message"].as_str().unwrap().contains("Bay \"b1\""), "{}", value);
    assert!(value["error"].get("next_step").is_none(), "a heal here does not reach the bay: {}", value);
}

// An operation stopped after its marker is written and before its position write leaves a marker
// whose source is where the bay stood. With the position and its copy both lost, heal writes that
// source and the recovery then finishes or undoes the operation from it.
// Revert: drop the marker rule from repair → heal refuses with no evidence.
#[test]
fn without_a_copy_a_standing_operations_source_rebuilds_the_position() {
    rebuilt_from_a_standing_operation(false);
}

// A copy left stale by an earlier crash, naming neither where the standing operation began nor
// where it was going, is no witness while that operation stands: its source is.
// Revert: drop the guard on a copy that matches neither side of the marker → from "copy".
#[test]
fn a_stale_copy_gives_way_to_a_standing_operations_source() {
    rebuilt_from_a_standing_operation(true);
}

fn rebuilt_from_a_standing_operation(stale_copy: bool) {
    use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

    for at in 1..200 {
        let area = Area::new(&format!("marker-{}-{}", stale_copy, at));
        let root = area.warehouse();
        let dir = state_dir(&root, None);

        let stopped = area.command(&root, &["shift", "main"])
            .env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string())
            .output()
            .unwrap();
        if stopped.status.code() != Some(CRASH_POINT_EXIT_CODE) {
            break;
        }
        if !dir.join("operation").exists() {
            continue;
        }

        // A stopped process leaves its lock behind, as a power loss would; the operator clears it.
        let _ = forklift_core::util::file_utils::remove_file(dir.join("lock"));
        let _ = forklift_core::util::file_utils::remove_file(dir.join("position"));
        let _ = forklift_core::util::file_utils::remove_file(dir.join("position-copy"));
        if stale_copy {
            // On "main", but at feature's head: neither the shift's source nor its target.
            let stale = forklift_core::util::record_utils::to_bytes("position", 2, ["main", &head(&root, "feature")]);
            std::fs::write(dir.join("position-copy"), stale).unwrap();
        }

        let data = parsed(&area.run_ok(&root, &["--json", "heal"]))["data"].clone();
        assert_eq!(data["position_rebuilt"]["from"], "operation", "{}", data);
        assert!(!dir.join("operation").exists(), "the recovery settled the operation: {}", data);

        area.stack(&root, "c.txt", "three");
        return;
    }

    panic!("no rename stops the shift while its marker stands");
}

// A deleted position is lost, not unmade: prepare over it would put the bay on the default pallet
// with no base and write over the copy heal rebuilds it from.
// Revert: drop the lost-position guard from create_position_if_not_exists → prepare writes main.
#[test]
fn prepare_refuses_to_recreate_a_lost_position() {
    let area = Area::new("prepare");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    let before = std::fs::read(dir.join("position")).unwrap();
    forklift_core::util::file_utils::remove_file(dir.join("position")).unwrap();

    let refused = position_damaged(&area.run(&root, &["--json", "prepare"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{}", refused);
    assert!(!dir.join("position").exists(), "prepare wrote nothing");

    area.run_ok(&root, &["heal"]);
    assert_eq!(std::fs::read(dir.join("position")).unwrap(), before);
}

// Revert: drop the newer-copy refusal → heal writes this version's record over the copy's.
#[test]
fn a_copy_a_newer_version_wrote_is_never_written_over() {
    let area = Area::new("newer-copy");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    let newer = forklift_core::util::record_utils::to_bytes("position", 99, ["feature", &head(&root, "feature")]);
    std::fs::write(dir.join("position-copy"), &newer).unwrap();
    damage(&dir.join("position"), Damage::Truncate);

    for args in [&["--json", "heal"][..], &["--json", "heal", "--position", "main"][..]] {
        let refused = area.run(&root, args);
        assert!(!refused.status.success(), "{:?}: {}", args, text(&refused.stdout));
        assert_eq!(std::fs::read(dir.join("position-copy")).unwrap(), newer, "{:?}", args);
    }
}

// Revert: keep rewriting a copy that does not read as this version's → the newer record is lost.
#[test]
fn a_newer_copy_beside_a_position_that_reads_is_kept() {
    let area = Area::new("newer-copy-kept");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    let newer = forklift_core::util::record_utils::to_bytes("position", 99, ["feature", &head(&root, "feature")]);
    std::fs::write(dir.join("position-copy"), &newer).unwrap();

    area.run_ok(&root, &["heal"]);
    assert_eq!(std::fs::read(dir.join("position-copy")).unwrap(), newer);
}

// A copy that cannot be read witnesses nothing, so it blocks no repair.
// Revert: keep failing on a copy it cannot rewrite → every heal stops at the position step.
#[test]
fn a_copy_that_cannot_be_read_does_not_stop_heal() {
    let area = Area::new("copy-dir");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    forklift_core::util::file_utils::remove_file(dir.join("position-copy")).unwrap();
    std::fs::create_dir_all(dir.join("position-copy").join("inner")).unwrap();

    let data = parsed(&area.run_ok(&root, &["--json", "heal"]))["data"].clone();
    assert!(data.get("position_rebuilt").is_none(), "{}", data);
}

// Exact evidence outranks the operator's guess.
// Revert: let --position win over the copy → the bay is put on main.
#[test]
fn position_is_refused_while_the_copy_says_where_the_bay_stood() {
    let area = Area::new("chosen-over-copy");
    let root = area.warehouse();
    let position = state_dir(&root, None).join("position");
    let before = std::fs::read(&position).unwrap();
    damage(&position, Damage::Delete);

    let refused = area.run(&root, &["--json", "heal", "--position", "main"]);
    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    assert!(!position.exists(), "nothing was written");

    area.run_ok(&root, &["heal"]);
    assert_eq!(std::fs::read(&position).unwrap(), before);
}

// Revert: the bay scan wrapping the error in a plain String → the code is lost.
#[test]
fn deleting_a_pallet_over_another_bays_damaged_position_keeps_the_code() {
    let area = Area::new("bays-on");
    let root = area.warehouse();
    area.bay(&root, "b1");
    damage(&state_dir(&root, Some("b1")).join("position"), Damage::Truncate);

    let value = position_damaged(&area.run(&root, &["--json", "pallet", "delete", "main"]));
    assert!(value["error"]["message"].as_str().unwrap().contains("bay \"b1\""), "{}", value);
}

// `--ref` writes run before the position step, so the refusal says the ref can be set with it.
// Revert: drop the hint from chosen_position → the message names no --ref.
#[test]
fn position_on_a_pallet_whose_ref_is_damaged_names_ref() {
    let area = Area::new("chosen-ref");
    let root = area.warehouse();
    let dir = state_dir(&root, None);
    damage(&dir.join("position"), Damage::Delete);
    forklift_core::util::file_utils::remove_file(dir.join("position-copy")).unwrap();
    damage(&root.join(".forklift").join("pallets").join("feature"), Damage::Zero);

    let refused = area.run(&root, &["--json", "heal", "--position", "feature"]);
    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    let message = parsed(&refused)["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("--ref feature=<parcel>"), "{}", message);
}

// Only an absent position is one never made.
// Revert: test the path with exists() → prepare writes the default over the link.
#[cfg(unix)]
#[test]
fn prepare_never_writes_over_a_position_it_cannot_follow() {
    let area = Area::new("dangling");
    let root = area.base.join("wh");
    std::fs::create_dir_all(&root).unwrap();
    area.run_ok(&root, &["prepare"]);
    let dir = state_dir(&root, None);
    forklift_core::util::file_utils::remove_file(dir.join("position")).unwrap();
    let _ = forklift_core::util::file_utils::remove_file(dir.join("position-copy"));
    std::os::unix::fs::symlink(area.base.join("nowhere"), dir.join("position")).unwrap();

    let _ = area.run(&root, &["prepare"]);
    assert!(std::fs::symlink_metadata(dir.join("position")).unwrap().file_type().is_symlink());
}
