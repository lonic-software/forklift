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

/// A damaged ref whose head sits loose in a fan-out folder that cannot be listed: its salvage is
/// unknown, never "names nothing", and the ref is left alone. Skipped as root, which lists any
/// folder.
///
/// Revert: let `object_hashes_with_prefix` read an unlistable folder as empty → the salvage
/// names nothing and the refusal says no more → red.
#[cfg(unix)]
#[test]
fn a_salvage_through_a_fan_out_folder_that_cannot_be_listed_is_unknown() {
    use std::os::unix::fs::PermissionsExt;

    if unsafe { libc::geteuid() } == 0 {
        return;
    }

    let area = Area::new("salvage-unlistable");
    let (root, side) = side_only_its_bytes_name(&area);
    let truncated = side.as_bytes()[..20].to_vec();
    std::fs::write(ref_path(&root, "side"), &truncated).unwrap();
    let folder = root.join(".forklift").join("objects").join(&side[..2]);

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o000)).unwrap();
    let output = area.run(&root, &["--json", "heal"]);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

    let refused = ref_damaged(&output);
    let reason = refused["error"]["details"]["refs_refused"][0]["reason"].as_str().unwrap().to_string();
    assert!(reason.starts_with("its salvage is unknown"), "{}", refused);
    assert!(reason.contains("could not be listed") && reason.contains(&format!("objects/{}", &side[..2])), "{}", reason);
    assert_eq!(std::fs::read(ref_path(&root, "side")).unwrap(), truncated, "the ref must be left alone");

    healed(&area, &root);
    assert_eq!(head(&root, "side"), side, "once the folder lists, the salvage finds the head");
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
    let removed = &value["data"]["refs_removed"][0];
    let kept = root.join(removed["kept_ref"].as_str().unwrap());
    assert_eq!(std::fs::read(&kept).unwrap(), zeros);
    assert!(removed["kept_copy"].is_null(), "a user pallet has no copy: {}", value);
    assert!(!ref_path(&root, "side").exists());

    // A numbered slot holding the same bytes may be another pallet's (here a pallet "side.1"'s
    // removal), so a second removal of "side" keeps its bytes in a free one.
    // Mutation: reuse a slot holding the same bytes for a user ref → kept in side.1 → red.
    let removed_refs = root.join(".forklift").join("removed-refs");
    std::fs::write(removed_refs.join("side"), b"other").unwrap();
    std::fs::write(removed_refs.join("side.1"), &zeros).unwrap();
    std::fs::write(ref_path(&root, "side"), &zeros).unwrap();
    let output = area.run_ok(&root, &["--json", "heal", "--remove-ref", "side"]);
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let kept = value["data"]["refs_removed"][0]["kept_ref"].as_str().unwrap().to_string();
    assert!(kept.ends_with("side.2"), "{}", value);
    assert_eq!(std::fs::read(removed_refs.join("side.2")).unwrap(), zeros);

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
    // Without its copy, heal cannot rebuild @tags, so the walk meets it unreadable.
    forklift_core::util::file_utils::remove_file(&root.join(".forklift").join("meta-copy").join("tags")).unwrap();

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
    // Without its copy, nothing names the head of @tags, so heal leaves it damaged.
    forklift_core::util::file_utils::remove_file(&root.join(".forklift").join("meta-copy").join("tags")).unwrap();
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

    // The working file no longer holds it, so heal cannot store it again from there.
    std::fs::write(root.join("a.txt"), "changed\n").unwrap();
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
    assert_eq!(lost["corrupt"], 0, "{}", value);
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

/// The `objects_lost` refusal of a `--json heal` run in `dir`, as its `error` object.
fn lost_refusal(area: &Area, dir: &Path) -> serde_json::Value {
    let output = area.run(dir, &["--json", "heal"]);
    assert_eq!(output.status.code(), Some(58), "{}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error"]["code"], "objects_lost", "{}", value);
    value["error"].clone()
}

/// Delete the loose file of `hash` under `root`.
fn lose(root: &Path, hash: &str) {
    let path = loose_object(root, hash);
    assert!(path.is_file(), "the fixture must find {} loose", hash);
    forklift_core::util::file_utils::remove_file(&path).unwrap();
}

#[test]
fn heal_stores_a_lost_blob_again_from_the_working_file_that_holds_it() {
    // FORK-493: a lost blob the working tree still holds, at a path the inventory lists with that
    // hash, is stored again from the file, whether history or only a staged load reaches it, and
    // the run passes. Nothing else is written.
    // Mutation: skip the restore (heal refuses objects_lost) → red.
    let area = Area::new("restore-working");
    let root = area.warehouse();
    healed(&area, &root);
    std::fs::write(root.join("c.txt"), "staged\n").unwrap();
    area.run_ok(&root, &["load", "c.txt"]);

    lose(&root, &blob_hash("one\n"));
    lose(&root, &blob_hash("staged\n"));

    let data = healed(&area, &root);
    assert_eq!(data["restored_from_working_tree"], serde_json::json!(["a.txt", "c.txt"]), "{}", data);
    assert!(loose_object(&root, &blob_hash("one\n")).is_file());
    assert!(loose_object(&root, &blob_hash("staged\n")).is_file());

    let human = area.run_ok(&root, &["heal"]);
    assert!(text(&human.stdout).contains("Nothing to heal"), "{}", text(&human.stdout));
}

#[test]
fn heal_names_unload_when_only_a_staged_load_reaches_a_lost_blob() {
    // FORK-493: a lost blob only a load never stacked reaches, whose file has changed or gone, is
    // the user's to unstage: the refusal says so, names the path and gives `forklift unload`,
    // never a backup, and the unload mends it.
    // Mutation: drop the staged-only arm of the next step (backup again) → red.
    // A parent folder replaced by a file reads as the path gone, never as a file that may hold it.
    for (case, path) in [("changed", "a.txt"), ("gone", "a.txt"), ("parent-a-file", "d/a.txt")] {
        let area = Area::new(&format!("staged-{}", case));
        let root = area.warehouse();
        std::fs::create_dir_all(root.join("d")).unwrap();
        std::fs::write(root.join(path), "staged\n").unwrap();
        area.run_ok(&root, &["load", path]);
        lose(&root, &blob_hash("staged\n"));
        match case {
            "gone" => forklift_core::util::file_utils::remove_file(root.join(path)).unwrap(),
            "changed" => std::fs::write(root.join(path), "changed\n").unwrap(),
            _ => {
                forklift_core::util::file_utils::remove_dir_all(root.join("d")).unwrap();
                std::fs::write(root.join("d"), "a file now\n").unwrap();
            }
        }

        let error = lost_refusal(&area, &root);
        let lost = &error["details"]["objects_lost"];
        assert_eq!(lost["staged_only"], true, "{}", error);
        assert_eq!(lost["staged"], serde_json::json!([path]), "{}", error);
        assert_eq!(lost["pallets"], serde_json::json!([]), "{}", error);
        assert!(error["details"].get("restored_from_working_tree").is_none(), "nothing else is stored: {}", error);
        assert!(!error["details"]["notes"].to_string().contains("may hold"), "{}", error);
        assert_eq!(
            error["next_step"],
            format!(
                "from the working tree's root, run \"forklift unload {}\", then load the file again if you mean to stage it",
                path
            ),
            "{}",
            error
        );
        let message = error["message"].as_str().unwrap();
        assert!(message.contains("only staged content still reaches"), "{}", message);
        assert!(!message.contains("other refs"), "{}", message);

        area.run_ok(&root, &["unload", path]);
        healed(&area, &root);
    }
}

#[test]
fn heal_warns_that_a_damaged_staged_object_stays_lost_when_loaded_again() {
    // FORK-493: a staged-only object held by a corrupt loose file gets the unload step too, and
    // the refusal says that loading the same content again keeps the damaged file, as it does.
    // Mutation: drop the damaged-file line → red.
    let area = Area::new("staged-corrupt");
    let root = area.warehouse();
    std::fs::write(root.join("a.txt"), "staged\n").unwrap();
    area.run_ok(&root, &["load", "a.txt"]);
    std::fs::write(loose_object(&root, &blob_hash("staged\n")), "garbage").unwrap();

    let error = lost_refusal(&area, &root);
    assert_eq!(error["details"]["objects_lost"]["corrupt"], 1, "{}", error);
    assert!(error["next_step"].as_str().unwrap().contains("\"forklift unload a.txt\""), "{}", error);
    assert!(error["message"].as_str().unwrap().contains("loading the same content again keeps"), "{}", error);

    area.run_ok(&root, &["load", "a.txt"]);
    lost_refusal(&area, &root);
}

#[test]
fn heal_keeps_the_backup_step_while_history_reaches_a_lost_object() {
    // FORK-493: a staged-only loss beside one history reaches gets the backup step, since
    // unloading does not bring the history's object back, and no staged path is claimed: the walk
    // stops at what it lacks, so history may reach that content too.
    // Mutation: name unload whenever anything is staged only (ignore `staged_only`), or report the
    // staged paths short of all → red.
    let area = Area::new("staged-and-history");
    let root = area.warehouse();
    std::fs::write(root.join("a.txt"), "staged\n").unwrap();
    area.run_ok(&root, &["load", "a.txt"]);
    std::fs::write(root.join("a.txt"), "changed\n").unwrap();
    std::fs::write(root.join("b.txt"), "changed too\n").unwrap();
    lose(&root, &blob_hash("staged\n"));
    lose(&root, &blob_hash("two\n"));

    let error = lost_refusal(&area, &root);
    let lost = &error["details"]["objects_lost"];
    assert_eq!(lost["absent"], 2, "{}", error);
    assert_eq!(lost["staged_only"], false, "{}", error);
    assert!(lost.get("staged").is_none(), "{}", error);
    assert_eq!(lost["pallets"], serde_json::json!(["main"]), "{}", error);
    assert!(error["next_step"].as_str().unwrap().starts_with("restore the warehouse from a backup"), "{}", error);
}

