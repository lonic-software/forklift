//! A damaged inventory metadata file, the registered set of folders with staged content, is refused
//! as `inventory_damaged` (exit 66) by every command that reads it, and so is a missing one while
//! the shard store still holds shards. `heal` rebuilds the set from every folder it can find whose
//! shard is on disk: the file's own lines that still read, the working tree, the tree of the bay's
//! base and the trees an interrupted operation names. It writes the set only when that accounts for
//! every shard and every tree it read from; otherwise it refuses, and `heal --inventory` writes it
//! anyway. These tests damage the file the ways Proof Load does (cut short, zeroed, a byte flipped)
//! and delete it.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;
use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

const INVENTORY_DAMAGED_EXIT: i32 = 66;

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
    /// A prepared warehouse with the folders `.`, `a`, `a/b` and `c` stacked, and `c/z.txt`
    /// staged on top.
    fn new(name: &str) -> Area {
        let base = std::env::temp_dir().join(format!("forklift-inventory-heal-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("home")).unwrap();
        std::fs::create_dir_all(base.join("wh/a/b")).unwrap();
        std::fs::create_dir_all(base.join("wh/c")).unwrap();
        let base = std::fs::canonicalize(&base).unwrap();
        let base = match base.to_str().and_then(|shown| shown.strip_prefix(r"\\?\")) {
            Some(plain) => PathBuf::from(plain),
            None => base,
        };
        let area = Area { home: base.join("home"), base };

        area.run_ok(&["prepare"]);
        area.run_ok(&["config", "set", "operator.identifier", "inventory-heal@forklift"]);
        area.write("a/b/x.txt", "1\n");
        area.write("c/y.txt", "2\n");
        area.write("top.txt", "3\n");
        area.run_ok(&["load", "."]);
        area.run_ok(&["stack", "-m", "one"]);
        area.write("c/z.txt", "4\n");
        area.run_ok(&["load", "c"]);
        area
    }

    fn root(&self) -> PathBuf {
        self.base.join("wh")
    }

    fn metadata(&self) -> PathBuf {
        self.root().join(".forklift/inventory/metadata")
    }

    fn write(&self, path: &str, content: &str) {
        std::fs::write(self.root().join(path), content).unwrap();
    }

    fn command_in(&self, dir: &Path, args: &[&str]) -> std::process::Command {
        let mut command = forklift_command();
        command.args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));
        command
    }

    fn run_in(&self, dir: &Path, args: &[&str]) -> Output {
        self.command_in(dir, args).output().unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(&self.root(), args)
    }

    fn run_ok_in(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.run_in(dir, args);
        assert!(output.status.success(), "{:?} in {:?} failed: {}{}", args, dir, text(&output.stdout), text(&output.stderr));
        output
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        self.run_ok_in(&self.root(), args)
    }

    /// The `--json` data of a `heal` that must succeed.
    fn healed(&self, args: &[&str]) -> serde_json::Value {
        let output = self.run(&[&["--json", "heal"][..], args].concat());
        assert!(output.status.success(), "heal {:?} failed: {}{}", args, text(&output.stdout), text(&output.stderr));
        parsed(&output)["data"].clone()
    }

    /// The parcel `pallet`'s ref names.
    fn head(&self, pallet: &str) -> String {
        std::fs::read_to_string(self.root().join(".forklift/pallets").join(pallet)).unwrap().trim_end().to_string()
    }

    /// The `--json` content of the object `hash`.
    fn peek(&self, hash: &str) -> serde_json::Value {
        parsed(&self.run_ok(&["--json", "store", "peek", hash]))["data"].clone()
    }

    /// The tree of the parcel `pallet`'s ref names.
    fn head_tree(&self, pallet: &str) -> String {
        self.peek(&self.head(pallet))["tree"].as_str().unwrap().to_string()
    }

    /// Remove the loose object `hash` from the store.
    fn lose(&self, hash: &str) {
        let path = self.root().join(".forklift/objects").join(&hash[..2]).join(&hash[2..]);
        forklift_core::util::file_utils::remove_file(&path).unwrap();
    }

    /// Put a copy of a shard on disk at an address no folder's key hashes to, and return its folder.
    fn plant_shard(&self) -> PathBuf {
        let store = self.root().join(".forklift/inventory/inv_");
        let fan_out = std::fs::read_dir(&store).unwrap().next().unwrap().unwrap().path();
        let shard = std::fs::read_dir(&fan_out).unwrap().next().unwrap().unwrap().path();
        let planted = store.join("ab").join("c".repeat(62));
        std::fs::create_dir_all(&planted).unwrap();
        std::fs::copy(shard.join("data"), planted.join("data")).unwrap();
        planted
    }

    /// A stopped process leaves its lock behind, as a power loss would; the operator clears it.
    fn clear_stale_lock(&self) {
        let _ = forklift_core::util::file_utils::remove_file(self.root().join(".forklift/lock"));
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn parsed(output: &Output) -> serde_json::Value {
    serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({}): {}{}", e, text(&output.stdout), text(&output.stderr)))
}

/// The `--json` refusal of `output`, asserting it is `inventory_damaged` with exit 66.
fn inventory_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(INVENTORY_DAMAGED_EXIT), "expected inventory_damaged: {}{}",
        text(&output.stdout), text(&output.stderr));
    let value = parsed(output);
    assert_eq!(value["error"]["code"], "inventory_damaged", "{}", value);
    value["error"].clone()
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Truncate,
    Zero,
    FlipByte,
}

