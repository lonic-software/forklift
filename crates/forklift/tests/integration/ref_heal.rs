//! A lost or damaged pallet ref is rebuilt by `heal` from what the warehouse still says about its
//! head, or refused as `ref_damaged` (exit 53) with every head it found. These tests damage refs
//! the ways Proof Load does (deleted, the `pallets` folder deleted, truncated, zeroed, a byte
//! flipped), in the warehouse's own working tree and in a bay, check that the command that meets
//! the damage refuses with `ref_damaged` and `heal` as its next step, that `heal` rebuilds at the
//! right head and the refused command then succeeds, and that `heal` never guesses: a deleted
//! pallet, diverged evidence or no evidence leaves the ref alone, and `--ref`/`--remove-ref` settle
//! it.

use std::path::{Path, PathBuf};
use std::process::Output;

use crate::common;
use common::forklift_command;

const REF_DAMAGED_EXIT: i32 = 53;

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
        let base = std::env::temp_dir().join(format!("forklift-ref-heal-{}-{}", name, std::process::id()));
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

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"))
            .output()
            .unwrap()
    }

    fn run_ok(&self, dir: &Path, args: &[&str]) -> Output {
        let output = self.run(dir, args);
        assert!(output.status.success(), "{:?} in {:?} failed: {}{}", args, dir, text(&output.stdout), text(&output.stderr));
        output
    }

    /// A prepared warehouse with two parcels stacked on `main`.
    fn warehouse(&self) -> PathBuf {
        let root = self.base.join("wh");
        std::fs::create_dir_all(&root).unwrap();
        self.run_ok(&root, &["prepare"]);
        self.run_ok(&root, &["config", "set", "operator.identifier", "ref-heal@forklift"]);
        self.stack(&root, "a.txt", "one");
        self.stack(&root, "b.txt", "two");
        root
    }

    fn stack(&self, dir: &Path, file: &str, message: &str) {
        std::fs::write(dir.join(file), format!("{}\n", message)).unwrap();
        self.run_ok(dir, &["load", "."]);
        self.run_ok(dir, &["stack", "-m", message]);
    }

    /// A bay at `name` beside the warehouse, on new pallet `name`.
    fn bay(&self, root: &Path, name: &str) -> PathBuf {
        let dir = self.base.join(name);
        self.run_ok(root, &["bay", "add", name, dir.to_str().unwrap()]);
        dir
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

fn ref_path(root: &Path, pallet: &str) -> PathBuf {
    root.join(".forklift").join("pallets").join(pallet)
}

fn head(root: &Path, pallet: &str) -> String {
    std::fs::read_to_string(ref_path(root, pallet)).unwrap().trim_end().to_string()
}

/// The `--json` refusal of `output`, asserting it is `ref_damaged` with exit 53.
fn ref_damaged(output: &Output) -> serde_json::Value {
    assert_eq!(output.status.code(), Some(REF_DAMAGED_EXIT), "expected ref_damaged: {}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|e| panic!("not JSON ({}): {}{}", e, text(&output.stdout), text(&output.stderr)));
    assert_eq!(value["error"]["code"], "ref_damaged", "{}", value);
    value
}

/// The `--json` data of a heal that succeeded.
fn healed(area: &Area, dir: &Path) -> serde_json::Value {
    let output = area.run_ok(dir, &["--json", "heal"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["data"].clone()
}

#[derive(Clone, Copy, Debug)]
enum Damage {
    Delete,
    DeleteFolder,
    Truncate,
    Zero,
    FlipByte,
}

const DAMAGES: [Damage; 5] = [Damage::Delete, Damage::DeleteFolder, Damage::Truncate, Damage::Zero, Damage::FlipByte];

fn damage(root: &Path, pallet: &str, damage: Damage) {
    let path = ref_path(root, pallet);
    let bytes = std::fs::read(&path).unwrap();

    match damage {
        Damage::Delete => forklift_core::util::file_utils::remove_file(&path).unwrap(),
        Damage::DeleteFolder => forklift_core::util::file_utils::remove_dir_all(root.join(".forklift").join("pallets")).unwrap(),
        Damage::Truncate => std::fs::write(&path, &bytes[..bytes.len() / 2]).unwrap(),
        Damage::Zero => std::fs::write(&path, vec![0u8; bytes.len()]).unwrap(),
        Damage::FlipByte => std::fs::write(&path, flipped(&bytes, 20)).unwrap(),
    }
}

/// `bytes` with the byte at `at` flipped to one that is not a hex digit.
fn flipped(bytes: &[u8], at: usize) -> Vec<u8> {
    let mut bytes = bytes.to_vec();
    bytes[at] ^= 0x80;
    bytes
}

/// The command that meets the damage refuses with `ref_damaged` and names `heal`; `heal` rebuilds
/// the ref at the head the checkout stands on, and the refused command then succeeds.
fn heals_in(area: &Area, root: &Path, dir: &Path, pallet: &str, kind: Damage) {
    let before = head(root, pallet);
    damage(root, pallet, kind);

    std::fs::write(dir.join("c.txt"), "three\n").unwrap();
    area.run_ok(dir, &["load", "."]);
    let refused = ref_damaged(&area.run(dir, &["--json", "stack", "-m", "three"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{:?}: {}", kind, refused);

    let data = healed(area, dir);
    assert_eq!(data["refs_rebuilt"][0]["pallet"], pallet, "{:?}: {}", kind, data);
    assert_eq!(head(root, pallet), before, "{:?}: the ref must be rebuilt at the head it held", kind);

    area.run_ok(dir, &["stack", "-m", "three"]);
}

#[test]
fn every_damage_to_the_working_trees_ref_is_rebuilt() {
    // Mutation: drop pass 2 and pass 1 from heal → heal reports nothing, the ref stays damaged → red.
    for kind in DAMAGES {
        let area = Area::new(&format!("tree-{:?}", kind));
        let root = area.warehouse();
        heals_in(&area, &root, &root, "main", kind);
    }
}

#[test]
fn every_damage_to_a_bays_ref_is_rebuilt() {
    for kind in DAMAGES {
        let area = Area::new(&format!("bay-{:?}", kind));
        let root = area.warehouse();
        let bay = area.bay(&root, "feature");
        area.stack(&bay, "f.txt", "feature");
        heals_in(&area, &root, &bay, "feature", kind);
    }
}

/// A pallet `side` one parcel ahead of `main`, which nothing stands on, its journal entries gone,
/// so only its own bytes still name its head.
fn side_only_its_bytes_name(area: &Area) -> (PathBuf, String) {
    let root = area.warehouse();
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "s.txt", "side");
    area.run_ok(&root, &["shift", "main"]);
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("journal.json")).unwrap();
    let side = head(&root, "side");
    (root, side)
}

#[test]
fn a_ref_nothing_stands_on_is_salvaged_from_its_own_bytes() {
    // Mutation: make `salvage` return Nothing → each ref is refused as nothing naming its head → red.
    for (name, at) in [("truncated", None), ("hex-flipped", Some(30)), ("newline-flipped", Some(64))] {
        let area = Area::new(&format!("salvage-{}", name));
        let (root, side) = side_only_its_bytes_name(&area);
        let bytes = std::fs::read(ref_path(&root, "side")).unwrap();

        let damaged = match at {
            None => bytes[..20].to_vec(),
            Some(at) => flipped(&bytes, at),
        };
        std::fs::write(ref_path(&root, "side"), &damaged).unwrap();

        let data = healed(&area, &root);
        assert_eq!(data["refs_rebuilt"][0]["source"], "what the ref's own bytes still name", "{}: {}", name, data);
        assert_eq!(head(&root, "side"), side, "{}", name);
    }
}

#[test]
fn a_prefix_too_short_or_naming_no_parcel_salvages_nothing() {
    // Mutation: lower MIN_SALVAGE_PREFIX to 4 → the 7-character prefix salvages the parcel → red.
    // Mutation: count any matching object as a parcel → the blob's prefix is "salvaged" → red.
    let area = Area::new("salvage-none");
    let (root, side) = side_only_its_bytes_name(&area);

    let blob_hash = forklift_core::util::object_utils::hash_object_bytes(&forklift_core::util::object_utils::blob_bytes(b"side\n"));
    assert!(root.join(".forklift").join("objects").join(&blob_hash[..2]).join(&blob_hash[2..]).is_file(), "the fixture must name the blob");

    let cases = vec![("short", side.as_bytes()[..7].to_vec()), ("blob", blob_hash.as_bytes()[..16].to_vec())];

    for (name, bytes) in cases {
        std::fs::write(ref_path(&root, "side"), &bytes).unwrap();

        let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
        let refs = &refused["error"]["details"]["refs_refused"];
        assert_eq!(refs[0]["pallet"], "side", "{}: {}", name, refused);
        assert_eq!(refs[0]["candidates"].as_array().unwrap().len(), 0, "{}: {}", name, refused);
        assert_eq!(std::fs::read(ref_path(&root, "side")).unwrap(), bytes, "{}: the ref must be left alone", name);
    }

}

#[test]
fn a_second_checkout_ahead_on_the_same_pallet_wins() {
    // Mutation: choose the first verified candidate without the descent checks → the rebuild lands
    // at the working tree's base, below the bay → red.
    let area = Area::new("ahead");
    let root = area.warehouse();
    let bay = area.bay(&root, "feature");
    area.run_ok(&bay, &["shift", "main"]);
    area.stack(&bay, "f.txt", "ahead");
    let ahead = head(&root, "main");

    forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

    let data = healed(&area, &root);
    assert_eq!(head(&root, "main"), ahead, "{}", data);
    area.stack(&bay, "g.txt", "after");
}

#[test]
fn a_journal_head_whose_parcel_is_gone_is_dropped() {
    // The journal names a head that is not in the store (as a collected parcel leaves it): the
    // second pass drops it and rebuilds at the base.
    // Mutation: refuse instead of dropping an incomplete journal-only head → heal refuses → red.
    let area = Area::new("journal-gone");
    let root = area.warehouse();
    let base = head(&root, "main");
    let journal = root.join(".forklift").join("journal.json");
    let text = std::fs::read_to_string(&journal).unwrap();
    let gone = "f".repeat(64);
    std::fs::write(&journal, text.replace(&format!("\"main\":\"{}\"", base), &format!("\"main\":\"{}\"", gone))).unwrap();
    assert!(std::fs::read_to_string(&journal).unwrap().contains(&gone), "the fixture must plant the head");

    forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

    healed(&area, &root);
    assert_eq!(head(&root, "main"), base);
}

#[test]
fn a_journaled_delete_blocks_the_rebuild() {
    // Mutation: ignore `deleted_in` → nothing else names a head, so it is still refused, but as
    // "nothing names" → the reason check is red.
    let area = Area::new("deleted");
    let root = area.warehouse();
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "main"]);
    area.run_ok(&root, &["pallet", "delete", "side", "--force"]);
    std::fs::write(ref_path(&root, "side"), vec![0u8; 65]).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let reason = refused["error"]["details"]["refs_refused"][0]["reason"].as_str().unwrap().to_string();
    assert!(reason.contains("deleted the pallet"), "{}", refused);
}

#[test]
fn diverged_evidence_is_refused_and_ref_settles_it() {
    // The working tree stands on `main` at B, and the ref's own bytes name a parcel of `side`,
    // which diverged from B: heal refuses naming both, and `--ref` sets the one chosen.
    // Mutation: drop the floor descent check → the rebuild lands at one of them → red.
    let area = Area::new("diverged");
    let root = area.warehouse();
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "s.txt", "side");
    area.run_ok(&root, &["shift", "main"]);
    area.stack(&root, "m.txt", "main");
    let base = head(&root, "main");
    let side = head(&root, "side");
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("journal.json")).unwrap();
    std::fs::write(ref_path(&root, "main"), &side.as_bytes()[..20]).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let candidates = refused["error"]["details"]["refs_refused"][0]["candidates"].to_string();
    assert!(candidates.contains(&base) && candidates.contains(&side), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal --ref main=<parcel>", "{}", refused);

    area.run_ok(&root, &["heal", "--ref", &format!("main={}", base)]);
    assert_eq!(head(&root, "main"), base);
    area.stack(&root, "n.txt", "after");
}

#[test]
fn remove_ref_drops_only_an_unreadable_ref_nothing_names() {
    // Mutation: skip the candidate check in remove_damaged_ref → the truncated ref is removed → red.
    // Mutation: skip the damage check → the readable ref is removed → red.
    let area = Area::new("remove");
    let (root, side) = side_only_its_bytes_name(&area);

    // A readable ref is not heal's to remove.
    let readable = area.run(&root, &["heal", "--remove-ref", "main"]);
    assert!(!readable.status.success());
    assert!(ref_path(&root, "main").is_file());

    // Its own bytes still name a head.
    std::fs::write(ref_path(&root, "side"), &side.as_bytes()[..20]).unwrap();
    let named = area.run(&root, &["heal", "--remove-ref", "side"]);
    assert!(!named.status.success(), "{}", text(&named.stdout));
    assert_eq!(std::fs::read(ref_path(&root, "side")).unwrap(), side.as_bytes()[..20].to_vec());

    // Zeroed, with nothing naming a head: removed, the bytes kept aside.
    let zeros = vec![0u8; 65];
    std::fs::write(ref_path(&root, "side"), &zeros).unwrap();
    let output = area.run_ok(&root, &["--json", "heal", "--remove-ref", "side"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let kept = root.join(value["data"]["refs_removed"][0]["kept"].as_str().unwrap());
    assert_eq!(std::fs::read(&kept).unwrap(), zeros);
    assert!(!ref_path(&root, "side").exists());

    // Nothing is left to heal.
    healed(&area, &root);
}

#[test]
fn a_ref_that_is_not_damaged_is_not_set() {
    let area = Area::new("set-readable");
    let root = area.warehouse();
    let before = head(&root, "main");
    let first = text(&area.run_ok(&root, &["history", "--oneline"]).stdout);
    let oldest = first.lines().last().unwrap().split_whitespace().next().unwrap().to_string();

    let output = area.run(&root, &["heal", "--ref", &format!("main={}", oldest)]);
    assert!(!output.status.success(), "{}", text(&output.stdout));
    assert_eq!(head(&root, "main"), before);
}

#[test]
fn a_damaged_tags_ref_does_not_stop_the_rebuild() {
    // Mutation: bound the closure walk by every ref (failing on the unreadable one) → the rebuild
    // fails → red.
    let area = Area::new("tags");
    let root = area.warehouse();
    area.run_ok(&root, &["tag", "create", "v1"]);
    let tags = root.join(".forklift").join("pallets").join("@tags");
    let tags = if tags.exists() { tags } else { root.join(".forklift").join("meta").join("tags") };
    assert!(tags.is_file(), "the fixture must find the @tags ref");
    std::fs::write(&tags, b"not a hash").unwrap();

    let base = head(&root, "main");
    forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

    let _ = area.run(&root, &["heal"]);
    assert_eq!(head(&root, "main"), base);
}

#[test]
fn a_rolling_back_lowers_take_is_never_landed() {
    // A lower's marker records its take as the target; rolling back, the take was refused. The
    // rebuild lands at the start, never at the take.
    // Mutation: admit a lower's target as a candidate → the rebuild lands at the take → red.
    use forklift_core::util::operation_utils::{self, Operation, OperationKind};
    use forklift_core::util::pallet_utils::BayPosition;

    let area = Area::new("lower");
    let root = area.warehouse();
    let start = head(&root, "main");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "t.txt", "take");
    let take = head(&root, "side");
    area.run_ok(&root, &["shift", "main"]);

    let mut operation = Operation::new(
        OperationKind::Lower,
        BayPosition { pallet: "main".to_string(), base: Some(start.clone()) },
        None,
        BayPosition { pallet: "main".to_string(), base: Some(take.clone()) },
    );
    operation.rolling_back = true;
    operation.expected = Some(start.clone());
    operation_utils::raise_in(&root.join(".forklift"), &operation).unwrap();

    forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

    let _ = area.run(&root, &["heal"]);
    assert_eq!(head(&root, "main"), start);
}

/// A warehouse whose working tree is consolidating `side` into `main` with `a.txt` in conflict,
/// resolved and loaded, so the next `stack` completes the merge.
fn consolidating(area: &Area) -> PathBuf {
    let root = area.warehouse();
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.stack(&root, "a.txt", "side's a");
    area.run_ok(&root, &["shift", "main"]);
    area.stack(&root, "a.txt", "main's a");

    let consolidate = area.run(&root, &["consolidate", "side"]);
    assert!(!consolidate.status.success() || text(&consolidate.stdout).contains("onflict"),
        "the fixture must stop at conflicts: {}{}", text(&consolidate.stdout), text(&consolidate.stderr));
    std::fs::write(root.join("a.txt"), "resolved\n").unwrap();
    area.run_ok(&root, &["load", "."]);
    root
}

#[test]
fn stacking_a_merge_over_a_lost_ref_is_refused_as_damaged() {
    // Mutation: let `moved_under_merge` report a missing head as `behind` → red.
    let area = Area::new("merge-lost");
    let root = consolidating(&area);
    forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "stack", "-m", "merge"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{}", refused);
}

/// A warehouse whose working tree is on `main`, with `side` diverged from it and mergeable cleanly.
fn mergeable(area: &Area) -> PathBuf {
    let root = area.warehouse();
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.stack(&root, "s.txt", "side");
    area.run_ok(&root, &["shift", "main"]);
    area.stack(&root, "m.txt", "main");
    root
}

#[test]
fn an_interrupted_consolidation_is_rebuilt_at_its_parcel_and_rolled_forward() {
    // The consolidation stops after its ref write and before its position write; the ref is then
    // destroyed. The marker's completing parcel is the rebuild, and the operation recovery rolls
    // the bay forward onto it.
    // Mutation: drop the marker's completing parcel from the candidates → the rebuild lands at the
    // base, below the merge → red.
    use forklift_core::util::file_utils::CRASH_POINT_EXIT_CODE;

    for at in 1..200 {
        let area = Area::new(&format!("merge-crash-{}", at));
        let root = mergeable(&area);
        let before = head(&root, "main");

        let stopped = forklift_command()
            .args(["consolidate", "side"])
            .current_dir(&root)
            .env("FORKLIFT_GLOBAL_CONFIG", area.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", area.home.join("keys"))
            .env("FORKLIFT_DEBUG_CRASH_AT_RENAME", at.to_string())
            .output()
            .unwrap();
        assert_eq!(stopped.status.code(), Some(CRASH_POINT_EXIT_CODE),
            "consolidate finished before the ref moved past its position: {}{}", text(&stopped.stdout), text(&stopped.stderr));

        let moved = head(&root, "main");
        let position = std::fs::read_to_string(root.join(".forklift").join("position")).unwrap();
        if moved == before || position.contains(&moved) || !root.join(".forklift").join("operation").exists() {
            continue;
        }

        // A stopped process leaves its lock behind, as a power loss would; the operator clears it.
        let _ = forklift_core::util::file_utils::remove_file(root.join(".forklift").join("lock"));
        forklift_core::util::file_utils::remove_file(ref_path(&root, "main")).unwrap();

        let data = healed(&area, &root);
        assert_eq!(head(&root, "main"), moved, "{}", data);
        assert!(std::fs::read_to_string(root.join(".forklift").join("position")).unwrap().contains(&moved),
            "the operation must roll forward onto the merge: {}", data);
        area.stack(&root, "z.txt", "after");
        return;
    }

    panic!("no rename stops the consolidation between its ref write and its position write");
}

/// A rebuilt or chosen head becomes a local head, which a take does not verify again, so it must
/// pass the trust check as an undone delete's does. Unsigned work left out of the enrollment is
/// what the damaged ref's own bytes still name: `heal` refuses to rebuild at it, and so does
/// `--ref`, while a head the office admits is set.
/// Revert: drop the trust check from the rebuild → it lands at the unsigned parcel → red. Drop it
/// from `--ref` → the unsigned parcel is set → red.
#[test]
fn a_head_that_fails_the_trust_check_is_never_written() {
    let area = Area::new("trust");
    let root = area.warehouse();
    let admitted = head(&root, "main");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "side.txt", "unsigned");
    let unsigned = head(&root, "side");
    area.run_ok(&root, &["shift", "main"]);
    area.run_ok(&root, &["pallet", "delete", "side", "--force"]);
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["office", "enroll"]);

    std::fs::write(ref_path(&root, "side"), &unsigned.as_bytes()[..20]).unwrap();

    let refused = area.run(&root, &["--json", "heal"]);
    let value = ref_damaged(&refused);
    assert!(text(&refused.stdout).contains("trust check"), "{}", value);
    assert_eq!(std::fs::read(ref_path(&root, "side")).unwrap(), &unsigned.as_bytes()[..20], "the ref is left alone");

    let set = format!("side={}", unsigned);
    let refused = area.run(&root, &["heal", "--ref", &set]);
    assert!(!refused.status.success());
    assert!(text(&refused.stderr).contains("trust check"), "{}{}", text(&refused.stdout), text(&refused.stderr));
    assert_eq!(std::fs::read(ref_path(&root, "side")).unwrap(), &unsigned.as_bytes()[..20], "the ref is left alone");

    area.run_ok(&root, &["heal", "--ref", &format!("side={}", admitted)]);
    assert_eq!(head(&root, "side"), admitted);
}

/// `pallet create` leaves no journal entry, so the latest entry about a pallet made again after a
/// delete is still that delete. A checkout standing on it proves it was made again (a delete is
/// refused under one), so its lost ref is rebuilt at the checkout's base.
/// Revert: let a journaled delete refuse although a checkout stands on the pallet → red.
#[test]
fn a_pallet_made_again_after_a_delete_is_rebuilt_under_its_checkout() {
    let area = Area::new("made-again");
    let root = area.warehouse();
    let base = head(&root, "main");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.stack(&root, "side.txt", "gone");
    area.run_ok(&root, &["shift", "main"]);
    area.run_ok(&root, &["pallet", "delete", "side", "--force"]);
    area.run_ok(&root, &["pallet", "create", "side"]);
    forklift_core::util::file_utils::remove_file(ref_path(&root, "side")).unwrap();

    let data = healed(&area, &root);
    assert_eq!(head(&root, "side"), base, "{}", data);
}

#[test]
fn a_served_warehouses_ref_is_never_set_below_what_its_own_bytes_name() {
    // Mutation: skip refuse_below_a_floor → the served ref is set back to the older parcel → red.
    let area = Area::new("served-floor");
    let root = area.warehouse();
    area.stack(&root, "b.txt", "older");
    let older = head(&root, "main");
    area.stack(&root, "c.txt", "newer");
    let newer = head(&root, "main");
    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&root);
        assert!(forklift_core::util::tombstone_utils::mark_served(true).unwrap());
    }
    std::fs::write(ref_path(&root, "main"), &newer.as_bytes()[..20]).unwrap();

    let refused = area.run(&root, &["heal", "--ref", &format!("main={}", older)]);
    assert!(!refused.status.success(), "{}", text(&refused.stdout));
    assert!(text(&refused.stderr).contains("served"), "{}", text(&refused.stderr));
    assert_eq!(std::fs::read(ref_path(&root, "main")).unwrap(), newer.as_bytes()[..20].to_vec());

    area.run_ok(&root, &["heal", "--ref", &format!("main={}", newer)]);
    assert_eq!(head(&root, "main"), newer);
}

