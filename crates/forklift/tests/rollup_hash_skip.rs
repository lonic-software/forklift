//! Consumer-level coverage for the rollup-based skip (DESIGN.html §5.0 D item 8, stage 2):
//! `stocktake`/`diff --staged` (`stocktake_utils::walk_directory_staged`) and `stack`'s tree
//! build (`tree_utils::compute_rollup_skip_plan`) must never change *what* forklift reports or
//! commits — only how much work it does to get there.
//!
//! The core technique is a "twin": the same script of file writes and commands is applied, in
//! lockstep, to two otherwise-identical warehouses — one with the skip enabled (the default) and
//! one with it forced off via `FORKLIFT_DISABLE_ROLLUP_SKIP=1` (the kill switch stage 1 built
//! for exactly this). Every read (`stocktake`, `diff --staged`) is required to produce
//! byte-identical JSON on both; every `stack` is required to produce the same tree hash on both
//! (parcel hashes legitimately differ — they embed the wall clock).

use std::path::PathBuf;
use std::process::{Command, Output};

const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

struct Warehouse {
    root: PathBuf,
    home: PathBuf,
    disable_skip: bool,
}

impl Warehouse {
    fn new(name: &str, disable_skip: bool) -> Warehouse {
        let base = std::env::temp_dir().join(format!(
            "forklift-rollup-skip-{}-{}-{}",
            name, if disable_skip { "off" } else { "on" }, std::process::id()
        ));
        let root = base.join("warehouse");
        let home = base.join("home");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        Warehouse { root, home, disable_skip }
    }

    fn write_file(&self, relative: &str, content: &str) {
        let path = self.root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn command(&self, args: &[&str]) -> Command {
        self.command_at(&self.root, args)
    }

    fn command_at(&self, dir: &PathBuf, args: &[&str]) -> Command {
        let mut command = Command::new(FORKLIFT);
        command
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("keys"));

        if self.disable_skip {
            command.env("FORKLIFT_DISABLE_ROLLUP_SKIP", "1");
        }

        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
    }