const DAMAGES: [Damage; 3] = [Damage::Truncate, Damage::Zero, Damage::FlipByte];

fn damage(path: &Path, kind: Damage) {
    let mut bytes = std::fs::read(path).unwrap();
    let at = bytes.windows(3).position(|w| w == b"a/b").unwrap();
    match kind {
        Damage::Truncate => bytes.truncate(at + 2),
        Damage::Zero => bytes = vec![0u8; bytes.len()],
        Damage::FlipByte => bytes[at] = !bytes[at],
    }
    std::fs::write(path, bytes).unwrap();
}

/// Revert: map the funnel's record errors back to the uncoded message → exit 1 → red. Read a
/// missing header as an older Forklift's file → the zeroed file's message says so → red. Each damage
/// leaves `c`, `a` and `a/b` named only by the working tree and the base.
#[test]
fn every_damage_is_refused_and_plain_heal_restores_the_set_exactly() {
    for kind in DAMAGES {
        let area = Area::new(&format!("{:?}", kind).to_lowercase());
        let sound = std::fs::read(area.metadata()).unwrap();
        damage(&area.metadata(), kind);

        for command in [&["load", "c"][..], &["unload", "c/z.txt"], &["status"]] {
            let error = inventory_damaged(&area.run(&[&["--json"][..], command].concat()));
            assert_eq!(error["next_step"], "forklift heal", "{:?} {:?}: {}", kind, command, error);
            // A zeroed file has no header, which is damage, not a file an older Forklift wrote.
            assert!(!error["message"].as_str().unwrap().contains("older"), "{:?}: {}", kind, error);
        }

        let data = area.healed(&[]);
        assert_eq!(data["inventory_rebuilt"]["dropped"], 0, "{:?}: {}", kind, data);
        assert_eq!(std::fs::read(area.metadata()).unwrap(), sound, "{:?}: the set is back exactly", kind);
        let status = parsed(&area.run_ok(&["--json", "status"]));
        assert_eq!(status["data"]["staged"][0]["path"], "c/z.txt", "{:?}: {}", kind, status);
    }
}

/// Revert: read a missing file as nothing registered whatever the store holds → `status` succeeds
/// → red.
#[test]
fn a_deleted_file_over_shards_is_refused_and_heal_rebuilds_it() {
    let area = Area::new("deleted");
    let sound = std::fs::read(area.metadata()).unwrap();
    forklift_core::util::file_utils::remove_file(&area.metadata()).unwrap();

    let error = inventory_damaged(&area.run(&["--json", "status"]));
    assert!(error["message"].as_str().unwrap().contains("is missing"), "{}", error);

    area.healed(&[]);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), sound);
}

/// The folder `a/b` is gone from the working tree with its removal unstaged, so only the base's
/// tree still names it. Revert: drop the base's tree from the candidates → its shard is unmatched
/// and heal refuses → red.
#[test]
fn a_folder_only_the_base_names_is_restored() {
    let area = Area::new("base-only");
    let sound = std::fs::read(area.metadata()).unwrap();
    forklift_core::util::file_utils::remove_dir_all(area.root().join("a/b")).unwrap();
    damage(&area.metadata(), Damage::Zero);

    area.healed(&[]);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), sound);
}

/// A second checkout shifts onto `main` and stacks `a/b`'s removal, so the ref's tree no longer
/// names it while this tree's base still does. Revert: take the tree from the pallet's ref instead
/// of the position's base → `a/b`'s shard is unmatched and heal refuses → red.
#[test]
fn the_base_names_the_tree_and_never_the_ref() {
    let area = Area::new("base-not-ref");
    let sound = std::fs::read(area.metadata()).unwrap();
    let bay = area.base.join("b2");
    area.run_ok(&["bay", "add", "b2", bay.to_str().unwrap()]);
    area.run_ok_in(&bay, &["shift", "main"]);
    forklift_core::util::file_utils::remove_dir_all(bay.join("a/b")).unwrap();
    area.run_ok_in(&bay, &["remove", "a/b"]);
    area.run_ok_in(&bay, &["stack", "-m", "drop a/b"]);
    forklift_core::util::file_utils::remove_dir_all(area.root().join("a/b")).unwrap();
    damage(&area.metadata(), Damage::Zero);

    area.healed(&[]);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), sound);
}