#[test]
fn meta_history_is_refused_however_spelled_and_a_refusal_names_what_the_run_already_set() {
    // Mutation: drop refuse_meta_history → main is set to the office head → red.
    // Mutation: return the bare refusal from the --ref loop → the earlier rebuild goes unreported → red.
    // Mutation: leave the notes out of that refusal → the skipped @tags check goes unreported → red.
    let area = Area::new("set-partial");
    let (root, side) = side_only_its_bytes_name(&area);
    area.run_ok(&root, &["tag", "create", "v1"]);
    area.run_ok(&root, &["office", "enroll"]);
    let office = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&root);
        forklift_core::util::pallet_utils::get_meta_pallet_head("office").unwrap().expect("an enrolled office has a head")
    };
    let main = head(&root, "main");
    std::fs::write(ref_path(&root, "main"), &main.as_bytes()[..20]).unwrap();

    for spelled in ["@office".to_string(), office.clone(), office[..16].to_string()] {
        let meta = area.run(&root, &["heal", "--ref", &format!("main={}", spelled)]);
        assert!(!meta.status.success(), "{}: {}", spelled, text(&meta.stdout));
        assert!(text(&meta.stderr).contains("meta history"), "{}: {}", spelled, text(&meta.stderr));
        assert_eq!(std::fs::read(ref_path(&root, "main")).unwrap(), main.as_bytes()[..20].to_vec(), "{}", spelled);
    }
    area.run_ok(&root, &["heal", "--ref", &format!("main={}", main)]);

    std::fs::write(ref_path(&root, "side"), &side.as_bytes()[..20]).unwrap();
    let tags = root.join(".forklift").join("pallets").join("@tags");
    let tags = if tags.exists() { tags } else { root.join(".forklift").join("meta").join("tags") };
    assert!(tags.is_file(), "the fixture must find the @tags ref");
    std::fs::write(&tags, b"not a hash").unwrap();
    let partial = area.run(&root, &["heal", "--ref", &format!("side={}", side), "--ref", &format!("main={}", main)]);
    assert!(!partial.status.success(), "{}", text(&partial.stdout));
    let stderr = text(&partial.stderr);
    assert!(stderr.contains("is not damaged") && stderr.contains("Rebuilt the ref of pallet \"side\""), "{}", stderr);
    assert!(stderr.contains("Note: Whether") && stderr.contains("\"@tags\""), "{}", stderr);
    assert_eq!(head(&root, "side"), side);
}

