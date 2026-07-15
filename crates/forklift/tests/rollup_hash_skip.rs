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