/// Revert: fail the rebuild on a tree it cannot read → heal exits with the read error → red. Count
/// an unread tree as read and empty → plain heal writes the set → red.
#[test]
fn an_unreadable_base_tree_holds_the_rebuild_until_heal_inventory() {
    let area = Area::new("unread-base");
    let tree = area.head_tree("main");
    area.lose(&tree);
    damage(&area.metadata(), Damage::Zero);
    let damaged = std::fs::read(area.metadata()).unwrap();

    let error = inventory_damaged(&area.run(&["--json", "heal"]));
    assert_eq!(error["next_step"], "forklift heal --inventory", "{}", error);
    let held = &error["details"]["inventory_held"];
    assert_eq!(held["unread"][0]["tree"], tree.as_str(), "{}", error);
    assert_eq!(held["unread"][0]["from"], "base", "{}", error);
    assert!(error["details"]["objects_lost"]["hashes"].to_string().contains(&tree), "the lost tree is still reported: {}", error);
    assert!(!error["message"].as_str().unwrap().contains("Next step"), "{}", error);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), damaged, "plain heal must not write the set");

    let output = area.run(&["--json", "heal", "--inventory"]);
    let value = parsed(&output);
    assert!(value["error"]["details"]["inventory_rebuilt"].is_object(), "{}", value);
    assert_ne!(std::fs::read(area.metadata()).unwrap(), damaged, "--inventory writes the set");
    let status = parsed(&area.run(&["--json", "status"]));
    assert_ne!(status["error"]["code"], "inventory_damaged", "{}", status);
}

/// A shard no folder heal can find holds the rebuild, and the repairs heal makes after it still
/// land: here, the report of `top.txt`'s lost content, which the working tree no longer holds.
/// Revert: write whatever the rebuild found → the set is written → red. Return at the gate → the
/// lost content is not in the refusal → red.
#[test]
fn an_unmatched_shard_holds_the_rebuild_and_later_repairs_still_land() {
    let area = Area::new("gate");
    let planted = area.plant_shard();
    let lost = area.peek(&area.head_tree("main"))["entries"].as_array().unwrap().iter()
        .find(|entry| entry["name"] == "top.txt").unwrap()["hash"].as_str().unwrap().to_string();
    area.lose(&lost);
    area.write("top.txt", "changed\n");
    damage(&area.metadata(), Damage::FlipByte);
    let damaged = std::fs::read(area.metadata()).unwrap();

    let error = inventory_damaged(&area.run(&["--json", "heal"]));
    assert_eq!(error["next_step"], "forklift heal --inventory", "{}", error);
    assert_eq!(error["details"]["inventory_held"]["dropped"], 1, "{}", error);
    assert_eq!(error["details"]["objects_lost"]["hashes"][0]["hash"], lost.as_str(), "{}", error);
    assert!(!error["details"]["notes"].to_string().contains('\u{1f}'), "a note carries no frame: {}", error);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), damaged, "plain heal must not write the set");

    let value = parsed(&area.run(&["--json", "heal", "--inventory"]));
    assert_eq!(value["error"]["details"]["inventory_rebuilt"]["dropped"], 1, "{}", value);
    assert!(planted.join("data").exists(), "the unmatched shard is left for the next sweep");
    let status = parsed(&area.run(&["--json", "status"]));
    assert_eq!(status["ok"], true, "{}", status);
}

/// Stop `park args` at each rename in turn, in a fresh area set up by `setup` with the folder `n`
/// staged on top, until one leaves the operation marker
/// standing with `keep` true of the area; return that area.
fn interrupted_park(name: &str, setup: impl Fn(&Area), args: &[&str], keep: impl Fn(&Area) -> bool) -> Area {
    for at in 1..60 {
        let area = Area::new(&format!("{}-{}", name, at));
        setup(&area);
        std::fs::create_dir_all(area.root().join("n")).unwrap();
        area.write("n/q.txt", "q\n");
        area.run_ok(&["load", "n"]);
        let output = area.command_in(&area.root(), &[&["park"][..], args].concat())
            .env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string())
            .output().unwrap();
        assert_eq!(output.status.code(), Some(CRASH_POINT_EXIT_CODE),
            "park finished before a rename left its marker: {}{}", text(&output.stdout), text(&output.stderr));
        area.clear_stale_lock();
        if area.root().join(".forklift/operation").exists() && keep(&area) {
            return area;
        }
    }
    panic!("no rename stopped park {:?} with its marker standing", args);
}