#[test]
fn heal_names_the_bay_whose_staged_load_reaches_a_lost_blob() {
    // FORK-493: content only another checkout's staged load reaches is that checkout's to mend:
    // heal here names the bay and has it run heal there, which stores the content again from its
    // own working tree.
    // Mutation: skip the other checkouts' inventories (no staged_elsewhere, backup step) → red.
    let area = Area::new("staged-bay");
    let root = area.warehouse();
    let bay = area.base.join("bay");
    area.run_ok(&root, &["bay", "add", "side", bay.to_str().unwrap()]);
    std::fs::write(bay.join("c.txt"), "in the bay\n").unwrap();
    area.run_ok(&bay, &["load", "c.txt"]);
    lose(&root, &blob_hash("in the bay\n"));

    let error = lost_refusal(&area, &root);
    let lost = &error["details"]["objects_lost"];
    assert_eq!(lost["staged_only"], true, "{}", error);
    assert!(lost.get("staged").is_none(), "{}", error);
    assert_eq!(lost["staged_elsewhere"].as_array().map(Vec::len), Some(1), "{}", error);
    assert!(lost["staged_elsewhere"][0].as_str().unwrap().starts_with("bay \"side\""), "{}", error);
    assert_eq!(error["next_step"], "from each checkout named above, run \"forklift heal\"", "{}", error);

    // Lost here too: the step unloads this checkout's path, then has the bay run heal.
    // Mutation: leave the other checkouts out of the unload step → red.
    std::fs::write(root.join("d.txt"), "here\n").unwrap();
    area.run_ok(&root, &["load", "d.txt"]);
    lose(&root, &blob_hash("here\n"));
    std::fs::write(root.join("d.txt"), "changed\n").unwrap();
    let error = lost_refusal(&area, &root);
    assert_eq!(error["details"]["objects_lost"]["staged"], serde_json::json!(["d.txt"]), "{}", error);
    assert_eq!(
        error["next_step"],
        "from the working tree's root, run \"forklift unload d.txt\", then load the file again if you mean to stage it; \
        then, from each other checkout named above, run \"forklift heal\"",
        "{}",
        error
    );
    area.run_ok(&root, &["unload", "d.txt"]);

    let data = healed(&area, &bay);
    assert_eq!(data["restored_from_working_tree"], serde_json::json!(["c.txt"]), "{}", data);
    healed(&area, &root);
}

#[test]
fn heal_stores_a_lost_chunk_again_from_the_working_file() {
    // FORK-493: a chunked file's lost chunk is content its recipe reaches, so heal stores it again
    // from the working file that still holds it.
    // Mutation: match entries by their own hash only, never a recipe's chunks → red.
    let area = Area::new("restore-chunk");
    let root = area.warehouse();
    let objects = root.join(".forklift").join("objects");
    let before = loose_files(&objects);

    let mut state: u64 = 0x9e37_79b9_7f4a_7c15;
    let content: Vec<u8> = (0..9 * 1024 * 1024)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 56) as u8
        })
        .collect();
    std::fs::write(root.join("big.bin"), &content).unwrap();
    area.run_ok(&root, &["load", "big.bin"]);

    let (chunk, size) = loose_files(&objects)
        .into_iter()
        .filter(|(path, _)| !before.iter().any(|(old, _)| old == path))
        .max_by_key(|(_, size)| *size)
        .expect("the load wrote objects");
    assert!(size > 64 * 1024, "the largest new object must be a chunk, not the recipe");
    forklift_core::util::file_utils::remove_file(&chunk).unwrap();

    let data = healed(&area, &root);
    assert_eq!(data["restored_from_working_tree"], serde_json::json!(["big.bin"]), "{}", data);
    assert!(chunk.is_file());
}

/// Every loose object file under `objects`, with its size.
fn loose_files(objects: &Path) -> Vec<(PathBuf, u64)> {
    let mut files = Vec::new();
    for fan in std::fs::read_dir(objects).unwrap().flatten() {
        if fan.path().is_dir() {
            for file in std::fs::read_dir(fan.path()).unwrap().flatten() {
                files.push((file.path(), file.metadata().unwrap().len()));
            }
        }
    }
    files
}

#[test]
fn a_corrupt_loose_object_refuses_reads_and_heal_names_it_lost() {
    // FORK-447: a reachable blob whose loose file is corrupt fails a read with a code naming
    // heal, and heal, whose walk reads no leaf's bytes, names it lost instead of passing. The
    // file is kept.
    // Mutation: leave a corrupt loose read unframed (exit 1), or skip heal's loose scan (heal
    // passes) → red.
    let area = Area::new("corrupt-loose");
    let root = area.warehouse();
    healed(&area, &root);

    let hash = blob_hash("one\n");
    let path = loose_object(&root, &hash);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0xff;
    std::fs::write(&path, &bytes).unwrap();

    let out = area.base.join("exported");
    let read = area.run(&root, &["--json", "git", "export", out.to_str().unwrap()]);
    assert_eq!(read.status.code(), Some(57), "{}{}", text(&read.stdout), text(&read.stderr));
    let value: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(value["error"]["code"], "object_unreadable", "{}", value);
    assert_eq!(value["error"]["next_step"], "forklift heal", "{}", value);

    let output = area.run(&root, &["--json", "heal"]);
    assert_eq!(output.status.code(), Some(58), "{}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let lost = &value["error"]["details"]["objects_lost"];
    assert_eq!((&lost["absent"], &lost["unreadable"], &lost["corrupt"]), (&serde_json::json!(0), &serde_json::json!(0), &serde_json::json!(1)), "{}", value);
    assert_eq!(lost["hashes"][0]["hash"], hash.as_str(), "{}", value);
    assert_eq!(lost["hashes"][0]["reason"], "corrupt", "{}", value);
    assert_eq!(lost["pallets"], serde_json::json!(["main"]), "{}", value);
    assert_eq!(std::fs::read(&path).unwrap(), bytes, "heal keeps the corrupt file");

    let human = area.run(&root, &["heal"]);
    assert!(text(&human.stderr).contains("0 absent, 0 unreadable, 1 corrupt"), "{}", text(&human.stderr));

    // A damaged file no history needs is not counted lost, only noted as left in place.
    let stray = loose_object(&root, &blob_hash("never stored\n"));
    std::fs::create_dir_all(stray.parent().unwrap()).unwrap();
    std::fs::write(&stray, b"not zstd").unwrap();
    let again = area.run(&root, &["--json", "heal"]);
    assert_eq!(again.status.code(), Some(58), "{}", text(&again.stdout));
    let value: serde_json::Value = serde_json::from_slice(&again.stdout).unwrap();
    assert_eq!(value["error"]["details"]["objects_lost"]["corrupt"], 1, "{}", value);
    assert!(value["error"]["details"]["notes"].as_array().unwrap().iter()
        .any(|note| note.as_str().unwrap().starts_with("1 damaged loose object file(s) that no history needs are left in place")), "{}", value);
}

#[test]
fn a_damaged_pack_record_refuses_reads_and_heal_names_it_lost() {
    // FORK-495: once the store is compacted, a flipped byte in the last record of the pack data
    // file, past every header, fails a read with a code naming heal, and heal names the object
    // lost as corrupt instead of passing or aborting its walk. The pack is kept.
    // Mutation: leave a damaged record's read unframed (exit 1), or skip heal's pack scan (heal
    // passes, or its walk fails on a damaged tree) → red.
    let area = Area::new("damaged-record");
    let root = area.warehouse();
    healed(&area, &root);
    area.run_ok(&root, &["store", "compact"]);

    let pack = std::fs::read_dir(root.join(".forklift").join("objects").join("pack")).unwrap().flatten()
        .map(|entry| entry.path()).find(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .expect("compact writes a pack");
    let mut bytes = std::fs::read(&pack).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&pack, &bytes).unwrap();

    let out = area.base.join("exported");
    let read = area.run(&root, &["--json", "git", "export", out.to_str().unwrap()]);
    assert_eq!(read.status.code(), Some(57), "{}{}", text(&read.stdout), text(&read.stderr));
    let value: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(value["error"]["code"], "object_unreadable", "{}", value);
    assert_eq!(value["error"]["next_step"], "forklift heal", "{}", value);

    let output = area.run(&root, &["--json", "heal"]);
    assert_eq!(output.status.code(), Some(58), "{}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let lost = &value["error"]["details"]["objects_lost"];
    assert_eq!((&lost["absent"], &lost["unreadable"]), (&serde_json::json!(0), &serde_json::json!(0)), "{}", value);
    assert!(lost["corrupt"].as_u64().unwrap() >= 1, "{}", value);
    assert!(lost["hashes"].as_array().unwrap().iter().all(|entry| entry["reason"] == "corrupt"), "{}", value);
    assert_eq!(std::fs::read(&pack).unwrap(), bytes, "heal keeps the damaged pack");
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

// A meta pallet's ref, and the copy every meta head write leaves beside it.

fn meta_path(root: &Path, pallet: &str) -> PathBuf {
    root.join(".forklift").join("meta").join(pallet)
}

fn meta_copy_path(root: &Path, pallet: &str) -> PathBuf {
    root.join(".forklift").join("meta-copy").join(pallet)
}

fn meta_head(root: &Path, pallet: &str) -> Option<String> {
    std::fs::read_to_string(meta_path(root, pallet)).ok().map(|bytes| bytes.trim_end().to_string())
}

/// A prepared warehouse with tag `v1`, then tag `v2`: `@tags` at H2, whose first parent is H1.
/// Returns the warehouse, H1 and H2.
fn tagged(area: &Area) -> (PathBuf, String, String) {
    let root = area.warehouse();
    area.run_ok(&root, &["tag", "create", "v1", "main"]);
    let first = meta_head(&root, "tags").unwrap();
    area.run_ok(&root, &["tag", "create", "v2", "main"]);
    let second = meta_head(&root, "tags").unwrap();
    (root, first, second)
}

/// The meta refs `data` says were rebuilt; an empty list is left out of the report.
fn rebuilt_meta(data: &serde_json::Value) -> Vec<(String, String, String)> {
    data["refs_rebuilt"].as_array().into_iter().flatten()
        .filter(|rebuilt| rebuilt["pallet"].as_str().unwrap().starts_with('@'))
        .map(|rebuilt| (rebuilt["pallet"].as_str().unwrap().to_string(), rebuilt["head"].as_str().unwrap().to_string(), rebuilt["source"].as_str().unwrap().to_string()))
        .collect()
}

// Mutation: skip both meta passes in heal → @tags stays lost, red. Either pass alone rebuilds it
// here; the pass ahead of `--ref` and the refusing pass after the taint are pinned below.
#[test]
fn a_lost_meta_ref_is_rebuilt_from_its_copy() {
    let area = Area::new("meta-lost");
    let (root, _, second) = tagged(&area);

    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
    // Strays beside the copies, under a name no pallet may have and under a valid one no meta
    // pallet has, are neither pallets nor a reason to stop. Mutation: list a stray as a copy → the
    // heal refuses it, red.
    std::fs::write(meta_copy_path(&root, "Tags~"), b"stray").unwrap();
    std::fs::write(meta_copy_path(&root, "tags.bak"), format!("{}\n", second)).unwrap();
    // And in the refs' folder: a stray there is no pallet to mend a copy for. Mutation: take the
    // names from the refs' folder → heal writes a copy for it, red.
    std::fs::write(meta_path(&root, "tags.orig"), format!("{}\n", second)).unwrap();
    // A folder at an unborn pallet's copy names no head: the pallet stays unborn. Mutation: judge
    // it as a lost ref → refused, red.
    std::fs::create_dir_all(meta_copy_path(&root, "kv").join("in-the-way")).unwrap();

    let data = healed(&area, &root);
    assert_eq!(rebuilt_meta(&data), vec![("@tags".to_string(), second.clone(), "the ref's copy".to_string())], "{}", data);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()));
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), format!("{}\n", second).into_bytes());

    let listed = text(&area.run_ok(&root, &["tag", "list"]).stdout);
    assert!(listed.contains("v1") && listed.contains("v2"), "{}", listed);

    assert!(!meta_copy_path(&root, "tags.orig").exists(), "a stray ref gets no copy");
    assert!(meta_head(&root, "kv").is_none(), "@kv stays unborn");

    // A second heal finds nothing to do.
    assert!(rebuilt_meta(&healed(&area, &root)).is_empty());
}

