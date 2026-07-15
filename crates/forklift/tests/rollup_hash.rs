//! End-to-end coverage for the per-shard rollup hash's maintenance (DESIGN.html §5.0 D item 8,
//! stage 1: format + maintenance — no consumer reads it yet).
//!
//! Drives the real `forklift` binary through the writers the design calls out, then inspects
//! shard files directly (via `forklift-core`) against an independently computed oracle: the
//! subtree hash `resolve_subtree_hash` derives from the pallet head's tree. A shard's rollup
//! must always be either absent or exactly that hash — never stale.

use std::path::PathBuf;
use std::process::{Command, Output};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// One isolated warehouse with its own home for global config + keys — mirrors
/// `determinism.rs`'s harness.
struct Warehouse {
    root: PathBuf,
    home: PathBuf,
}

impl Warehouse {
    fn new(name: &str) -> Warehouse {
        let base = std::env::temp_dir().join(format!("forklift-rollup-hash-{}-{}", name, std::process::id()));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Warehouse { root, home }
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);
        command
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));
        command
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.command(args).output().unwrap();
        assert!(output.status.success(),
            "`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        output
    }

    fn prepare(&self) {
        self.run_ok(&["prepare"]);
        self.run_ok(&["config", "operator.name", "rollup-hash@forklift"]);
        self.run_ok(&["config", "operator.identifier", "rollup-hash@forklift"]);
    }

    fn head(&self, pallet: &str) -> String {
        std::fs::read_to_string(self.root.join(".forklift").join("pallets").join(pallet))
            .unwrap().trim().to_string()
    }

    /// The pallet head's tree hash (deterministic content, no wall clock).
    fn head_tree_hash(&self, pallet: &str) -> String {
        let output = self.run_ok(&["--json", "peek", &self.head(pallet)]);
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["data"]["tree"].as_str().unwrap().to_string()
    }
}

impl Drop for Warehouse {
    fn drop(&mut self) {
        if let Some(base) = self.root.parent() {
            let _ = std::fs::remove_dir_all(base);
        }
    }
}

/// A small worktree with enough nesting (depth ≥ 3) for ancestor-invalidation checks, and a
/// distant, untouched sibling subtree to check *isn't* invalidated by an unrelated change.
fn populate(warehouse: &Warehouse) {
    warehouse.write_file("dir0/sub0/deep/file0.txt", "content of dir0 sub0 deep file0\n");
    warehouse.write_file("dir0/sub0/deep/file1.txt", "content of dir0 sub0 deep file1\n");
    warehouse.write_file("dir0/sub1/file0.txt", "content of dir0 sub1 file0\n");
    warehouse.write_file("sibling/file0.txt", "content of the untouched sibling\n");
    warehouse.write_file("root.txt", "content of the root file\n");
}

/// This shard's rollup hash, read straight off disk (`None` on a missing shard or an absent
/// rollup — the caller decides which it expects).
fn shard_rollup(warehouse: &Warehouse, key: &str) -> Option<String> {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);

    let (_, bytes) = forklift_core::util::file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
    let bytes = bytes?;
    let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes).unwrap();

    inventory.get_rollup_hash().cloned()
}

/// Every warehouse path key with a registered inventory shard.
fn registered_shard_keys(warehouse: &Warehouse) -> Vec<String> {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);

    let (_, metadata) = forklift_core::util::file_utils::retrieve_inventory_metadata_or_none().unwrap();

    metadata.unwrap_or_default().iter()
        .map(|entry| forklift_core::util::inventory_utils::metadata_entry_to_key(entry).to_string())
        .collect()
}

/// The oracle: assert every registered shard's rollup is either absent, or exactly the subtree
/// hash `resolve_subtree_hash` derives at that key from `head_tree_hash` — a rollup must never
/// disagree with what the tree `stack` would build there actually is.
fn assert_rollups_match_head(warehouse: &Warehouse, head_tree_hash: &str) {
    for key in registered_shard_keys(warehouse) {
        let Some(rollup) = shard_rollup(warehouse, &key) else { continue };

        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        let expected = forklift_core::util::tree_utils::resolve_subtree_hash(head_tree_hash, &key)
            .unwrap()
            .unwrap_or_else(|| panic!(
                "shard \"{}\" carries a rollup but the head has no matching subtree there", key
            ));

        assert_eq!(rollup, expected, "shard \"{}\" rollup disagrees with the head", key);
    }
}

#[test]
fn stack_stamps_every_shard_rollup_to_exactly_match_the_new_head() {
    let warehouse = Warehouse::new("stack-stamps");
    warehouse.prepare();
    populate(&warehouse);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "layout"]);

    let head_tree = warehouse.head_tree_hash("main");
    assert_rollups_match_head(&warehouse, &head_tree);

    // At least the deep and root shards actually got a rollup — the assertion above would
    // vacuously pass if nothing were ever stamped.
    assert!(shard_rollup(&warehouse, "").is_some(), "the root shard must be stamped after a stack");
    assert!(shard_rollup(&warehouse, "dir0/sub0/deep").is_some(),
        "a deep, non-empty shard must be stamped after a stack");
}