/// Whether the standing park's marker records the park as on the list, past which its recovery
/// rolls forward, rewriting the staging wholesale. Before it, the recovery clears the marker and
/// leaves the staging as it was.
fn listed(area: &Area) -> bool {
    std::fs::read_to_string(area.root().join(".forklift/operation")).unwrap().lines().any(|line| line == "listed")
}

fn parked(area: &Area) -> usize {
    parsed(&area.run_ok(&["--json", "park", "list"]))["data"]["parked"].as_array().unwrap().len()
}

/// A partial park's recovery reads the set before it rewrites part of it. Revert: rebuild after
/// the operation recovery for this marker too → the recovery fails on the damaged file → red.
#[test]
fn a_partial_park_is_finished_over_a_damaged_inventory() {
    let area = interrupted_park("partial", |_| (), &["n"], |area| area.root().join("n").exists());
    damage(&area.metadata(), Damage::Zero);

    let data = area.healed(&[]);
    assert!(data["inventory_rebuilt"].is_object(), "{}", data);
    assert!(data["operation"].is_object(), "{}", data);
    assert!(!area.root().join(".forklift/operation").exists());
    area.run_ok(&["status"]);
}

/// The folder `n`, staged only, is gone from the working tree while its shard stays, so only the
/// parked parcel's tree names it. Revert: drop the marker's trees from the candidates → `n`'s shard
/// is unmatched, the rebuild is held and the park waits → red.
#[test]
fn a_partial_park_whose_folder_is_gone_is_finished_from_the_parked_tree() {
    let area = interrupted_park("partial-gone", |_| (), &["n"], |area| area.root().join("n").exists());
    forklift_core::util::file_utils::remove_dir_all(area.root().join("n")).unwrap();
    damage(&area.metadata(), Damage::Zero);

    let data = area.healed(&[]);
    assert_eq!(data["inventory_rebuilt"]["dropped"], 0, "{}", data);
    assert!(data["operation"].is_object(), "{}", data);
    area.run_ok(&["status"]);
}

/// A whole park's recovery rewrites the set wholesale, and clears every shard it does not keep.
/// Revert: rebuild before every recovery → the planted shard is unmatched and heal refuses → red.
#[test]
fn a_wholesale_recovery_needs_no_rebuild() {
    let area = interrupted_park("wholesale", |_| (), &[], listed);
    area.plant_shard();
    damage(&area.metadata(), Damage::Zero);

    let data = area.healed(&[]);
    assert!(data["operation"].is_object(), "{}", data);
    assert_eq!(parked(&area), 1, "{}", data);
    area.run_ok(&["status"]);
}

/// The interrupted park waits on `main`'s ref, which heal cannot rebuild: what its bytes still name
/// diverged from the base. The rebuild waits with it. Revert: rebuild while the operation waits →
/// the planted shard holds it and heal refuses `inventory_damaged` instead of `ref_damaged` → red.
#[test]
fn a_wholesale_recovery_waiting_on_a_ref_leaves_the_set_alone() {
    let diverged = |area: &Area| {
        area.run_ok(&["pallet", "create", "side"]);
        area.run_ok(&["shift", "side"]);
        area.write("s.txt", "s\n");
        area.run_ok(&["load", "s.txt"]);
        area.run_ok(&["stack", "-m", "side"]);
        area.run_ok(&["shift", "main"]);
        area.write("m.txt", "m\n");
        area.run_ok(&["load", "m.txt"]);
        area.run_ok(&["stack", "-m", "main"]);
    };
    let area = interrupted_park("waits", diverged, &[], listed);
    let side = area.head("side");
    forklift_core::util::file_utils::remove_file(&area.root().join(".forklift/journal.json")).unwrap();
    std::fs::write(area.root().join(".forklift/pallets/main"), &side.as_bytes()[..20]).unwrap();
    area.plant_shard();
    damage(&area.metadata(), Damage::Zero);
    let damaged = std::fs::read(area.metadata()).unwrap();

    let value = parsed(&area.run(&["--json", "heal"]));
    assert_eq!(value["error"]["code"], "ref_damaged", "{}", value);
    assert_eq!(value["error"]["details"]["refs_refused"][0]["pallet"], "main", "{}", value);
    assert_eq!(std::fs::read(area.metadata()).unwrap(), damaged, "the set waits with the operation");
}