/// What `--remove-ref` kept of meta pallet `pallet`'s files: the ref's bytes, then the copy's.
fn kept_meta(root: &Path, pallet: &str) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let kept = root.join(".forklift").join("removed-refs");
    (std::fs::read(kept.join(format!("@{}", pallet))).ok(), std::fs::read(kept.join(format!("@{}.copy", pallet))).ok())
}

/// `forklift tag list` in `root`, as JSON.
fn tag_list(area: &Area, root: &Path) -> Output {
    area.run(root, &["--json", "tag", "list"])
}

fn tag_names(output: &Output) -> Vec<String> {
    assert!(output.status.success(), "tag list failed: {}{}", text(&output.stdout), text(&output.stderr));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    value["data"]["tags"].as_array().unwrap().iter().map(|tag| tag["name"].as_str().unwrap().to_string()).collect()
}

// A lost meta ref is refused where it is read, never read as unborn, while its copy stands. The
// read names heal, which judges whose the refusal is: it rebuilds from a copy that parses, and
// otherwise names the removal, which keeps the copy's bytes and leaves the pallet unborn. A read
// never names the removal itself: only heal judges what admits it. Mutations: read a missing
// ref as unborn whatever its copy → `tag list` prints no tags, red; have the read name the
// removal for a copy that does not parse → the second case red; refuse with no copy → the
// third case red.
#[test]
fn a_lost_meta_ref_is_refused_where_it_is_read_while_its_copy_stands() {
    let area = Area::new("meta-loud");
    let root = area.warehouse();
    area.run_ok(&root, &["tag", "create", "v1"]);
    let head = meta_head(&root, "tags").unwrap();

    // The copy parses: the step is heal, and heal rebuilds the ref so the tag reads again.
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
    let refused = ref_damaged(&tag_list(&area, &root));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.starts_with("The ref of pallet \"@tags\" (") && message.contains("is lost: its file is missing, and its copy (") && message.ends_with(") still stands. Nothing was changed."), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{}", refused);
    healed(&area, &root);
    assert_eq!(tag_names(&tag_list(&area, &root)), vec!["v1".to_string()]);

    // The copy does not parse: the read names heal, which has nothing to rebuild from, so its
    // step is the removal, which keeps the copy's bytes and leaves the pallet unborn, as a ref
    // lost with its copy reads.
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
    std::fs::write(meta_copy_path(&root, "tags"), b"not a hash").unwrap();
    let refused = ref_damaged(&tag_list(&area, &root));
    assert!(refused["error"]["message"].as_str().unwrap().contains(") stands but does not hold a parcel hash. Nothing was changed."), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{}", refused);
    let judged = ref_damaged(&area.run(&root, &["--json", "heal"]));
    assert_eq!(judged["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", judged);
    let removed = area.run_ok(&root, &["--json", "heal", "--remove-ref", "@tags"]);
    let removed: serde_json::Value = serde_json::from_slice(&removed.stdout).unwrap();
    assert_eq!(removed["data"]["refs_removed"][0]["pallet"], "@tags", "{}", removed);
    assert!(removed["data"]["refs_removed"][0]["kept_ref"].is_null(), "a lost ref keeps nothing: {}", removed);
    let kept_copy = Path::new(removed["data"]["refs_removed"][0]["kept_copy"].as_str().unwrap());
    assert_eq!(kept_copy.file_name().unwrap(), "@tags.copy", "{}", removed);
    assert_eq!(kept_meta(&root, "tags"), (None, Some(b"not a hash".to_vec())), "the copy's bytes are kept; the ref had none");
    assert!(!meta_path(&root, "tags").exists() && !meta_copy_path(&root, "tags").exists());
    let listed = area.run_ok(&root, &["tag", "list"]);
    assert!(text(&listed.stdout).contains("No tags yet"), "{}", text(&listed.stdout));

    // The ref put back by hand reads again.
    std::fs::write(meta_path(&root, "tags"), format!("{}\n", head)).unwrap();
    assert_eq!(tag_names(&tag_list(&area, &root)), vec!["v1".to_string()]);
}

// A damaged meta ref with no copy whose own bytes still name its head names heal, which rebuilds
// it from them. Mutation: have heal ignore what the ref's own bytes name → heal refuses, red.
#[test]
fn a_damaged_meta_ref_whose_bytes_salvage_names_heal_with_no_copy() {
    let area = Area::new("meta-salvage-step");
    let root = area.warehouse();
    area.run_ok(&root, &["tag", "create", "v1"]);
    let head = meta_head(&root, "tags").unwrap();

    forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "tags")).unwrap();
    std::fs::write(meta_path(&root, "tags"), &head.as_bytes()[..12]).unwrap();

    let refused = ref_damaged(&tag_list(&area, &root));
    assert!(refused["error"]["message"].as_str().unwrap().contains("is damaged: it does not hold a parcel hash."), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal", "{}", refused);

    healed(&area, &root);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(head.as_str()));
    assert_eq!(tag_names(&tag_list(&area, &root)), vec!["v1".to_string()]);

    // Bytes that name nothing, with no copy, leave heal nothing: the read names heal, whose step
    // is the removal.
    forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "tags")).unwrap();
    std::fs::write(meta_path(&root, "tags"), b"not a hash").unwrap();
    assert_eq!(ref_damaged(&tag_list(&area, &root))["error"]["next_step"], "forklift heal");
    assert_eq!(ref_damaged(&area.run(&root, &["--json", "heal"]))["error"]["next_step"], "forklift heal --remove-ref @tags");
}