#[test]
fn load_after_a_deep_change_clears_ancestors_but_spares_an_unrelated_sibling() {
    let warehouse = Warehouse::new("load-clears-ancestors");
    warehouse.prepare();
    populate(&warehouse);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "layout"]);

    // Every shard on the changed file's chain (and the root) was stamped by the stack above.
    assert!(shard_rollup(&warehouse, "").is_some());
    assert!(shard_rollup(&warehouse, "dir0").is_some());
    assert!(shard_rollup(&warehouse, "dir0/sub0").is_some());
    assert!(shard_rollup(&warehouse, "dir0/sub0/deep").is_some());
    assert!(shard_rollup(&warehouse, "sibling").is_some());
    let sibling_rollup_before = shard_rollup(&warehouse, "sibling");

    // A real content change three levels deep.
    warehouse.write_file("dir0/sub0/deep/file0.txt", "changed content\n");
    warehouse.run_ok(&["load", "dir0/sub0/deep/file0.txt"]);

    assert_eq!(shard_rollup(&warehouse, ""), None, "the root's rollup must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "dir0"), None, "an intermediate ancestor must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "dir0/sub0"), None, "the immediate parent must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "dir0/sub0/deep"), None, "the mutated shard itself must be cleared");

    // An unrelated sibling subtree keeps its (still-valid) rollup — nothing changed under it.
    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before,
        "an untouched sibling subtree's rollup must survive an unrelated deep change");
}

#[test]
fn single_file_unload_leaves_every_shard_matching_head_or_unstamped() {
    let warehouse = Warehouse::new("unload-restores");
    warehouse.prepare();
    populate(&warehouse);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "layout"]);
    let head_tree = warehouse.head_tree_hash("main");

    // Stage a change, then unstage it again (`restore --staged`, a.k.a. `unload`) — the net
    // staged state ends up matching head exactly.
    warehouse.write_file("dir0/sub1/file0.txt", "changed then unloaded\n");
    warehouse.run_ok(&["load", "dir0/sub1/file0.txt"]);
    assert_eq!(shard_rollup(&warehouse, "dir0/sub1"), None, "the load must have invalidated this shard");

    warehouse.run_ok(&["unload", "dir0/sub1/file0.txt"]);

    // Every remaining rollup (any shard the unstage didn't touch) is still exactly right, and
    // nothing was stamped incorrectly by the reset itself.
    assert_rollups_match_head(&warehouse, &head_tree);
}

#[test]
fn park_materializes_a_stamped_reset_and_pop_reinvalidates_only_the_touched_chain() {
    let warehouse = Warehouse::new("park-pop");
    warehouse.prepare();
    populate(&warehouse);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "layout"]);
    let head_tree = warehouse.head_tree_hash("main");

    let sibling_rollup_before = shard_rollup(&warehouse, "sibling");

    warehouse.write_file("dir0/sub0/deep/file1.txt", "parked change\n");
    warehouse.run_ok(&["load", "dir0/sub0/deep/file1.txt"]);
    assert_eq!(shard_rollup(&warehouse, "dir0/sub0/deep"), None);

    // `park` materializes the warehouse back to head from a known tree (a materializer): every
    // resulting shard is either stamped exactly right or left unstamped.
    warehouse.run_ok(&["park"]);
    assert_rollups_match_head(&warehouse, &head_tree);
    assert!(shard_rollup(&warehouse, "dir0/sub0/deep").is_some(),
        "park's reset-to-head must re-stamp the previously invalidated shard");
    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before,
        "an untouched sibling subtree's rollup survives the reset unchanged");

    // `park pop` re-stages the parked change through the ordinary mutation funnel: only its own
    // ancestor chain is invalidated again.
    warehouse.run_ok(&["park", "pop"]);
    assert_eq!(shard_rollup(&warehouse, ""), None);
    assert_eq!(shard_rollup(&warehouse, "dir0/sub0/deep"), None);
    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before,
        "an untouched sibling subtree's rollup survives popping an unrelated parked change");
}

#[test]
fn consolidate_fast_forward_materializes_a_stamped_tree() {
    let warehouse = Warehouse::new("consolidate-ff");
    warehouse.prepare();
    populate(&warehouse);

    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "layout"]);

    // A second pallet, branched from main, that moves ahead — main stays untouched so the
    // consolidate below is a clean fast-forward.
    warehouse.run_ok(&["palletize", "feature"]);
    warehouse.write_file("dir0/sub1/file0.txt", "feature branch change\n");
    warehouse.run_ok(&["load", "dir0/sub1/file0.txt"]);
    warehouse.run_ok(&["stack", "feature work"]);
    let feature_tree = warehouse.head_tree_hash("feature");

    warehouse.run_ok(&["shift", "main"]);
    warehouse.run_ok(&["consolidate", "feature"]);

    let main_tree = warehouse.head_tree_hash("main");
    assert_eq!(main_tree, feature_tree, "a clean consolidate must fast-forward main to feature's tree");

    // The fast-forward rebuilds the inventory from the target tree (a materializer): every
    // shard is either stamped exactly right or left unstamped.
    assert_rollups_match_head(&warehouse, &main_tree);
    assert!(shard_rollup(&warehouse, "dir0/sub1").is_some(),
        "the fast-forwarded shard must be re-stamped from the target tree");
}