    fn run_ok(&self, args: &[&str]) -> Output {
        let output = self.run(args);
        assert!(output.status.success(),
            "`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        output
    }

    fn run_ok_at(&self, dir: &PathBuf, args: &[&str]) -> Output {
        let output = self.command_at(dir, args).output().unwrap();
        assert!(output.status.success(),
            "`{}` failed: {}", args.join(" "), String::from_utf8_lossy(&output.stderr));
        output
    }

    fn prepare(&self) {
        self.run_ok(&["prepare"]);
        self.run_ok(&["config", "operator.name", "rollup-skip@forklift"]);
        self.run_ok(&["config", "operator.identifier", "rollup-skip@forklift"]);
    }

    fn head_tree_hash(&self, pallet: &str) -> String {
        let head = std::fs::read_to_string(self.root.join(".forklift").join("pallets").join(pallet))
            .unwrap().trim().to_string();
        let output = self.run_ok(&["--json", "peek", &head]);
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

/// Two otherwise-identical warehouses driven by the same script — see the module doc comment.
struct Twin {
    on: Warehouse,
    off: Warehouse,
}

impl Twin {
    fn new(name: &str) -> Twin {
        let twin = Twin { on: Warehouse::new(name, false), off: Warehouse::new(name, true) };
        twin.on.prepare();
        twin.off.prepare();
        twin
    }

    fn write_file(&self, relative: &str, content: &str) {
        self.on.write_file(relative, content);
        self.off.write_file(relative, content);
    }

    fn remove_file(&self, relative: &str) {
        std::fs::remove_file(self.on.root.join(relative)).unwrap();
        std::fs::remove_file(self.off.root.join(relative)).unwrap();
    }

    /// Run the same mutating command on both twins, requiring the same exit status on each.
    /// Stdout is deliberately *not* compared here — a `stack`'s human message embeds the new
    /// parcel hash, which legitimately differs between the twins (a parcel embeds the wall
    /// clock, so two independently run stacks of identical content never share one). The real
    /// equivalence checks are [`Twin::assert_equivalent`] (content) and
    /// [`Twin::assert_tree_matches`] (the tree a stack actually committed).
    fn run_ok(&self, args: &[&str]) {
        self.on.run_ok(args);
        self.off.run_ok(args);
    }

    /// Run the same read-only `--json` command on both twins and assert the parsed JSON is
    /// identical — the equivalence property `stocktake`/`diff --staged` must hold. `data.head`
    /// (`stocktake`'s report of the current *parcel* hash) is masked before comparing: the
    /// twins are independent stack lineages, so their parcel hashes (which embed the wall
    /// clock) legitimately diverge even when every tree they ever built agreed — see
    /// `Twin::assert_tree_matches` for the content-level check that actually matters there.
    fn assert_json_matches(&self, args: &[&str]) -> serde_json::Value {
        let a = self.on.run_ok(args);
        let b = self.off.run_ok(args);

        let mut value_a: serde_json::Value = serde_json::from_slice(&a.stdout).unwrap();
        let mut value_b: serde_json::Value = serde_json::from_slice(&b.stdout).unwrap();

        if let Some(head) = value_a.pointer_mut("/data/head") {
            *head = serde_json::Value::String("<masked>".to_string());
        }
        if let Some(head) = value_b.pointer_mut("/data/head") {
            *head = serde_json::Value::String("<masked>".to_string());
        }

        assert_eq!(value_a, value_b,
            "`{}` JSON diverged between skip on/off:\non:  {}\noff: {}",
            args.join(" "), value_a, value_b);

        value_a
    }

    /// After a `stack` on both twins, assert the resulting tree hash agrees (parcel hashes
    /// legitimately differ — they embed the wall clock).
    fn assert_tree_matches(&self, pallet: &str) {
        assert_eq!(self.on.head_tree_hash(pallet), self.off.head_tree_hash(pallet),
            "the stacked tree hash diverged between skip on/off");
    }

    /// The full equivalence check this test suite runs after every scripted step: `stocktake`
    /// and `diff --staged`, both as JSON, must match byte-for-byte between skip on and off.
    fn assert_equivalent(&self) {
        self.assert_json_matches(&["--json", "stocktake"]);
        self.assert_json_matches(&["--json", "diff", "--staged"]);
    }
}

#[test]
fn skip_on_and_off_agree_after_every_step_of_a_varied_script() {
    let twin = Twin::new("equivalence");

    twin.write_file("root.txt", "root v1\n");
    twin.write_file("dir_a/sub/deep/file1.txt", "deep file1 v1\n");
    twin.write_file("dir_a/sub/deep/file2.txt", "deep file2 v1\n");
    twin.write_file("dir_b/file.txt", "dir_b v1\n");
    twin.write_file("dir_c/only.txt", "dir_c v1\n");

    twin.run_ok(&["load", "."]);
    twin.assert_equivalent();
    twin.run_ok(&["stack", "base"]);
    twin.assert_tree_matches("main");
    twin.assert_equivalent();

    // Modify a deep file, load it, stack.
    twin.write_file("dir_a/sub/deep/file1.txt", "deep file1 v2\n");
    twin.run_ok(&["load", "dir_a/sub/deep/file1.txt"]);
    twin.assert_equivalent();
    twin.run_ok(&["stack", "modify deep"]);
    twin.assert_tree_matches("main");
    twin.assert_equivalent();

    // Add a new deep file, load it, stack.
    twin.write_file("dir_a/sub/deep/file3.txt", "deep file3 v1\n");
    twin.run_ok(&["load", "dir_a/sub/deep/file3.txt"]);
    twin.assert_equivalent();
    twin.run_ok(&["stack", "add deep"]);
    twin.assert_tree_matches("main");
    twin.assert_equivalent();

    // Remove the whole of dir_c (prunes it from the tree entirely), load, stack.
    twin.remove_file("dir_c/only.txt");
    twin.run_ok(&["load", "."]);
    twin.assert_equivalent();
    twin.run_ok(&["stack", "remove dir_c"]);
    twin.assert_tree_matches("main");
    twin.assert_equivalent();

    // Stage a change, unload it (restore --staged) instead of stacking, and confirm the walk
    // agrees at every point along the way.
    twin.write_file("dir_b/file.txt", "dir_b v2 (about to be unloaded)\n");
    twin.run_ok(&["load", "dir_b/file.txt"]);
    twin.assert_equivalent();
    twin.run_ok(&["unload", "dir_b/file.txt"]);
    twin.assert_equivalent();

    // Park a real change, confirm equivalence, pop it, confirm again, then stack it.
    twin.write_file("dir_b/file.txt", "dir_b v2 (parked)\n");
    twin.run_ok(&["load", "dir_b/file.txt"]);
    twin.run_ok(&["park"]);
    twin.assert_equivalent();
    twin.run_ok(&["park", "pop"]);
    twin.assert_equivalent();
    twin.run_ok(&["stack", "modify dir_b"]);
    twin.assert_tree_matches("main");
    twin.assert_equivalent();
}

#[test]
fn a_deep_mutation_after_a_stamped_stack_is_reported_by_stocktake_and_diff() {
    // The adversarial staleness check: a rollup-skipped subtree must never hide a real change.
    let warehouse = Warehouse::new("adversarial-staleness", false);
    warehouse.prepare();

    warehouse.write_file("a/b/c/deep.txt", "deep v1\n");
    warehouse.write_file("sibling/other.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    // Everything is stamped and matches head; a stocktake right after a stack must be clean.
    let clean = warehouse.run_ok(&["--json", "stocktake"]);
    let clean_value: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(clean_value["data"]["staged_count"], 0);
    assert_eq!(clean_value["data"]["unstaged_count"], 0);

    // Mutate the deep file directly on disk (an unstaged change) and confirm stocktake reports
    // it without loading first.
    warehouse.write_file("a/b/c/deep.txt", "deep v2 (mutated)\n");
    let unstaged = warehouse.run_ok(&["--json", "stocktake"]);
    let unstaged_value: serde_json::Value = serde_json::from_slice(&unstaged.stdout).unwrap();
    let unstaged_files = unstaged_value["data"]["unstaged"].as_array()
        .expect("stocktake must report the unstaged change, not hide it behind a rollup skip");
    assert!(unstaged_files.iter().any(|c| c["path"] == "a/b/c/deep.txt" && c["kind"] == "modified"),
        "the deep mutation must be reported: {}", unstaged_value);

    // Stage it and confirm `diff --staged` reports it too.
    warehouse.run_ok(&["load", "a/b/c/deep.txt"]);
    let staged = warehouse.run_ok(&["--json", "diff", "--staged"]);
    let staged_value: serde_json::Value = serde_json::from_slice(&staged.stdout).unwrap();
    let staged_files = staged_value["data"]["files"].as_array().unwrap();
    assert!(staged_files.iter().any(|f| f["path"] == "a/b/c/deep.txt"),
        "the staged deep mutation must be reported: {}", staged_value);
}

/// The rollup-skip-count line the `FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT` debug hook prints to
/// stderr (see `main.rs`) — the number of subtree roots a rollup skip actually applied to.
fn skip_count(output: &Output) -> u64 {
    let stderr = String::from_utf8_lossy(&output.stderr);
    stderr.lines()
        .find_map(|line| line.strip_prefix("rollup-skip-count: "))
        .unwrap_or_else(|| panic!("no rollup-skip-count line in stderr: {}", stderr))
        .trim()
        .parse()
        .unwrap()
}

#[test]
fn a_second_stack_preserves_an_untouched_subtrees_rollup_and_a_later_walk_skips_it() {
    let warehouse = Warehouse::new("repeat-stack-preservation", false);
    warehouse.prepare();

    warehouse.write_file("subtree_a/file.txt", "a v1\n");
    warehouse.write_file("subtree_b/deep/file.txt", "b v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    let rollup_before = shard_rollup(&warehouse, "subtree_b/deep");
    assert!(rollup_before.is_some(), "subtree_b/deep must be stamped after the base stack");

    // Touch only subtree_a and stack again.
    warehouse.write_file("subtree_a/file.txt", "a v2\n");
    warehouse.run_ok(&["load", "subtree_a/file.txt"]);
    warehouse.run_ok(&["stack", "touch a"]);

    // subtree_b's rollup must be byte-for-byte the same as before — cleanup left it untouched
    // rather than clearing (or redundantly re-stamping) it.
    let rollup_after = shard_rollup(&warehouse, "subtree_b/deep");
    assert_eq!(rollup_before, rollup_after,
        "an untouched subtree's rollup must survive an unrelated stack exactly as it was");

    // A later stocktake actually applies the skip against subtree_b (the debug counter is the
    // ground truth that the optimization fired, not just that the output happens to be right).
    let mut command = warehouse.command(&["stocktake"]);
    command.env("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert!(skip_count(&output) >= 1, "stocktake must have skipped at least one unchanged subtree");
}

/// This shard's rollup hash, read straight off disk (mirrors `rollup_hash.rs`'s helper).
fn shard_rollup(warehouse: &Warehouse, key: &str) -> Option<String> {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);

    let (_, bytes) = forklift_core::util::file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
    let bytes = bytes?;
    let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes).unwrap();

    inventory.get_rollup_hash().cloned()
}

#[test]
fn a_narrowed_sparse_bay_still_produces_correct_stocktake_and_stack_with_skips_active() {
    let trunk = Warehouse::new("sparse-trunk", false);
    trunk.prepare();

    trunk.write_file("scope/deep/a.txt", "a v1\n");
    trunk.write_file("scope/deep/b.txt", "b v1\n");
    trunk.write_file("scope/other.txt", "other v1\n");
    trunk.write_file("outside/x.txt", "outside v1\n");
    trunk.run_ok(&["load", "."]);
    trunk.run_ok(&["stack", "base"]);

    let full_dir = trunk.home.join("bay-full");
    let scoped_dir = trunk.home.join("bay-scoped");
    trunk.run_ok(&["bay", "add", "full", full_dir.to_str().unwrap()]);
    trunk.run_ok(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "scope/deep"]);

    // The same edit, applied in both bays, once with the skip active (the default) in the
    // scoped bay and once in the full bay for comparison.
    std::fs::write(scoped_dir.join("scope/deep/a.txt"), "a v2\n").unwrap();
    std::fs::write(full_dir.join("scope/deep/a.txt"), "a v2\n").unwrap();
    trunk.run_ok_at(&scoped_dir, &["load", "."]);
    trunk.run_ok_at(&full_dir, &["load", "."]);

    // Equivalence still holds inside a scoped bay: stocktake/diff must agree whether the skip
    // is active or forced off.
    let mut off_command = trunk.command_at(&scoped_dir, &["--json", "stocktake"]);
    off_command.env("FORKLIFT_DISABLE_ROLLUP_SKIP", "1");
    let off_output = off_command.output().unwrap();
    assert!(off_output.status.success());
    let on_output = trunk.run_ok_at(&scoped_dir, &["--json", "stocktake"]);

    let on_value: serde_json::Value = serde_json::from_slice(&on_output.stdout).unwrap();
    let off_value: serde_json::Value = serde_json::from_slice(&off_output.stdout).unwrap();
    assert_eq!(on_value, off_value, "a scoped bay's stocktake must agree whether the skip is active or not");

    trunk.run_ok_at(&scoped_dir, &["stack", "scoped change"]);
    trunk.run_ok_at(&full_dir, &["stack", "full change"]);

    // The scoped bay's pallet's tree at "scope/deep" must match the full bay's — a scoped
    // stack with the skip active is still byte-identical to a full stack of the same content.
    // Pallets live in the *shared* warehouse root, not a bay's own working directory (a bay's
    // own ".forklift" is a redirect file, not a directory).
    let scoped_head = std::fs::read_to_string(trunk.root.join(".forklift").join("pallets").join("scoped"))
        .unwrap().trim().to_string();
    let full_head = std::fs::read_to_string(trunk.root.join(".forklift").join("pallets").join("full"))
        .unwrap().trim().to_string();

    let scoped_peek = trunk.run_ok_at(&scoped_dir, &["--json", "peek", &scoped_head]);
    let full_peek = trunk.run_ok_at(&full_dir, &["--json", "peek", &full_head]);
    let scoped_tree = serde_json::from_slice::<serde_json::Value>(&scoped_peek.stdout).unwrap()
        ["data"]["tree"].as_str().unwrap().to_string();
    let full_tree = serde_json::from_slice::<serde_json::Value>(&full_peek.stdout).unwrap()
        ["data"]["tree"].as_str().unwrap().to_string();

    // Resolve both trees' "scope/deep" subtree hash via the shared warehouse object store and
    // compare — a byte-identical subtree, even though the two pallets otherwise diverge.
    let _scope = forklift_core::globals::StorageRootScope::enter(&trunk.root);
    let scoped_subtree_hash = forklift_core::util::tree_utils::resolve_subtree_hash(&scoped_tree, "scope/deep")
        .unwrap().unwrap();
    let full_subtree_hash = forklift_core::util::tree_utils::resolve_subtree_hash(&full_tree, "scope/deep")
        .unwrap().unwrap();
    assert_eq!(scoped_subtree_hash, full_subtree_hash,
        "a scoped stack's in-scope subtree must be byte-identical to a full stack's");

    // A second, deeper edit in the scoped bay: the untouched sibling file's shard is unaffected
    // and a later stocktake still agrees between skip on and off.
    std::fs::write(scoped_dir.join("scope/deep/b.txt"), "b v2\n").unwrap();
    trunk.run_ok_at(&scoped_dir, &["load", "scope/deep/b.txt"]);
    trunk.run_ok_at(&scoped_dir, &["stack", "scoped change 2"]);

    let mut off_command2 = trunk.command_at(&scoped_dir, &["--json", "stocktake"]);
    off_command2.env("FORKLIFT_DISABLE_ROLLUP_SKIP", "1");
    let off_output2 = off_command2.output().unwrap();
    let on_output2 = trunk.run_ok_at(&scoped_dir, &["--json", "stocktake"]);
    let on_value2: serde_json::Value = serde_json::from_slice(&on_output2.stdout).unwrap();
    let off_value2: serde_json::Value = serde_json::from_slice(&off_output2.stdout).unwrap();
    assert_eq!(on_value2, off_value2);
}

// -------------------------------------------------------------------------------------------
// Regressions for the `load .` join-point redesign (DESIGN.html §5.0 D item 8, review fix 1):
// the pre-clear approach it replaced unconditionally wiped every rollup in the loaded scope on
// every load — for `load .`, the whole warehouse, changed or not — defeating the feature on the
// most common workflow ("load .; stack"). These pin the fixed behavior directly.
// -------------------------------------------------------------------------------------------

#[test]
fn a_no_op_load_dot_preserves_every_rollup_and_a_later_stack_still_skips() {
    let warehouse = Warehouse::new("noop-load-preserves", false);
    warehouse.prepare();

    warehouse.write_file("a/b/c/file.txt", "deep v1\n");
    warehouse.write_file("sibling/file.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    let root_rollup_before = shard_rollup(&warehouse, "");
    let deep_rollup_before = shard_rollup(&warehouse, "a/b/c");
    let sibling_rollup_before = shard_rollup(&warehouse, "sibling");
    assert!(root_rollup_before.is_some(), "the root must be stamped after the base stack");
    assert!(deep_rollup_before.is_some(), "the deep subtree must be stamped after the base stack");
    assert!(sibling_rollup_before.is_some(), "the sibling subtree must be stamped after the base stack");

    // A no-op `load .`: nothing on disk changed since the stack above.
    warehouse.run_ok(&["load", "."]);

    assert_eq!(shard_rollup(&warehouse, ""), root_rollup_before,
        "a no-op `load .` must not touch the root's rollup");
    assert_eq!(shard_rollup(&warehouse, "a/b/c"), deep_rollup_before,
        "a no-op `load .` must not touch a deep, unchanged subtree's rollup");
    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before,
        "a no-op `load .` must not touch an unrelated sibling's rollup");

    // A later stack of a real, unrelated change must still find something to skip — proving the
    // no-op load didn't just leave the *values* looking right while quietly losing the ability
    // to skip on them.
    warehouse.write_file("a/b/c/file.txt", "deep v2\n");
    warehouse.run_ok(&["load", "a/b/c/file.txt"]);

    let mut command = warehouse.command(&["stack", "touch deep"]);
    command.env("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success());
    assert!(skip_count(&output) >= 1,
        "a stack after a no-op load must still skip the untouched sibling subtree");
}

#[test]
fn load_dot_after_a_deep_edit_invalidates_only_the_changed_spine() {
    let warehouse = Warehouse::new("load-dot-spine", false);
    warehouse.prepare();

    warehouse.write_file("a/b/c/file.txt", "deep v1\n");
    warehouse.write_file("sibling/file.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    let sibling_rollup_before = shard_rollup(&warehouse, "sibling");
    assert!(sibling_rollup_before.is_some());
    assert!(shard_rollup(&warehouse, "").is_some());
    assert!(shard_rollup(&warehouse, "a").is_some());
    assert!(shard_rollup(&warehouse, "a/b").is_some());
    assert!(shard_rollup(&warehouse, "a/b/c").is_some());

    warehouse.write_file("a/b/c/file.txt", "deep v2\n");
    warehouse.run_ok(&["load", "."]); // a whole-directory load, not a narrow single-file one

    assert_eq!(shard_rollup(&warehouse, ""), None, "the root must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "a"), None, "an intermediate ancestor must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "a/b"), None, "the immediate parent must be invalidated");
    assert_eq!(shard_rollup(&warehouse, "a/b/c"), None, "the changed shard itself must be invalidated");

    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before,
        "an untouched sibling subtree's rollup must survive a `load .` that changed something elsewhere");
}

#[test]
fn skip_count_is_unaffected_by_an_interposed_no_op_load_dot() {
    let warehouse = Warehouse::new("noop-load-ab", false);
    warehouse.prepare();

    warehouse.write_file("a/b/c/file.txt", "deep v1\n");
    warehouse.write_file("sibling1/file.txt", "s1 v1\n");
    warehouse.write_file("sibling2/file.txt", "s2 v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    // A: touch the deep file, narrow-load it, stack — no interposed no-op load.
    warehouse.write_file("a/b/c/file.txt", "deep v2\n");
    warehouse.run_ok(&["load", "a/b/c/file.txt"]);
    let mut command_a = warehouse.command(&["stack", "touch a"]);
    command_a.env("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT", "1");
    let output_a = command_a.output().unwrap();
    assert!(output_a.status.success());
    let count_a = skip_count(&output_a);

    // B: touch again, narrow-load, but interpose a no-op `load .` before the stack this time.
    warehouse.write_file("a/b/c/file.txt", "deep v3\n");
    warehouse.run_ok(&["load", "a/b/c/file.txt"]);
    warehouse.run_ok(&["load", "."]);
    let mut command_b = warehouse.command(&["stack", "touch b"]);
    command_b.env("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT", "1");
    let output_b = command_b.output().unwrap();
    assert!(output_b.status.success());
    let count_b = skip_count(&output_b);

    assert_eq!(count_a, count_b,
        "an interposed no-op `load .` must not change how many subtrees a later stack can skip");
    assert!(count_a >= 1, "the scenario must actually exercise a skip for this comparison to mean anything");
}

#[test]
fn a_shard_that_vanishes_out_from_under_load_still_invalidates_its_ancestors() {
    // Regression (multi-agent review of PR #59, finding 3): the dirty-path branch for a shard
    // whose *file* has gone missing (not just its working-directory content) used to drop the
    // key from metadata without extending the ancestor invalidation its sibling branch (a
    // shard whose entries got marked `Deleted`) already applied. A stale-but-still-matching
    // ancestor rollup then let `stocktake`/`diff --staged`/`stack` skip straight past the whole
    // subtree — silently reporting (and re-stacking) a tree that still contained the deleted
    // directory.
    let warehouse = Warehouse::new("vanished-shard-invalidates-ancestors", false);
    warehouse.prepare();

    warehouse.write_file("a/b/c/file.txt", "deep v1\n");
    warehouse.write_file("sibling/file.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    assert!(shard_rollup(&warehouse, "").is_some(), "the root must be stamped after the base stack");
    assert!(shard_rollup(&warehouse, "a").is_some(), "\"a\" must be stamped after the base stack");
    assert!(shard_rollup(&warehouse, "a/b").is_some(), "\"a/b\" must be stamped after the base stack");

    // Delete the tracked directory from the working tree *and* its inventory shard directly —
    // the shard file itself going missing (not merely its content) is what routes `load`'s
    // dirty-path pass into the specific branch this regression targets, rather than the
    // already-correct "shard exists, mark everything Deleted" branch a plain working-directory
    // deletion alone would hit.
    std::fs::remove_dir_all(warehouse.root.join("a/b/c")).unwrap();
    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        let shard_folder = forklift_core::util::file_utils::get_inventory_folder_for_key("a/b/c");
        std::fs::remove_dir_all(&shard_folder).unwrap();
    }

    warehouse.run_ok(&["load", "."]);

    // Every ancestor of the vanished shard must be invalidated exactly like any other real
    // content change — a directory disappearing entirely is one.
    assert_eq!(shard_rollup(&warehouse, ""), None,
        "the root's rollup must be cleared when a tracked descendant's shard vanishes");
    assert_eq!(shard_rollup(&warehouse, "a"), None,
        "an intermediate ancestor's rollup must be cleared too");
    assert_eq!(shard_rollup(&warehouse, "a/b"), None,
        "the immediate parent's rollup must be cleared too");

    // `diff --staged` must report the deletion, not silently skip past it via a stale rollup
    // that (before the fix) still matched head.
    let staged = warehouse.run_ok(&["--json", "diff", "--staged"]);
    let staged_value: serde_json::Value = serde_json::from_slice(&staged.stdout).unwrap();
    let staged_files = staged_value["data"]["files"].as_array().unwrap();
    assert!(staged_files.iter().any(|f| f["path"] == "a/b/c/file.txt" && f["kind"] == "removed"),
        "the vanished deep file must be reported as a staged removal: {}", staged_value);

    // A subsequent stack must not include the deleted subtree — the old (pre-deletion) subtree
    // must never be reused via a stale-but-still-matching rollup.
    warehouse.run_ok(&["stack", "remove a/b/c"]);
    let tree_hash = warehouse.head_tree_hash("main");

    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
    let resolved = forklift_core::util::tree_utils::resolve_subtree_hash(&tree_hash, "a/b/c").unwrap();
    assert!(resolved.is_none(), "the stacked tree must not contain the deleted subtree \"a/b/c\"");
}

// -------------------------------------------------------------------------------------------
// `park` regression (DESIGN.html §5.0 D item 10, finding #3): `park`'s tree build used to call
// `tree_utils::build_tree_from_inventory` (no head to compare rollups against, so no skip was
// ever possible on this path) instead of `build_tree_from_inventory_deferred`, unlike `stack`.
// This pins that `park` now gets the same rollup-based skip `stack` already had.
// -------------------------------------------------------------------------------------------

#[test]
fn park_gets_the_rollup_based_skip_stack_already_had() {
    let warehouse = Warehouse::new("park-rollup-skip", false);
    warehouse.prepare();

    warehouse.write_file("subtree_a/file.txt", "a v1\n");
    warehouse.write_file("subtree_b/deep/file.txt", "b v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    let rollup_before = shard_rollup(&warehouse, "subtree_b/deep");
    assert!(rollup_before.is_some(), "subtree_b/deep must be stamped after the base stack");

    // Touch only subtree_a, and park without an intervening explicit `load` — `park`'s own
    // working-directory refresh (`refresh_tracked_entries`) stages tracked changes itself.
    warehouse.write_file("subtree_a/file.txt", "a v2\n");

    let mut command = warehouse.command(&["park"]);
    command.env("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT", "1");
    let output = command.output().unwrap();
    assert!(output.status.success(), "park failed: {}", String::from_utf8_lossy(&output.stderr));

    // The debug counter is the ground truth that the skip actually fired for `park`'s tree
    // build, not just that the resulting parked parcel happens to be correct.
    assert!(skip_count(&output) >= 1,
        "park's tree build must skip the untouched subtree_b/deep subtree, exactly like stack's does");

    // subtree_b/deep's rollup must still name the same (unchanged) subtree hash. `park` always
    // rewrites every shard from the head tree at the end (resetting the warehouse back to head),
    // so this is not "untouched bytes on disk" the way the stack regression above checks it —
    // but the value stamped by that reset is the subtree's tree hash either way, so an unchanged
    // subtree stamps back to the exact same rollup regardless of whether the skip fired.
    let rollup_after = shard_rollup(&warehouse, "subtree_b/deep");
    assert_eq!(rollup_before, rollup_after,
        "an untouched subtree's rollup must be the same value before and after a park");
}

// -------------------------------------------------------------------------------------------
// Regression for a PR #61 review finding on `refresh_tracked_entries` (`park`'s
// working-directory refresh): its pass-1/pass-2 split read every tracked shard's content in
// pass 1, then published each pass 2 in metadata order. A directory sorting *before* the root's
// "./" metadata entry (any name starting with a byte < 0x2E — e.g. this route-group-style name)
// whose own real content change ran through the ancestor-clear funnel in pass 2 correctly
// cleared the root's rollup on disk — but the root's *own*, later pass-2 write then restamped
// the stale pass-1-decided rollup right back over that clear, because it never re-checked
// whether it had just become an ancestor of some other change decided in the very same pass.
// `park` then took the whole-tree rollup-skip fast path (the restamped rollup matched head) and
// refused with "nothing to park", silently dropping the real edit.
// -------------------------------------------------------------------------------------------

#[test]
fn park_never_restamps_a_stale_rollup_over_a_same_pass_ancestor_clear() {
    let warehouse = Warehouse::new("park-ancestor-clear-race", false);
    warehouse.prepare();

    // "(marketing)" sorts before the root's "./" metadata entry (0x28 < 0x2E) — the exact
    // ordering the review's repro depends on.
    warehouse.write_file("(marketing)/page.tsx", "page v1\n");
    warehouse.write_file("README.md", "readme v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    let head_before = warehouse.head_tree_hash("main");
    let root_rollup_before = shard_rollup(&warehouse, "");
    assert_eq!(root_rollup_before.as_deref(), Some(head_before.as_str()),
        "the root shard must be stamped with the head tree hash after the base stack");

    // A real content change inside the directory that sorts before "./" in metadata order...
    warehouse.write_file("(marketing)/page.tsx", "page v2 -- a real edit\n");
    // ...and a same-*content* rewrite of a root-level file: its shard entry is rebuilt (the
    // rewrite advances its mtime past the shard's own, so the stat-cache fast path cannot trust
    // it), but the rebuilt hash matches the old one exactly, so this is "changed" without being
    // "content_changed" — the exact combination that hit the buggy `save_inventory` path
    // (as opposed to the ancestor-clearing `write_shard_mutation` path) for the root shard.
    warehouse.write_file("README.md", "readme v1\n");

    let park = warehouse.run(&["park"]);
    assert!(park.status.success(),
        "park must capture the real edit under (marketing)/, not report nothing to park: {}",
        String::from_utf8_lossy(&park.stderr));

    // Not just a successful exit: pop the parked parcel right back and confirm the edit is
    // actually there, not silently dropped along the way.
    warehouse.run_ok(&["park", "pop"]);

    let content = std::fs::read_to_string(warehouse.root.join("(marketing)/page.tsx")).unwrap();
    assert_eq!(content, "page v2 -- a real edit\n",
        "the parked-then-popped content must be the real edit, not silently dropped");

    // The pallet head must still be exactly where `stack base` left it — a park never advances
    // it (only stacks do), so this also confirms park did not (wrongly) no-op against a stale,
    // restamped root rollup that happened to equal head.
    assert_eq!(warehouse.head_tree_hash("main"), head_before);
}

// A PR #61 review finding (#6) on `refresh_tracked_entries` — a shard rewritten only because its
// stat data drifted (no real content change) used to publish with the wall clock's value *when
// the whole refresh finished deciding every tracked shard*, not the instant this particular shard
// was actually verified — is covered by
// `crates/forklift-core/tests/refresh_tracked_entries_verified_at.rs`, not here: `park`'s own
// reset-to-head step (`inventory_utils::replace_all_inventories`) unconditionally rewrites every
// shard right after `refresh_tracked_entries` returns, so a shard's mtime observed after a full
// `park` command reflects that later, unrelated rewrite — not what this refresh itself published.
// That dedicated test calls `refresh_tracked_entries` directly instead.

// -------------------------------------------------------------------------------------------
// Regressions for the batched merge/replay funnel (DESIGN.html §5.0 D item 10, findings #2/#4):
// `apply_merge_action` (consolidate/cherry-pick), `restore <dir>`'s plain replay and `park pop`'s
// replay all used to pay `update_shard`/`stage_file_entry_from_stat`'s full two-barrier funnel
// per action/file — now routed through `inventory_utils::ShardMutationBatch`, the same shared
// join-point primitive `load` and `park`'s working-directory refresh already use. The shape that
// bit PR A's round-2 review (finding #1) was a multi-decision batch where one decision's ancestor
// is *another* decision's own shard, decided in an order the ancestor-clearing logic must not
// depend on. These pin that shape specifically for the merge path.
// -------------------------------------------------------------------------------------------

#[test]
fn consolidate_clears_every_ancestor_rollup_across_a_batch_where_one_actions_ancestor_is_another_actions_own_shard() {
    let warehouse = Warehouse::new("consolidate-batch-ancestor-clear", false);
    warehouse.prepare();

    // "dirA" is the immediate ancestor of "dirA/dirB" — the shape the finding #1 repro needs: a
    // single merge batch must touch both a nested shard *and* that shard's own parent directly.
    // "zzz/deep" is a *second*, unrelated nested change whose own intermediate ancestor ("zzz")
    // is never itself directly touched by anything — sorted after "dirA/dirB" in every BTreeMap
    // key order this batch could iterate in, so a clear-keys computation that only accounted for
    // the *first*-processed decision (rather than every decision in the batch) would silently
    // leave "zzz" carrying a stale rollup.
    warehouse.write_file("dirA/dirB/deep.txt", "deep v1\n");
    warehouse.write_file("dirA/direct.txt", "direct v1\n");
    warehouse.write_file("zzz/deep/leaf.txt", "leaf v1\n");
    warehouse.write_file("conflict.txt", "conflict v1\n");
    warehouse.write_file("sibling/other.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    assert!(shard_rollup(&warehouse, "dirA").is_some(), "\"dirA\" must be stamped after the base stack");
    assert!(shard_rollup(&warehouse, "dirA/dirB").is_some(), "\"dirA/dirB\" must be stamped after the base stack");
    assert!(shard_rollup(&warehouse, "zzz").is_some(), "\"zzz\" must be stamped after the base stack");

    // Diverge: feature changes both the nested file and dirA's own direct file, the unrelated
    // deep "zzz" file, plus the line that will conflict.
    warehouse.run_ok(&["palletize", "feature"]);
    warehouse.write_file("dirA/dirB/deep.txt", "deep v2 (feature)\n");
    warehouse.write_file("dirA/direct.txt", "direct v2 (feature)\n");
    warehouse.write_file("zzz/deep/leaf.txt", "leaf v2 (feature)\n");
    warehouse.write_file("conflict.txt", "feature version\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "feature work"]);

    // Main diverges independently: an unrelated sibling edit (so dirA/dirA-dirB's rollups stay
    // exactly as the base stack left them going into the merge) and a conflicting edit to the
    // same single-line file feature also touched, so the merge below cannot auto-stack — it
    // stops right after `apply_merge_actions`, leaving the batch's own decisions directly
    // inspectable instead of immediately overwritten by a following stack's own rollup stamping.
    warehouse.run_ok(&["shift", "main"]);
    warehouse.write_file("sibling/other.txt", "sibling v2 (main)\n");
    warehouse.write_file("conflict.txt", "main version\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "main work"]);

    let sibling_rollup_before_merge = shard_rollup(&warehouse, "sibling");
    assert!(sibling_rollup_before_merge.is_some(), "\"sibling\" must be stamped after main's own stack");

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert!(merge.status.success(), "consolidate must report conflicts, not fail outright: {}",
        String::from_utf8_lossy(&merge.stderr));
    let merge_stdout = String::from_utf8_lossy(&merge.stdout).to_string();
    assert!(merge_stdout.contains("conflict"), "the merge must report the conflict.txt conflict: {}", merge_stdout);

    // The batch's own decisions, inspectable now because the conflict stopped the auto-stack:
    // every ancestor of a real content change must be cleared, whether or not that ancestor was
    // *also* directly touched by another action in the very same batch.
    assert_eq!(shard_rollup(&warehouse, "dirA/dirB"), None,
        "the directly-touched nested shard must have its own rollup cleared");
    assert_eq!(shard_rollup(&warehouse, "dirA"), None,
        "\"dirA\" must be cleared: it is both an ancestor of the dirA/dirB change *and* the \
         target of its own direct-file action in the same batch");
    assert_eq!(shard_rollup(&warehouse, ""), None, "the root must be cleared as an ancestor of both changes");
    assert_eq!(shard_rollup(&warehouse, "zzz"), None,
        "\"zzz\" must be cleared too: an ancestor of a *different*, unrelated change in the same \
         batch that is never itself directly touched by any action");

    // An unrelated shard the merge batch never touched must be completely unaffected.
    assert_eq!(shard_rollup(&warehouse, "sibling"), sibling_rollup_before_merge,
        "an unrelated shard outside the merge's diff must keep exactly the rollup it had going in");

    // Not just rollup bookkeeping: the actual content the batch decided must be present and
    // correct on disk — a wrongly-restamped-then-skipped ancestor is exactly what would have let
    // one of these two changes go missing.
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dirA/dirB/deep.txt")).unwrap(), "deep v2 (feature)\n");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dirA/direct.txt")).unwrap(), "direct v2 (feature)\n");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("zzz/deep/leaf.txt")).unwrap(), "leaf v2 (feature)\n");

    // Resolve the conflict and complete the consolidation with an ordinary stack.
    warehouse.write_file("conflict.txt", "resolved\n");
    warehouse.run_ok(&["load", "conflict.txt"]);
    warehouse.run_ok(&["stack", "resolve conflict"]);

    // The completing stack's own cleanup restamps every shard against the newly-committed tree —
    // confirm it lands on the *correct* (post-merge) tree, not a value derived from a rollup the
    // merge batch should have cleared but didn't.
    let head_tree = warehouse.head_tree_hash("main");
    let dira_hash;
    let dira_dirb_hash;
    {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        dira_hash = forklift_core::util::tree_utils::resolve_subtree_hash(&head_tree, "dirA").unwrap().unwrap();
        dira_dirb_hash = forklift_core::util::tree_utils::resolve_subtree_hash(&head_tree, "dirA/dirB").unwrap().unwrap();
    }

    assert_eq!(shard_rollup(&warehouse, "dirA"), Some(dira_hash),
        "\"dirA\"'s rollup after the completing stack must match the real, merged tree");
    assert_eq!(shard_rollup(&warehouse, "dirA/dirB"), Some(dira_dirb_hash),
        "\"dirA/dirB\"'s rollup after the completing stack must match the real, merged tree");

    // Final belt-and-braces: a stocktake must agree byte-for-byte whether the rollup skip is
    // forced off or left on — the same equivalence property every other rollup regression in
    // this file checks, applied to the just-completed merge.
    let mut off_command = warehouse.command(&["--json", "stocktake"]);
    off_command.env("FORKLIFT_DISABLE_ROLLUP_SKIP", "1");
    let off_output = off_command.output().unwrap();
    assert!(off_output.status.success());
    let on_output = warehouse.run_ok(&["--json", "stocktake"]);
    let on_value: serde_json::Value = serde_json::from_slice(&on_output.stdout).unwrap();
    let off_value: serde_json::Value = serde_json::from_slice(&off_output.stdout).unwrap();
    assert_eq!(on_value, off_value, "stocktake after the merge must agree whether the skip is active or not");
}

#[test]
fn apply_merge_actions_collapses_two_actions_in_the_same_shard_into_one_correct_read_modify_write() {
    // Same-shard collapse (DESIGN.html §5.0 D item 10, implementation note): a delete and a
    // take-theirs both landing in "dirA" must both survive in the final shard, not have one
    // clobber the other via a lost read-modify-write.
    let warehouse = Warehouse::new("consolidate-same-shard-collapse", false);
    warehouse.prepare();

    warehouse.write_file("dirA/keep.txt", "keep v1\n");
    warehouse.write_file("dirA/removed.txt", "removed v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    warehouse.run_ok(&["palletize", "feature"]);
    // Feature adds a brand-new file in "dirA" and deletes another existing one — two distinct
    // merge actions ("dirA/new.txt" TakeTheirs, "dirA/removed.txt" Delete) that both target the
    // very same shard key ("dirA").
    warehouse.write_file("dirA/new.txt", "new v1\n");
    std::fs::remove_file(warehouse.root.join("dirA/removed.txt")).unwrap();
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "feature work"]);

    warehouse.run_ok(&["shift", "main"]);
    warehouse.write_file("sibling.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "main work"]);

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert!(merge.status.success(), "consolidate must succeed cleanly (no overlapping changes): {}",
        String::from_utf8_lossy(&merge.stderr));

    // Both of "dirA"'s actions must have landed: the new file present, the removed file gone,
    // and the untouched file in the same shard still present.
    assert!(warehouse.root.join("dirA/new.txt").exists(), "the take-theirs addition must survive the collapse");
    assert!(!warehouse.root.join("dirA/removed.txt").exists(), "the delete must survive the collapse");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dirA/keep.txt")).unwrap(), "keep v1\n",
        "an untouched entry in the same shard must be unaffected by the collapse");

    let status = String::from_utf8_lossy(&warehouse.run(&["stocktake"]).stdout).to_string();
    assert!(status.contains("matches"), "the warehouse must report clean after the merge: {}", status);
}

// -------------------------------------------------------------------------------------------
// Adversarial-review finding (PR B, DESIGN.html §5.0 D item 10, findings #2/#4): a
// `ShardMutationBatch::publish` failure — as opposed to a per-action decision failure — is a
// wider case than the old per-action immediate funnel had, because every action's
// working-directory write already happened, unconditionally, before the batch is published. The
// old code could only ever leave *one* action's file diverged from its shard (the one whose own
// `write_shard_mutation` call failed); this batch can leave several. Not data loss — every
// diverged file's real content is exactly what the merge decided, and "load ." (recommended by
// the enriched error message this finding motivated) always reconciles the inventory with
// whatever the working directory actually holds, from scratch, regardless of why they diverged.
// This pins that recovery path end to end: force a real `batch.publish()` failure (an ancestor
// shard the merge's own actions never touch, corrupted out from under it), confirm the working
// directory already shows the merge's real content despite the failure, confirm the error message
// carries the promised guidance, then confirm "load ." actually recovers cleanly.
// -------------------------------------------------------------------------------------------

// Unix-only: simulates the write-side I/O failure `ShardMutationBatch::publish` can hit
// (`stage_rollup_clear`/`WriteBatch::stage` failing to create a temp file) via a read-only
// directory. A *read*-side failure (a corrupt shard) does not actually reach this code path in
// practice — `consolidate`'s own pre-check (`ensure_warehouse_is_clean`, which walks and parses
// every tracked shard before any merge action runs) already catches an unreadable shard earlier,
// with its own clear parse-error message, before `apply_merge_actions` ever starts. A write-side
// failure has no equivalent earlier guard (nothing writes during the pre-check), so it is the
// realistic way this batch's own failure path actually triggers.
#[cfg(unix)]
#[test]
fn consolidate_recovers_via_load_after_a_batch_publish_failure_from_an_unwritable_inventory_root() {
    let warehouse = Warehouse::new("consolidate-batch-publish-failure-recovery", false);
    warehouse.prepare();

    warehouse.write_file("dirA/dirB/file.txt", "deep v1\n");
    warehouse.write_file("dirA/direct.txt", "direct v1\n");
    warehouse.write_file("sibling.txt", "sibling v1\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "base"]);

    warehouse.run_ok(&["palletize", "feature"]);
    warehouse.write_file("dirA/dirB/file.txt", "deep v2 (feature)\n");
    warehouse.write_file("dirA/direct.txt", "direct v2 (feature)\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "feature work"]);

    warehouse.run_ok(&["shift", "main"]);
    warehouse.write_file("sibling.txt", "sibling v2 (main)\n");
    warehouse.run_ok(&["load", "."]);
    warehouse.run_ok(&["stack", "main work"]);

    // The root shard's own inventory folder, read-only: `ensure_warehouse_is_clean`'s pre-check
    // (read-only itself) still passes cleanly, but `apply_merge_actions`' own
    // `ShardMutationBatch::publish` fails when phase A tries to stage the root's ancestor-clear
    // temp file there — after both merge actions' working-directory writes already landed.
    let root_inventory_folder = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        let mut path = forklift_core::util::file_utils::get_inventory_data_path_for_key("");
        path.pop();
        path
    };

    use std::os::unix::fs::PermissionsExt;
    let original_permissions = std::fs::metadata(&root_inventory_folder).unwrap().permissions();
    std::fs::set_permissions(&root_inventory_folder, std::fs::Permissions::from_mode(0o555)).unwrap();

    let merge = warehouse.run(&["consolidate", "feature"]);

    // Always restore permissions before any assertion can early-return/panic, so a failing
    // assertion below never leaves the warehouse directory permanently read-only for `Drop`'s
    // own cleanup (`std::fs::remove_dir_all`) to choke on.
    std::fs::set_permissions(&root_inventory_folder, original_permissions).unwrap();

    assert!(!merge.status.success(), "the merge must fail: its root shard's folder is unwritable");
    let stderr = String::from_utf8_lossy(&merge.stderr);
    assert!(stderr.contains("load"),
        "the failure must tell the operator to run \"load .\" to reconcile: {}", stderr);

    // The working-directory writes already landed despite the batch publish failure — exactly
    // the wider blast radius this finding is about, not data loss: the real content is right
    // there on disk, just not yet reflected in the (unpublished) inventory.
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dirA/dirB/file.txt")).unwrap(), "deep v2 (feature)\n");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dirA/direct.txt")).unwrap(), "direct v2 (feature)\n");

    // The recommended recovery: "load ." reconciles the inventory with the working directory
    // from scratch, regardless of why they diverged.
    warehouse.run_ok(&["load", "."]);

    // The merge's real content is now correctly staged — nothing was lost, just deferred.
    let staged = warehouse.run_ok(&["--json", "diff", "--staged"]);
    let staged_value: serde_json::Value = serde_json::from_slice(&staged.stdout).unwrap();
    let staged_files = staged_value["data"]["files"].as_array().unwrap();
    assert!(staged_files.iter().any(|f| f["path"] == "dirA/dirB/file.txt"),
        "the recovered load must stage the merge's real deep change: {}", staged_value);
    assert!(staged_files.iter().any(|f| f["path"] == "dirA/direct.txt"),
        "the recovered load must stage the merge's real direct change: {}", staged_value);

    // The store stays fully usable: stacking the recovered state succeeds cleanly.
    warehouse.run_ok(&["stack", "recovered after unwritable-root failure"]);
    let clean = warehouse.run_ok(&["--json", "stocktake"]);
    let clean_value: serde_json::Value = serde_json::from_slice(&clean.stdout).unwrap();
    assert_eq!(clean_value["data"]["staged_count"], 0);
}