// A store sweep after a meta ref is lost refuses rather than delete the parcels only that ref
// held: the listing names the pallet by its copy, so the sweep's fail-closed read meets the
// refusal. Mutation: list meta pallets by their ref files alone → the sweep runs and deletes the
// @tags parcel, red.
#[test]
fn a_sweep_after_a_meta_ref_is_lost_refuses_and_keeps_what_it_held() {
    let area = Area::new("meta-lost-gc");
    let root = area.warehouse();
    area.run_ok(&root, &["tag", "create", "v1"]);
    let head = meta_head(&root, "tags").unwrap();
    let (folder, file) = forklift_core::util::file_utils::get_path_for_object(&head).unwrap();
    let parcel = root.join(folder).join(file);
    assert!(parcel.is_file(), "the fixture must find the @tags parcel loose");

    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

    let swept = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&root);
        forklift_core::util::gc_utils::collect_garbage(0)
    };
    let error = swept.err().expect("the sweep must refuse over a lost meta ref");
    assert!(forklift_core::error::rendered_frames(&error).contains("is lost"), "{}", error);
    assert!(parcel.is_file(), "the @tags parcel must be kept");
}

// Mutation: prefer what the ref's bytes name over a copy descending from it → rebuilt at H1, red.
#[test]
fn a_damaged_meta_ref_takes_its_copy_when_the_copy_descends_from_what_its_bytes_name() {
    let area = Area::new("meta-ahead");
    let (root, first, second) = tagged(&area);

    // The ref's bytes still name H1, by a prefix; the copy names H2, which descends from it.
    std::fs::write(meta_path(&root, "tags"), &first.as_bytes()[..16]).unwrap();

    let data = healed(&area, &root);
    assert_eq!(rebuilt_meta(&data), vec![("@tags".to_string(), second.clone(), "the ref's copy".to_string())], "{}", data);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()));
}

// Mutation: take the copy whatever it descends from → rebuilt at H1, red.
#[test]
fn a_copy_that_does_not_descend_from_what_the_ref_names_is_refused_with_both() {
    let area = Area::new("meta-conflict");
    let (root, first, second) = tagged(&area);

    // The copy is put back at H1, which does not descend from H2, what the ref's bytes name.
    std::fs::write(meta_copy_path(&root, "tags"), format!("{}\n", first)).unwrap();
    std::fs::write(meta_path(&root, "tags"), &second.as_bytes()[..16]).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("The ref of pallet \"@tags\" was not rebuilt: its copy names") && message.contains(&first) && message.contains(&second), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", refused);
    assert_eq!(std::fs::read(meta_path(&root, "tags")).unwrap(), second.as_bytes()[..16].to_vec(), "the ref is left as it was");

    // Beside a refused user ref, which nothing about @tags blocks, --ref is the step. Mutation:
    // name the removal for any refused meta ref → red.
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "s.txt", "side");
    area.run_ok(&root, &["shift", "main"]);
    area.stack(&root, "m.txt", "main");
    let side = head(&root, "side");
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("journal.json")).unwrap();
    std::fs::write(ref_path(&root, "main"), &side.as_bytes()[..20]).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal --ref main=<parcel>", "{}", refused);
}

// Mutation: skip the verification of the chosen head → the user parcel is written as @tags, red.
#[test]
fn a_copy_naming_a_parcel_that_fails_verification_is_refused_not_written() {
    let area = Area::new("meta-unverified");
    let (root, _, _) = tagged(&area);

    // A user parcel is no @tags history.
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
    std::fs::write(meta_copy_path(&root, "tags"), format!("{}\n", head(&root, "main"))).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("The ref of pallet \"@tags\" was not rebuilt: parcel") && message.contains("does not pass verification"), "{}", refused);
    assert!(!meta_path(&root, "tags").exists(), "nothing was written");

    // The refusal is the ref's own, so it names the removal and what that costs, which then runs.
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", refused);
    assert!(message.contains("Its removal leaves the pallet empty"), "{}", refused);
    area.run_ok(&root, &["heal", "--remove-ref", "@tags"]);
    assert_eq!(kept_meta(&root, "tags"), (None, Some(format!("{}\n", head(&root, "main")).into_bytes())));
    assert!(tag_names(&tag_list(&area, &root)).is_empty());
}

// A removal stopped after the ref went says so, with where its bytes are kept, and a rerun keeps
// the copy's bytes in the file the first run kept them in. Mutations: report the stop as removing
// nothing → no "Removed the damaged ref" sentence, red; drop the stopped removal's entry → same,
// red; keep in a fresh file whatever stands → @tags.copy.1, red.
#[cfg(unix)]
#[test]
fn a_meta_removal_stopped_after_the_ref_says_what_it_removed_and_a_rerun_keeps_the_same_file() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("meta-remove-partial");
    let (root, _, _) = tagged(&area);
    let copy = format!("{}\n", head(&root, "main")).into_bytes();
    std::fs::write(meta_path(&root, "tags"), b"not a hash").unwrap();
    std::fs::write(meta_copy_path(&root, "tags"), &copy).unwrap();
    let folder = meta_copy_path(&root, "tags").parent().unwrap().to_path_buf();

    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();
    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("Removed the damaged ref of pallet \"@tags\", keeping its bytes in"), "{}", said);
    assert!(said.contains("was removed, but its copy (") && said.contains("still stands: run \"forklift heal\" again"), "{}", said);
    assert!(!meta_path(&root, "tags").exists());
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), copy, "the copy stands");
    assert_eq!(kept_meta(&root, "tags"), (Some(b"not a hash".to_vec()), Some(copy.clone())));

    // The named step: heal judges the copy alone, refuses it as its own, and its removal keeps
    // the copy's bytes in the same file.
    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", refused);
    let removal = area.run_ok(&root, &["heal", "--remove-ref", "@tags"]);
    assert!(text(&removal.stdout).contains("Removed the lost ref of pallet \"@tags\", and its copy, keeping the copy's bytes in \""), "{}", text(&removal.stdout));
    assert!(!meta_copy_path(&root, "tags").exists());
    assert!(!root.join(".forklift").join("removed-refs").join("@tags.copy.1").exists(), "kept once");
    assert_eq!(kept_meta(&root, "tags"), (Some(b"not a hash".to_vec()), Some(copy)));
}

// A damaged ref beside a copy path nothing can be kept from has one way out, said alike by heal, its
// removal and a read of the pallet: the removal is no step there, and its cost is not said.
// Mutation: name the removal whatever stands at the copy's path → next_step is the removal, red.
#[cfg(unix)]
#[test]
fn a_damaged_meta_ref_beside_a_blocked_copy_path_has_one_way_out() {
    let area = Area::new("meta-copy-blocked");
    let (root, _, _) = tagged(&area);
    let copy = meta_copy_path(&root, "tags");
    let copy_bytes = std::fs::read(&copy).unwrap();
    std::fs::write(meta_path(&root, "tags"), b"not a hash").unwrap();
    forklift_core::util::file_utils::remove_file(&copy).unwrap();
    std::os::unix::fs::symlink(root.join("nowhere"), &copy).unwrap();

    let way_out = format!(
        "Replace what stands at the copy's path (\"{}\") with the copy's file, or restore the ref's file from a backup, then run \"forklift heal\" again. \
        With neither, moving it aside leaves the pallet empty, as the ref's removal would.",
        Path::new(".forklift").join("meta-copy").join("tags").to_string_lossy()
    );
    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let next_step = refused["error"]["next_step"].as_str().unwrap();
    assert!(next_step.ends_with(&way_out[way_out.find("meta-copy").unwrap()..]) && next_step.starts_with("Replace what stands"), "{}", refused);
    assert!(!refused["error"]["message"].as_str().unwrap().contains("Its removal leaves the pallet empty"), "{}", refused);

    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(text(&removal.stderr).contains(next_step), "{}", text(&removal.stderr));

    let read = tag_list(&area, &root);
    assert!(!read.status.success());
    let read: serde_json::Value = serde_json::from_slice(&read.stdout).unwrap();
    assert_eq!(read["error"]["next_step"], next_step.replace("run \"forklift heal\" again", "run the command again"), "{}", read);

    // The named way out: the copy's file back at its path, heal rebuilds the ref from it.
    forklift_core::util::file_utils::remove_file(&copy).unwrap();
    std::fs::write(&copy, &copy_bytes).unwrap();
    let data = healed(&area, &root);
    assert_eq!(rebuilt_meta(&data).len(), 1, "{}", data);
    assert_eq!(tag_names(&tag_list(&area, &root)), vec!["v1", "v2"]);
}

// A removal of a meta ref heal did not judge says why from that pallet's own note, never
// another's. Mutation: read any note as this pallet's → @kv is told heal could not judge it, red.
#[cfg(unix)]
#[test]
fn a_meta_removal_heal_did_not_judge_says_why_from_that_pallets_own_note() {
    use std::os::unix::fs::PermissionsExt;

    let area = Area::new("meta-remove-not-judged");
    let (root, _, _) = tagged(&area);
    let copy = meta_copy_path(&root, "tags");
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o000)).unwrap();

    let unborn = area.run(&root, &["heal", "--remove-ref", "@kv"]);
    let tags = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o644)).unwrap();

    let said = text(&unborn.stderr);
    assert!(!unborn.status.success(), "{}", text(&unborn.stdout));
    assert!(said.contains("Pallet \"@kv\" has neither a ref nor a copy, so it reads as unborn and there is nothing to remove."), "{}", said);
    assert!(!said.contains("did not judge the ref of pallet \"@kv\""), "{}", said);

    let said = text(&tags.stderr);
    assert!(!tags.status.success(), "{}", text(&tags.stdout));
    assert!(said.contains("Heal did not judge the ref of pallet \"@tags\", for what the note says, so it was not removed"), "{}", said);
    assert_eq!(said.matches("The copy of pallet \"@tags\"'s ref could not be read").count(), 1, "said once: {}", said);
    assert!(meta_path(&root, "tags").exists(), "nothing was removed");

    // The named step: with what the note names settled, the removal runs, and judges the ref.
    std::fs::write(meta_path(&root, "tags"), b"not a hash").unwrap();
    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    assert!(text(&removal.stderr).contains("The ref of pallet \"@tags\" reads, so there is nothing to remove."), "{}", text(&removal.stderr));
    assert!(text(&removal.stderr).contains("Rebuilt the ref of pallet \"@tags\""), "{}", text(&removal.stderr));
    assert_eq!(tag_names(&tag_list(&area, &root)), vec!["v1", "v2"]);
}