/// The loose file of `hash` in the warehouse at `root`.
fn loose_object(root: &Path, hash: &str) -> PathBuf {
    let (folder, file) = forklift_core::util::file_utils::get_path_for_object(hash).unwrap();
    root.join(folder).join(file)
}

/// The hash of the blob holding `content`.
fn blob_hash(content: &str) -> String {
    use forklift_core::util::object_utils;
    object_utils::hash_object_bytes(&object_utils::blob_bytes(content.as_bytes()))
}

#[test]
fn heal_refuses_as_objects_lost_when_a_reachable_object_is_absent() {
    // An untainted store with one reachable blob deleted: nothing earlier in the run looks for it,
    // so only the closing verdict can say it is lost.
    // Mutation: skip the verdict (`objects_lost` always None) → heal passes → red.
    let area = Area::new("lost");
    let root = area.warehouse();
    healed(&area, &root);

    let hash = blob_hash("one\n");
    let path = loose_object(&root, &hash);
    assert!(path.is_file(), "the fixture must find the blob loose");
    forklift_core::util::file_utils::remove_file(&path).unwrap();

    let output = area.run(&root, &["--json", "heal"]);
    assert_eq!(output.status.code(), Some(58), "{}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "objects_lost", "{}", value);
    let lost = &value["error"]["details"]["objects_lost"];
    assert_eq!(lost["absent"], 1, "{}", value);
    assert_eq!(lost["unreadable"], 0, "{}", value);
    assert_eq!(lost["hashes"][0]["hash"], hash.as_str(), "{}", value);
    assert_eq!(lost["hashes"][0]["reason"], "absent", "{}", value);
    assert_eq!(lost["pallets"], serde_json::json!(["main"]), "{}", value);
    assert!(
        value["error"]["next_step"].as_str().unwrap().starts_with("restore the warehouse from a backup"),
        "{}",
        value
    );

    // The verdict records nothing: the next run says the same.
    let again = area.run(&root, &["--json", "heal"]);
    assert_eq!(again.status.code(), Some(58), "{}", text(&again.stdout));
}

#[test]
fn a_refusing_heal_still_rebuilds_a_lost_ref() {
    // The verdict runs last, so the repairs before it land even when it refuses.
    // Mutation: return the verdict before the ref pass → the ref stays lost → red.
    let area = Area::new("lost-and-ref");
    let root = area.warehouse();
    let before = head(&root, "main");
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "d.txt", "side only");
    area.run_ok(&root, &["shift", "main"]);

    // Lost under `side` alone, so `main`'s history stays complete and its ref can be rebuilt.
    forklift_core::util::file_utils::remove_file(loose_object(&root, &blob_hash("side only\n"))).unwrap();
    damage(&root, "main", Damage::Delete);

    let output = area.run(&root, &["--json", "heal"]);
    assert_eq!(output.status.code(), Some(58), "{}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["details"]["refs_rebuilt"][0]["pallet"], "main", "{}", value);
    assert_eq!(value["error"]["details"]["objects_lost"]["pallets"], serde_json::json!(["side"]), "{}", value);
    assert_eq!(head(&root, "main"), before);
}

#[test]
fn a_reachability_walk_that_cannot_finish_claims_nothing_lost() {
    // A staged shard that does not parse stops the walk: heal says it does not know, and never
    // answers `objects_lost`.
    // Mutation: read a walk error as "nothing lost" → heal passes → red.
    let area = Area::new("walk-error");
    let root = area.warehouse();
    std::fs::write(root.join("c.txt"), "three\n").unwrap();
    area.run_ok(&root, &["load", "."]);
    garble_staged_shards(&root);

    let output = area.run(&root, &["--json", "heal"]);
    assert!(!output.status.success(), "{}", text(&output.stdout));
    let shown = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(shown.contains("heal could not finish its reachability check"), "{}", shown);
    assert!(!shown.contains("objects_lost"), "{}", shown);
}

#[test]
fn a_refused_ref_keeps_its_code_when_the_walk_cannot_finish() {
    // The walk's failure is said, but a ref heal refused still answers `ref_damaged` with its own
    // next step.
    // Mutation: return the walk's untyped error ahead of a refused ref → exit 1 → red.
    let area = Area::new("walk-error-ref");
    let (root, side) = side_only_its_bytes_name(&area);
    std::fs::write(ref_path(&root, "side"), &side.as_bytes()[..7]).unwrap();
    std::fs::write(root.join("c.txt"), "three\n").unwrap();
    area.run_ok(&root, &["load", "."]);
    garble_staged_shards(&root);

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    assert_eq!(refused["error"]["details"]["refs_refused"][0]["pallet"], "side", "{}", refused);
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("heal could not finish its reachability check"), "{}", refused);
}

/// Overwrites every staged inventory shard with bytes that do not parse.
fn garble_staged_shards(root: &Path) {
    let mut shards = Vec::new();
    let mut folders = vec![root.join(".forklift").join("inventory")];
    while let Some(folder) = folders.pop() {
        for entry in std::fs::read_dir(&folder).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                folders.push(path);
            } else if path.file_name().is_some_and(|name| name == "data") {
                shards.push(path);
            }
        }
    }
    assert!(!shards.is_empty(), "the fixture must find a staged shard");
    for shard in &shards {
        std::fs::write(shard, b"not an inventory").unwrap();
    }
}