// The removal of a meta ref heal refuses as its own, with no taint standing; and refused, writing
// nothing, while a taint stands. Mutations: drop the taint condition from the meta admission →
// "tainted" red (the kept bytes land, and the write reports the taint); refuse a meta ref whose
// salvage is unknown, as a user ref's removal does → "unknown" red; read an object whose file could
// not be read as one the store holds damaged → "untold" next_step is the removal, red.
#[test]
fn a_meta_ref_heal_refuses_as_its_own_is_removed_with_no_taint_standing() {
    let cases: &[&str] = if cfg!(unix) { &["tainted", "unknown", "untold"] } else { &["tainted", "unknown"] };
    for &case in cases {
        let area = Area::new(&format!("meta-remove-{}", case));
        let (root, _, _) = tagged(&area);
        let tags = meta_head(&root, "tags").unwrap();

        match case {
            // The ref lost beside a copy that does not parse, and a taint naming an object that
            // is absent, which no remote gives back.
            "tainted" => {
                forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
                std::fs::write(meta_copy_path(&root, "tags"), b"not a hash").unwrap();
                plant_taint(&root, &[&"ab".repeat(32)]);
            }
            // No copy, and the ref's bytes a prefix of a head whose loose file cannot be read:
            // that says nothing of the object, so the step is to settle the read.
            "untold" => {
                forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "tags")).unwrap();
                std::fs::write(meta_path(&root, "tags"), &tags.as_bytes()[..12]).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(loose_object(&root, &tags), std::fs::Permissions::from_mode(0o000)).unwrap();
                }
            }
            // No copy, and the ref's bytes a prefix of a head whose loose object is corrupt: a
            // verdict of the store, unlike a read it could not make, so the salvage is unknown.
            _ => {
                forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "tags")).unwrap();
                std::fs::write(meta_path(&root, "tags"), &tags.as_bytes()[..12]).unwrap();
                let object = loose_object(&root, &tags);
                forklift_core::util::file_utils::remove_file(&object).unwrap();
                std::fs::write(&object, b"no object").unwrap();
            }
        }

        match case {
            "tainted" => {
                let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
                assert!(!removal.status.success(), "{}", text(&removal.stdout));
                assert!(text(&removal.stderr).contains("run \"forklift heal\" first, without --remove-ref"), "{}", text(&removal.stderr));
                assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
                assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), b"not a hash");
            }
            "untold" => {
                let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
                let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(loose_object(&root, &tags), std::fs::Permissions::from_mode(0o644)).unwrap();
                }
                let step = forklift_core::util::ref_heal::unread_step("@tags");
                assert_eq!(refused["error"]["next_step"], step.as_str(), "{}", refused);
                assert!(!removal.status.success(), "{}", text(&removal.stdout));
                assert!(text(&removal.stderr).contains("heal could not read what it needs to judge it"), "{}", text(&removal.stderr));
                assert!(text(&removal.stderr).contains(&step), "{}", text(&removal.stderr));
                assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
                // The step leads somewhere: with the file readable again, heal rebuilds the ref.
                assert_eq!(rebuilt_meta(&healed(&area, &root)), vec![("@tags".to_string(), tags.clone(), "what the ref's own bytes still name".to_string())]);
            }
            _ => {
                let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
                assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", refused);
                let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
                assert!(removal.status.success(), "{}{}", text(&removal.stdout), text(&removal.stderr));
                assert_eq!(kept_meta(&root, "tags"), (Some(tags.as_bytes()[..12].to_vec()), None));
                assert!(tag_names(&tag_list(&area, &root)).is_empty());
            }
        }
    }
}

// A lost meta ref whose copy's head fails verification on a parcel the store holds damaged is the
// ref's own refusal, and its removal is the step; on a parcel that cannot be read, the verdict says
// nothing of the ref, so the step is to settle the read, the removal is refused, and once the file
// reads again heal rebuilds the ref. Mutations: read an
// untold failure as the ref's own → "unreadable" next_step is the removal, red; read a content
// failure as untold → "corrupt" next_step is the settle step, red.
#[cfg(unix)]
#[test]
fn a_meta_head_failing_verification_is_the_refs_own_only_on_what_the_store_holds() {
    use std::os::unix::fs::PermissionsExt;

    for case in ["corrupt", "unreadable"] {
        let area = Area::new(&format!("meta-verify-{}", case));
        let (root, _, second) = tagged(&area);
        forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
        let object = loose_object(&root, &second);
        match case {
            "corrupt" => {
                forklift_core::util::file_utils::remove_file(&object).unwrap();
                std::fs::write(&object, b"no object").unwrap();
            }
            _ => std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o000)).unwrap(),
        }

        let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
        let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
        std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o644)).unwrap();
        let said = text(&removal.stderr);
        match case {
            "corrupt" => {
                assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @tags", "{}", refused);
                assert!(removal.status.success(), "{}{}", text(&removal.stdout), said);
            }
            _ => {
                let step = forklift_core::util::ref_heal::unread_step("@tags");
                assert_eq!(refused["error"]["next_step"], step.as_str(), "{}", refused);
                assert!(!removal.status.success(), "{}", text(&removal.stdout));
                assert!(said.contains("heal could not read what it needs to judge it"), "{}", said);
                assert!(said.contains(&step), "{}", said);
                assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
                // The step leads somewhere: with the file readable again, heal rebuilds the ref.
                assert_eq!(rebuilt_meta(&healed(&area, &root)), vec![("@tags".to_string(), second.clone(), "the ref's copy".to_string())]);
            }
        }
    }
}

/// The one pack `store compact --all` left in `root`, without its extension.
fn only_pack(root: &Path) -> PathBuf {
    let packs: Vec<PathBuf> = std::fs::read_dir(root.join(".forklift").join("objects").join("pack")).unwrap().flatten()
        .map(|entry| entry.path()).filter(|path| path.extension().is_some_and(|ext| ext == "pack"))
        .collect();
    assert_eq!(packs.len(), 1, "{:?}", packs);
    packs[0].with_extension("")
}

// A lost meta ref whose copy's head sits only in a pack the store cannot read in full is no
// verdict on the ref: the head is unread, never decided absent. An index lost beside its data file
// is heal's own to settle: it rebuilds the index, then the ref, in one run. A data file lost, or one
// heal cannot read beside a lost index, leaves the head unread: the refusal's reason carries no
// frame of the read error's own step, the removal is refused, and once the file reads again heal
// rebuilds the ref.
// Mutations: read pack damage or an unindexed data file as the store's verdict → the removal is
// admitted, red; keep the read error's frame in the reason → it names "forklift heal", red.
#[test]
fn a_meta_head_in_a_pack_the_store_cannot_read_in_full_is_unread() {
    let cases: &[&str] = if cfg!(unix) { &["index lost", "data lost", "data unreadable"] } else { &["index lost", "data lost"] };
    for case in cases.iter().copied() {
        let area = Area::new(&format!("meta-pack-{}", case.replace(' ', "-")));
        let (root, _, second) = tagged(&area);
        area.run_ok(&root, &["store", "compact", "--all"]);
        assert!(!loose_object(&root, &second).exists(), "the head is packed");
        let pack = only_pack(&root);
        let saved = area.base.join("saved");
        match case {
            "index lost" => forklift_core::util::file_utils::rename(&pack.with_extension("idx"), &saved).unwrap(),
            "data lost" => forklift_core::util::file_utils::rename(&pack.with_extension("pack"), &saved).unwrap(),
            // An index lost beside a data file heal cannot read: the index cannot be rebuilt, so
            // whether the head is in it stays undecided.
            _ => {
                forklift_core::util::file_utils::rename(&pack.with_extension("idx"), &saved).unwrap();
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(pack.with_extension("pack"), std::fs::Permissions::from_mode(0o000)).unwrap();
                }
            }
        }
        forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

        if case != "index lost" {
            // The pack holds the warehouse's history too, so heal refuses on the lost objects
            // first; the ref's refusal stands beside it.
            let output = area.run(&root, &["--json", "heal"]);
            let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
            match case {
                "data lost" => forklift_core::util::file_utils::rename(&saved, &pack.with_extension("pack")).unwrap(),
                _ => {
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(pack.with_extension("pack"), std::fs::Permissions::from_mode(0o644)).unwrap();
                    }
                }
            }
            assert_eq!(output.status.code(), Some(58), "{}: {}{}", case, text(&output.stdout), text(&output.stderr));
            let refused: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            let refs = refused["error"]["details"]["refs_refused"].as_array().unwrap();
            assert_eq!(refs.len(), 1, "{}: {}", case, refused);
            assert_eq!(refs[0]["pallet"], "@tags", "{}: {}", case, refused);
            let reason = refs[0]["reason"].as_str().unwrap();
            assert!(reason.contains(&format!("parcel {} does not pass verification: ", second)), "{}: {}", case, refused);
            assert!(!reason.contains("forklift heal"), "{}: {}", case, refused);
            assert!(!removal.status.success(), "{}: {}", case, text(&removal.stdout));
            assert!(text(&removal.stderr).contains("heal could not read what it needs to judge it"), "{}: {}", case, text(&removal.stderr));
            assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
        }
        assert_eq!(rebuilt_meta(&healed(&area, &root)), vec![("@tags".to_string(), second.clone(), "the ref's copy".to_string())], "{}", case);
        assert_eq!(tag_names(&tag_list(&area, &root)).len(), 2, "{}", case);
    }
}

// A meta ref heal chose no head for is still the office's refusal while the office is refused,
// since its removal would be judged against no office. Mutation: keep the choice's own owner
// beside a refused office → @tags is removed, red.
#[test]
fn a_meta_ref_with_no_head_beside_a_refused_office_waits_on_the_office() {
    let area = Area::new("meta-remove-no-head");
    let (root, _, _) = tagged(&area);
    area.run_ok(&root, &["office", "enroll"]);

    forklift_core::util::file_utils::remove_file(&meta_path(&root, "office")).unwrap();
    std::fs::write(meta_copy_path(&root, "office"), b"not a hash").unwrap();
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();
    std::fs::write(meta_copy_path(&root, "tags"), b"not a hash either").unwrap();

    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("heal refused it only because it refused the ref of pallet \"@office\""), "{}", said);
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), b"not a hash either", "the copy stands");
}

// A meta ref refused only because the office was is not removed: the removal names the office's
// refusal and its step, and once the office is removed, the office's way back, never the
// verifier's "lift the office". Mutations: admit a cascaded refusal → @tags and its sound copy
// removed, red; admit one beside an unborn office → the second removal succeeds, red; name the
// verifier's step there → "Lift the office" red.
#[test]
fn a_meta_ref_refused_only_because_the_office_was_names_the_office_and_is_not_removed() {
    let area = Area::new("meta-remove-cascade");
    let (root, _, _) = tagged(&area);
    area.run_ok(&root, &["office", "enroll"]);
    let tags_copy = std::fs::read(meta_copy_path(&root, "tags")).unwrap();

    forklift_core::util::file_utils::remove_file(&meta_path(&root, "office")).unwrap();
    std::fs::write(meta_copy_path(&root, "office"), b"not a hash").unwrap();
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @office", "{}", refused);
    assert!(message.contains("The ref of pallet \"@office\" was not rebuilt: neither its copy nor its own bytes name a head"), "{}", refused);
    assert!(message.contains("The ref of pallet \"@tags\" was not rebuilt: it is verified against the office and checked against the redactions, and the ref of pallet \"@office\" is damaged too"), "{}", refused);

    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("heal refused it only because it refused the ref of pallet \"@office\": neither its copy nor its own bytes name a head"), "{}", said);
    assert!(said.contains("Run \"forklift heal --remove-ref @office\"."), "{}", said);
    assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
    assert!(!meta_path(&root, "tags").exists());
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), tags_copy, "the sound copy stands");

    // The office removed, it is unborn beside the anchor: @tags waits on the office coming back,
    // so the same run still refuses it.
    ref_damaged(&area.run(&root, &["--json", "heal", "--remove-ref", "@office"]));
    assert_eq!(kept_meta(&root, "office"), (None, Some(b"not a hash".to_vec())));
    assert!(!meta_path(&root, "office").exists() && !meta_copy_path(&root, "office").exists());
    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("only because the trust file holds this warehouse's anchor while the office pallet is unborn"), "{}", said);
    assert!(said.contains("With no remote that holds it to take it from, restore its ref") && said.contains("\"forklift office enroll\" again"), "{}", said);
    assert!(!said.contains("Lift the office"), "{}", said);
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), tags_copy, "the sound copy stands");

    // Whether the warehouse is served cannot be read: the office's way back turns on it, so the
    // step is what settles it, never the client's remedy.
    std::fs::write(root.join(".forklift").join("tombstones"), b"not a tombstone list").unwrap();
    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("Whether this warehouse is served could not be read, and the way back for its unborn office pallet turns on it"), "{}", said);
    assert!(said.contains("forklift-server tombstone repair --root"), "{}", said);
    assert!(!said.contains("With no remote that holds it"), "{}", said);
}

// A damaged signature beside a parcel of a lost meta ref's history is no verdict on the ref, on
// the office or beside it: the store holds the parcel whole, and the signature over it is what
// cannot be read. So is a signature that decodes but cannot be checked (it is not 64 bytes). Heal
// refuses the ref as unread, never as its own, and the removal is refused and keeps it. A forged
// signature, one that is checked against its key and refused, is a verdict: the removal is the
// step, and it removes the ref. Mutations: read the damaged sidecar of a non-office parcel as a
// refused signature → "tags damaged" next_step is the removal, red; read a damaged signature on
// the office's chain as the anchor's own refusal → "office damaged" next_step is the removal,
// red; read a signature that cannot be checked as refused → "tags short" next_step is the
// removal, red.
#[test]
fn a_damaged_signature_beside_a_meta_parcel_is_never_the_refs_own() {
    for (pallet, damage) in [("tags", "damaged"), ("office", "damaged"), ("tags", "short"), ("tags", "forged")] {
        let case = format!("{} {}", pallet, damage);
        let area = Area::new(&format!("meta-signature-{}-{}", pallet, damage));
        let (root, _, _) = tagged(&area);
        area.run_ok(&root, &["office", "enroll"]);
        area.run_ok(&root, &["tag", "create", "v3", "main"]);
        let head = meta_head(&root, pallet).unwrap();
        let sidecar = PathBuf::from(format!("{}.sig", loose_object(&root, &head).display()));
        let signed = std::fs::read(&sidecar).unwrap_or_else(|e| panic!("{}: the head is signed: {}", case, e));
        // Version 1, the key id, then a one-byte length and the 64-byte signature.
        assert_eq!(signed[signed.len() - 65], 64, "{}: {:?}", case, signed);
        let key = signed[..signed.len() - 65].to_vec();
        let bytes = match damage {
            "damaged" => vec![1u8, 0xff],
            "short" => [key, vec![63], vec![7; 63]].concat(),
            _ => [key, vec![64], vec![0; 64]].concat(),
        };
        std::fs::write(&sidecar, bytes).unwrap();
        forklift_core::util::file_utils::remove_file(&meta_path(&root, pallet)).unwrap();

        let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
        let removal = area.run(&root, &["heal", "--remove-ref", &format!("@{}", pallet)]);
        let said = text(&removal.stderr);
        if damage == "forged" {
            assert_eq!(refused["error"]["next_step"], format!("forklift heal --remove-ref @{}", pallet).as_str(), "{}: {}", case, refused);
            assert!(removal.status.success(), "{}: {}{}", case, text(&removal.stdout), said);
            assert!(text(&removal.stdout).contains("which heal refused as its own: a parcel of its history is not signed as trust requires"), "{}: {}", case, text(&removal.stdout));
            assert!(!meta_copy_path(&root, pallet).exists(), "{}: the copy is removed", case);
            continue;
        }
        let step = forklift_core::util::ref_heal::unread_step(&format!("@{}", pallet));
        assert_eq!(refused["error"]["next_step"], step.as_str(), "{}: {}", case, refused);
        assert!(!removal.status.success(), "{}: {}", case, text(&removal.stdout));
        assert!(said.contains("was not removed: heal could not read what it needs to judge it"), "{}: {}", case, said);
        assert!(!root.join(".forklift").join("removed-refs").exists(), "{}: nothing is kept", case);
        assert_eq!(std::fs::read_to_string(meta_copy_path(&root, pallet)).unwrap().trim_end(), head, "{}: the copy stands", case);
    }
}

// A meta ref heal cannot verify because the office stands while no trust file holds its anchor
// is refused as the trust state's, never as unread or its own: the removal names the lost
// anchor's ways out, and taking the first of them (the trust file restored from a backup) lets a
// later heal rebuild the ref. Mutation: read the lost anchor as unread → the removal names
// "could not read", red.
#[test]
fn a_meta_ref_beside_a_lost_anchor_names_its_ways_out_and_is_rebuilt_once_the_trust_file_is_back() {
    let area = Area::new("meta-lost-anchor");
    let (root, _, second) = tagged(&area);
    area.run_ok(&root, &["office", "enroll"]);
    let tags = meta_head(&root, "tags").unwrap();
    assert_eq!(tags, second);

    let trust = root.join(".forklift").join("trust");
    let backup = std::fs::read(&trust).expect("an enrolled warehouse has a trust file");
    for file in [trust.clone(), root.join(".forklift").join("trust-copy")] {
        if file.exists() {
            forklift_core::util::file_utils::remove_file(&file).unwrap();
        }
    }
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

    let step = "Take one of the ways out the trust file's refusal names, then run \"forklift heal\" again.";
    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    assert!(refused["error"]["message"].as_str().unwrap().contains("The ref of pallet \"@tags\" was not rebuilt"), "{}", refused);
    assert_eq!(refused["error"]["next_step"], step, "{}", refused);

    let removal = area.run(&root, &["heal", "--remove-ref", "@tags"]);
    let said = text(&removal.stderr);
    assert!(!removal.status.success(), "{}", text(&removal.stdout));
    assert!(said.contains("The ref of pallet \"@tags\" was not removed: the trust state it is verified against refuses"), "{}", said);
    assert!(said.contains("trust was established here and its anchor was lost"), "{}", said);
    assert!(said.contains("Restore the trust file from a backup if you have one."), "{}", said);
    assert!(said.contains(step), "{}", said);
    assert!(!said.contains("could not read"), "{}", said);
    assert!(!root.join(".forklift").join("removed-refs").exists(), "nothing is kept");
    assert!(!meta_path(&root, "tags").exists());

    // The way out, performed: the trust file back from its backup, a later heal rebuilds @tags.
    std::fs::write(&trust, &backup).unwrap();
    let data = healed(&area, &root);
    assert_eq!(rebuilt_meta(&data), vec![("@tags".to_string(), tags.clone(), "the ref's copy".to_string())], "{}", data);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(tags.as_str()));
}

/// Remove `a.txt` from `main`, record a redaction of it as `main` held it, and return the
/// `@redactions` head that made.
fn redacted(area: &Area, root: &Path) -> String {
    let leaked = head(root, "main");
    area.run_ok(root, &["remove", "a.txt"]);
    area.run_ok(root, &["stack", "-m", "drop a.txt"]);
    area.run_ok(root, &["redact", "record", &format!("{}:a.txt", leaked), "-m", "leaked"]);
    meta_head(root, "redactions").unwrap()
}

/// Lose meta pallet `pallet`'s ref, or damage it: its copy stands either way.
fn break_meta(root: &Path, pallet: &str, lost: bool) {
    match lost {
        true => forklift_core::util::file_utils::remove_file(&meta_path(root, pallet)).unwrap(),
        false => std::fs::write(meta_path(root, pallet), b"not a head\n").unwrap(),
    }
}

fn sorted(mut rebuilt: Vec<(String, String, String)>) -> Vec<(String, String, String)> {
    rebuilt.sort();
    rebuilt
}

// Mutation: check @redactions' own rebuild against its ref as read (no chosen head passed to the
// write) → the damaged ref fails the check and nothing is rebuilt, red; a lost one is refused where it is
// read, so that case is red too.
#[test]
fn a_lost_or_damaged_redactions_ref_is_rebuilt_from_its_copy_ahead_of_the_refs_checked_against_it() {
    for lost in [true, false] {
        let area = Area::new(if lost { "redactions-lost" } else { "redactions-damaged" });
        let (root, _, second) = tagged(&area);
        let redactions = redacted(&area, &root);
        break_meta(&root, "redactions", lost);
        forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

        let data = healed(&area, &root);
        assert_eq!(sorted(rebuilt_meta(&data)), vec![
            ("@redactions".to_string(), redactions.clone(), "the ref's copy".to_string()),
            ("@tags".to_string(), second.clone(), "the ref's copy".to_string()),
        ], "lost: {}: {}", lost, data);
        assert_eq!(meta_head(&root, "redactions").as_deref(), Some(redactions.as_str()), "lost: {}", lost);
        assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()), "lost: {}", lost);
        assert_eq!(std::fs::read(meta_copy_path(&root, "redactions")).unwrap(), format!("{}\n", redactions).into_bytes());
    }
}

// Mutations: verify the head chosen for @redactions against the office ref as read → in this
// anchored warehouse the office reads as unborn or damaged, so both are refused, red; check the
// office's write against the @redactions ref as read → the damaged ref fails it, and a lost one is
// refused where it is read, red in both cases.
#[test]
fn a_lost_or_damaged_office_beside_a_lost_or_damaged_redactions_ref_is_rebuilt_with_it() {
    for lost in [true, false] {
        let area = Area::new(if lost { "office-redactions-lost" } else { "office-redactions-damaged" });
        let (root, _, _) = tagged(&area);
        area.run_ok(&root, &["office", "enroll"]);
        let redactions = redacted(&area, &root);
        let office = meta_head(&root, "office").unwrap();
        break_meta(&root, "office", lost);
        break_meta(&root, "redactions", lost);

        let data = healed(&area, &root);
        assert_eq!(sorted(rebuilt_meta(&data)), vec![
            ("@office".to_string(), office.clone(), "the ref's copy".to_string()),
            ("@redactions".to_string(), redactions.clone(), "the ref's copy".to_string()),
        ], "lost: {}: {}", lost, data);
        assert_eq!(meta_head(&root, "office").as_deref(), Some(office.as_str()), "lost: {}", lost);
        assert_eq!(meta_head(&root, "redactions").as_deref(), Some(redactions.as_str()), "lost: {}", lost);

        // A second heal finds nothing to do.
        assert!(rebuilt_meta(&healed(&area, &root)).is_empty(), "lost: {}", lost);
    }
}

// In both, the step is @redactions' removal, which the office's refusal waits on: never heal
// again, which would refuse alike. Mutations: write the office beside a damaged @redactions with no
// head heal can verify, checked against nothing → the office is rebuilt, red; judge @redactions
// through choice_owner where the office's rebuild judges it → a mutual cascade, "forklift heal",
// red.
#[test]
fn a_lost_office_beside_a_redactions_ref_heal_cannot_verify_waits_on_it() {
    let area = Area::new("meta-office-waits");
    let (root, _, second) = tagged(&area);
    area.run_ok(&root, &["office", "enroll"]);

    // A @tags parcel is no @redactions history.
    std::fs::create_dir_all(meta_copy_path(&root, "redactions").parent().unwrap()).unwrap();
    std::fs::write(meta_copy_path(&root, "redactions"), format!("{}\n", second)).unwrap();
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "office")).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("The ref of pallet \"@office\" was not rebuilt: it is checked against the redactions, and the ref of pallet \"@redactions\" is damaged too, with no head heal can verify: set that first"), "{}", refused);
    assert!(message.contains(&format!("The ref of pallet \"@redactions\" was not rebuilt: parcel {} does not pass verification", second)), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @redactions", "{}", refused);
    assert!(!meta_path(&root, "office").exists() && !meta_path(&root, "redactions").exists(), "nothing was written");

    // Nothing names a @redactions head at all: no head was chosen, so none was verified.
    forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "redactions")).unwrap();
    std::fs::write(meta_path(&root, "redactions"), b"not a head\n").unwrap();
    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains("The ref of pallet \"@office\" was not rebuilt: it is checked against the redactions, and the ref of pallet \"@redactions\" is damaged too, and heal chose no head for it: set that first"), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @redactions", "{}", refused);
    assert!(!meta_path(&root, "office").exists(), "nothing was written");
}

// Mutation: refuse only the office and @redactions themselves, not the refs judged against them →
// @tags is rebuilt beside a lost @redactions, red.
#[test]
fn a_meta_ref_beside_a_lost_redactions_ref_is_refused_naming_it() {
    let area = Area::new("meta-blocked");
    let (root, _, second) = tagged(&area);
    // A second pallet, for the second half, made before the damage: a write reads @redactions,
    // and a lost one refuses it.
    area.run_ok(&root, &["pallet", "create", "side"]);
    area.run_ok(&root, &["shift", "side"]);
    area.stack(&root, "s.txt", "side");
    area.run_ok(&root, &["shift", "main"]);
    area.stack(&root, "m.txt", "main");
    let side = head(&root, "side");

    // A lost @redactions: its copy stands, its ref does not, and it names a @tags parcel, which
    // heal refuses as @redactions history.
    std::fs::create_dir_all(meta_copy_path(&root, "redactions").parent().unwrap()).unwrap();
    std::fs::write(meta_copy_path(&root, "redactions"), format!("{}\n", second)).unwrap();
    forklift_core::util::file_utils::remove_file(&meta_path(&root, "tags")).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let message = refused["error"]["message"].as_str().unwrap();
    assert!(message.contains(&format!("The ref of pallet \"@redactions\" was not rebuilt: parcel {} does not pass verification", second)), "{}", refused);
    assert!(message.contains("The ref of pallet \"@tags\" was not rebuilt: it is verified against the office and checked against the redactions, and the ref of pallet \"@redactions\" is damaged too"), "{}", refused);
    assert!(meta_head(&root, "tags").is_none() && meta_head(&root, "redactions").is_none(), "nothing was written");
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @redactions", "{}", refused);

    // Beside a user ref refused too, @redactions' step still comes first: a user ref's rebuild
    // is checked against the redactions. Mutation: name a meta ref's step only when every
    // refused ref is a meta ref → the step is --ref main, red.
    forklift_core::util::file_utils::remove_file(root.join(".forklift").join("journal.json")).unwrap();
    std::fs::write(ref_path(&root, "main"), &side.as_bytes()[..20]).unwrap();

    let refused = ref_damaged(&area.run(&root, &["--json", "heal"]));
    let pallets: Vec<&str> = refused["error"]["details"]["refs_refused"].as_array().unwrap().iter().map(|refused| refused["pallet"].as_str().unwrap()).collect();
    assert!(pallets.contains(&"main") && pallets.contains(&"@tags"), "{}", refused);
    assert_eq!(refused["error"]["next_step"], "forklift heal --remove-ref @redactions", "{}", refused);
}

// Mutation: skip the copy mend → the copy stays lost / at H1, red.
#[test]
fn a_lost_or_stale_copy_beside_an_intact_ref_is_mended() {
    let area = Area::new("meta-mend");
    let (root, first, second) = tagged(&area);
    let bytes = format!("{}\n", second).into_bytes();

    forklift_core::util::file_utils::remove_file(&meta_copy_path(&root, "tags")).unwrap();
    let output = text(&area.run_ok(&root, &["heal"]).stdout);
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), bytes, "{}", output);
    assert!(output.contains("The copy of pallet \"@tags\"'s ref was lost or damaged; it was set to the ref's head"), "{}", output);

    std::fs::write(meta_copy_path(&root, "tags"), format!("{}\n", first)).unwrap();
    let output = text(&area.run_ok(&root, &["heal"]).stdout);
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), bytes, "{}", output);
    assert!(output.contains(&format!("The copy of pallet \"@tags\"'s ref named {}, not the ref's head {}", first, second)), "{}", output);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()), "the ref is not touched");

    // The ref's head in other bytes is said so, not as another head. Mutation: drop the
    // same-head note → "named H, not the ref's head H", red.
    std::fs::write(meta_copy_path(&root, "tags"), &second).unwrap();
    let output = text(&area.run_ok(&root, &["heal"]).stdout);
    assert_eq!(std::fs::read(meta_copy_path(&root, "tags")).unwrap(), bytes, "{}", output);
    assert!(output.contains(&format!("The copy of pallet \"@tags\"'s ref named the ref's head {}, in other bytes; it was set to the ref's bytes.", second)), "{}", output);
}

// A folder planted at the copy's path fails the first write's copy, after its ref landed.
// Mutation: skip the copy mend → the copy stays absent, red.
#[test]
fn a_first_meta_write_whose_copy_failed_stands_and_heal_mends_the_copy() {
    let area = Area::new("meta-birth");
    let root = area.warehouse();
    let planted = meta_copy_path(&root, "tags");
    std::fs::create_dir_all(planted.join("in-the-way")).unwrap();

    let output = area.run(&root, &["tag", "create", "v1", "main"]);
    assert!(!output.status.success(), "the copy's error is returned: {}{}", text(&output.stdout), text(&output.stderr));
    let landed = meta_head(&root, "tags").expect("the ref stands at the new head");

    forklift_core::util::file_utils::remove_dir_all(&planted).unwrap();
    area.run_ok(&root, &["heal"]);
    assert_eq!(std::fs::read(&planted).unwrap(), format!("{}\n", landed).into_bytes());
}

// `--ref` checks the parcel it is given against every meta pallet's history, so the meta refs are
// rebuilt before it. Mutation: drop the meta pass ahead of `--ref` → @tags is still damaged when
// main's parcel is checked against it, and that check is skipped with a note, red.
#[test]
fn a_damaged_meta_ref_is_rebuilt_before_ref_checks_a_parcel_against_it() {
    let area = Area::new("meta-before-ref");
    let (root, _, second) = tagged(&area);
    let main = head(&root, "main");
    std::fs::write(ref_path(&root, "main"), &main.as_bytes()[..20]).unwrap();
    std::fs::write(meta_path(&root, "tags"), &second.as_bytes()[..32]).unwrap();

    let output = area.run_ok(&root, &["heal", "--ref", &format!("main={}", main)]);
    let said = format!("{}{}", text(&output.stdout), text(&output.stderr));
    assert!(!said.contains("is history of meta pallet"), "{}", said);
    assert_eq!(head(&root, "main"), main);
    assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()));
}

/// Plant a complete taint in the warehouse at `root` recording the loose files of `hashes`, as a
/// crash before their writes were synced leaves it.
fn plant_taint(root: &Path, hashes: &[&str]) {
    let dir = forklift_core::util::taint_utils::taint_dir_path_under(root);
    std::fs::create_dir_all(&dir).unwrap();
    let paths = hashes.iter().map(|hash| format!("objects/{}/{}", &hash[..2], &hash[2..]));
    std::fs::write(dir.join("taint-1-0"), forklift_core::util::taint_utils::taint_file_bytes(paths)).unwrap();
}

/// A meta ref and a taint. Lost beside a copy that verifies, the ref is rebuilt before the taint
/// recovery, and a refusal there names the rebuild. One heal cannot settle before the recovery
/// (its bytes name an object the taint records, or its copy names a parcel the taint took) is
/// left for the pass after it: a refusal at a taint that stands names it with its heads, and when
/// the recovery resolves the taint, the pass after it rebuilds the ref.
// Mutations: drop the deferred sentences → "deferred" and "lost-deferred" red; record only an
// unreadable ref as deferred → "lost-deferred" red; read a write that landed under the taint and
// reported its error as not landed → "rebuilt" red (written, never named); skip the meta pass
// after the taint → "resolved" red; never say the taint holds a ref → "torn" and "unread" red; take
// an unreadable taint for a torn one → "unread" red.
#[test]
fn a_meta_ref_heal_cannot_settle_before_the_taint_waits_on_it_and_is_judged_once_it_is_resolved() {
    // A taint file nobody can read needs a permission bit Windows does not have.
    let unread: &[&str] = if cfg!(unix) { &["unread"] } else { &[] };
    for case in ["rebuilt", "deferred", "lost-deferred", "torn", "resolved"].iter().chain(unread).copied() {
        let area = Area::new(&format!("meta-taint-{}", case));
        let (root, _, second) = tagged(&area);
        let mut tainted = vec![second.clone()];

        // A parcel only `side` reaches, gone and recorded: no remote can give it back, so the
        // taint stands and the run stops there.
        if !["resolved", "torn", "unread"].contains(&case) {
            area.run_ok(&root, &["pallet", "create", "side"]);
            area.run_ok(&root, &["shift", "side"]);
            area.stack(&root, "d.txt", "side only");
            area.run_ok(&root, &["shift", "main"]);
            let side = head(&root, "side");
            forklift_core::util::file_utils::remove_file(&loose_object(&root, &side)).unwrap();
            tainted.push(side);
        }

        match case {
            "rebuilt" => break_meta(&root, "tags", true),
            "lost-deferred" => {
                break_meta(&root, "tags", true);
                forklift_core::util::file_utils::remove_file(&loose_object(&root, &second)).unwrap();
            }
            _ => std::fs::write(meta_path(&root, "tags"), &second.as_bytes()[..32]).unwrap(),
        }
        let broken = std::fs::read(meta_path(&root, "tags")).ok();
        match case {
            // Bytes that are not UTF-8: a torn record, of which nothing is trusted.
            "torn" => {
                let dir = forklift_core::util::taint_utils::taint_dir_path_under(&root);
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(dir.join("taint-1-0"), [0xff, 0xfe, b'\n']).unwrap();
            }
            _ => plant_taint(&root, &tainted.iter().map(String::as_str).collect::<Vec<_>>()),
        }
        #[cfg(unix)]
        if case == "unread" {
            use std::os::unix::fs::PermissionsExt;
            let dir = forklift_core::util::taint_utils::taint_dir_path_under(&root);
            for entry in std::fs::read_dir(&dir).unwrap() {
                std::fs::set_permissions(entry.unwrap().path(), std::fs::Permissions::from_mode(0o000)).unwrap();
            }
        }

        if case == "resolved" {
            let data = healed(&area, &root);
            assert_eq!(rebuilt_meta(&data), vec![("@tags".to_string(), second.clone(), "the ref's copy".to_string())], "{}", data);
            assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()));
            let listed = text(&area.run_ok(&root, &["tag", "list"]).stdout);
            assert!(listed.contains("v1") && listed.contains("v2"), "{}", listed);
            continue;
        }

        let output = area.run(&root, &["--json", "heal"]);
        assert_eq!(output.status.code(), Some(21), "{}: {}{}", case, text(&output.stdout), text(&output.stderr));
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(value["error"]["code"], "durability_taint", "{}: {}", case, value);
        let message = value["error"]["message"].as_str().unwrap();
        let rebuilt = format!("Rebuilt the ref of pallet \"@tags\" at {}, from the ref's copy.", second);
        let left = "The ref of pallet \"@tags\" was not rebuilt yet: ";
        let heads = format!("; candidates: {} (the ref's copy).", second);
        let again = "This run stopped before judging that ref a second time; a later heal judges it again.";

        match case {
            "rebuilt" => {
                assert!(message.contains(&rebuilt) && !message.contains("was not rebuilt yet"), "{}", message);
                assert_eq!(meta_head(&root, "tags").as_deref(), Some(second.as_str()));
            }
            _ => {
                let closing = match case {
                    "torn" => " The durability taint's record is torn, so every object the ref's bytes name counts as possibly lost, and a later heal leaves it again; ",
                    "unread" => " The durability taint could not be read, so every object the ref's bytes name counted as possibly lost; a later heal that can read the taint judges it again.",
                    _ => again,
                };
                assert!(message.contains(left) && message.contains(&heads) && message.contains(closing), "{}: {}", case, message);
                assert!(!message.contains("Rebuilt the ref of pallet \"@tags\""), "{}: {}", case, message);
                assert_eq!(std::fs::read(meta_path(&root, "tags")).ok(), broken, "{}: the deferred ref is left as it was", case);
            }
        }
    }
}
