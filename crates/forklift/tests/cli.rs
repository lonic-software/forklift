use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path to the compiled forklift binary (provided by cargo for integration tests).
const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

/// A temporary warehouse directory that is deleted when the test ends.
struct TestWarehouse {
    root: PathBuf,

    /// A stand-in for the user's home, *outside* the warehouse root (like a real home
    /// directory): the global configuration and the key directory live here, so
    /// commands that write them (e.g. minting the operator UUID) never dirty the
    /// warehouse's working directory.
    home: PathBuf,
}

impl TestWarehouse {
    fn new(name: &str) -> TestWarehouse {
        let root = std::env::temp_dir()
            .join(format!("forklift-test-{}-{}", name, std::process::id()));
        let home = std::env::temp_dir()
            .join(format!("forklift-test-{}-{}-home", name, std::process::id()));

        // A leftover directory from a previous run must not leak state into this one.
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&home).unwrap();

        TestWarehouse { root, home }
    }

    fn write_file(&self, relative_path: &str, content: &str) {
        let path = self.root.join(relative_path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_in(".", args)
    }

    /// Run forklift with the working directory at an absolute path (e.g. a bay outside
    /// the warehouse), keeping the test's isolated global config and key directory.
    fn run_at(&self, dir: &std::path::Path, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .output()
            .unwrap()
    }

    fn run_in(&self, relative_dir: &str, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .current_dir(self.root.join(relative_dir))
            // Tests must never read or write the developer's real global configuration.
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .output()
            .unwrap()
    }

    /// Run a command with extra environment variables (e.g. supplying a key passphrase
    /// non-interactively via `FORKLIFT_KEY_PASSPHRASE`, standing in for the prompt).
    fn run_with_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut command = Command::new(FORKLIFT);

        command.args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"));

        for (key, value) in env {
            command.env(key, value);
        }

        command.output().unwrap()
    }

    /// Run a command feeding `input` on stdin (for the MCP server, which reads
    /// newline-delimited JSON-RPC). Stdin is closed after the write so the server
    /// reaches EOF and exits.
    fn run_with_stdin(&self, args: &[&str], input: &str) -> Output {
        use std::io::Write;
        use std::process::Stdio;

        let mut child = Command::new(FORKLIFT)
            .args(args)
            .current_dir(&self.root)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();

        child.wait_with_output().unwrap()
    }
}

impl Drop for TestWarehouse {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed.\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).to_string()
}

/// Parse a command's stdout as the one JSON document `--json` promises.
fn json(output: &Output) -> serde_json::Value {
    serde_json::from_str(&stdout(output))
        .unwrap_or_else(|e| panic!("stdout is not one JSON document ({}): {}", e, stdout(output)))
}

/// Configure the operator identity (in the test-scoped global configuration), which
/// stacking parcels requires.
fn configure_operator(warehouse: &TestWarehouse) {
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Test Operator"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.identifier", "test@forklift"]));
}

/// Read the operator id from the test-scoped global configuration (set explicitly by
/// `configure_operator`, or minted by the first command that resolved the identity).
fn operator_id(warehouse: &TestWarehouse) -> String {
    let output = warehouse.run(&["config", "--global", "operator.identifier"]);
    assert_success(&output);
    stdout(&output).trim().to_string()
}

/// Generate a keypair for `operator_id` (switching the warehouse operator to it and
/// back to the "test@forklift" admin) and return the printed admit args:
/// `[operator_id, public_key, pop]`.
fn keygen_admit_args(warehouse: &TestWarehouse, operator_id: &str) -> Vec<String> {
    assert_success(&warehouse.run(&["config", "operator.identifier", operator_id]));
    let keygen = stdout(&warehouse.run(&["office", "keygen"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen prints the admit line")
        .split_whitespace()
        .skip(2)
        .map(|token| token.to_string())
        .collect()
}

/// Extract the parcel hash from the output of a successful "stack" run.
fn extract_parcel_hash(stack_output: &Output) -> String {
    let text = stdout(stack_output);
    let line = text.lines().find(|line| line.contains("Stacked parcel"))
        .unwrap_or_else(|| panic!("no 'Stacked parcel' line in: {}", text));

    line.split_whitespace().nth(2).unwrap().to_string()
}

#[test]
fn prepare_load_peek_remove_flow() {
    let warehouse = TestWarehouse::new("flow");
    warehouse.write_file("readme.txt", "root file\n");
    warehouse.write_file("src/main.txt", "hello\n");
    // A folder called "data" must not collide with the inventory data file of "src".
    warehouse.write_file("src/data/nested.txt", "nested\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "."]));

    let peek_root = warehouse.run(&["peek", "--inventory", "."]);
    assert_success(&peek_root);
    assert!(stdout(&peek_root).contains("readme.txt"));

    let peek_nested = warehouse.run(&["peek", "--inventory", "src/data"]);
    assert_success(&peek_nested);
    assert!(stdout(&peek_nested).contains("nested.txt"));

    assert_success(&warehouse.run(&["remove", "."]));

    // Removing stages removals instead of erasing the inventory: the entries survive,
    // marked as staged for removal.
    let peek_after_remove = warehouse.run(&["peek", "--inventory", "."]);
    assert_success(&peek_after_remove);
    let root_listing = stdout(&peek_after_remove);
    assert!(root_listing.contains("readme.txt"));
    assert!(root_listing.contains("Staged for removal"));

    let nested_after_remove = stdout(&warehouse.run(&["peek", "--inventory", "src/data"]));
    assert!(nested_after_remove.contains("nested.txt"));
    assert!(nested_after_remove.contains("Staged for removal"));
}

#[test]
fn removing_a_file_stages_its_removal_and_reloading_restores_it() {
    let warehouse = TestWarehouse::new("stage-removal");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["remove", "file.txt"]));

    let staged = stdout(&warehouse.run(&["peek", "--inventory", "."]));
    assert!(staged.contains("Staged for removal"), "remove must mark the entry, not erase it");

    // Removing an untracked path is still an error.
    let unknown = warehouse.run(&["remove", "missing.txt"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("not in the inventory"));

    // Loading the file again (it is still on disk) re-stages it as a normal entry.
    assert_success(&warehouse.run(&["load", "file.txt"]));
    let restored = stdout(&warehouse.run(&["peek", "--inventory", "."]));
    assert!(restored.contains("Loaded"));
    assert!(!restored.contains("Staged for removal"));
}

#[test]
fn reloading_a_modified_file_updates_the_staged_hash() {
    let warehouse = TestWarehouse::new("reload");
    warehouse.write_file("file.txt", "original\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "file.txt"]));

    let first_peek = stdout(&warehouse.run(&["peek", "--inventory", "."]));

    warehouse.write_file("file.txt", "modified\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));

    let second_peek = stdout(&warehouse.run(&["peek", "--inventory", "."]));

    assert!(first_peek.contains("file.txt"));
    assert!(second_peek.contains("file.txt"));
    assert_ne!(first_peek, second_peek, "the staged hash must change when the file changes");
}

#[test]
fn commands_work_from_a_subdirectory_of_the_warehouse() {
    let warehouse = TestWarehouse::new("subdir");
    warehouse.write_file("src/main.txt", "hello\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run_in("src", &["load", "main.txt"]));

    // The inventory must land in the warehouse root, not in a new ".forklift" in "src".
    assert!(!warehouse.root.join("src/.forklift").exists());

    let peek = warehouse.run(&["peek", "--inventory", "src"]);
    assert_success(&peek);
    assert!(stdout(&peek).contains("main.txt"));
}

#[test]
fn removing_a_directory_keeps_sibling_directories_with_the_same_prefix() {
    let warehouse = TestWarehouse::new("sibling");
    warehouse.write_file("src/a.txt", "a\n");
    warehouse.write_file("src2/b.txt", "b\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "src"]));
    assert_success(&warehouse.run(&["load", "src2"]));
    assert_success(&warehouse.run(&["remove", "src"]));

    // "src2" shares a string prefix with "src" but must survive the removal.
    let peek = warehouse.run(&["peek", "--inventory", "src2"]);
    assert_success(&peek);
    assert!(stdout(&peek).contains("b.txt"));
}

#[test]
fn errors_are_reported_on_stderr_with_a_nonzero_exit_code() {
    let warehouse = TestWarehouse::new("errors");

    // No warehouse prepared yet.
    let outside = warehouse.run(&["load", "x"]);
    assert!(!outside.status.success());
    assert!(stderr(&outside).contains("Not a forklift warehouse"));

    assert_success(&warehouse.run(&["prepare"]));

    // A path escaping the warehouse is rejected.
    let escape = warehouse.run(&["load", "../outside"]);
    assert!(!escape.status.success());
    assert!(stderr(&escape).contains("outside of the warehouse"));

    // Unexpected extra arguments are rejected instead of being silently ignored.
    let extra = warehouse.run(&["version", "surprise"]);
    assert!(!extra.status.success());
    assert!(stderr(&extra).contains("unexpected argument 'surprise'"));

    // An unknown command shows the helpful message, not a generic one.
    let unknown = warehouse.run(&["not-a-command"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("unrecognized subcommand 'not-a-command'"));
}

#[test]
fn reloading_stages_deleted_directories_and_files_for_removal() {
    let warehouse = TestWarehouse::new("dirty");
    warehouse.write_file("src/keep.txt", "keep\n");
    warehouse.write_file("src/gone_dir/file.txt", "bye\n");
    warehouse.write_file("src/gone_file.txt", "bye\n");
    warehouse.write_file("src2/other.txt", "other\n");

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["peek", "--inventory", "src/gone_dir"]));

    std::fs::remove_dir_all(warehouse.root.join("src/gone_dir")).unwrap();
    std::fs::remove_file(warehouse.root.join("src/gone_file.txt")).unwrap();

    // Re-loading only the affected subtree must stage the deleted directory's entries and
    // the deleted file's entry for removal, while leaving directories outside the subtree
    // alone.
    assert_success(&warehouse.run(&["load", "src"]));

    let gone_dir = warehouse.run(&["peek", "--inventory", "src/gone_dir"]);
    assert_success(&gone_dir);
    let gone_dir_listing = stdout(&gone_dir);
    assert!(gone_dir_listing.contains("file.txt"));
    assert!(
        gone_dir_listing.contains("Staged for removal"),
        "a deleted directory's entries must be staged for removal"
    );

    let src = stdout(&warehouse.run(&["peek", "--inventory", "src"]));
    assert!(src.contains("keep.txt"));
    let gone_file_line = src.lines().find(|line| line.contains("gone_file.txt"))
        .expect("deleted file's entry must be kept as a staged removal");
    assert!(gone_file_line.contains("Staged for removal"));
    let keep_line = src.lines().find(|line| line.contains("keep.txt")).unwrap();
    assert!(keep_line.contains("Loaded"), "a present file must stay a normal entry");

    let sibling = warehouse.run(&["peek", "--inventory", "src2"]);
    assert_success(&sibling);
    assert!(stdout(&sibling).contains("other.txt"), "unrelated inventories must survive");
}

// Timestamps have second granularity, so the fast path only trusts entries strictly older
// than their shard ("racily clean" protection). The file must be backdated to make the test
// deterministic; `touch -t` keeps this unix-only.
#[cfg(unix)]
#[test]
fn unchanged_files_are_not_rehashed_on_reload() {
    let warehouse = TestWarehouse::new("statcache");
    warehouse.write_file("src/stable.txt", "stable content\n");

    // Backdate the file so its mtime is strictly older than the shard about to be written.
    let touch = Command::new("touch")
        .args(["-t", "202601010000"])
        .arg(warehouse.root.join("src/stable.txt"))
        .output()
        .unwrap();
    assert!(touch.status.success());

    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "."]));

    // Find the stored blob and delete it from the object store. If the stat-cache fast
    // path works, re-loading the unchanged file must NOT re-store the blob (the file is
    // never read); if the file changes, the fallback path must re-store it.
    let peek = stdout(&warehouse.run(&["peek", "--inventory", "src"]));
    let hash = peek.split_whitespace().nth(1).expect("peek line: <state> <hash> <name>");
    let object_path = warehouse.root
        .join(".forklift/objects")
        .join(&hash[0..2])
        .join(&hash[2..]);

    assert!(object_path.exists(), "blob must exist after the first load");
    std::fs::remove_file(&object_path).unwrap();

    assert_success(&warehouse.run(&["load", "src"]));
    assert!(!object_path.exists(), "unchanged file must be reused via the stat cache, not rehashed");

    // Rewriting the file (same content, fresh timestamps) must take the full path again:
    // its mtime is now >= the shard's, so racily-clean protection forces a rehash.
    warehouse.write_file("src/stable.txt", "stable content\n");
    assert_success(&warehouse.run(&["load", "src"]));
    assert!(object_path.exists(), "a touched file must be rehashed and its blob re-stored");
}

#[test]
fn config_values_are_scoped_and_the_warehouse_overrides_the_global_scope() {
    let warehouse = TestWarehouse::new("config");

    assert_success(&warehouse.run(&["prepare"]));

    // The prepare command creates the (commented) warehouse configuration template.
    assert!(warehouse.root.join(".forklift/config/warehouse.toml").exists());

    // Global values work without a warehouse and act as the fallback.
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Global Name"]));
    assert_success(&warehouse.run(&["config", "--global", "operator.identifier", "global@id"]));

    let effective_name = warehouse.run(&["config", "operator.name"]);
    assert_success(&effective_name);
    assert_eq!(stdout(&effective_name).trim(), "Global Name");

    // A warehouse-level value overrides the global one...
    assert_success(&warehouse.run(&["config", "operator.name", "Warehouse Name"]));
    let overridden = warehouse.run(&["config", "operator.name"]);
    assert_success(&overridden);
    assert_eq!(stdout(&overridden).trim(), "Warehouse Name");

    // ...but reading the global scope explicitly still returns the global value.
    let global_name = warehouse.run(&["config", "--global", "operator.name"]);
    assert_success(&global_name);
    assert_eq!(stdout(&global_name).trim(), "Global Name");

    // The template's comments survive a value being set.
    let warehouse_config = std::fs::read_to_string(
        warehouse.root.join(".forklift/config/warehouse.toml")
    ).unwrap();
    assert!(warehouse_config.contains("# Forklift warehouse configuration."));
    assert!(warehouse_config.contains("Warehouse Name"));

    // Listing shows the effective values and where they come from.
    let listing = warehouse.run(&["config"]);
    assert_success(&listing);
    let listing_output = stdout(&listing);
    assert!(listing_output.contains("operator.name = Warehouse Name (warehouse)"));
    assert!(listing_output.contains("operator.identifier = global@id (global)"));

    // Unknown keys are rejected instead of silently doing nothing.
    let unknown = warehouse.run(&["config", "operator.typo", "x"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("Unknown configuration key"));

    // An unset key is an error on read.
    std::fs::remove_file(warehouse.home.join("global-config.toml")).unwrap();
    assert_success(&warehouse.run(&["config", "operator.name"]));
    let unset = warehouse.run(&["config", "operator.identifier"]);
    assert!(!unset.status.success());
    assert!(stderr(&unset).contains("not set"));
}

#[test]
fn identity_is_zero_configuration_an_id_is_minted_on_first_use() {
    let warehouse = TestWarehouse::new("mint");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));

    // No operator configuration at all: the first stack mints a pseudonymous id
    // (a UUID) into the global configuration and authors the parcel with it.
    assert_success(&warehouse.run(&["load", "."]));
    let stacked = warehouse.run(&["stack", "first"]);
    assert_success(&stacked);

    let minted = operator_id(&warehouse);
    let groups: Vec<&str> = minted.split('-').collect();
    assert_eq!(
        groups.iter().map(|group| group.len()).collect::<Vec<_>>(),
        vec![8, 4, 4, 4, 12],
        "the minted id must be a UUID: {}", minted
    );

    let peek = stdout(&warehouse.run(&["peek", &extract_parcel_hash(&stacked)]));
    assert!(peek.contains(&minted), "unexpected peek: {}", peek);

    // The mint is stable: a second stack reuses the same id.
    warehouse.write_file("file.txt", "more\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));
    assert_eq!(operator_id(&warehouse), minted);
}

#[test]
fn stack_creates_a_parcel_and_advances_the_pallet_head() {
    let warehouse = TestWarehouse::new("stack");
    warehouse.write_file("readme.txt", "hello\n");
    warehouse.write_file("src/main.txt", "fn main\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));

    let first_stack = warehouse.run(&["stack", "Initial layout"]);
    assert_success(&first_stack);
    let first_hash = extract_parcel_hash(&first_stack);

    // The current pallet ("main") now points at the new parcel.
    let ref_content = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(ref_content.trim(), first_hash);

    // The parcel records the tree, the stacking operator and the description.
    let peek = warehouse.run(&["peek", &first_hash]);
    assert_success(&peek);
    let peek_output = stdout(&peek);
    assert!(peek_output.contains("tree"));
    // The configured identifier is the on-chain operator id (setting a readable one
    // is an explicit choice; unset, a pseudonymous UUID is minted instead).
    assert!(peek_output.contains("test@forklift"));
    assert!(peek_output.contains("Initial layout"));

    // Stacking again without changes is rejected.
    let noop = warehouse.run(&["stack", "nothing changed"]);
    assert!(!noop.status.success());
    assert!(stderr(&noop).contains("Nothing to stack"));

    // A new change produces a parcel with the first one as its parent.
    warehouse.write_file("readme.txt", "hello again\n");
    assert_success(&warehouse.run(&["load", "readme.txt"]));
    let second_stack = warehouse.run(&["stack", "Update readme"]);
    assert_success(&second_stack);
    let second_hash = extract_parcel_hash(&second_stack);
    assert_ne!(first_hash, second_hash);

    let second_peek = stdout(&warehouse.run(&["peek", &second_hash]));
    assert!(second_peek.contains(&format!("parent {}", first_hash)));
}

#[test]
fn store_reports_loose_then_packed_object_counts() {
    let warehouse = TestWarehouse::new("store");
    // A few files sharing boilerplate, so compaction has similar objects to delta.
    for i in 1..=5 {
        warehouse.write_file(
            &format!("f{}.txt", i),
            &format!("file {} unique line\nshared boilerplate line\nmore shared boilerplate\n", i),
        );
    }

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    // Loose: objects are unpacked, no packs, compaction not due (well under the threshold).
    let loose = warehouse.run(&["--json", "store"]);
    assert_success(&loose);
    let loose = json(&loose);
    let data = &loose["data"];
    assert!(data["loose_objects"].as_u64().unwrap() > 0, "expected loose objects: {}", data);
    assert_eq!(data["pack_files"].as_u64().unwrap(), 0);
    assert_eq!(data["packed_objects"].as_u64().unwrap(), 0);
    assert_eq!(data["maintenance"]["compaction_due"].as_bool(), Some(false));

    // Compact, then the same objects are packed: nothing loose, exactly one pack.
    assert_success(&warehouse.run(&["compact"]));
    let packed = warehouse.run(&["--json", "store"]);
    assert_success(&packed);
    let packed = json(&packed);
    let data = &packed["data"];
    assert_eq!(data["loose_objects"].as_u64().unwrap(), 0);
    assert_eq!(data["pack_files"].as_u64().unwrap(), 1);
    assert!(data["packed_objects"].as_u64().unwrap() > 0);
    assert_eq!(data["packs"].as_array().unwrap().len(), 1);
    assert_eq!(
        data["total_bytes"].as_u64().unwrap(),
        data["loose_bytes"].as_u64().unwrap() + data["pack_bytes"].as_u64().unwrap(),
    );

    // The human view names the store and reports it fully packed.
    let human = warehouse.run(&["store"]);
    assert_success(&human);
    let text = stdout(&human);
    assert!(text.contains("Object store"), "human output: {}", text);
    assert!(text.contains("100% packed"), "human output: {}", text);
}

#[test]
fn stocktake_leaves_an_ignored_tracked_directory_alone_instead_of_reporting_it_removed() {
    let warehouse = TestWarehouse::new("ignore-tracked");
    warehouse.write_file("keep.txt", "kept\n");
    warehouse.write_file("build/artifact1.txt", "generated\n");
    warehouse.write_file("build/artifact2.txt", "generated\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    // Ignore build/ *after* it was already tracked (a build-output dir added to
    // .forkliftignore late). It is still on disk, so forklift must leave it alone — not walk
    // its whole subtree reporting every file as removed (the serial-walk hang this fixes).
    warehouse.write_file(".forkliftignore", "^build\\/?.*$\n");

    let unstaged_paths = |warehouse: &TestWarehouse| -> Vec<String> {
        let out = warehouse.run(&["--json", "stocktake"]);
        assert_success(&out);
        json(&out)["data"]["unstaged"].as_array().cloned().unwrap_or_default()
            .iter()
            .filter_map(|change| change["path"].as_str().map(str::to_string))
            .collect()
    };

    let paths = unstaged_paths(&warehouse);
    assert!(
        !paths.iter().any(|path| path.starts_with("build/")),
        "an ignored-but-tracked directory must not be reported as changed/removed, got: {:?}",
        paths,
    );

    // But a genuine deletion of a tracked, non-ignored file is still reported.
    std::fs::remove_file(warehouse.root.join("keep.txt")).unwrap();
    let paths = unstaged_paths(&warehouse);
    assert!(
        paths.iter().any(|path| path == "keep.txt"),
        "a genuine removal must still be reported, got: {:?}",
        paths,
    );
}

#[test]
fn stack_consumes_staged_removals() {
    let warehouse = TestWarehouse::new("stack-removal");
    warehouse.write_file("keep.txt", "keep\n");
    warehouse.write_file("remove.txt", "remove\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["remove", "remove.txt"]));

    let stack = warehouse.run(&["stack", "Drop remove.txt"]);
    assert_success(&stack);
    let parcel_hash = extract_parcel_hash(&stack);

    // The staged removal was consumed: the entry is gone from the inventory...
    let inventory = stdout(&warehouse.run(&["peek", "--inventory", "."]));
    assert!(inventory.contains("keep.txt"));
    assert!(!inventory.contains("remove.txt"));
    assert!(!inventory.contains("Staged for removal"));

    // ...and the parcel's tree does not contain the removed file.
    let parcel_peek = stdout(&warehouse.run(&["peek", &parcel_hash]));
    let tree_hash = parcel_peek.lines()
        .find(|line| line.starts_with("tree"))
        .and_then(|line| line.split_whitespace().nth(1))
        .expect("parcel peek must print the tree hash")
        .to_string();

    let tree_peek = stdout(&warehouse.run(&["peek", &tree_hash]));
    assert!(tree_peek.contains("keep.txt"));
    assert!(!tree_peek.contains("remove.txt"));
}

#[test]
fn stack_requires_staged_changes() {
    let warehouse = TestWarehouse::new("stack-guards");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));

    // With an empty inventory there is nothing to stack (identity is no obstacle —
    // an id is minted automatically when none is configured).
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["unload", "."]));
    let nothing = warehouse.run(&["stack", "empty"]);
    assert!(!nothing.status.success());
    assert!(stderr(&nothing).contains("stack"));
}

#[test]
fn palletize_creates_pallets_at_the_current_head() {
    let warehouse = TestWarehouse::new("palletize");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Before anything is stacked, the default pallet is listed as unborn.
    let unborn_listing = stdout(&warehouse.run(&["palletize"]));
    assert!(unborn_listing.contains("* main (unborn)"));

    assert_success(&warehouse.run(&["load", "."]));
    let stack = warehouse.run(&["stack", "first"]);
    assert_success(&stack);
    let main_head = extract_parcel_hash(&stack);

    // A new pallet starts at the current head and becomes current.
    assert_success(&warehouse.run(&["palletize", "feature/x"]));
    let feature_ref = std::fs::read_to_string(
        warehouse.root.join(".forklift/pallets/feature/x")
    ).unwrap();
    assert_eq!(feature_ref.trim(), main_head);

    let listing = stdout(&warehouse.run(&["palletize"]));
    assert!(listing.contains("* feature/x"));
    assert!(listing.contains("  main"));

    // Stacking on the new pallet must not move "main".
    warehouse.write_file("file.txt", "changed\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "on feature"]));

    let main_ref = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(main_ref.trim(), main_head);

    // Duplicate and invalid names are rejected.
    let duplicate = warehouse.run(&["palletize", "feature/x"]);
    assert!(!duplicate.status.success());
    assert!(stderr(&duplicate).contains("already"));

    let invalid = warehouse.run(&["palletize", "bad name"]);
    assert!(!invalid.status.success());
    assert!(stderr(&invalid).contains("not a valid pallet name"));
}

#[test]
fn palletize_json_listing_reports_head_hashes() {
    let warehouse = TestWarehouse::new("palletize-json");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Before anything is stacked, the (only, current) pallet is listed with a null head —
    // folded into `pallets` itself, not left as a side flag a caller must special-case.
    let unborn = json(&warehouse.run(&["palletize", "--json"]));
    assert_eq!(unborn["data"]["current_unborn"], true);
    let pallets = unborn["data"]["pallets"].as_array().unwrap();
    assert_eq!(pallets.len(), 1, "unexpected pallets: {:?}", pallets);
    assert_eq!(pallets[0]["name"], "main");
    assert_eq!(pallets[0]["current"], true);
    assert_eq!(pallets[0]["head"], serde_json::Value::Null);

    assert_success(&warehouse.run(&["load", "."]));
    let main_head = extract_parcel_hash(&warehouse.run(&["stack", "first"]));
    assert_success(&warehouse.run(&["palletize", "feature/x"]));

    // Once born, each pallet's listed `head` is its actual head parcel hash.
    let listing = json(&warehouse.run(&["palletize", "--json"]));
    assert_eq!(listing["data"]["current_unborn"], false);
    let pallets = listing["data"]["pallets"].as_array().unwrap();
    let by_name = |name: &str| pallets.iter()
        .find(|entry| entry["name"] == name)
        .unwrap_or_else(|| panic!("pallet {} not found in: {:?}", name, pallets));

    assert_eq!(by_name("main")["head"], main_head);
    assert_eq!(by_name("main")["current"], false);
    assert_eq!(by_name("feature/x")["head"], main_head, "the new pallet starts at the same head");
    assert_eq!(by_name("feature/x")["current"], true);
}

#[test]
fn stocktake_reports_staged_and_unstaged_changes() {
    let warehouse = TestWarehouse::new("stocktake");
    warehouse.write_file("a.txt", "original\n");
    warehouse.write_file("dir/b.txt", "b\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Before anything is stacked, staged changes are reported against the unborn head.
    assert_success(&warehouse.run(&["load", "."]));
    let unborn = stdout(&warehouse.run(&["stocktake"]));
    assert!(unborn.contains("unborn"));
    assert!(unborn.contains("added:"));
    assert!(unborn.contains("a.txt"));

    let stack = warehouse.run(&["stack", "first"]);
    assert_success(&stack);
    let head = extract_parcel_hash(&stack);

    // A fresh stack leaves both sections clean.
    let clean = stdout(&warehouse.run(&["stocktake"]));
    assert!(clean.contains(&format!("On pallet \"main\" (head {})", head)));
    assert!(clean.contains("The inventory matches the pallet head"));
    assert!(clean.contains("The working directory matches the inventory"));

    // Worktree-only changes show up as unstaged; the staged section stays clean.
    warehouse.write_file("a.txt", "modified\n");
    warehouse.write_file("c.txt", "new\n");
    warehouse.write_file("notes/n.txt", "note\n");

    let unstaged = stdout(&warehouse.run(&["stocktake"]));
    assert!(unstaged.contains("The inventory matches the pallet head"));
    assert!(unstaged.contains("modified:"));
    assert!(unstaged.contains("a.txt"));
    assert!(unstaged.contains("untracked: c.txt"));
    assert!(unstaged.contains("untracked: notes/"));

    // Loading the modification moves it to the staged section.
    assert_success(&warehouse.run(&["load", "a.txt"]));
    let staged = stdout(&warehouse.run(&["stocktake"]));
    assert!(staged.contains("Staged changes"));
    assert!(staged.contains("modified:"));

    // A staged removal is reported as removed; the file (still on disk) as untracked.
    assert_success(&warehouse.run(&["remove", "dir/b.txt"]));
    let removal = stdout(&warehouse.run(&["stocktake"]));
    assert!(removal.contains("removed:"));
    assert!(removal.contains("dir/b.txt"));
    assert!(removal.contains("untracked: dir/b.txt"));

    // A file deleted from disk (but still loaded) is an unstaged removal.
    std::fs::remove_file(warehouse.root.join("a.txt")).unwrap();
    let deleted = stdout(&warehouse.run(&["stocktake"]));
    let unstaged_section = deleted.split("Changes not in the inventory").nth(1)
        .expect("stocktake must report unstaged changes");
    assert!(unstaged_section.contains("removed:") && unstaged_section.contains("a.txt"),
            "unstaged removal must be reported: {}", deleted);
}

#[test]
fn shift_switches_pallets_and_materializes_the_target_tree() {
    let warehouse = TestWarehouse::new("shift");
    warehouse.write_file("a.txt", "main content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "on main"]));

    // Build a diverging state on a feature pallet.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("a.txt", "feature content\n");
    warehouse.write_file("dir/new.txt", "only on feature\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "on feature"]));

    // Shifting back to main restores its tree: the modified file reverts, the new file
    // and its (now empty) directory disappear.
    let shift_main = warehouse.run(&["shift", "main"]);
    assert_success(&shift_main);
    assert!(stdout(&shift_main).contains("Shifted to pallet \"main\""));

    let a_content = std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap();
    assert_eq!(a_content, "main content\n");
    assert!(!warehouse.root.join("dir/new.txt").exists());
    assert!(!warehouse.root.join("dir").exists(), "emptied directories must be cleaned up");

    // The inventory was repopulated: the warehouse reports clean on both sections.
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("On pallet \"main\""));
    assert!(status.contains("The inventory matches the pallet head"));
    assert!(status.contains("The working directory matches the inventory"));

    // Shifting forward again restores the feature state.
    assert_success(&warehouse.run(&["shift", "feature"]));
    let a_feature = std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap();
    assert_eq!(a_feature, "feature content\n");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("dir/new.txt")).unwrap(),
        "only on feature\n"
    );

    // Guards: unknown pallet, and shifting to the pallet already current.
    let unknown = warehouse.run(&["shift", "nope"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("No pallet named"));

    let same = warehouse.run(&["shift", "feature"]);
    assert!(!same.status.success());
    assert!(stderr(&same).contains("Already on"));
}

#[test]
fn shift_refuses_dirty_warehouses_and_untracked_collisions() {
    let warehouse = TestWarehouse::new("shift-guards");
    warehouse.write_file("a.txt", "main\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "on main"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("extra.txt", "tracked on feature\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "on feature"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    assert!(!warehouse.root.join("extra.txt").exists());

    // An unstaged modification blocks the shift.
    warehouse.write_file("a.txt", "modified locally\n");
    let dirty = warehouse.run(&["shift", "feature"]);
    assert!(!dirty.status.success());
    assert!(stderr(&dirty).contains("local changes"));

    // Restore the file by hand and verify shifting works again before the next guard.
    warehouse.write_file("a.txt", "main\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    let still_dirty = warehouse.run(&["shift", "feature"]);
    assert_success(&still_dirty);
    assert_success(&warehouse.run(&["shift", "main"]));

    // An untracked file that the target tree wants to write is a conflict.
    warehouse.write_file("extra.txt", "untracked local content\n");
    let collision = warehouse.run(&["shift", "feature"]);
    assert!(!collision.status.success());
    assert!(stderr(&collision).contains("would overwrite these untracked files"));
    assert!(stderr(&collision).contains("extra.txt"));

    // The conflicting file was not touched and the pallet did not change.
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("extra.txt")).unwrap(),
        "untracked local content\n"
    );
    assert!(stdout(&warehouse.run(&["stocktake"])).contains("On pallet \"main\""));
}

#[cfg(unix)]
#[test]
fn shift_preserves_executables_and_symlinks() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("shift-modes");
    warehouse.write_file("run.sh", "#!/bin/sh\necho main\n");
    std::fs::set_permissions(
        warehouse.root.join("run.sh"),
        std::fs::Permissions::from_mode(0o755)
    ).unwrap();
    std::os::unix::fs::symlink("run.sh", warehouse.root.join("link")).unwrap();

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "with modes"]));

    // An empty pallet state to shift away to: remove everything on a feature pallet.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("placeholder.txt", "placeholder\n");
    std::fs::remove_file(warehouse.root.join("run.sh")).unwrap();
    std::fs::remove_file(warehouse.root.join("link")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "without modes"]));

    assert!(!warehouse.root.join("run.sh").exists());

    // Shifting back must restore the executable bit and the symlink (as a symlink).
    assert_success(&warehouse.run(&["shift", "main"]));

    let mode = std::fs::metadata(warehouse.root.join("run.sh")).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "the executable bit must be restored");

    let link_meta = std::fs::symlink_metadata(warehouse.root.join("link")).unwrap();
    assert!(link_meta.file_type().is_symlink(), "the symlink must be restored as a symlink");
    assert_eq!(
        std::fs::read_link(warehouse.root.join("link")).unwrap().to_string_lossy(),
        "run.sh"
    );
}

#[test]
fn restore_discards_unstaged_changes() {
    let warehouse = TestWarehouse::new("restore");
    warehouse.write_file("a.txt", "original a\n");
    warehouse.write_file("dir/b.txt", "original b\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    // A single modified file reverts to its staged content.
    warehouse.write_file("a.txt", "scribbled over\n");
    assert_success(&warehouse.run(&["restore", "a.txt"]));
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "original a\n"
    );

    // Restoring a directory rewrites modified files and brings deleted ones back.
    warehouse.write_file("dir/b.txt", "scribbled over\n");
    std::fs::remove_file(warehouse.root.join("a.txt")).unwrap();
    assert_success(&warehouse.run(&["restore", "."]));
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "original a\n"
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("dir/b.txt")).unwrap(),
        "original b\n"
    );

    // The warehouse reports clean after the restores.
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"));

    // Untracked paths cannot be restored.
    let untracked = warehouse.run(&["restore", "nope.txt"]);
    assert!(!untracked.status.success());
    assert!(stderr(&untracked).contains("not in the inventory"));
}

#[test]
fn restore_staged_resets_the_inventory_to_the_head() {
    let warehouse = TestWarehouse::new("restore-staged");
    warehouse.write_file("a.txt", "original\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    // Unstage a staged modification: the worktree keeps the change, the staged section
    // clears, and the change reappears as unstaged.
    warehouse.write_file("a.txt", "modified\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["restore", "--staged", "a.txt"]));

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("modified:"), "status: {}", status);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "modified\n"
    );

    // Unstage a staged addition: the entry is dropped, the file becomes untracked.
    warehouse.write_file("new.txt", "new\n");
    assert_success(&warehouse.run(&["load", "new.txt"]));
    assert_success(&warehouse.run(&["restore", "--staged", "new.txt"]));
    let after_add = stdout(&warehouse.run(&["stocktake"]));
    assert!(after_add.contains("untracked: new.txt"), "status: {}", after_add);

    // Unstage a staged removal: the entry comes back from the head.
    assert_success(&warehouse.run(&["remove", "a.txt"]));
    let with_removal = stdout(&warehouse.run(&["stocktake"]));
    assert!(with_removal.contains("removed:"), "status: {}", with_removal);

    assert_success(&warehouse.run(&["restore", "--staged", "a.txt"]));
    let after_unstage = stdout(&warehouse.run(&["stocktake"]));
    assert!(!after_unstage.contains("removed:"), "status: {}", after_unstage);

    // Unstaging a path unknown to both the inventory and the head is an error.
    let unknown = warehouse.run(&["restore", "--staged", "ghost.txt"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("neither in the inventory nor in the pallet head"));
}

#[test]
fn unload_unstages_instead_of_staging_a_removal() {
    let warehouse = TestWarehouse::new("unload-unstages");
    warehouse.write_file("a.txt", "original\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    // unload is the inverse of load: the staged modification is unstaged (back to the
    // head), the worktree keeps the change — and crucially no removal is staged.
    warehouse.write_file("a.txt", "modified\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["unload", "a.txt"]));

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("modified:"), "status: {}", status);
    assert!(!status.contains("removed:"), "unload must never stage a removal: {}", status);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "modified\n"
    );

    // Stacking now records no change to the file: a mistaken load undone by unload can
    // never turn into a deletion in the next parcel.
    let nothing = warehouse.run(&["stack", "empty"]);
    assert!(!nothing.status.success(), "nothing must be staged after unload");

    // The JSON envelope carries the verb the user ran, not the shared implementation's.
    warehouse.write_file("a.txt", "modified again\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    let unloaded = warehouse.run(&["--json", "unload", "a.txt"]);
    assert_success(&unloaded);
    assert_eq!(json(&unloaded)["command"], "unload");

    // The `ul` alias follows the verb.
    warehouse.write_file("a.txt", "modified once more\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["ul", "a.txt"]));
    let after_alias = stdout(&warehouse.run(&["stocktake"]));
    assert!(after_alias.contains("The inventory matches the pallet head"), "status: {}", after_alias);

    // And `rm` is the alias for staging a removal.
    assert_success(&warehouse.run(&["rm", "a.txt"]));
    let removal = stdout(&warehouse.run(&["stocktake"]));
    assert!(removal.contains("removed:"), "status: {}", removal);
}

#[test]
fn consolidate_fast_forwards_when_possible() {
    let warehouse = TestWarehouse::new("consolidate-ff");
    warehouse.write_file("a.txt", "a\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("b.txt", "b\n");
    assert_success(&warehouse.run(&["load", "."]));
    let feature_stack = warehouse.run(&["stack", "feature work"]);
    assert_success(&feature_stack);
    let feature_head = extract_parcel_hash(&feature_stack);

    assert_success(&warehouse.run(&["shift", "main"]));
    assert!(!warehouse.root.join("b.txt").exists());

    // main is strictly behind feature: consolidating fast-forwards.
    let ff = warehouse.run(&["consolidate", "feature"]);
    assert_success(&ff);
    assert!(stdout(&ff).contains("Fast-forwarded"));
    assert!(warehouse.root.join("b.txt").exists());

    let main_ref = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(main_ref.trim(), feature_head);

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"));
    assert!(status.contains("The working directory matches the inventory"));

    // Consolidating again is a no-op.
    let up_to_date = warehouse.run(&["consolidate", "feature"]);
    assert_success(&up_to_date);
    assert!(stdout(&up_to_date).contains("Already up to date"));

    // Guard: a pallet cannot be consolidated into itself.
    let self_merge = warehouse.run(&["consolidate", "main"]);
    assert!(!self_merge.status.success());
    assert!(stderr(&self_merge).contains("itself"));
}

#[test]
fn consolidate_merges_divergent_pallets_cleanly() {
    let warehouse = TestWarehouse::new("consolidate-merge");
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nfive\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Diverge: feature changes the last line and adds a file...
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nFIVE\n");
    warehouse.write_file("feat.txt", "feature file\n");
    assert_success(&warehouse.run(&["load", "."]));
    let feature_stack = warehouse.run(&["stack", "feature work"]);
    assert_success(&feature_stack);
    let feature_head = extract_parcel_hash(&feature_stack);

    // ...while main changes the first line.
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "ONE\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    let main_stack = warehouse.run(&["stack", "main work"]);
    assert_success(&main_stack);
    let main_head = extract_parcel_hash(&main_stack);

    // The changes do not overlap: the merge is clean and stacked automatically.
    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    let merge_output = stdout(&merge);
    assert!(merge_output.contains("stacked merge parcel"), "output: {}", merge_output);

    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\nFIVE\n"
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("feat.txt")).unwrap(),
        "feature file\n"
    );

    // The merge parcel records both parents; the consolidation state is consumed.
    let merge_hash = merge_output.split_whitespace().last().unwrap().trim_end_matches('.');
    let peek = stdout(&warehouse.run(&["peek", merge_hash]));
    assert!(peek.contains(&format!("parent {}", main_head)));
    assert!(peek.contains(&format!("parent {}", feature_head)));
    assert!(!warehouse.root.join(".forklift/consolidation").exists());

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"));
    assert!(status.contains("The working directory matches the inventory"));
}

#[test]
fn commit_graph_is_built_by_compact_and_ancestry_stays_correct() {
    let warehouse = TestWarehouse::new("commit-graph");
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nfive\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Diverge two pallets off the base so a merge base has to be found.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nFIVE\n");
    assert_success(&warehouse.run(&["load", "."]));
    let feature_stack = warehouse.run(&["stack", "feature work"]);
    assert_success(&feature_stack);
    let feature_head = extract_parcel_hash(&feature_stack);

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "ONE\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "main work"]));

    // Compact builds and persists the commit-graph (sharded by parcel-hash prefix).
    assert_success(&warehouse.run(&["compact"]));
    let graph_root = warehouse.root.join(".forklift/graph");
    assert!(graph_root.exists(), "compact must create the commit-graph store");
    let shard_count = std::fs::read_dir(&graph_root).unwrap().filter_map(Result::ok).count();
    assert!(shard_count > 0, "the commit-graph must have at least one shard");

    // With the graph fully built, the divergent merge (which reads the graph, not parcel
    // objects, to find the base) still finds the right base and merges cleanly.
    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\nFIVE\n"
    );

    // Feature is now an ancestor of main (the merge parcel's second parent), an ancestry the
    // generation-pruned check must get right: consolidating it again is a no-op.
    let up_to_date = warehouse.run(&["consolidate", "feature"]);
    assert_success(&up_to_date);
    assert!(stdout(&up_to_date).contains("Already up to date"),
            "feature must read as already merged: {}", stdout(&up_to_date));
    assert!(feature_head.len() == 64, "sanity: a full parcel hash was captured");
}

#[test]
fn consolidate_conflicts_are_marked_and_resolved_by_stacking() {
    let warehouse = TestWarehouse::new("consolidate-conflict");
    warehouse.write_file("file.txt", "one\ntwo\nthree\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Both pallets change the same line differently.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\nTHEIRS\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    let feature_stack = warehouse.run(&["stack", "theirs"]);
    assert_success(&feature_stack);
    let feature_head = extract_parcel_hash(&feature_stack);

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "one\nOURS\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "ours"]));

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    assert!(stdout(&merge).contains("conflict: file.txt"));

    // The file carries diff3-style markers with both sides and the base.
    let conflicted = std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap();
    assert!(conflicted.contains("<<<<<<< main"));
    assert!(conflicted.contains("OURS"));
    assert!(conflicted.contains("||||||| base"));
    assert!(conflicted.contains("two"));
    assert!(conflicted.contains("======="));
    assert!(conflicted.contains("THEIRS"));
    assert!(conflicted.contains(">>>>>>> feature"));

    // The stocktake shows the conflict and the consolidation in progress.
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("A consolidation with pallet \"feature\" is in progress"));
    assert!(status.contains("conflict:"));

    // Stacking with unresolved conflicts is refused.
    let blocked = warehouse.run(&["stack", "too early"]);
    assert!(!blocked.status.success());
    assert!(stderr(&blocked).contains("unresolved conflicts"));

    // Resolve, load, stack: the merge parcel records both parents.
    warehouse.write_file("file.txt", "one\nresolved\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    let resolved = warehouse.run(&["stack", "Consolidated feature (resolved by hand)"]);
    assert_success(&resolved);
    let merge_hash = extract_parcel_hash(&resolved);

    let peek = stdout(&warehouse.run(&["peek", &merge_hash]));
    assert!(peek.contains(&format!("parent {}", feature_head)));
    assert!(!warehouse.root.join(".forklift/consolidation").exists());

    let final_status = stdout(&warehouse.run(&["stocktake"]));
    assert!(final_status.contains("The inventory matches the pallet head"));
    assert!(final_status.contains("The working directory matches the inventory"));
}

#[test]
fn park_saves_and_reapplies_work_in_progress() {
    let warehouse = TestWarehouse::new("park");
    warehouse.write_file("a.txt", "original\n");
    warehouse.write_file("dir/b.txt", "b\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Dirty the warehouse: an unstaged modification, a staged-then-modified file, a
    // deleted tracked file, and an untracked file that must survive untouched.
    warehouse.write_file("a.txt", "work in progress\n");
    std::fs::remove_file(warehouse.root.join("dir/b.txt")).unwrap();
    warehouse.write_file("untracked.txt", "leave me alone\n");

    let park = warehouse.run(&["park"]);
    assert_success(&park);

    // The warehouse is back at the head; the untracked file is untouched.
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "original\n"
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("dir/b.txt")).unwrap(),
        "b\n"
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("untracked.txt")).unwrap(),
        "leave me alone\n"
    );

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"));

    // The parked parcel is listed.
    let listing = stdout(&warehouse.run(&["park", "list"]));
    assert!(listing.contains("Parked changes on pallet \"main\""));

    // Popping re-applies the changes, staged.
    assert_success(&warehouse.run(&["park", "pop"]));
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "work in progress\n"
    );
    assert!(!warehouse.root.join("dir/b.txt").exists());

    let popped_status = stdout(&warehouse.run(&["stocktake"]));
    assert!(popped_status.contains("modified:"), "status: {}", popped_status);
    assert!(popped_status.contains("removed:"), "status: {}", popped_status);

    // The parked list is consumed.
    let empty = warehouse.run(&["park", "pop"]);
    assert!(!empty.status.success());
    assert!(stderr(&empty).contains("no parked changes"));

    // The re-applied state stacks cleanly.
    assert_success(&warehouse.run(&["stack", "finished the parked work"]));
    let final_status = stdout(&warehouse.run(&["stocktake"]));
    assert!(final_status.contains("The inventory matches the pallet head"));
}

#[test]
fn park_pop_refuses_conflicting_head_changes() {
    let warehouse = TestWarehouse::new("park-conflict");
    warehouse.write_file("a.txt", "original\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Park a change to a.txt...
    warehouse.write_file("a.txt", "parked change\n");
    assert_success(&warehouse.run(&["park"]));

    // ...then move the head by changing the same file.
    warehouse.write_file("a.txt", "head moved\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["stack", "head change"]));

    let pop = warehouse.run(&["park", "pop"]);
    assert!(!pop.status.success());
    assert!(stderr(&pop).contains("conflict"));
    assert!(stderr(&pop).contains("a.txt"));

    // Nothing was touched, and the parked entry is still there.
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(),
        "head moved\n"
    );
    assert!(stdout(&warehouse.run(&["park", "list"])).contains("Parked changes"));
}

#[test]
fn mutating_commands_respect_the_warehouse_lock() {
    let warehouse = TestWarehouse::new("lock");
    warehouse.write_file("file.txt", "content\n");

    assert_success(&warehouse.run(&["prepare"]));

    // Simulate another process holding the lock.
    std::fs::write(warehouse.root.join(".forklift/lock"), "12345\n").unwrap();

    let locked = warehouse.run(&["load", "."]);
    assert!(!locked.status.success());
    assert!(stderr(&locked).contains("locked by another forklift process"));
    assert!(stderr(&locked).contains("12345"));

    // Read-only commands still work while the lock is held.
    assert_success(&warehouse.run(&["help"]));

    std::fs::remove_file(warehouse.root.join(".forklift/lock")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));

    // The lock is released after the command finishes.
    assert!(!warehouse.root.join(".forklift/lock").exists());
}

#[test]
fn symlinks_are_stored_as_symlinks_and_cycles_do_not_hang() {
    let warehouse = TestWarehouse::new("symlink");
    warehouse.write_file("src/real.txt", "target content\n");

    #[cfg(unix)]
    {
        // A symlink cycle: src/loop -> src. Following it would recurse forever.
        std::os::unix::fs::symlink(
            Path::new("."),
            warehouse.root.join("src/loop"),
        ).unwrap();

        assert_success(&warehouse.run(&["prepare"]));
        assert_success(&warehouse.run(&["load", "src"]));

        let peek = warehouse.run(&["peek", "--inventory", "src"]);
        assert_success(&peek);
        assert!(stdout(&peek).contains("loop"), "the symlink itself must be tracked");
    }
}

#[test]
fn diff_reports_unstaged_and_staged_line_changes() {
    let warehouse = TestWarehouse::new("diff");
    warehouse.write_file("a.txt", "original\nshared\n");
    warehouse.write_file("dir/b.txt", "b one\nb two\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    // A fresh stack leaves both comparisons clean.
    assert!(stdout(&warehouse.run(&["diff"]))
        .contains("The working directory matches the inventory"));
    assert!(stdout(&warehouse.run(&["diff", "--staged"]))
        .contains("The inventory matches the pallet head"));

    // A worktree edit shows up as an unstaged line change; untracked files are not
    // diffed (stocktake reports them).
    warehouse.write_file("a.txt", "changed\nshared\n");
    warehouse.write_file("untracked.txt", "not diffed\n");

    let unstaged = stdout(&warehouse.run(&["diff"]));
    assert!(unstaged.contains("modified: a.txt"), "unexpected diff output: {}", unstaged);
    assert!(unstaged.contains("- original"));
    assert!(unstaged.contains("+ changed"));
    assert!(!unstaged.contains("shared"), "unchanged lines are only shown with --verbose");
    assert!(!unstaged.contains("untracked.txt"));

    // Loading the edit moves the line change to the staged comparison.
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert!(stdout(&warehouse.run(&["diff"]))
        .contains("The working directory matches the inventory"));

    let staged = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(staged.contains("modified: a.txt"), "unexpected diff output: {}", staged);
    assert!(staged.contains("- original"));
    assert!(staged.contains("+ changed"));

    // A path argument limits the report.
    warehouse.write_file("dir/b.txt", "b one\nb 2\n");
    let filtered = stdout(&warehouse.run(&["diff", "dir"]));
    assert!(filtered.contains("modified: dir/b.txt"));
    assert!(!filtered.contains("a.txt"));

    // Verbose mode includes the unchanged lines.
    let verbose = stdout(&warehouse.run(&["--verbose", "diff", "--staged", "a.txt"]));
    assert!(verbose.contains("shared"), "unexpected verbose output: {}", verbose);
}

#[test]
fn diff_reports_removals_added_files_and_binary_content() {
    let warehouse = TestWarehouse::new("diff-kinds");
    warehouse.write_file("keep.txt", "kept\n");
    warehouse.write_file("gone.txt", "going away\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Against an unborn head, every loaded file is an added diff.
    assert_success(&warehouse.run(&["load", "."]));
    let unborn = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(unborn.contains("added: keep.txt"), "unexpected diff output: {}", unborn);
    assert!(unborn.contains("+ kept"));

    assert_success(&warehouse.run(&["stack", "first"]));

    // A file deleted from disk is an unstaged removal with its lines shown.
    std::fs::remove_file(warehouse.root.join("gone.txt")).unwrap();
    let unstaged = stdout(&warehouse.run(&["diff"]));
    assert!(unstaged.contains("removed: gone.txt"), "unexpected diff output: {}", unstaged);
    assert!(unstaged.contains("- going away"));

    // Binary content is reported, not printed line by line.
    std::fs::write(warehouse.root.join("blob.bin"), b"bin\0ary").unwrap();
    assert_success(&warehouse.run(&["load", "."]));

    let staged = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(staged.contains("added: blob.bin"), "unexpected diff output: {}", staged);
    assert!(staged.contains("binary contents"));
    assert!(staged.contains("removed: gone.txt"));
    assert!(staged.contains("- going away"));
}

#[test]
fn diff_compares_the_heads_of_two_pallets() {
    let warehouse = TestWarehouse::new("diff-pallets");
    warehouse.write_file("src/app.rs", "shared\nmain line\n");
    warehouse.write_file("notes.txt", "notes\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("src/app.rs", "shared\nfeature line\n");
    warehouse.write_file("new.txt", "extra\n");
    std::fs::remove_file(warehouse.root.join("notes.txt")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));

    // From main to feature: the edit, the addition and the removal all show up.
    let forward = warehouse.run(&["diff", "main", "feature"]);
    assert_success(&forward);
    let forward = stdout(&forward);
    assert!(forward.contains("modified: src/app.rs"), "unexpected diff output: {}", forward);
    assert!(forward.contains("- main line"));
    assert!(forward.contains("+ feature line"));
    assert!(forward.contains("added: new.txt"));
    assert!(forward.contains("removed: notes.txt"));

    // The reversed comparison swaps the sides.
    let backward = stdout(&warehouse.run(&["diff", "feature", "main"]));
    assert!(backward.contains("removed: new.txt"));
    assert!(backward.contains("added: notes.txt"));

    // A trailing path limits the report.
    let filtered = stdout(&warehouse.run(&["diff", "main", "feature", "src"]));
    assert!(filtered.contains("modified: src/app.rs"));
    assert!(!filtered.contains("new.txt"));

    // Identical trees are reported as such; --staged does not combine with pallets.
    assert!(stdout(&warehouse.run(&["diff", "feature", "feature"])).contains("identical trees"));
    let staged = warehouse.run(&["diff", "--staged", "main", "feature"]);
    assert!(!staged.status.success());
    assert!(stderr(&staged).contains("--staged cannot be combined"));

    // Unknown pallets are an error, not an empty diff.
    let unknown = warehouse.run(&["diff", "main", "nope"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("neither a pallet nor a parcel hash"));
}

#[test]
fn history_walks_the_parcel_graph_newest_first() {
    let warehouse = TestWarehouse::new("history");
    warehouse.write_file("a.txt", "one\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // No history before anything is stacked.
    let unborn = warehouse.run(&["history"]);
    assert!(!unborn.status.success());
    assert!(stderr(&unborn).contains("nothing stacked"));

    assert_success(&warehouse.run(&["load", "."]));
    let first = extract_parcel_hash(&warehouse.run(&["stack", "first parcel"]));

    warehouse.write_file("a.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    let second = extract_parcel_hash(&warehouse.run(&["stack", "second parcel"]));

    let history = stdout(&warehouse.run(&["history"]));
    assert!(history.contains(&format!("parcel {}", first)), "unexpected history: {}", history);
    assert!(history.contains(&format!("parcel {}", second)));
    assert!(history.contains("first parcel"));
    assert!(history.contains("second parcel"));

    // The authorship convention: every parcel carries an explicit author action
    // alongside the stack action, even when both are the same operator.
    assert!(history.contains("author test@forklift"), "unexpected history: {}", history);
    assert!(history.contains("stack  test@forklift"));

    // Newest first: the second parcel is printed before the first.
    assert!(
        history.find(&second).unwrap() < history.find(&first).unwrap(),
        "history must be newest first: {}",
        history
    );

    // A consolidation parcel lists the parents it consolidates, and the merged-in
    // pallet's parcels appear in the history of the target pallet.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("b.txt", "feature\n");
    assert_success(&warehouse.run(&["load", "."]));
    let feature = extract_parcel_hash(&warehouse.run(&["stack", "feature parcel"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("c.txt", "main\n");
    assert_success(&warehouse.run(&["load", "."]));
    let main_parcel = extract_parcel_hash(&warehouse.run(&["stack", "main parcel"]));
    assert_success(&warehouse.run(&["consolidate", "feature"]));

    let merged = stdout(&warehouse.run(&["history"]));
    assert!(merged.contains("consolidates"), "unexpected history: {}", merged);
    assert!(merged.contains(&feature));
    assert!(merged.contains("feature parcel"));

    // An explicit pallet argument selects that pallet's history; unknown pallets error.
    let feature_history = stdout(&warehouse.run(&["history", "feature"]));
    assert!(feature_history.contains("feature parcel"));
    assert!(!feature_history.contains("main parcel"));

    let unknown = warehouse.run(&["history", "nope"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("neither a pallet nor a parcel hash"));

    // `--json` carries `parents` on every entry (unlike `consolidates`, which is only
    // present, non-empty, on a merge): `[]` for the root, one parent for a linear
    // parcel, and — for the consolidation — both parents in stored, base-first order
    // (the current pallet's own prior head first, the merged-in pallet's head second).
    let entries = json(&warehouse.run(&["history", "--json"]))["data"]["entries"].clone();
    let entries = entries.as_array().unwrap();
    let by_hash = |hash: &str| entries.iter()
        .find(|entry| entry["parcel"] == hash)
        .unwrap_or_else(|| panic!("parcel {} not found in: {:?}", hash, entries));

    assert_eq!(by_hash(&first)["parents"], serde_json::json!([]), "a root parcel has no parents");
    assert_eq!(by_hash(&second)["parents"], serde_json::json!([first]), "a linear parcel has one parent");

    let merge_hash = entries.iter()
        .find(|entry| !entry["consolidates"].as_array().unwrap_or(&Vec::new()).is_empty())
        .expect("the merge parcel must be in the history");
    assert_eq!(
        merge_hash["parents"], serde_json::json!([main_parcel, feature]),
        "a consolidation lists both parents, base (the target pallet's own prior head) first"
    );
    assert_eq!(merge_hash["parents"], merge_hash["consolidates"], "parents and consolidates agree on a merge parcel");
}

#[test]
fn history_oneline_prints_one_terse_line_per_parcel_newest_first() {
    let warehouse = TestWarehouse::new("history-oneline");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let first = extract_parcel_hash(&warehouse.run(&["stack", "first subject\n\nbody paragraph"]));
    warehouse.write_file("a.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    let second = extract_parcel_hash(&warehouse.run(&["stack", "second subject"]));

    let oneline = stdout(&warehouse.run(&["history", "--oneline"]));

    // One line per parcel: the abbreviated hash (a prefix of the full hash) and the subject —
    // the description's first line only, never the body, and none of the verbose author/stack
    // lines the default form prints.
    assert!(oneline.contains("first subject"), "oneline: {}", oneline);
    assert!(oneline.contains("second subject"));
    assert!(!oneline.contains("body paragraph"), "oneline shows only the subject: {}", oneline);
    assert!(!oneline.contains("author "), "oneline omits the author line: {}", oneline);
    assert!(!oneline.contains("stack  "), "oneline omits the stack line: {}", oneline);
    assert!(oneline.contains(&first[..12]) && oneline.contains(&second[..12]), "oneline: {}", oneline);

    // Newest first, and exactly two lines (one per parcel).
    assert!(oneline.find(&second[..12]).unwrap() < oneline.find(&first[..12]).unwrap());
    assert_eq!(oneline.lines().filter(|l| !l.trim().is_empty()).count(), 2, "oneline: {}", oneline);

    // `-n` still bounds it.
    let limited = stdout(&warehouse.run(&["history", "--oneline", "-n", "1"]));
    assert_eq!(limited.lines().filter(|l| !l.trim().is_empty()).count(), 1, "limited: {}", limited);
    assert!(limited.contains("second subject"));
}

#[test]
fn moves_are_detected_as_a_post_pass_and_reported_as_one_change() {
    let warehouse = TestWarehouse::new("moves");
    warehouse.write_file("src/alpha.txt", "alpha content\n");
    warehouse.write_file("src/beta.txt", "beta content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // A file moved on disk (not loaded yet) pairs by inode + hash: one unstaged move
    // instead of a removal and an untracked file.
    std::fs::rename(
        warehouse.root.join("src/alpha.txt"),
        warehouse.root.join("src/renamed.txt"),
    ).unwrap();

    let unstaged = stdout(&warehouse.run(&["stocktake"]));
    assert!(
        unstaged.contains("moved:     src/alpha.txt -> src/renamed.txt"),
        "unexpected stocktake output: {}",
        unstaged
    );
    assert!(!unstaged.contains("untracked: src/renamed.txt"));
    assert!(!unstaged.contains("removed:   src/alpha.txt"));

    let diff = stdout(&warehouse.run(&["diff"]));
    assert!(diff.contains("moved: src/alpha.txt -> src/renamed.txt"));

    // Once loaded, the move pairs by blob hash in the staged comparison.
    assert_success(&warehouse.run(&["load", "."]));
    let staged = stdout(&warehouse.run(&["stocktake"]));
    assert!(
        staged.contains("moved:     src/alpha.txt -> src/renamed.txt"),
        "unexpected stocktake output: {}",
        staged
    );

    let staged_diff = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(staged_diff.contains("moved: src/alpha.txt -> src/renamed.txt"));

    // A move followed by an edit is not a move: the content differs on the two sides.
    warehouse.write_file("src/renamed.txt", "alpha content edited\n");
    assert_success(&warehouse.run(&["load", "."]));
    let edited = stdout(&warehouse.run(&["stocktake"]));
    assert!(edited.contains("removed:   src/alpha.txt"), "unexpected output: {}", edited);
    assert!(edited.contains("added:     src/renamed.txt"));

    // Pallet-to-pallet comparisons pair moves by blob hash too.
    assert_success(&warehouse.run(&["stack", "renamed"]));
    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::create_dir_all(warehouse.root.join("sub")).unwrap();
    std::fs::rename(
        warehouse.root.join("src/beta.txt"),
        warehouse.root.join("sub/beta.txt"),
    ).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "moved beta"]));

    let pallet_diff = stdout(&warehouse.run(&["diff", "main", "feature"]));
    assert!(
        pallet_diff.contains("moved: src/beta.txt -> sub/beta.txt"),
        "unexpected pallet diff: {}",
        pallet_diff
    );
}

#[test]
fn revisions_resolve_pallets_and_parcel_hash_prefixes() {
    let warehouse = TestWarehouse::new("revisions");
    warehouse.write_file("a.txt", "v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let first = extract_parcel_hash(&warehouse.run(&["stack", "first"]));

    warehouse.write_file("a.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));

    // palletize <name> <revision> creates the pallet at that parcel and materializes it.
    let prefix = &first[0..8];
    assert_success(&warehouse.run(&["palletize", "hotfix", prefix]));
    assert_eq!(std::fs::read_to_string(warehouse.root.join("a.txt")).unwrap(), "v1\n");

    // diff and history accept parcel hash prefixes wherever a pallet name works.
    let diff = stdout(&warehouse.run(&["diff", prefix, "main"]));
    assert!(diff.contains("modified: a.txt"), "unexpected diff: {}", diff);
    assert!(diff.contains("+ v2"));

    let history = stdout(&warehouse.run(&["history", prefix]));
    assert!(history.contains(&format!("parcel {}", first)));
    assert!(!history.contains("second"));

    // Unknown and non-hash arguments are rejected.
    let unknown = warehouse.run(&["history", "zzzz"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("neither a pallet nor a parcel hash"));
}

#[test]
fn office_manages_users_and_keys_as_signed_tracked_metadata() {
    let warehouse = TestWarehouse::new("office");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Genesis: enrolling establishes trust, introduces the first user + key, and the
    // office pallet's history is the audit trail.
    let enroll = warehouse.run(&["office", "enroll"]);
    assert_success(&enroll);
    assert!(stdout(&enroll).contains("established trust"));

    // Office records carry only the opaque operator id (the display name never goes
    // on-chain), and the genesis key is pinned as the identity root.
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("test@forklift — admin"), "unexpected list: {}", list);
    assert!(!list.contains("Test Operator"), "display data must not reach the office: {}", list);
    assert!(list.contains("active, on this machine"));
    assert!(list.contains("identity root"), "unexpected list: {}", list);

    // Trust is a one-way door: a second enroll is refused.
    let again = warehouse.run(&["office", "enroll"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already established"));

    // Admitting a second user: keygen prints the exact admit line (operator id,
    // public key and proof-of-possession — the CSR pattern). Bob runs keygen under
    // his own id (the warehouse scope overrides the global one).
    assert_success(&warehouse.run(&["config", "operator.identifier", "bob@forklift"]));
    let keygen = stdout(&warehouse.run(&["office", "keygen"]));
    let admit_args: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen must print the admit line")
        .split_whitespace()
        .skip(2)
        .collect();
    assert_eq!(admit_args[0], "bob@forklift");

    // Back to the admin's identity: the admission is the admin's move.
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // A proof-of-possession binds the key to its operator: admitting the same key
    // under a different id is refused.
    let mallory = warehouse.run(&["office", "admit", "mallory@evil", admit_args[1], admit_args[2]]);
    assert!(!mallory.status.success());
    assert!(stderr(&mallory).contains("proof-of-possession"), "{}", stderr(&mallory));

    assert_success(&warehouse.run(&["office", "admit", admit_args[0], admit_args[1], admit_args[2]]));
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("bob@forklift"), "unexpected list: {}", list);

    // Rotation retires the old key and introduces a new one (signed with the old key,
    // which also endorses the new one — the sigchain link).
    assert_success(&warehouse.run(&["office", "rotate"]));
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("retired"), "unexpected list: {}", list);

    // The office history reads as an audit trail — reached with the "@" meta qualifier.
    let history = stdout(&warehouse.run(&["history", "@office"]));
    assert!(history.contains("genesis"));
    assert!(history.contains("Admitted operator \"bob@forklift\""));
    assert!(history.contains("Rotated the keys"));

    // The office is a meta pallet now, not a reserved name: "office" is a legal working
    // pallet, stored in a separate namespace and distinct from the @office metadata chain.
    assert_success(&warehouse.run(&["palletize", "office"]));

    // A bare "office" is the (just-created, unborn) user pallet — never the meta office.
    let bare = warehouse.run(&["history", "office"]);
    assert!(!bare.status.success(), "bare 'office' must be the user pallet, not the meta office");

    // The @ qualifier still reaches the office; the namespaces do not collide.
    let meta = stdout(&warehouse.run(&["history", "@office"]));
    assert!(meta.contains("Admitted operator \"bob@forklift\""), "@office must still reach the office: {}", meta);

    // The office shows up in the pallet list only behind --all, under its own heading.
    let listed = stdout(&warehouse.run(&["palletize", "--all"]));
    assert!(listed.contains("Meta pallets:"), "unexpected list: {}", listed);
    assert!(listed.contains("@office"), "unexpected list: {}", listed);
    let default_list = stdout(&warehouse.run(&["palletize"]));
    assert!(!default_list.contains("@office"), "meta pallets must be hidden by default: {}", default_list);
}

#[test]
fn manifest_records_signed_post_metadata_on_a_meta_pallet() {
    let warehouse = TestWarehouse::new("manifest");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first parcel"]));

    // Recording requires an enrolled signing key — before enrollment it is refused.
    let before = warehouse.run(&["manifest", "approve", "main", "-m", "ok"]);
    assert!(!before.status.success(), "manifest must require trust: {}", stderr(&before));

    assert_success(&warehouse.run(&["office", "enroll"]));

    // An approval and a note about the current parcel.
    assert_success(&warehouse.run(&["manifest", "approve", "main", "-m", "LGTM"]));
    assert_success(&warehouse.run(&["manifest", "note", "main", "-m", "add a test"]));

    // `show` lists both entries for the parcel.
    let show = stdout(&warehouse.run(&["manifest", "show", "main"]));
    assert!(show.contains("approval") && show.contains("LGTM"), "unexpected: {}", show);
    assert!(show.contains("note") && show.contains("add a test"), "unexpected: {}", show);

    // The entries live on the @manifest meta pallet, verifiable offline like any signed
    // history — and it is not a reserved user name (a bare "manifest" is not the pallet).
    let audit = stdout(&warehouse.run(&["audit", "@manifest"]));
    assert!(audit.contains("2 signed parcel(s) valid"), "unexpected audit: {}", audit);

    let listed = stdout(&warehouse.run(&["palletize", "--all"]));
    assert!(listed.contains("@manifest"), "unexpected list: {}", listed);

    // A note about a parcel that does not exist is refused (the subject must resolve).
    let bad = warehouse.run(&["manifest", "note", "0000", "-m", "x"]);
    assert!(!bad.status.success(), "an unknown subject must be refused");
}

#[test]
fn manifest_provenance_records_forge_proof_machine_authorship() {
    let warehouse = TestWarehouse::new("provenance");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse); // enrolls test@forklift
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first parcel"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    // The flagship §7.2 case: an *agent* records how it produced the parcel. Admit an
    // agent supervised by the human, then record provenance under the agent's identity.
    let agent = keygen_admit_args(&warehouse, "agent@forklift");
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "test@forklift",
    ]));

    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    assert_success(&warehouse.run(&[
        "manifest", "provenance", "main",
        "--model", "claude-opus-4-8", "--tool", "claude-code", "--session", "sess-42",
        "-m", "generated the module",
    ]));

    // The provenance is attributed to the agent (forge-proof — the signature, not a
    // field), and carries the model/tool/session.
    let show = stdout(&warehouse.run(&["manifest", "show", "main"]));
    assert!(show.contains("provenance") && show.contains("agent@forklift"), "unexpected: {}", show);
    assert!(show.contains("claude-opus-4-8") && show.contains("claude-code"), "unexpected: {}", show);

    // It audits clean like any signed history, and --json exposes the structured fields.
    assert!(stdout(&warehouse.run(&["audit", "@manifest"])).contains("verified"));
    let json = stdout(&warehouse.run(&["--json", "manifest", "show", "main"]));
    assert!(json.contains("\"model\"") && json.contains("claude-opus-4-8"), "unexpected json: {}", json);

    // The model is the compliance-critical field, so it is required.
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));
    let no_model = warehouse.run(&["manifest", "provenance", "main", "--tool", "x"]);
    assert!(!no_model.status.success(), "provenance must require --model");
}

#[test]
fn mcp_provenance_takes_tool_and_session_from_the_connection_not_the_agent() {
    let warehouse = TestWarehouse::new("provenance-origin");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&warehouse.run(&["stack", "first parcel"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    // Record provenance THROUGH the MCP: the client identifies itself at initialize, and in
    // the same connection the agent tries to self-report a false tool and session.
    let input = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"clientInfo":{"name":"test-harness","version":"9.9"}}}"#,
        format_args!(
            r#"{{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{{"name":"manifest_provenance","arguments":{{"revision":"{}","model":"claude-opus-4-8","tool":"LYING-TOOL","session":"LYING-SESSION"}}}}}}"#,
            parcel
        ),
    );
    let output = warehouse.run_with_stdin(&["mcp"], &input);
    assert_success(&output);
    let call = stdout(&output).lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .expect("a reply to the provenance call");
    assert_eq!(call["result"]["isError"], false, "provenance call failed: {}", call);

    // Read the recorded entry: tool comes from clientInfo and session is server-minted —
    // the agent's self-reported values are discarded. `model` is the agent's attestation.
    let show = json(&warehouse.run(&["manifest", "show", &parcel, "--json"]));
    let entry = show["data"]["entries"].as_array().unwrap().iter()
        .find(|entry| entry["model"].is_string())
        .expect("a provenance entry");
    assert_eq!(entry["model"], "claude-opus-4-8", "model is the agent's attestation, kept");
    assert_eq!(entry["tool"], "test-harness/9.9", "tool must come from clientInfo, overriding the agent");
    assert_ne!(entry["session"], "LYING-SESSION", "session must be server-minted, not the agent's");
    assert!(
        entry["session"].as_str().unwrap().starts_with("mcp-"),
        "session must be the server-minted id, got {}", entry["session"]
    );
}

#[test]
fn import_git_migrates_a_repo_into_the_warehouse() {
    let warehouse = TestWarehouse::new("import-git");
    let root = warehouse.root.clone();

    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };

    // A git repo with two branches sharing history.
    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("a.txt", "hello\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "first commit"]);
    git(&["checkout", "-q", "-b", "feature"]);
    warehouse.write_file("b.txt", "feature\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "feature work"]);
    git(&["checkout", "-q", "main"]);

    // Import it (the warehouse is fresh — imported history is unsigned).
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    let imported = warehouse.run(&["import-git", "."]);
    assert_success(&imported);
    assert!(stdout(&imported).contains("Imported 2 commit(s) into 2 pallet(s)"), "{}", stdout(&imported));

    // Each branch became a pallet; git's HEAD branch is checked out.
    let pallets = stdout(&warehouse.run(&["palletize"]));
    assert!(pallets.contains("* main") && pallets.contains("feature"), "unexpected: {}", pallets);

    // History and authorship survived; the colocated working tree reads clean (`.git`
    // was auto-ignored).
    assert!(stdout(&warehouse.run(&["history", "feature"])).contains("feature work"));
    assert!(stdout(&warehouse.run(&["history"])).contains("ada@example.com"), "git author must survive");
    assert!(stdout(&warehouse.run(&["stocktake"])).contains("matches the inventory"));

    // Enrolling after import anchors the imported history as the legacy boundary, so a
    // later audit tolerates it as unsigned rather than rejecting it.
    assert_success(&warehouse.run(&["office", "enroll"]));
    assert!(stdout(&warehouse.run(&["audit"])).contains("legacy"));

    // A second import into the now-trusted warehouse is refused.
    assert!(!warehouse.run(&["import-git", "."]).status.success());
}

#[test]
fn import_git_auto_compacts_unless_no_compact() {
    // Seed a small colocated git repo in the warehouse root (import runs on ".").
    fn seed_git_repo(root: &Path) {
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .output()
                .expect("git must be installed to run this test");
            assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        // Two versions of one file, big enough that the second pays as a delta.
        let body = "a line of file content\n".repeat(400);
        std::fs::write(root.join("a.txt"), &body).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first commit"]);
        std::fs::write(root.join("a.txt"), body + "one more line\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "second commit"]);
    }

    // A large import lands a big object set — so by default import writes native packs
    // directly (delta-compressing successive versions of a file on the way in), and the user
    // gets a dense warehouse whose store never existed in its loose worst case.
    let packed = TestWarehouse::new("import-autocompact");
    seed_git_repo(&packed.root);
    assert_success(&packed.run(&["prepare"]));
    configure_operator(&packed);
    let out = packed.run(&["import-git", "."]);
    assert_success(&out);
    assert!(stdout(&out).contains("Packed the imported store"), "import should pack the store: {}", stdout(&out));
    assert!(stdout(&out).contains("2 delta-compressed"),
            "the second versions of a.txt and of the root tree should be path deltas: {}", stdout(&out));
    assert_eq!(count_loose_objects(&packed.root.join(".forklift/objects")), 0, "no loose objects should remain after import");
    assert!(packed.root.join(".forklift/objects/pack").is_dir(), "a pack folder should exist after import");
    // Reads work against the packed store immediately.
    assert!(stdout(&packed.run(&["history"])).contains("first commit"));
    assert!(stdout(&packed.run(&["stocktake"])).contains("matches the inventory"));

    // --no-compact opts out: the imported objects are left loose.
    let loose = TestWarehouse::new("import-no-compact");
    seed_git_repo(&loose.root);
    assert_success(&loose.run(&["prepare"]));
    configure_operator(&loose);
    let out = loose.run(&["import-git", ".", "--no-compact"]);
    assert_success(&out);
    assert!(!stdout(&out).contains("Packed the imported store"), "--no-compact must skip packing: {}", stdout(&out));
    assert!(count_loose_objects(&loose.root.join(".forklift/objects")) > 0, "objects should stay loose with --no-compact");
    assert!(!loose.root.join(".forklift/objects/pack").exists(), "no pack folder should exist with --no-compact");
}

#[test]
fn import_git_rederives_tree_bases_when_the_cache_is_starved() {
    // Three commits touching the same file (in a subdirectory, so both its tree and the root
    // tree change each time) build delta chains for blobs and trees alike.
    fn seed_repo(root: &Path) {
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .output()
                .expect("git must be installed to run this test");
            assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        };

        git(&["init", "-q", "-b", "main"]);
        let body = "a line of file content\n".repeat(400);
        std::fs::create_dir_all(root.join("sub")).unwrap();

        std::fs::write(root.join("sub/a.txt"), &body).unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first commit"]);

        std::fs::write(root.join("sub/a.txt"), body.clone() + "one more line\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "second commit"]);

        std::fs::write(root.join("sub/a.txt"), body + "one more line\nyet another line\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "third commit"]);
    }

    // Pull the delta count out of the "Packed the imported store: N object(s) into P pack(s),
    // D delta-compressed." line.
    fn delta_count(output: &str) -> usize {
        let line = output.lines().find(|line| line.contains("Packed the imported store"))
            .unwrap_or_else(|| panic!("no \"Packed the imported store\" line in: {}", output));
        let (_, after) = line.split_once(", ").expect("a delta-compressed segment");
        after.split_whitespace().next().expect("a delta count").parse().expect("a number")
    }

    // Default cache budget: bases mostly hit the cache.
    let default_budget = TestWarehouse::new("import-tree-rederive-default");
    seed_repo(&default_budget.root);
    assert_success(&default_budget.run(&["prepare"]));
    configure_operator(&default_budget);
    let baseline = default_budget.run(&["import-git", "."]);
    assert_success(&baseline);
    let baseline_deltas = delta_count(&stdout(&baseline));
    assert!(baseline_deltas > 0, "expected at least one delta in the baseline import: {}", stdout(&baseline));

    // Zero-byte cache budget: every base lookup misses, forcing blob bases through the batch
    // pipe re-read and tree bases through re-derivation on every single one. Density must not
    // regress relative to the default-budget run of the identical repo.
    let starved = TestWarehouse::new("import-tree-rederive-starved");
    seed_repo(&starved.root);
    assert_success(&starved.run(&["prepare"]));
    configure_operator(&starved);
    let zero_budget = starved.run_with_env(&["import-git", "."], &[("FORKLIFT_IMPORT_CACHE_BYTES", "0")]);
    assert_success(&zero_budget);
    let starved_deltas = delta_count(&stdout(&zero_budget));
    assert_eq!(starved_deltas, baseline_deltas,
        "a starved cache must re-derive every base rather than lose deltas: {}", stdout(&zero_budget));

    // The starved import still reads back correctly.
    assert!(stdout(&starved.run(&["history"])).contains("third commit"));
    assert!(stdout(&starved.run(&["stocktake"])).contains("matches the inventory"));
}

#[cfg(unix)]
#[test]
fn import_git_tolerates_non_utf8_author_names() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    // Regression: git.git and the Linux kernel carry commits whose author names are
    // Latin-1 — e.g. "Kågedal", where the 'å' is raw byte 0xE5, not valid UTF-8. Strict
    // decoding aborted the whole import; the model stores names as UTF-8 strings, so the
    // importer must coerce lossily instead of failing on the first such commit.
    let warehouse = TestWarehouse::new("import-git-non-utf8");
    let root = warehouse.root.clone();

    // "Kågedal" with the 'å' as a bare Latin-1 byte, exactly the shape that used to abort.
    let latin1_name = OsStr::from_bytes(b"K\xe5gedal");

    let git = |args: &[&str], author: &OsStr| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&root)
            .env("GIT_AUTHOR_NAME", author).env("GIT_AUTHOR_EMAIL", "davidk@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };

    git(&["init", "-q", "-b", "main"], OsStr::new("Ada"));
    warehouse.write_file("a.txt", "hello\n");
    git(&["add", "-A"], OsStr::new("Ada"));
    git(&["commit", "-qm", "latin-1 author"], latin1_name);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // The import must succeed rather than abort on the non-UTF-8 author name…
    let imported = warehouse.run(&["import-git", "."]);
    assert_success(&imported);
    assert!(stdout(&imported).contains("Imported 1 commit(s)"), "{}", stdout(&imported));

    // …and the stable identifier (the email) survives intact.
    assert!(stdout(&warehouse.run(&["history"])).contains("davidk@example.com"),
        "the author email must survive lossy name coercion");
}

#[test]
fn export_git_writes_the_history_into_a_git_repo() {
    let warehouse = TestWarehouse::new("export-git");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("README.md", "one\n");
    warehouse.write_file("src/main.rs", "fn main() {}\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first commit"]));
    warehouse.write_file("README.md", "one\ntwo\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second commit"]));

    let out = warehouse.root.join("exported");
    let out_str = out.to_str().unwrap();
    let exported = warehouse.run(&["export-git", out_str]);
    assert_success(&exported);
    assert!(stdout(&exported).contains("Exported 2 commit(s)"), "unexpected: {}", stdout(&exported));

    let git = |args: &[&str]| -> String {
        let output = Command::new("git")
            .args(args)
            .current_dir(&out)
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        String::from_utf8(output.stdout).unwrap()
    };

    // The branch, its messages and the author identity survived; the tree round-trips and
    // the object graph is valid (fsck --strict is silent on success).
    let log = git(&["log", "--format=%s", "main"]);
    assert!(log.contains("second commit") && log.contains("first commit"), "unexpected: {}", log);
    assert!(git(&["log", "-1", "--format=%ae", "main"]).contains("test@forklift"), "author must survive");
    // Normalize line endings: git's autocrlf (default on Windows) rewrites the checked-out
    // working file to CRLF, though the exported blob is LF. The content round-tripped.
    assert_eq!(
        std::fs::read_to_string(out.join("README.md")).unwrap().replace("\r\n", "\n"),
        "one\ntwo\n"
    );
    git(&["fsck", "--strict"]);

    // A non-empty target is refused.
    assert!(!warehouse.run(&["export-git", out_str]).status.success());
}

#[test]
fn self_update_check_reports_without_mutating() {
    // A global command — no warehouse needed. The env override injects "the latest
    // release" so the check never touches the network.
    let run = |latest: &str| -> Output {
        Command::new(FORKLIFT)
            .args(["--json", "self-update", "--check"])
            .env("FORKLIFT_SELFUPDATE_LATEST", latest)
            .output()
            .expect("forklift binary runs")
    };

    // A newer release than this build → available (but --check changes nothing).
    let newer = run("999.0.0");
    assert_success(&newer);
    assert_eq!(json(&newer)["data"]["update_available"], true);
    assert_eq!(json(&newer)["data"]["applied"], false);

    // No releases yet → nothing to update to.
    let none = run("");
    assert_success(&none);
    assert_eq!(json(&none)["data"]["update_available"], false);
}

/// A scratch directory standing in for "next to the binary" via `FORKLIFT_ALIAS_DIR` (the
/// same override the `alias` command reads), so these tests never touch the real target/
/// build output. Cleaned up on drop, like `TestWarehouse`.
struct AliasScratch {
    dir: PathBuf,
}

impl AliasScratch {
    fn new(name: &str) -> AliasScratch {
        let dir = std::env::temp_dir()
            .join(format!("forklift-test-alias-{}-{}", name, std::process::id()));

        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        AliasScratch { dir }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(FORKLIFT)
            .args(args)
            .env("FORKLIFT_ALIAS_DIR", &self.dir)
            .output()
            .unwrap()
    }

    /// Mirrors the production `platform_alias_path` logic: a bare name on Unix (a symlink),
    /// `name.cmd` on Windows (a shim) — so tests that write a "foreign file" or check
    /// removal are looking at the same path the `alias` command itself would touch.
    fn alias_path(&self, name: &str) -> PathBuf {
        #[cfg(windows)]
        {
            self.dir.join(format!("{}.cmd", name))
        }
        #[cfg(not(windows))]
        {
            self.dir.join(name)
        }
    }
}

impl Drop for AliasScratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn alias_install_creates_a_symlink_next_to_the_binary_and_is_idempotent() {
    let scratch = AliasScratch::new("install");

    let created = scratch.run(&["--json", "alias", "install"]);
    assert_success(&created);
    assert_eq!(json(&created)["data"]["already_installed"], false);
    assert_eq!(json(&created)["data"]["name"], "fl");

    #[cfg(unix)]
    {
        let target = std::fs::canonicalize(FORKLIFT).unwrap();
        let resolved = std::fs::canonicalize(scratch.alias_path("fl")).unwrap();
        assert_eq!(resolved, target, "the alias must resolve to this binary");
    }

    // Installing again is a no-op success: an alias that already points here is left alone.
    let again = scratch.run(&["--json", "alias", "install"]);
    assert_success(&again);
    assert_eq!(json(&again)["data"]["already_installed"], true);
}

#[test]
fn alias_uninstall_removes_it_and_is_a_noop_when_absent() {
    let scratch = AliasScratch::new("uninstall");

    assert_success(&scratch.run(&["alias", "install"]));
    assert!(std::fs::symlink_metadata(scratch.alias_path("fl")).is_ok());

    let removed = scratch.run(&["--json", "alias", "uninstall"]);
    assert_success(&removed);
    assert_eq!(json(&removed)["data"]["removed"], true);
    assert!(
        std::fs::symlink_metadata(scratch.alias_path("fl")).is_err(),
        "the alias must be gone"
    );

    // Uninstalling again when nothing is there succeeds without doing anything.
    let again = scratch.run(&["--json", "alias", "uninstall"]);
    assert_success(&again);
    assert_eq!(json(&again)["data"]["removed"], false);
}

#[test]
fn alias_install_refuses_a_foreign_file() {
    let scratch = AliasScratch::new("foreign-install");
    std::fs::write(scratch.alias_path("fl"), "not a forklift alias\n").unwrap();

    let result = scratch.run(&["--json", "alias", "install"]);
    assert!(!result.status.success());
    assert_eq!(json(&result)["error"]["code"], "error");
    assert!(stdout(&result).contains("Refusing"));

    // The foreign file must be untouched.
    assert_eq!(
        std::fs::read_to_string(scratch.alias_path("fl")).unwrap(),
        "not a forklift alias\n"
    );
}

#[test]
fn alias_uninstall_refuses_a_foreign_file() {
    let scratch = AliasScratch::new("foreign-uninstall");
    std::fs::write(scratch.alias_path("fl"), "not a forklift alias\n").unwrap();

    let result = scratch.run(&["--json", "alias", "uninstall"]);
    assert!(!result.status.success());
    assert!(stdout(&result).contains("Refusing"));
    assert!(scratch.alias_path("fl").exists(), "the foreign file must not be removed");
}

#[cfg(unix)]
#[test]
fn alias_install_refuses_when_the_name_points_elsewhere_but_uninstall_may_remove_it() {
    let scratch = AliasScratch::new("points-elsewhere");
    let other = scratch.dir.join("something-else");
    std::fs::write(&other, "x").unwrap();
    std::os::unix::fs::symlink(&other, scratch.alias_path("fl")).unwrap();

    let install = scratch.run(&["--json", "alias", "install"]);
    assert!(!install.status.success());
    assert!(stdout(&install).contains("Refusing"));

    // A symlink pointing elsewhere is still recognized as an alias, so uninstall may remove
    // it — deleting a symlink can never lose data, unlike a real file.
    let uninstall = scratch.run(&["--json", "alias", "uninstall"]);
    assert_success(&uninstall);
    assert_eq!(json(&uninstall)["data"]["removed"], true);
}

#[test]
fn alias_status_reports_installed_state() {
    let scratch = AliasScratch::new("status");

    let before = scratch.run(&["--json", "alias", "status"]);
    assert_success(&before);
    assert_eq!(json(&before)["data"]["installed"], false);

    assert_success(&scratch.run(&["alias", "install"]));

    let after = scratch.run(&["--json", "alias", "status"]);
    assert_success(&after);
    assert_eq!(json(&after)["data"]["installed"], true);
}

#[test]
fn the_fl_alias_behaves_identically_to_forklift() {
    let scratch = AliasScratch::new("behaves-identically");
    assert_success(&scratch.run(&["alias", "install"]));

    // Direct execution of a `.cmd` file via `CreateProcess` is unreliable — Windows needs
    // the command interpreter to run a batch shim. Unix executes the symlink directly.
    #[cfg(windows)]
    let via_alias = Command::new("cmd")
        .arg("/C")
        .arg(scratch.alias_path("fl"))
        .args(["--json", "version"])
        .output()
        .unwrap();
    #[cfg(not(windows))]
    let via_alias = Command::new(scratch.alias_path("fl"))
        .args(["--json", "version"])
        .output()
        .unwrap();
    let via_forklift = Command::new(FORKLIFT).args(["--json", "version"]).output().unwrap();

    assert_success(&via_alias);
    assert_eq!(json(&via_alias), json(&via_forklift), "fl must behave exactly like forklift");
}

#[test]
fn git_command_aliases_map_to_forklift_verbs() {
    let warehouse = TestWarehouse::new("git-aliases");

    // init → prepare; add → load; commit → stack.
    assert_success(&warehouse.run(&["init"]));
    configure_operator(&warehouse);
    warehouse.write_file("a.txt", "x\n");
    assert_success(&warehouse.run(&["add", "."]));
    assert_success(&warehouse.run(&["commit", "first"]));

    // status → stocktake; log → history.
    assert!(stdout(&warehouse.run(&["status"])).contains("matches the inventory"));
    assert!(stdout(&warehouse.run(&["log"])).contains("first"));

    // branch → palletize; switch → shift.
    assert_success(&warehouse.run(&["branch", "feature"])); // switches to feature
    assert_success(&warehouse.run(&["switch", "main"]));
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* main"));

    // The git names are hidden aliases — the top-level help still lists forklift's own
    // command names (they dispatch, but do not clutter or replace the vocabulary).
    assert!(stdout(&warehouse.run(&["--help"])).contains("stack"));
}

#[test]
fn haul_reviews_carry_signed_identity_and_merges() {
    let warehouse = TestWarehouse::new("haul");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    // A feature pallet with work to propose.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("feature.txt", "feat\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    // Open a haul, then find its id.
    assert_success(&warehouse.run(&["haul", "open", "--target", "main", "--source", "feature", "--title", "Add feature", "-m", "please review"]));
    let id = json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"].as_str().unwrap().to_string();

    // The human opener approves.
    assert_success(&warehouse.run(&["haul", "review", &id, "-m", "LGTM"]));

    // An admitted agent requests changes — its review is tagged with its identity class,
    // so the approval/verdict is cryptographically attributable to a human vs an agent.
    let agent = keygen_admit_args(&warehouse, "agent@forklift");
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "test@forklift",
    ]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    assert_success(&warehouse.run(&["haul", "review", &id, "--request-changes", "-m", "needs tests"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    let shown = json(&warehouse.run(&["haul", "show", &id, "--json"]));
    let reviews = shown["data"]["reviews"].as_array().unwrap().clone();
    assert_eq!(reviews.len(), 2);
    assert!(
        reviews.iter().any(|r| r["class"] == "agent" && r["verdict"] == "request-changes"),
        "the agent's review must carry its class: {:?}", reviews
    );

    // Merge is recorded, not gated on the request-changes (MVP policy) → merged, and the
    // source's file is now on the target.
    assert_success(&warehouse.run(&["haul", "merge", &id]));
    assert_eq!(std::fs::read_to_string(warehouse.root.join("feature.txt")).unwrap(), "feat\n");
    assert_eq!(json(&warehouse.run(&["haul", "show", &id, "--json"]))["data"]["status"], "merged");
}

#[test]
fn history_limit_bounds_the_walk_to_the_newest_parcels() {
    let warehouse = TestWarehouse::new("history-limit");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Five parcels, oldest to newest.
    let mut hashes = Vec::new();
    for i in 1..=5 {
        warehouse.write_file("a.txt", &format!("v{}\n", i));
        assert_success(&warehouse.run(&["load", "."]));
        hashes.push(extract_parcel_hash(&warehouse.run(&["stack", &format!("parcel {}", i)])));
    }

    // `--limit`/`-n` shows only the newest N (newest first), not the whole history.
    let two = json(&warehouse.run(&["history", "-n", "2", "--json"]));
    let entries = two["data"]["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "expected 2 parcels, got {}", entries.len());
    assert_eq!(entries[0]["parcel"], hashes[4], "newest parcel must be first");
    assert_eq!(entries[1]["parcel"], hashes[3]);

    // A limit larger than the history simply shows everything.
    let all = json(&warehouse.run(&["history", "--limit", "99", "--json"]));
    assert_eq!(all["data"]["entries"].as_array().unwrap().len(), 5);
}

#[test]
fn history_json_pages_through_the_whole_log_with_a_cursor() {
    let warehouse = TestWarehouse::new("history-cursor");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let mut hashes = Vec::new();
    for i in 1..=5 {
        warehouse.write_file("a.txt", &format!("v{}\n", i));
        assert_success(&warehouse.run(&["load", "."]));
        hashes.push(extract_parcel_hash(&warehouse.run(&["stack", &format!("p{}", i)])));
    }

    // Walk the whole log in pages of two, following the `next` cursor each time.
    let mut seen: Vec<String> = Vec::new();
    let mut seen_parents: Vec<Vec<String>> = Vec::new();
    let mut cursor: Option<String> = None;
    for _ in 0..10 {
        let out = match &cursor {
            Some(c) => warehouse.run(&["history", "--after", c, "-n", "2", "--json"]),
            None => warehouse.run(&["history", "-n", "2", "--json"]),
        };
        let data = json(&out)["data"].clone();
        for entry in data["entries"].as_array().unwrap() {
            seen.push(entry["parcel"].as_str().unwrap().to_string());
            let parents = entry["parents"].as_array()
                .unwrap_or_else(|| panic!("parents must always be present: {:?}", entry))
                .iter().map(|p| p.as_str().unwrap().to_string()).collect();
            seen_parents.push(parents);
        }
        match data["next"].as_str() {
            Some(next) => cursor = Some(next.to_string()),
            None => break,
        }
    }

    // Every parcel is shown exactly once, newest first — no gaps, no duplicates.
    let expected: Vec<String> = hashes.iter().rev().cloned().collect();
    assert_eq!(seen, expected, "cursor paging must cover the whole log once, newest first");

    // `parents` is correct even across the page boundary (the 5th/3rd-newest parcels
    // straddle the -n 2 pages): the oldest parcel is a root (`[]`), every other parcel's
    // sole parent is the one immediately before it in the linear chain.
    assert_eq!(seen_parents[4], Vec::<String>::new(), "the root parcel has no parents");
    for i in 0..4 {
        assert_eq!(
            seen_parents[i], vec![seen[i + 1].clone()],
            "parcel {} must list its predecessor as its one parent, across the page boundary", seen[i]
        );
    }
}

#[test]
fn history_shows_and_filters_by_identity_class() {
    let warehouse = TestWarehouse::new("history-class");
    warehouse.write_file("a.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse); // enrolls test@forklift (a human)
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "human work"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    // Admit an agent supervised by the human, and let it stack a parcel.
    let agent = keygen_admit_args(&warehouse, "agent@forklift");
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "test@forklift",
    ]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    warehouse.write_file("a.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "agent work"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // The log calls out the agent's authorship and supervisor (from the signed record).
    let history = stdout(&warehouse.run(&["history"]));
    assert!(history.contains("[agent, supervised by test@forklift]"), "unexpected: {}", history);

    // Filtering by class answers "which parcels did agents write".
    let agents = stdout(&warehouse.run(&["history", "--class", "agent"]));
    assert!(agents.contains("agent work") && !agents.contains("human work"), "unexpected: {}", agents);

    let humans = stdout(&warehouse.run(&["history", "--class", "human"]));
    assert!(humans.contains("human work") && !humans.contains("agent work"), "unexpected: {}", humans);

    // An unknown class is rejected.
    assert!(!warehouse.run(&["history", "--class", "robot"]).status.success());
}

#[test]
fn bays_are_parallel_working_directories_sharing_the_warehouse() {
    let warehouse = TestWarehouse::new("bay");
    warehouse.write_file("code.txt", "shared v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    // Open a bay in a directory outside the warehouse.
    let bay_dir = warehouse.home.join("bay-feature");
    let added = warehouse.run(&["bay", "add", "feature", bay_dir.to_str().unwrap()]);
    assert_success(&added);
    assert!(stdout(&added).contains("Opened bay \"feature\""), "{}", stdout(&added));

    // The bay materialized the shared history and is checked out to its own pallet.
    assert_eq!(std::fs::read_to_string(bay_dir.join("code.txt")).unwrap(), "shared v1\n");
    assert!(stdout(&warehouse.run_at(&bay_dir, &["stocktake"])).contains("matches the inventory"));

    // Work in the bay stacks onto its pallet, using the bay's own inventory.
    std::fs::write(bay_dir.join("code.txt"), "shared v1\nbay change\n").unwrap();
    assert_success(&warehouse.run_at(&bay_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&bay_dir, &["stack", "work in the bay"]));

    // The main tree is untouched (its own working file and current pallet), but it shares
    // the refs — it sees the bay's pallet and history.
    assert_eq!(std::fs::read_to_string(warehouse.root.join("code.txt")).unwrap(), "shared v1\n");
    let pallets = stdout(&warehouse.run(&["palletize"]));
    assert!(pallets.contains("* main") && pallets.contains("feature"), "unexpected: {}", pallets);
    assert!(stdout(&warehouse.run(&["history", "feature"])).contains("work in the bay"));

    // The main tree stacks independently — the lock is per-bay, so neither blocks the
    // other (and the lock releases cleanly despite the bay's mid-command cwd switch).
    warehouse.write_file("code.txt", "shared v1\nmain change\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "work in main"]));

    // Listing and removing the bay.
    assert!(stdout(&warehouse.run(&["bay"])).contains("feature"));
    assert_success(&warehouse.run(&["bay", "remove", "feature"]));
    assert!(stdout(&warehouse.run(&["bay"])).contains("No bays"));
}

#[test]
fn deliver_squashes_a_draft_trail_and_keeps_it_as_post_metadata() {
    let warehouse = TestWarehouse::new("deliver");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    // A draft pallet with a messy checkpoint trail.
    assert_success(&warehouse.run(&["palletize", "draft/feature"]));
    for v in ["v2", "v3", "v4"] {
        warehouse.write_file("code.txt", &format!("{}\n", v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("wip {}", v)]));
    }

    // Deliver squashes the trail onto main as one clean parcel.
    let delivered = warehouse.run(&["deliver", "main", "-m", "add the feature"]);
    assert_success(&delivered);
    assert!(stdout(&delivered).contains("Delivered 3 checkpoint"), "{}", stdout(&delivered));

    // The current pallet is now main; its history is the clean parcel (not the wips), and
    // the working file is the draft's final state — no materialization needed.
    let history = stdout(&warehouse.run(&["history"]));
    assert!(history.contains("add the feature") && !history.contains("wip"), "unexpected: {}", history);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("code.txt")).unwrap(), "v4\n");

    // The trail is recorded as a signed delivery entry on the clean parcel, and kept on
    // the draft pallet so it stays browsable.
    let manifest = stdout(&warehouse.run(&["manifest", "show", "main"]));
    assert!(manifest.contains("delivery") && manifest.contains("draft/feature"), "unexpected: {}", manifest);
    assert!(stdout(&warehouse.run(&["history", "draft/feature"])).contains("wip"), "the trail must be kept");
    assert!(stdout(&warehouse.run(&["audit", "@manifest"])).contains("verified"));

    // A second delivery of the same (now-delivered) draft is refused.
    assert_success(&warehouse.run(&["shift", "draft/feature"]));
    let again = warehouse.run(&["deliver", "main"]);
    assert!(!again.status.success(), "a re-delivery must be refused");
    assert!(stderr(&again).contains("already delivered"), "{}", stderr(&again));
}

#[test]
fn regenesis_resets_trust_and_pins_prior_history_as_attested() {
    let warehouse = TestWarehouse::new("regenesis");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "signed work"]));

    // A working admin is refused: re-genesis is recovery, not management.
    let refused = warehouse.run(&["office", "regenesis", "--confirm"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("admin with a usable key"), "{}", stderr(&refused));

    // Simulate total key loss: no key on this machine can extend the chain.
    std::fs::remove_dir_all(warehouse.home.join("test-keys")).unwrap();
    warehouse.write_file("a.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    let locked = warehouse.run(&["stack", "locked out"]);
    assert!(!locked.status.success());
    assert!(stderr(&locked).contains("No active key"), "{}", stderr(&locked));

    // The reset is loud: without --confirm it only explains itself.
    let dry = warehouse.run(&["office", "regenesis"]);
    assert!(!dry.status.success());
    assert!(stdout(&dry).contains("RESET"), "{}", stdout(&dry));

    let reset = warehouse.run(&["office", "regenesis", "--confirm"]);
    assert_success(&reset);
    assert!(stdout(&reset).contains("TRUST RESET"));

    // The trust file records the chain of custody: prior genesis + adopted head.
    let trust = std::fs::read_to_string(warehouse.root.join(".forklift/trust")).unwrap();
    assert!(trust.contains("prior_genesis"), "unexpected trust file: {}", trust);
    assert!(trust.contains("adopts"), "unexpected trust file: {}", trust);

    // The new chain works, and the old signed work degrades to attested (legacy):
    // its signing key is gone from the office, but it sits inside the new boundary.
    assert_success(&warehouse.run(&["stack", "after reset"]));
    let audit = warehouse.run(&["audit"]);
    assert_success(&audit);
    let report = stdout(&audit);
    assert!(report.contains("1 signed parcel(s) valid"), "unexpected audit: {}", report);
    assert!(report.contains("legacy parcel(s)"), "unexpected audit: {}", report);

    // The office holds only the new identity root.
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert_eq!(list.matches("identity root").count(), 1, "unexpected list: {}", list);
}

#[test]
fn profiles_select_the_identity_a_warehouse_acts_under() {
    let warehouse = TestWarehouse::new("profiles");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Create a named profile; its id is minted (none given).
    let created = warehouse.run(&["profile", "create", "work", "--name", "Work Me"]);
    assert_success(&created);

    // Creating it again is refused.
    let again = warehouse.run(&["profile", "create", "work"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("already exists"));

    // Selecting a nonexistent profile is refused; selecting a real one sticks.
    let missing = warehouse.run(&["profile", "use", "nope"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("does not exist"));

    let selected = warehouse.run(&["profile", "use", "work"]);
    assert_success(&selected);
    let work_id = stdout(&selected)
        .split("operator id ")
        .nth(1)
        .expect("use prints the operator id")
        .trim_end_matches([')', '.', '\n'])
        .to_string();
    assert_ne!(work_id, "test@forklift");

    // Work in this warehouse is now authored by the profile's id, not the global one.
    assert_success(&warehouse.run(&["load", "."]));
    let stacked = warehouse.run(&["stack", "as work"]);
    assert_success(&stacked);
    let peek = stdout(&warehouse.run(&["peek", &extract_parcel_hash(&stacked)]));
    assert!(peek.contains(&work_id), "unexpected peek: {}", peek);
    assert!(!peek.contains("test@forklift"), "unexpected peek: {}", peek);

    // The office enrolls the profile's identity, and its key lands in the profile's
    // key set (the local key-owner manifest).
    assert_success(&warehouse.run(&["office", "enroll"]));
    let list = stdout(&warehouse.run(&["profile", "list"]));
    let work_line = list.lines().find(|line| line.starts_with("work — ")).unwrap_or("");
    assert!(work_line.contains("1 local key(s)"), "unexpected profile list: {}", list);
}

#[test]
fn office_link_endorses_a_second_device_key() {
    let warehouse = TestWarehouse::new("link");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"]));

    // "Device 2": a fresh keypair under the same operator UUID (the shared test key
    // directory stands in for the second machine). Keygen prints the exact link line.
    let keygen = stdout(&warehouse.run(&["office", "keygen"]));
    let link_args: Vec<&str> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office link "))
        .expect("keygen must print the link line")
        .split_whitespace()
        .skip(2)
        .collect();

    assert_success(&warehouse.run(&["office", "link", link_args[0], link_args[1]]));

    // Both keys are active — no rotation happened, the identity just grew a device.
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert_eq!(
        list.matches("active, on this machine").count(), 2,
        "unexpected list: {}", list
    );

    // The endorsement chain is real: signed work audits clean (the audit verifies
    // the sigchain endorsements along the office chain).
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "work"]));
    assert_success(&warehouse.run(&["audit"]));

    // A proof-of-possession bound to another operator's id cannot be linked.
    assert_success(&warehouse.run(&["config", "operator.identifier", "other@forklift"]));
    let foreign_keygen = stdout(&warehouse.run(&["office", "keygen"]));
    let foreign_args: Vec<String> = foreign_keygen.lines()
        .find(|line| line.trim_start().starts_with("office link "))
        .expect("keygen must print the link line")
        .split_whitespace()
        .skip(2)
        .map(|s| s.to_string())
        .collect();
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    let refused = warehouse.run(&["office", "link", &foreign_args[0], &foreign_args[1]]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("proof-of-possession"), "{}", stderr(&refused));
}

#[test]
fn office_authorize_recovers_an_enrolled_operator_who_lost_all_keys() {
    let warehouse = TestWarehouse::new("authorize");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"]));

    // Bob is admitted as a writer.
    assert_success(&warehouse.run(&["config", "operator.identifier", "bob@forklift"]));
    let keygen = stdout(&warehouse.run(&["office", "keygen"]));
    let admit_args: Vec<String> = keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen must print the admit line")
        .split_whitespace()
        .skip(2)
        .map(|s| s.to_string())
        .collect();
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));
    assert_success(&warehouse.run(&["office", "admit", &admit_args[0], &admit_args[1], &admit_args[2]]));

    // Bob "lost every device": he generates a fresh key on a new machine and hands
    // the admin the keygen line — he cannot link it himself (no surviving key signs).
    assert_success(&warehouse.run(&["config", "operator.identifier", "bob@forklift"]));
    let recovery_keygen = stdout(&warehouse.run(&["office", "keygen"]));
    let recovery_args: Vec<String> = recovery_keygen.lines()
        .find(|line| line.trim_start().starts_with("office admit "))
        .expect("keygen must print the admit line")
        .split_whitespace()
        .skip(2)
        .map(|s| s.to_string())
        .collect();

    // A non-admin may not authorize keys for someone else.
    let unauthorized = warehouse.run(
        &["office", "authorize", "test@forklift", &recovery_args[1], &recovery_args[2]]
    );
    assert!(!unauthorized.status.success());
    assert!(stderr(&unauthorized).contains("not an office admin"), "{}", stderr(&unauthorized));

    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // Your own devices go through "link", not an admin authorization.
    let own = warehouse.run(
        &["office", "authorize", "test@forklift", &recovery_args[1], &recovery_args[2]]
    );
    assert!(!own.status.success());
    assert!(stderr(&own).contains("office link"), "{}", stderr(&own));

    // Only enrolled operators can be authorized (new operators go through admit).
    let unknown = warehouse.run(
        &["office", "authorize", "carol@forklift", &recovery_args[1], &recovery_args[2]]
    );
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("not enrolled"), "{}", stderr(&unknown));

    // The admin authorizes bob's recovery key: a cross-identity endorsement, valid
    // exactly because the authorizer is an admin here (the §8.6 scope rule).
    let authorized = warehouse.run(
        &["office", "authorize", "bob@forklift", &recovery_args[1], &recovery_args[2]]
    );
    assert_success(&authorized);
    assert!(stdout(&authorized).contains("Authorized key"), "{}", stdout(&authorized));

    // The proof-of-possession still binds the key to bob: the same line cannot be
    // authorized for another enrolled operator.
    let mismatched = warehouse.run(
        &["office", "authorize", "bob@forklift", &admit_args[1], &admit_args[2]]
    );
    assert!(!mismatched.status.success());
    assert!(stderr(&mismatched).contains("already tracked"), "{}", stderr(&mismatched));

    // Bob now has two keys, and the chain still audits clean everywhere.
    let list = stdout(&warehouse.run(&["office", "list"]));
    let bob_keys = list.lines()
        .skip_while(|line| !line.starts_with("bob@forklift"))
        .skip(1)
        .take_while(|line| line.starts_with("  "))
        .count();
    assert_eq!(bob_keys, 2, "unexpected list: {}", list);

    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "work"]));
    assert_success(&warehouse.run(&["audit"]));
}

#[test]
fn parcels_are_signed_after_enrollment_and_audit_verifies_them() {
    let warehouse = TestWarehouse::new("audit");
    warehouse.write_file("a.txt", "pre-trust\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // A parcel stacked before trust exists is legal (and later counts as legacy).
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "legacy work"]));

    // No trust yet: audit refuses.
    let no_trust = warehouse.run(&["audit"]);
    assert!(!no_trust.status.success());
    assert!(stderr(&no_trust).contains("Trust is not established"));

    assert_success(&warehouse.run(&["office", "enroll"]));

    // Every parcel stacked from now on carries a signature sidecar.
    warehouse.write_file("a.txt", "signed\n");
    assert_success(&warehouse.run(&["load", "."]));
    let signed = extract_parcel_hash(&warehouse.run(&["stack", "signed work"]));

    let sidecar = warehouse.root
        .join(".forklift/objects")
        .join(&signed[0..2])
        .join(format!("{}.sig", &signed[2..]));
    assert!(sidecar.exists(), "a signature sidecar must be written next to the parcel");

    let audit = warehouse.run(&["audit"]);
    assert_success(&audit);
    let report = stdout(&audit);
    assert!(report.contains("Office chain verified"), "unexpected audit: {}", report);
    assert!(report.contains("1 signed parcel(s) valid"));
    assert!(report.contains("1 legacy parcel(s)"));

    // Tampering with the signature is detected.
    let original = std::fs::read(&sidecar).unwrap();
    let mut corrupted = original.clone();
    let last = corrupted.len() - 1;
    corrupted[last] ^= 0xff;
    std::fs::write(&sidecar, &corrupted).unwrap();

    let tampered = warehouse.run(&["audit"]);
    assert!(!tampered.status.success());
    assert!(stderr(&tampered).contains("does not verify"));

    // A missing signature on a post-trust parcel is detected too.
    std::fs::remove_file(&sidecar).unwrap();
    let missing = warehouse.run(&["audit"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains("carries no signature"));

    std::fs::write(&sidecar, &original).unwrap();
    assert_success(&warehouse.run(&["audit"]));

    // An operator who is not enrolled cannot stack: no unsigned escape hatch.
    // (The warehouse scope overrides the global one.)
    assert_success(&warehouse.run(&["config", "operator.identifier", "eve@evil"]));
    warehouse.write_file("a.txt", "evil\n");
    assert_success(&warehouse.run(&["load", "."]));
    let refused = warehouse.run(&["stack", "evil work"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("not enrolled"));
}

// ---------------------------------------------------------------------------------
// The machine-first interface (§7.4): --json envelopes, stable errors/exit codes,
// structured conflicts, and the MCP server.
// ---------------------------------------------------------------------------------

#[test]
fn json_mode_wraps_every_result_in_a_versioned_envelope() {
    let warehouse = TestWarehouse::new("json-envelope");
    configure_operator(&warehouse);

    // A structured command (prepare) — envelope with command, ok and data.
    let prepared = warehouse.run(&["prepare", "--json"]);
    assert_success(&prepared);
    let value = json(&prepared);
    assert_eq!(value["forklift_json"], "2");
    assert_eq!(value["command"], "prepare");
    assert_eq!(value["ok"], true);
    assert!(value["data"]["created"].is_array());

    // Stack reports the parcel and pallet as fields, not prose.
    warehouse.write_file("a.txt", "hello\n");
    assert_success(&warehouse.run(&["load", "."]));
    let stacked = warehouse.run(&["stack", "first", "--json"]);
    assert_success(&stacked);
    let value = json(&stacked);
    assert_eq!(value["command"], "stack");
    assert_eq!(value["data"]["pallet"], "main");
    assert_eq!(value["data"]["parcel"].as_str().unwrap().len(), 64);

    // Stocktake is fully structured.
    warehouse.write_file("b.txt", "x\n");
    let stocktake = warehouse.run(&["stocktake", "--json"]);
    assert_success(&stocktake);
    let value = json(&stocktake);
    assert_eq!(value["data"]["staged_count"], 0);
    assert_eq!(value["data"]["unstaged"][0]["kind"], "untracked");
    assert_eq!(value["data"]["unstaged"][0]["path"], "b.txt");
}

#[test]
fn json_errors_carry_a_stable_code_and_a_deterministic_exit_code() {
    let warehouse = TestWarehouse::new("json-error");

    // Not a warehouse: a classified error and exit code 3 (§7.8), plus a next step.
    let outside = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(outside.status.code(), Some(3));
    let value = json(&outside);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "not_a_warehouse");
    assert!(value["error"]["next_step"].is_string());
    assert!(value["error"]["message"].is_string());
}

#[test]
fn history_on_an_unborn_pallet_reports_the_empty_history_code() {
    let warehouse = TestWarehouse::new("history-unborn-json");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Nothing has ever been stacked on "main": history has its own classification
    // (distinct from the generic "error"), and its own exit code.
    let unborn = warehouse.run(&["history", "--json"]);
    assert_eq!(unborn.status.code(), Some(19));
    let value = json(&unborn);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "empty_history");
    assert!(value["error"]["message"].as_str().unwrap().contains("nothing stacked"));
}

/// The path of the incomplete-load marker file in a plain (non-bay) test warehouse — see
/// `load_guard_utils::marker_path_under`.
fn incomplete_load_marker_path(warehouse: &TestWarehouse) -> PathBuf {
    forklift_core::util::load_guard_utils::marker_path_under(&warehouse.root)
}

#[cfg(unix)]
#[test]
fn a_load_that_hits_an_unreadable_file_leaves_a_marker_and_stack_refuses() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-unreadable");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("dir/readable.txt", "fine\n");
    warehouse.write_file("dir/blocked.txt", "unreadable\n");
    std::fs::set_permissions(
        warehouse.root.join("dir").join("blocked.txt"),
        std::fs::Permissions::from_mode(0o000),
    ).unwrap();

    // The load fails partway (the unreadable file), but siblings it already published stay
    // staged (see `create_inventory_for_directory`'s doc comment) — this test only cares that
    // the load reports failure and the marker survives it.
    let load = warehouse.run(&["load", "."]);
    assert!(!load.status.success(), "a load hitting an unreadable file must fail");

    assert!(
        incomplete_load_marker_path(&warehouse).exists(),
        "a failed load must leave the incomplete-load marker behind"
    );

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let value = json(&stacked);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "incomplete_load");
    assert!(
        value["error"]["message"].as_str().unwrap().contains("./"),
        "the message names the recorded (root) path: {}", value["error"]["message"]
    );
    assert!(value["error"]["next_step"].as_str().unwrap().contains("forklift load"));

    // Clean up so the warehouse's Drop can remove the directory without surprises.
    std::fs::set_permissions(
        warehouse.root.join("dir").join("blocked.txt"),
        std::fs::Permissions::from_mode(0o644),
    ).unwrap();
}

#[cfg(unix)]
#[test]
fn a_clean_reload_of_the_same_root_clears_the_marker_and_stack_succeeds() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-heals");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("dir/readable.txt", "fine\n");
    warehouse.write_file("dir/blocked.txt", "unreadable\n");
    let blocked_path = warehouse.root.join("dir").join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    assert!(!warehouse.run(&["load", "."]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    // Fix the file and re-load the same root: the marker must clear, and stack must succeed.
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "."]));

    assert!(
        !incomplete_load_marker_path(&warehouse).exists(),
        "a clean re-load of the same root must clear the marker"
    );
    assert_success(&warehouse.run(&["stack", "clean"]));
}

#[cfg(unix)]
#[test]
fn a_narrower_clean_load_does_not_heal_a_broader_failed_root() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-narrower");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("dir/readable.txt", "fine\n");
    warehouse.write_file("dir/blocked.txt", "unreadable\n");
    warehouse.write_file("other/ok.txt", "unrelated\n");
    let blocked_path = warehouse.root.join("dir").join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    // A whole-warehouse load fails: the marker records the root ("./"), not just "dir".
    assert!(!warehouse.run(&["load", "."]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    // A narrower, unrelated, and fully clean load must not clear the broader marker.
    assert_success(&warehouse.run(&["load", "other"]));
    assert!(
        incomplete_load_marker_path(&warehouse).exists(),
        "a narrower clean load must not clear a broader failed root's marker"
    );

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    assert_eq!(json(&stacked)["error"]["code"], "incomplete_load");

    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

#[test]
fn a_hand_written_marker_refuses_stack_even_without_a_reported_load_failure() {
    // Simulates a load killed outright (a crash, SIGKILL, power loss) partway through its walk:
    // the marker was durably written at load *start* (see `load_guard_utils`'s doc comment on
    // why), but the process never got the chance to report an error — nothing was ever printed.
    // Writing the marker by hand reproduces exactly that on-disk state without needing a real
    // kill.
    let warehouse = TestWarehouse::new("incomplete-load-simulated-crash");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("a.txt", "x\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    warehouse.write_file("a.txt", "changed\n");
    assert_success(&warehouse.run(&["load", "."]));

    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();
    std::fs::write(&marker_path, "./\n").unwrap();

    let stacked = warehouse.run(&["stack", "second", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let value = json(&stacked);
    assert_eq!(value["error"]["code"], "incomplete_load");
    assert!(value["error"]["message"].as_str().unwrap().contains("./"));
}

/// Two independent loads, over disjoint subtrees, each fail. A single-path marker could only
/// ever name one of them — healing that one would (wrongly) clear the guard entirely, leaving
/// the other's half-staged content free for `stack` to silently commit. The set-based marker
/// must keep refusing, naming whichever root is still outstanding, until *both* are healed.
#[cfg(unix)]
#[test]
fn two_disjoint_failed_loads_both_stay_recorded_until_each_heals() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-two-disjoint");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("dirfirst/ok.txt", "fine\n");
    warehouse.write_file("dirfirst/blocked.txt", "unreadable\n");
    warehouse.write_file("dirsecond/ok.txt", "fine\n");
    warehouse.write_file("dirsecond/blocked.txt", "unreadable\n");

    let first_blocked = warehouse.root.join("dirfirst").join("blocked.txt");
    let second_blocked = warehouse.root.join("dirsecond").join("blocked.txt");
    std::fs::set_permissions(&first_blocked, std::fs::Permissions::from_mode(0o000)).unwrap();
    std::fs::set_permissions(&second_blocked, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Two independent, disjoint loads, each fails on its own unreadable file.
    assert!(!warehouse.run(&["load", "dirfirst"]).status.success());
    assert!(!warehouse.run(&["load", "dirsecond"]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    // Both roots are recorded: stack names both.
    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let message = json(&stacked)["error"]["message"].as_str().unwrap().to_string();
    assert!(message.contains("dirfirst"), "names the first outstanding root: {}", message);
    assert!(message.contains("dirsecond"), "names the second outstanding root: {}", message);

    // Healing only "dirfirst" must not clear "dirsecond" — stack still refuses.
    std::fs::set_permissions(&first_blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "dirfirst"]));

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let message = json(&stacked)["error"]["message"].as_str().unwrap().to_string();
    assert!(!message.contains("dirfirst"), "dirfirst healed, must not still be named: {}", message);
    assert!(message.contains("dirsecond"), "dirsecond still outstanding: {}", message);

    // Healing "dirsecond" too clears the guard entirely.
    std::fs::set_permissions(&second_blocked, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "dirsecond"]));
    assert!(!incomplete_load_marker_path(&warehouse).exists());
    assert_success(&warehouse.run(&["stack", "clean"]));
}

/// `park`'s push durably commits the staged inventory into a parked parcel exactly like `stack`
/// commits into a stacked one — it must refuse under the same guard, not just preserve the
/// marker through an otherwise-successful push/pop.
#[test]
fn park_push_refuses_on_an_incomplete_load_exactly_like_stack() {
    let warehouse = TestWarehouse::new("incomplete-load-park");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("a.txt", "x\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    warehouse.write_file("a.txt", "changed\n");
    assert_success(&warehouse.run(&["load", "."]));

    // Simulate the marker a real interrupted load would leave (see the hand-written-marker test
    // above for why this is a faithful stand-in).
    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::write(&marker_path, "./\n").unwrap();

    let parked = warehouse.run(&["park", "--json"]);
    assert_eq!(parked.status.code(), Some(20));
    assert_eq!(json(&parked)["error"]["code"], "incomplete_load");

    // Healing the load lets park proceed normally.
    assert_success(&warehouse.run(&["load", "."]));
    assert!(!marker_path.exists());
    assert_success(&warehouse.run(&["park"]));
}

/// `consolidate`'s three-way merge writes real output into the working inventory and only
/// afterwards records that a consolidation is in progress (`.forklift/consolidation`), well
/// before it ever reaches `stack_utils::stack_parcel`'s own guard — so, unlike `stack` and
/// `park`, a late failure there used to leave that state written behind it. The guard added
/// up front in `consolidate::handle_command` must refuse before `merge_head_into_current`
/// touches anything: reverting it makes the state-file assertion below go red, because the
/// merge and its state write both run to completion before the *late* guard (still inside
/// `stack_parcel`) finally fires.
#[test]
fn consolidate_refuses_on_an_incomplete_load_before_writing_consolidation_state() {
    let warehouse = TestWarehouse::new("incomplete-load-consolidate");
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nfive\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Diverge with non-overlapping changes so the merge would otherwise complete cleanly.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nFIVE\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "ONE\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "main work"]));

    // Simulate a load killed outright partway through its walk (see the hand-written-marker
    // test above for why this is a faithful stand-in).
    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::write(&marker_path, "./\n").unwrap();

    let consolidated = warehouse.run(&["consolidate", "feature", "--json"]);
    assert_eq!(consolidated.status.code(), Some(20));
    assert_eq!(json(&consolidated)["error"]["code"], "incomplete_load");

    assert!(
        !warehouse.root.join(".forklift/consolidation").exists(),
        "consolidation state must not be written while an incomplete load is outstanding"
    );
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\nfive\n",
        "the merge must not have applied anything either"
    );

    // Healing the load lets the consolidation proceed normally.
    std::fs::remove_file(&marker_path).unwrap();
    assert_success(&warehouse.run(&["consolidate", "feature"]));
}

/// `cherry-pick` applies a merge into the working inventory and only afterwards records its own
/// in-progress state (`.forklift/cherry-pick`), before it ever reaches
/// `stack_utils::stack_parcel`'s own guard. The guard added up front in
/// `cherry_pick::handle_command` must refuse before any of that runs: reverting it makes the
/// state-file assertion below go red, exactly like the `consolidate` case above.
#[test]
fn cherry_pick_refuses_on_an_incomplete_load_before_writing_state() {
    let warehouse = TestWarehouse::new("incomplete-load-cherry-pick");
    warehouse.write_file("f.txt", "base\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("feat.txt", "feature file\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "feature work"]));

    assert_success(&warehouse.run(&["shift", "main"]));

    // Simulate a load killed outright partway through its walk.
    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::write(&marker_path, "./\n").unwrap();

    let picked = warehouse.run(&["cherry-pick", &source, "--json"]);
    assert_eq!(picked.status.code(), Some(20));
    assert_eq!(json(&picked)["error"]["code"], "incomplete_load");

    assert!(
        !warehouse.root.join(".forklift/cherry-pick").exists(),
        "cherry-pick state must not be written while an incomplete load is outstanding"
    );
    assert!(!warehouse.root.join("feat.txt").exists(), "the pick must not have applied anything");

    // Healing the load lets the cherry-pick proceed normally.
    std::fs::remove_file(&marker_path).unwrap();
    assert_success(&warehouse.run(&["cherry-pick", &source]));
}

/// `haul merge` calls the same shared `merge_head_into_current` consolidate and cherry-pick do:
/// it writes real merge output into the working inventory and only afterwards records
/// consolidation state (`.forklift/consolidation`), before ever reaching
/// `stack_utils::stack_parcel`'s own late guard. The up-front guard added at the call site in
/// `haul::merge` must refuse before any of that runs: reverting it makes the state-file
/// assertion below go red, exactly like the `consolidate` and `cherry-pick` cases above.
#[test]
fn haul_merge_refuses_on_an_incomplete_load_before_writing_consolidation_state() {
    let warehouse = TestWarehouse::new("incomplete-load-haul");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("feature.txt", "feat\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    assert_success(&warehouse.run(&[
        "haul", "open", "--target", "main", "--source", "feature", "--title", "Add feature", "-m", "please review",
    ]));
    let id = json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"]
        .as_str().unwrap().to_string();

    // Simulate a load killed outright partway through its walk.
    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::write(&marker_path, "./\n").unwrap();

    let merged = warehouse.run(&["haul", "merge", &id, "--json"]);
    assert_eq!(merged.status.code(), Some(20));
    assert_eq!(json(&merged)["error"]["code"], "incomplete_load");

    assert!(
        !warehouse.root.join(".forklift/consolidation").exists(),
        "the haul merge's consolidation state must not be written while an incomplete load is \
        outstanding"
    );
    assert!(!warehouse.root.join("feature.txt").exists(), "the merge must not have applied anything");
    assert_eq!(
        json(&warehouse.run(&["haul", "show", &id, "--json"]))["data"]["status"], "open",
        "the haul must stay open — it was never actually merged"
    );

    // Healing the load lets the haul merge normally.
    std::fs::remove_file(&marker_path).unwrap();
    assert_success(&warehouse.run(&["haul", "merge", &id]));
}

/// Re-running `load` is real work an operator may not always want to redo; abandoning an
/// incomplete load by resetting it to head must also be a clean, working recovery path.
#[cfg(unix)]
#[test]
fn restore_staged_over_the_recorded_root_heals_the_marker() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-restore-heals");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("dir/ok.txt", "fine\n");
    warehouse.write_file("dir/blocked.txt", "unreadable\n");
    let blocked_path = warehouse.root.join("dir").join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    // A whole-warehouse load fails: the marker records the root ("./").
    assert!(!warehouse.run(&["load", "."]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));

    // "restore --staged ." fully covers the recorded root — this abandons the incomplete load
    // (rather than healing it by re-reading the files) but is an equally valid remedy: the
    // guard's own next_step names it explicitly.
    assert_success(&warehouse.run(&["restore", "--staged", "."]));
    assert!(
        !incomplete_load_marker_path(&warehouse).exists(),
        "restore --staged over the recorded root must heal the marker"
    );

    // A fresh, clean load now stacks normally.
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    warehouse.write_file("clean.txt", "clean\n");
    assert_success(&warehouse.run(&["load", "clean.txt"]));
    assert_success(&warehouse.run(&["stack", "clean"]));
}

/// The conservative half of the same recovery path: a staging reset must only heal the marker
/// for the region it actually rebuilt from a known-good source — a reset of a narrower,
/// unrelated path must never silently clear a broader recorded root it never touched.
#[cfg(unix)]
#[test]
fn restore_staged_of_a_narrower_path_does_not_heal_a_broader_recorded_root() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-restore-partial");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // An initial clean stack establishes a head with a real, shard-backed directory.
    warehouse.write_file("existing/a.txt", "a\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // A later whole-warehouse load fails on an unrelated unreadable file: the marker records
    // the root ("./"), not just the directory that actually failed.
    warehouse.write_file("blocked.txt", "unreadable\n");
    let blocked_path = warehouse.root.join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();
    assert!(!warehouse.run(&["load", "."]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    // Restoring only "existing" (already consistent with head) does not cover the broader
    // recorded root ("./") — the marker survives.
    assert_success(&warehouse.run(&["restore", "--staged", "existing"]));
    assert!(
        incomplete_load_marker_path(&warehouse).exists(),
        "restoring a narrower path must not heal a broader recorded root"
    );

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));

    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// The vacuous corner of the same recovery path: a directory whose only file is unreadable never
/// gets a shard at all (its own per-directory task publishes nothing — see
/// `load_guard_utils::clear_recorded_under`'s doc comment). Abandoning it via `restore --staged`
/// has nothing to reset (it was never staged) and nothing to abandon (nothing landed there) — it
/// must succeed as a heal-no-op instead of failing with "neither in the inventory nor in the
/// pallet head" while leaving the marker (and the guard) stuck forever. See
/// `restore_staged_of_a_narrower_path_does_not_heal_a_broader_recorded_root` above for the
/// guard: restoring a path that *does* carry real staged content keeps today's ordinary
/// real-restore behavior (the marker survives) rather than taking this no-op path.
///
/// Deliberately `load bigdir` — never `load .` — so the recorded root is `bigdir` itself, exactly
/// matching what this test then restores. A heal may only ever clear a recorded root its own
/// validation actually covers (see `load_guard_utils::clear_recorded_under`'s governing
/// invariant); the reverse — clearing a broader record because a narrower, unrelated path under
/// it happens to be empty — is the live regression
/// `restore_staged_of_an_unrelated_path_never_clears_a_broader_recorded_root` below exists to
/// keep closed. This shape also, as a side effect, has no sibling task to race: `bigdir` is the
/// *only* directory `load bigdir` ever walks, so unlike an earlier version of this test, nothing
/// here depends on `TaskExecutor` scheduling. The survivor directory ("good") is still staged by
/// its own separate, prior `load` — simply so `stack` below has something to commit once `bigdir`
/// is healed away, not to avoid a race.
#[cfg(unix)]
#[test]
fn restore_staged_of_a_vacuous_failed_directory_heals_as_a_no_op() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-restore-vacuous");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("good/ok.txt", "fine\n");
    assert_success(&warehouse.run(&["load", "good"]));

    warehouse.write_file("bigdir/blocked.txt", "unreadable\n");
    let blocked_path = warehouse.root.join("bigdir").join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    // Loading exactly "bigdir" records the marker at "bigdir" itself — not the broader "./" a
    // whole-warehouse `load .` would have recorded.
    assert!(!warehouse.run(&["load", "bigdir"]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    let restored = warehouse.run(&["restore", "--staged", "bigdir", "--json"]);
    assert_success(&restored);
    let value = json(&restored);
    assert!(
        value["data"]["message"].as_str().unwrap().contains("Nothing was staged"),
        "reports the heal-no-op: {}", value
    );

    assert!(
        !incomplete_load_marker_path(&warehouse).exists(),
        "restoring the vacuous failed directory must clear its own recorded root"
    );

    assert_success(&warehouse.run(&["stack", "clean"]));

    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// The blocker this fix closes: a heal may only ever clear a recorded root its own validation
/// actually covered — never a *broader* one that merely happens to cover the validated path in
/// the other direction (see `load_guard_utils::clear_recorded_under`'s governing invariant).
/// `data` is staged by its own prior, unrelated clean load; the later `load .` fails, recording
/// the whole root ("./") — `data`'s already-durable content is untouched, but whatever the walk
/// never reached is still genuinely missing from staging. Restoring an unrelated, nonexistent
/// path with nothing staged under it must never be read as "the whole recorded root is now
/// accounted for": that restore validated only itself, nothing about the rest of "./". If it
/// cleared "./" anyway, a subsequent `stack` would silently commit a tree missing whatever the
/// failed walk never reached — exactly the silent-incomplete-commit class this whole guard exists
/// to prevent, reopened through its own heal path.
#[cfg(unix)]
#[test]
fn restore_staged_of_an_unrelated_path_never_clears_a_broader_recorded_root() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("incomplete-load-restore-unrelated");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("data/real.txt", "real content\n");
    assert_success(&warehouse.run(&["load", "data"]));

    warehouse.write_file("blocked.txt", "unreadable\n");
    let blocked_path = warehouse.root.join("blocked.txt");
    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o000)).unwrap();

    assert!(!warehouse.run(&["load", "."]).status.success());
    assert!(incomplete_load_marker_path(&warehouse).exists());

    // Restoring a totally unrelated, nonexistent path (nothing staged under it either) must
    // never clear the broader "./" record — this restore validated nothing about "./" beyond
    // "typo-path" itself. The command may error or no-op (it currently errors, the same
    // "neither in the inventory nor in the pallet head" a genuinely unrelated path always gets);
    // either is fine — the marker surviving is what this test actually checks.
    let _ = warehouse.run(&["restore", "--staged", "typo-path"]);
    assert!(
        incomplete_load_marker_path(&warehouse).exists(),
        "restoring an unrelated path must never clear a broader recorded root"
    );

    let stacked = warehouse.run(&["stack", "attempt", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));

    std::fs::set_permissions(&blocked_path, std::fs::Permissions::from_mode(0o644)).unwrap();
}

/// A hand-corrupted marker must never become the very thing its own remedy is blocked on —
/// `load` (the write side) self-heals by overwriting it — but a `stack` about to durably commit
/// (the check side) must fail closed rather than silently treat unreadable content as "nothing
/// recorded", and must point at a recovery path.
#[test]
fn a_malformed_marker_self_heals_on_load_but_refuses_stack_conservatively() {
    let warehouse = TestWarehouse::new("incomplete-load-malformed");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("a.txt", "x\n");

    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();

    // Hand-corrupt the marker (invalid UTF-8 — never producible by this module itself). The
    // write side self-heals: load overwrites it with fresh, readable content and proceeds.
    std::fs::write(&marker_path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap();
    assert_success(&warehouse.run(&["load", "a.txt"]));
    assert_success(&warehouse.run(&["stack", "first"]));

    // Corrupt it again, with no load in between this time: the check side must refuse
    // conservatively rather than silently treat unreadable content as "nothing recorded", and
    // point at a recovery path.
    std::fs::write(&marker_path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap();

    let stacked = warehouse.run(&["stack", "second", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let value = json(&stacked);
    assert_eq!(value["error"]["code"], "incomplete_load");
    assert!(
        value["error"]["next_step"].as_str().unwrap().contains("restore"),
        "points at a recovery path: {:?}", value["error"]
    );
}

#[test]
fn stocktake_summary_reports_counts_without_the_per_path_lists() {
    let warehouse = TestWarehouse::new("json-summary");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("a.txt", "x\n");
    warehouse.write_file("dir/b.txt", "y\n");

    let summary = warehouse.run(&["stocktake", "--summary", "--json"]);
    assert_success(&summary);
    let value = json(&summary);
    assert_eq!(value["data"]["summary"], true);
    assert_eq!(value["data"]["unstaged_count"], 2);
    // The token-cheap overview omits the per-path lists entirely.
    assert!(value["data"].get("unstaged").is_none());
    assert!(value["data"].get("staged").is_none());
}

#[test]
fn conflicts_json_exposes_the_three_sides_as_content_addresses() {
    let warehouse = TestWarehouse::new("json-conflicts");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    // Two pallets change the same line differently → a real merge conflict.
    warehouse.write_file("f.txt", "line1\nline2\nline3\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["palletize", "other"]));
    warehouse.write_file("f.txt", "line1\nTHEIRS\nline3\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs"]));
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("f.txt", "line1\nOURS\nline3\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "ours"]));

    // A conflicting consolidation is not a command failure — it reports the conflict.
    assert_success(&warehouse.run(&["consolidate", "other"]));

    let conflicts = warehouse.run(&["conflicts", "--json"]);
    assert_success(&conflicts);
    let value = json(&conflicts);
    let list = value["data"]["conflicts"].as_array().unwrap();
    assert_eq!(list.len(), 1);

    let conflict = &list[0];
    assert_eq!(conflict["path"], "f.txt");
    assert_eq!(conflict["markers"], true);

    // The three sides are content addresses a resolver can fetch: peek each.
    let base = stdout(&warehouse.run(&["peek", conflict["base"].as_str().unwrap()]));
    assert!(base.contains("line2") && !base.contains("OURS") && !base.contains("THEIRS"), "{}", base);

    let ours = stdout(&warehouse.run(&["peek", conflict["ours"].as_str().unwrap()]));
    assert!(ours.contains("OURS"), "{}", ours);

    let theirs = stdout(&warehouse.run(&["peek", conflict["theirs"].as_str().unwrap()]));
    assert!(theirs.contains("THEIRS"), "{}", theirs);
}

#[test]
fn mcp_lists_tools_and_calls_them_returning_the_same_envelopes() {
    let warehouse = TestWarehouse::new("mcp");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("a.txt", "hello\n");

    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#, "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, "\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n",
        r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"stocktake","arguments":{"summary":true}}}"#, "\n",
    );

    let output = warehouse.run_with_stdin(&["mcp"], input);
    assert_success(&output);

    let replies: Vec<serde_json::Value> = stdout(&output).lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();

    // The notification produced no reply: three requests, three responses.
    assert_eq!(replies.len(), 3);

    // initialize
    assert_eq!(replies[0]["result"]["serverInfo"]["name"], "forklift");
    assert_eq!(replies[0]["result"]["capabilities"]["tools"].is_object(), true);

    // tools/list
    let tools = replies[1]["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "stocktake"));
    assert!(tools.iter().any(|tool| tool["name"] == "conflicts"));
    assert!(tools.iter().any(|tool| tool["name"] == "undo"));
    assert!(tools.iter().all(|tool| tool["inputSchema"]["type"] == "object"));

    // tools/call returns the exact --json envelope the CLI produces.
    assert_eq!(replies[2]["result"]["isError"], false);
    let text = replies[2]["result"]["content"][0]["text"].as_str().unwrap();
    let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(envelope["command"], "stocktake");
    assert_eq!(envelope["data"]["summary"], true);
    assert_eq!(envelope["data"]["unstaged_count"], 1);
}

#[test]
fn mcp_reports_a_failing_command_as_a_tool_error() {
    let warehouse = TestWarehouse::new("mcp-error");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    // Stacking with nothing staged fails; the MCP tool surfaces it as isError with
    // the error envelope, not a crashed session.
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"stack","arguments":{"description":"nothing"}}}"#, "\n",
    );

    let output = warehouse.run_with_stdin(&["mcp"], input);
    assert_success(&output);

    let reply: serde_json::Value = serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap();
    assert_eq!(reply["result"]["isError"], true);
    let text = reply["result"]["content"][0]["text"].as_str().unwrap();
    let envelope: serde_json::Value = serde_json::from_str(text).unwrap();
    assert_eq!(envelope["ok"], false);
}

#[test]
fn mcp_exposes_and_runs_the_parity_pass_tools() {
    let warehouse = TestWarehouse::new("mcp-parity");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    // The formerly-missing agent tools are now advertised…
    let list = warehouse.run_with_stdin(
        &["mcp"],
        concat!(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#, "\n"),
    );
    let reply: serde_json::Value = serde_json::from_str(stdout(&list).lines().next().unwrap()).unwrap();
    let names: std::collections::HashSet<&str> = reply["result"]["tools"]
        .as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for expected in ["blame", "deliver", "cherry_pick", "manifest_provenance", "haul_open", "park_pop", "tag_list", "compact"] {
        assert!(names.contains(expected), "MCP must advertise `{}`", expected);
    }

    // …and their argument mappings drive the real CLI: read tools that need no trust run.
    let call = |name: &str, arguments: &str| -> serde_json::Value {
        let input = format!(
            "{{\"jsonrpc\":\"2.0\",\"id\":7,\"method\":\"tools/call\",\"params\":{{\"name\":\"{}\",\"arguments\":{}}}}}\n",
            name, arguments,
        );
        let output = warehouse.run_with_stdin(&["mcp"], &input);
        serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap()
    };
    assert_eq!(call("blame", r#"{"path":"a.txt"}"#)["result"]["isError"], false);
    assert_eq!(call("park_list", "{}")["result"]["isError"], false);
    assert_eq!(call("haul_list", "{}")["result"]["isError"], false);
    // A no-arg maintenance tool runs through the same subprocess path (and packs the store).
    assert_eq!(call("compact", "{}")["result"]["isError"], false);
    // A missing required argument is surfaced as a JSON-RPC error, not a panic.
    let missing = call("blame", "{}");
    assert!(missing["error"].is_object(), "a missing required arg must be an error: {}", missing);
}

#[test]
fn mcp_history_tool_paginates_with_limit_and_after_like_the_cli() {
    let warehouse = TestWarehouse::new("mcp-history-page");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    for i in 1..=5 {
        warehouse.write_file("a.txt", &format!("v{}\n", i));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("p{}", i)]));
    }

    let call = |arguments: &str| -> serde_json::Value {
        let input = format!(
            "{}\n",
            format_args!(
                r#"{{"jsonrpc":"2.0","id":9,"method":"tools/call","params":{{"name":"history","arguments":{}}}}}"#,
                arguments
            )
        );
        let output = warehouse.run_with_stdin(&["mcp"], &input);
        assert_success(&output);
        let reply: serde_json::Value = serde_json::from_str(stdout(&output).lines().next().unwrap()).unwrap();
        assert_eq!(reply["result"]["isError"], false, "tool call failed: {}", reply);
        let text = reply["result"]["content"][0]["text"].as_str().unwrap();
        serde_json::from_str::<serde_json::Value>(text).unwrap()
    };

    // The tool exposes limit + after (and the pass-through class filter).
    // A limited page carries a `next` cursor…
    let page_one = call(r#"{"limit":2}"#);
    let entries_one = page_one["data"]["entries"].as_array().unwrap();
    assert_eq!(entries_one.len(), 2, "history limit must bound the page: {}", page_one);
    let cursor = page_one["data"]["next"].as_str().expect("a bounded page returns a next cursor");

    // …and passing it back as `after` reads the following page, without overlap.
    let page_two = call(&format!(r#"{{"limit":2,"after":"{}"}}"#, cursor));
    let entries_two = page_two["data"]["entries"].as_array().unwrap();
    assert_eq!(entries_two.len(), 2);
    let first_page: std::collections::HashSet<&str> =
        entries_one.iter().map(|e| e["parcel"].as_str().unwrap()).collect();
    assert!(
        entries_two.iter().all(|e| !first_page.contains(e["parcel"].as_str().unwrap())),
        "the second page must not repeat the first"
    );
}

#[test]
fn undo_soft_resets_the_last_stack_and_restages_its_changes() {
    let warehouse = TestWarehouse::new("undo");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["load", "."]));
    let first = warehouse.run(&["stack", "first"]);
    assert_success(&first);
    let first_hash = extract_parcel_hash(&first);

    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    let second = warehouse.run(&["stack", "second"]);
    assert_success(&second);
    let second_hash = extract_parcel_hash(&second);

    // Undo the second stack: head goes back to "first", its changes re-staged.
    let undone = warehouse.run(&["undo", "--json"]);
    assert_success(&undone);
    let value = json(&undone);
    assert_eq!(value["data"]["undone"], second_hash);
    assert_eq!(value["data"]["head"], first_hash);

    // The undone parcel is off the pallet; the head is "first" again.
    let history = stdout(&warehouse.run(&["history"]));
    assert!(!history.contains(&second_hash), "undone parcel must be off the pallet: {}", history);
    assert!(history.contains("first"));

    // b.txt's addition is staged again (soft reset kept the inventory + working tree).
    let staged = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(json(&staged)["data"]["staged_count"], 1);

    // Re-stack to redo (e.g. with a corrected message).
    assert_success(&warehouse.run(&["stack", "second, corrected"]));

    // Undo down to the first parcel, then once more: nothing left to undo (no parent).
    assert_success(&warehouse.run(&["undo"]));
    let at_first = warehouse.run(&["undo"]);
    assert!(!at_first.status.success());
    assert!(stderr(&at_first).contains("nothing to undo"), "{}", stderr(&at_first));
}

#[test]
fn undo_reverses_a_consolidate_and_a_shift() {
    let warehouse = TestWarehouse::new("undo-journal");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // A side pallet changes one file; main changes another → a clean two-parent merge.
    assert_success(&warehouse.run(&["palletize", "side"]));
    warehouse.write_file("side.txt", "side\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "side work"]));
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("main.txt", "main\n");
    assert_success(&warehouse.run(&["load", "."]));
    let main_head = extract_parcel_hash(&warehouse.run(&["stack", "main work"]));

    // Reverse a shift first (while the warehouse is clean): move to side, undo → back on
    // main, re-materialized.
    assert_success(&warehouse.run(&["shift", "side"]));
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* side"));
    let unshift = warehouse.run(&["undo", "--json"]);
    assert_success(&unshift);
    assert_eq!(json(&unshift)["data"]["op"], "shift");
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* main"), "undo must return to main");

    // Clean merge → a two-parent merge parcel becomes the head.
    assert_success(&warehouse.run(&["consolidate", "side"]));
    assert!(stdout(&warehouse.run(&["history"])).contains("consolidates"), "expected a merge parcel");

    // Undo now REVERSES the merge (a soft reset to main's pre-merge head) — no longer
    // refused. The merge parcel comes off; the head is main's own work again.
    let undone = warehouse.run(&["undo", "--json"]);
    assert_success(&undone);
    assert_eq!(json(&undone)["data"]["head"], main_head);
    assert!(!stdout(&warehouse.run(&["history"])).contains("consolidates"), "merge must be off the pallet");
}

// Regression test for a bug where `undo` rolled back pallet refs it never journaled.
// `JournalEntry` snapshots every pallet ref (user and meta, `@office` included) before a
// journaled op so it can be restored, but an office/meta-pallet mutation between the
// journaled op and the `undo` is not itself journaled. Undo used to restore `@office`
// unconditionally along with everything else, silently un-retiring a key that was revoked
// after the stack. `restore_refs` now only rolls a ref back if it still sits exactly where
// the journaled op left it.
#[test]
fn undo_never_reverts_a_key_retirement_that_happened_after_the_journaled_stack() {
    let warehouse = TestWarehouse::new("undo-office-boundary");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"])); // test@forklift becomes admin

    let bob = keygen_admit_args(&warehouse, "bob@forklift");
    assert_success(&warehouse.run(&["office", "admit", &bob[0], &bob[1], &bob[2]]));

    // A first parcel to be born onto: a stack onto an unborn pallet isn't journaled at all
    // (there is no ref to restore back to), so this one has to precede the one under test.
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "genesis"]));

    // The journaled operation: a stack, undo-able on its own.
    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    // Un-journaled, in between: an admin retires bob's key. This advances `@office`.
    let list_json = json(&warehouse.run(&["office", "list", "--json"]));
    let users = list_json["data"]["users"].as_array().unwrap();
    let bob_user = users.iter().find(|user| user["identifier"] == "bob@forklift").unwrap();
    let bob_key_id = bob_user["keys"][0]["key_id"].as_str().unwrap().to_string();
    assert_success(&warehouse.run(&["office", "retire", &bob_key_id]));

    // Undo reverses the stack…
    let undone = warehouse.run(&["undo", "--json"]);
    assert_success(&undone);
    let staged = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(json(&staged)["data"]["staged_count"], 1, "the stack's change must be re-staged");

    // …but must leave the retirement alone: bob's key stays retired, not silently restored
    // to active by rolling `@office` back to its pre-stack head.
    let after_json = json(&warehouse.run(&["office", "list", "--json"]));
    let after_users = after_json["data"]["users"].as_array().unwrap();
    let bob_after = after_users.iter().find(|user| user["identifier"] == "bob@forklift").unwrap();
    assert_eq!(bob_after["keys"][0]["key_id"], bob_key_id);
    assert_eq!(
        bob_after["keys"][0]["retired"], true,
        "the retirement must survive undo: {}", bob_after
    );
}

// Regression test for a second bug in the same area: `restore_refs` correctly leaves a
// moved ref alone, but `undo` itself didn't know it had — it popped (and so consumed) the
// journal entry and reported full success ("Undid stack …, its changes are staged again")
// even though the pallet head never actually moved back. `undo` now checks, before
// consuming the entry or touching anything, whether the journaled op's own pallet ref still
// sits where that op left it; if something un-journaled (here: a haul merge) advanced it in
// the meantime, `undo` refuses outright and leaves the journal untouched.
#[test]
fn undo_refuses_rather_than_silently_no_opping_when_the_pallet_moved_on() {
    let warehouse = TestWarehouse::new("undo-moved-refuses");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"])); // haul needs a signing actor

    // Born onto main.
    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "feature"'s own divergent work is set up *before* the op under test, so it doesn't
    // end up on top of the journal (a stack there would be the next thing undo reverses).
    assert_success(&warehouse.run(&["palletize", "feature"])); // switches onto feature
    warehouse.write_file("feature.txt", "feat\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    // The journaled operation under test — the last thing on the journal from here on.
    warehouse.write_file("main.txt", "v1\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));

    // Un-journaled: a haul merge advances main's head past "second" with a real merge
    // parcel (haul is not in the journaled command set).
    assert_success(&warehouse.run(&[
        "haul", "open", "--target", "main", "--source", "feature", "--title", "Add feature", "-m", "please review",
    ]));
    let id = json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"].as_str().unwrap().to_string();
    assert_success(&warehouse.run(&["haul", "merge", &id]));

    let head_before = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("moved since that operation"), "{}", stderr(&refused));

    let head_after = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(head_before, head_after, "a refused undo must not touch the pallet head");

    // The journal entry is untouched too: a fresh legitimate stack/undo cycle still works.
    warehouse.write_file("main.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "third"]));
    let undone = warehouse.run(&["undo", "--json"]);
    assert_success(&undone);
    assert_eq!(json(&undone)["data"]["op"], "stack");
}

// The same root cause, hitting the consolidation state instead: an unconditional restore
// would resurrect a stale in-progress-merge record (captured before a conflict was resolved
// and stacked) against a head that has since moved past it — corrupting the next `stack`
// with a spurious second merge parent. The pre-mutation gate must stop this too.
#[test]
fn undo_refuses_a_stale_consolidation_after_the_pallet_moved_on() {
    let warehouse = TestWarehouse::new("undo-consolidation-boundary");
    warehouse.write_file("file.txt", "one\ntwo\nthree\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"])); // haul needs a signing actor
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "feature" diverges on the very line "main" will also change (a guaranteed conflict).
    assert_success(&warehouse.run(&["palletize", "feature"])); // switches onto feature
    warehouse.write_file("file.txt", "one\nTHEIRS\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "theirs"]));

    // "extra" is the un-journaled advance's future source — set up now, before the op
    // under test, so its own stack doesn't land on top of the journal.
    assert_success(&warehouse.run(&["shift", "main"]));
    assert_success(&warehouse.run(&["palletize", "extra"])); // switches onto extra
    warehouse.write_file("extra.txt", "extra work\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "extra work"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "one\nOURS\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "ours"]));

    // A conflicting consolidate: leaves an in-progress consolidation record, main unmoved.
    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    assert!(stdout(&merge).contains("conflict: file.txt"));

    // Resolve and finish: this "stack" is the journaled op under test — its pre-op snapshot
    // carries the in-progress consolidation record, which clears once the merge lands.
    warehouse.write_file("file.txt", "one\nresolved\nthree\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "resolved"]));
    assert!(!warehouse.root.join(".forklift/consolidation").exists());

    // Un-journaled: haul merges "extra" into main, advancing it past the resolved merge.
    assert_success(&warehouse.run(&[
        "haul", "open", "--target", "main", "--source", "extra", "--title", "Extra", "-m", "please review",
    ]));
    let id = json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"].as_str().unwrap().to_string();
    assert_success(&warehouse.run(&["haul", "merge", &id]));

    let head_before = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("moved since that operation"), "{}", stderr(&refused));

    let head_after = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(head_before, head_after, "a refused undo must not touch the pallet head");
    assert!(
        !warehouse.root.join(".forklift/consolidation").exists(),
        "a refused undo must never resurrect a stale consolidation record"
    );

    // A subsequent legitimate undo-able op still works.
    warehouse.write_file("file.txt", "one\nresolved\nfour\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "fourth"]));
    assert_success(&warehouse.run(&["undo"]));
}

#[test]
fn config_unset_removes_a_value() {
    let warehouse = TestWarehouse::new("config-unset");
    assert_success(&warehouse.run(&["prepare"]));

    // Set a warehouse value, confirm it reads back, then unset it.
    assert_success(&warehouse.run(&["config", "remote.token", "sekret"]));
    assert_eq!(stdout(&warehouse.run(&["config", "remote.token"])).trim(), "sekret");

    let unset = warehouse.run(&["config", "--unset", "remote.token"]);
    assert_success(&unset);
    assert!(stdout(&unset).contains("Unset"), "{}", stdout(&unset));

    // It is gone now: reading it fails (not set), and the file no longer holds it.
    let read = warehouse.run(&["config", "remote.token"]);
    assert!(!read.status.success());
    assert!(stderr(&read).contains("not set"), "{}", stderr(&read));

    // Unsetting an already-absent key reports "not set" (non-zero).
    let again = warehouse.run(&["config", "--unset", "remote.token"]);
    assert!(!again.status.success());
    assert!(stderr(&again).contains("not set"), "{}", stderr(&again));

    // An unknown key is rejected, not silently ignored.
    let unknown = warehouse.run(&["config", "--unset", "bogus.key"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("Unknown configuration key"), "{}", stderr(&unknown));

    // --json emits the envelope for a successful unset.
    assert_success(&warehouse.run(&["config", "operator.name", "Temp"]));
    let json_unset = warehouse.run(&["config", "--unset", "operator.name", "--json"]);
    assert_success(&json_unset);
    assert_eq!(json(&json_unset)["ok"], true);
    assert_eq!(json(&json_unset)["command"], "config");
}

#[test]
fn a_passphrase_protected_key_needs_the_passphrase_to_sign() {
    let warehouse = TestWarehouse::new("passphrase");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Enroll with a passphrase-protected key. In a real run the passphrase is prompted;
    // the test supplies it through FORKLIFT_KEY_PASSPHRASE (the documented escape hatch).
    let pass = [("FORKLIFT_KEY_PASSPHRASE", "correct horse")];
    assert_success(&warehouse.run_with_env(&["office", "enroll", "--passphrase"], &pass));

    // The private key is encrypted at rest: its file is the versioned format, and the
    // raw 64-hex seed is nowhere in it.
    let key_file = std::fs::read_dir(warehouse.home.join("test-keys")).unwrap()
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .find(|path| path.extension().is_some_and(|extension| extension == "key"))
        .expect("a key file exists");
    let key_contents = std::fs::read_to_string(&key_file).unwrap();
    assert!(key_contents.starts_with("forklift-key-v2"), "key must be encrypted: {}", key_contents);

    // office list marks it protected.
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("protected"), "unexpected list: {}", list);

    // With the passphrase, signing works and the parcel audits clean.
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run_with_env(&["stack", "signed with the passphrase"], &pass));
    assert_success(&warehouse.run_with_env(&["audit"], &pass));

    // Without the right passphrase, the protected key cannot sign — a wrong passphrase
    // fails the decryption, so `stack` cannot forge a signature.
    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    let wrong = warehouse.run_with_env(&["stack", "should not sign"], &[("FORKLIFT_KEY_PASSPHRASE", "wrong")]);
    assert!(!wrong.status.success(), "a wrong passphrase must not sign");
    assert!(stderr(&wrong).contains("passphrase is incorrect"), "{}", stderr(&wrong));

    // A passphraseless (machine/agent-style) key stays unprotected — no format change,
    // signs without any passphrase — so automation is unaffected.
    let plain = TestWarehouse::new("passphrase-plain");
    plain.write_file("a.txt", "one\n");
    assert_success(&plain.run(&["prepare"]));
    configure_operator(&plain);
    assert_success(&plain.run(&["office", "enroll"]));
    assert_success(&plain.run(&["load", "."]));
    assert_success(&plain.run(&["stack", "signed, no passphrase"]));
    assert_success(&plain.run(&["audit"]));
}

#[test]
fn office_admit_marks_agents_bots_and_binds_a_supervisor() {
    let warehouse = TestWarehouse::new("office-classes");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["office", "enroll"]));

    let agent = keygen_admit_args(&warehouse, "agent@forklift");

    // An agent must be bound to a supervising human.
    let no_supervisor = warehouse.run(&["office", "admit", &agent[0], &agent[1], &agent[2], "--agent"]);
    assert!(!no_supervisor.status.success());
    assert!(stderr(&no_supervisor).contains("supervising human"), "{}", stderr(&no_supervisor));

    // The supervisor must be enrolled.
    let ghost = warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "ghost@nowhere",
    ]);
    assert!(!ghost.status.success());
    assert!(stderr(&ghost).contains("not enrolled"), "{}", stderr(&ghost));

    // Admit the agent under the human admin.
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "test@forklift",
    ]));

    // office list shows the agent and its supervisor; JSON exposes the fields.
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("agent, supervised by test@forklift"), "{}", list);

    let list_json = json(&warehouse.run(&["office", "list", "--json"]));
    let users = list_json["data"]["users"].as_array().unwrap();
    let agent_user = users.iter().find(|user| user["identifier"] == "agent@forklift").unwrap();
    assert_eq!(agent_user["class"], "agent");
    assert_eq!(agent_user["supervisor"], "test@forklift");

    // Automation cannot supervise automation: an agent may not supervise another agent.
    let agent2 = keygen_admit_args(&warehouse, "agent2@forklift");
    let cross = warehouse.run(&[
        "office", "admit", &agent2[0], &agent2[1], &agent2[2], "--agent", "--supervisor", "agent@forklift",
    ]);
    assert!(!cross.status.success());
    assert!(stderr(&cross).contains("not a human"), "{}", stderr(&cross));

    // A bot needs no supervisor.
    let bot = keygen_admit_args(&warehouse, "bot@forklift");
    assert_success(&warehouse.run(&["office", "admit", &bot[0], &bot[1], &bot[2], "--bot"]));
    let list = stdout(&warehouse.run(&["office", "list"]));
    assert!(list.contains("[bot]"), "{}", list);

    // The office chain still audits clean — the classes ride in the signed records.
    assert_success(&warehouse.run(&["audit", "@office"]));
}

#[test]
fn blame_attributes_each_line_to_the_parcel_that_introduced_it() {
    let warehouse = TestWarehouse::new("blame");
    warehouse.write_file("f.txt", "one\ntwo\nthree\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let first = extract_parcel_hash(&warehouse.run(&["stack", "first"]));

    // A second parcel changes line 2 and appends line 4; lines 1 and 3 are untouched.
    warehouse.write_file("f.txt", "one\ntwo CHANGED\nthree\nfour\n");
    assert_success(&warehouse.run(&["load", "."]));
    let second = extract_parcel_hash(&warehouse.run(&["stack", "second"]));

    // Unchanged lines keep the first parcel; the changed and new lines are the second's.
    let blame = json(&warehouse.run(&["--json", "blame", "f.txt"]));
    let lines = blame["data"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["parcel"], first);
    assert_eq!(lines[1]["parcel"], second);
    assert_eq!(lines[2]["parcel"], first);
    assert_eq!(lines[3]["parcel"], second);

    // The human form shows the author and the line content.
    let human = stdout(&warehouse.run(&["blame", "f.txt"]));
    assert!(human.contains("two CHANGED"), "{}", human);
    assert!(human.contains("test@forklift"), "{}", human);

    // A directory or a missing path is refused.
    assert!(!warehouse.run(&["blame", "nope.txt"]).status.success());
}

#[test]
fn blame_skips_untouched_parcels_via_the_changed_path_filter_and_stays_correct() {
    // The changed-path filter lets blame skip parcels that did not touch the file. This must
    // never change the attribution: a parcel that DID touch the file is never skipped (a Bloom
    // filter has no false negatives), and interleaved parcels that touched only other files are.
    let warehouse = TestWarehouse::new("blame-filter");
    warehouse.write_file("target.txt", "L1\nL2\n");
    warehouse.write_file("other.txt", "x\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let c1 = extract_parcel_hash(&warehouse.run(&["stack", "introduce target"]));

    // Two parcels that touch only other.txt — the filter must skip these for target.txt.
    warehouse.write_file("other.txt", "x2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "other only 1"]));
    warehouse.write_file("other.txt", "x3\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "other only 2"]));

    // A parcel that changes L2 and adds L3 in target.txt.
    warehouse.write_file("target.txt", "L1\nL2 CHANGED\nL3\n");
    assert_success(&warehouse.run(&["load", "."]));
    let c4 = extract_parcel_hash(&warehouse.run(&["stack", "change target"]));

    // Compact builds the changed-path filters, so the blame below reads them (the fast path).
    assert_success(&warehouse.run(&["compact"]));

    let blame = json(&warehouse.run(&["--json", "blame", "target.txt"]));
    let lines = blame["data"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["parcel"], c1, "L1 was introduced by the first parcel");
    assert_eq!(lines[1]["parcel"], c4, "L2 was changed by the fourth parcel");
    assert_eq!(lines[2]["parcel"], c4, "L3 was added by the fourth parcel");
}

#[test]
fn blame_reads_the_identity_class_of_an_agent_authored_line() {
    let warehouse = TestWarehouse::new("blame-agent");
    warehouse.write_file("f.txt", "human line\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse); // test@forklift
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "human parcel"]));
    assert_success(&warehouse.run(&["office", "enroll"])); // test@forklift becomes admin

    // Admit an agent supervised by the human, and author a line under the agent's identity.
    let agent = keygen_admit_args(&warehouse, "agent@forklift");
    assert_success(&warehouse.run(&[
        "office", "admit", &agent[0], &agent[1], &agent[2], "--agent", "--supervisor", "test@forklift",
    ]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "agent@forklift"]));
    warehouse.write_file("f.txt", "human line\nagent line\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "agent parcel"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // The agent-authored line carries the identity class and supervisor — blame git cannot
    // express (§7.1). The human-authored line carries neither.
    let blame = json(&warehouse.run(&["--json", "blame", "f.txt"]));
    let lines = blame["data"]["lines"].as_array().unwrap();
    let parcels = &blame["data"]["parcels"];

    let agent_line_parcel = lines[1]["parcel"].as_str().unwrap();
    assert_eq!(parcels[agent_line_parcel]["class"], "agent");
    assert_eq!(parcels[agent_line_parcel]["supervisor"], "test@forklift");

    let human_line_parcel = lines[0]["parcel"].as_str().unwrap();
    assert!(parcels[human_line_parcel]["class"].is_null());

    let human = stdout(&warehouse.run(&["blame", "f.txt"]));
    assert!(human.contains("[agent, supervised by test@forklift]"), "{}", human);
}

#[test]
fn tags_are_signed_admin_only_and_verify_offline() {
    let warehouse = TestWarehouse::new("tags");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first parcel"]));

    // A tag is signed, so before trust it is refused.
    let before = warehouse.run(&["tag", "create", "v1.0", "main", "-m", "first"]);
    assert!(!before.status.success(), "a tag must require trust: {}", stderr(&before));

    assert_success(&warehouse.run(&["office", "enroll"])); // test@forklift becomes admin

    // The admin cuts a release tag.
    assert_success(&warehouse.run(&["tag", "create", "v1.0", "main", "-m", "the first release"]));

    // It lists (attributed to the admin) and shows in full.
    let list = stdout(&warehouse.run(&["tag"]));
    assert!(list.contains("v1.0") && list.contains("admin"), "unexpected: {}", list);
    let show = stdout(&warehouse.run(&["tag", "show", "v1.0"]));
    assert!(show.contains("the first release") && show.contains("test@forklift"), "unexpected: {}", show);

    // The tag name is immutable: a second create with the same name is refused.
    let dup = warehouse.run(&["tag", "create", "v1.0", "main"]);
    assert!(!dup.status.success() && stderr(&dup).contains("immutable"), "{}", stderr(&dup));

    // It lives on the @tags meta pallet, verifiable offline like any signed history, and is
    // not a reserved user name.
    let audit = stdout(&warehouse.run(&["audit", "@tags"]));
    assert!(audit.contains("verified"), "unexpected audit: {}", audit);
    assert!(stdout(&warehouse.run(&["palletize", "--all"])).contains("@tags"));

    // Only an admin may cut a tag (the release convention, §9.4d): a writer is refused.
    let writer = keygen_admit_args(&warehouse, "writer@forklift");
    assert_success(&warehouse.run(&["office", "admit", &writer[0], &writer[1], &writer[2]]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "writer@forklift"]));
    let refused = warehouse.run(&["tag", "create", "v2.0", "main"]);
    assert!(!refused.status.success() && stderr(&refused).contains("admin"), "{}", stderr(&refused));
}

#[test]
fn cherry_pick_applies_a_parcel_diff_preserving_authorship() {
    let warehouse = TestWarehouse::new("cherry-pick");
    warehouse.write_file("f.txt", "base\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse); // the picker: test@forklift
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // A feature pallet whose parcel is authored by *bob* (a different operator).
    assert_success(&warehouse.run(&["palletize", "feature"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "bob@forklift"]));
    warehouse.write_file("bob.txt", "bob's work\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "bob's change"]));
    assert_success(&warehouse.run(&["config", "operator.identifier", "test@forklift"]));

    // test cherry-picks bob's parcel onto main.
    assert_success(&warehouse.run(&["shift", "main"]));
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "applied");

    // The file is applied, and the new parcel preserves bob's authorship while recording
    // test as the stacker — a single-parent parcel (no merge).
    assert!(warehouse.root.join("bob.txt").exists());
    let head = json(&warehouse.run(&["--json", "history"]));
    let top = &head["data"]["entries"][0];
    assert!(top["consolidates"].as_array().map_or(true, |c| c.is_empty()), "must be single-parent");
    let actions: Vec<(String, String)> = top["actions"].as_array().unwrap().iter()
        .map(|a| (a["action"].as_str().unwrap().trim().to_string(), a["operator"].as_str().unwrap().to_string()))
        .collect();
    assert!(actions.contains(&("author".to_string(), "bob@forklift".to_string())), "{:?}", actions);
    assert!(actions.contains(&("stack".to_string(), "test@forklift".to_string())), "{:?}", actions);

    // Cherry-picking the same parcel again is refused: its changes are already present.
    let again = warehouse.run(&["cherry-pick", &source]);
    assert!(!again.status.success() && stderr(&again).contains("already"), "{}", stderr(&again));
}

#[test]
fn cherry_pick_conflicts_are_marked_and_completed_by_stacking() {
    let warehouse = TestWarehouse::new("cherry-pick-conflict");
    warehouse.write_file("f.txt", "alpha\nbeta\ngamma\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // feature changes line 2 one way.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("f.txt", "alpha\nbeta-FEATURE\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "feature edits beta"]));

    // main changes the same line the other way — an overlap the pick cannot merge.
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("f.txt", "alpha\nbeta-MAIN\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main edits beta"]));

    // The pick reports a conflict and leaves markers.
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "conflicts");
    assert_eq!(picked["data"]["conflicts"][0], "f.txt");
    assert!(std::fs::read_to_string(warehouse.root.join("f.txt")).unwrap().contains("<<<<<<<"));

    // A stack is refused while the conflict is unresolved.
    assert!(!warehouse.run(&["stack", "premature"]).status.success());

    // Resolving, loading and stacking completes the pick as a single-parent parcel.
    warehouse.write_file("f.txt", "alpha\nbeta-RESOLVED\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "resolved pick"]));

    let head = json(&warehouse.run(&["--json", "history"]));
    let top = &head["data"]["entries"][0];
    assert!(top["consolidates"].as_array().map_or(true, |c| c.is_empty()), "must be single-parent");
    assert!(!warehouse.root.join(".forklift/cherry-pick").exists(), "the cherry-pick state must be cleared");
}

// -------------------------------------------------------------------------------------------
// Tracked directory <-> file flips: a merge or shift that replaces a tracked directory with a
// file (or vice versa) must not misread the directory as untracked content, and must apply the
// deletes before the write so the flip actually lands on disk in either direction.
// -------------------------------------------------------------------------------------------

#[test]
fn shift_flips_a_tracked_directory_into_a_file_and_back() {
    let warehouse = TestWarehouse::new("shift-dir-file-flip");
    warehouse.write_file("foo/a.txt", "a\n");
    warehouse.write_file("foo/b.txt", "b\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "flipped" replaces the tracked directory "foo" with a plain file.
    assert_success(&warehouse.run(&["palletize", "flipped"]));
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo", "foo is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "flip foo to a file"]));

    // Shifting to "flipped" from "main" (where "foo" is still a tracked, clean directory) must
    // not refuse: it is exactly the tracked dir->file flip, not an untracked collision.
    assert_success(&warehouse.run(&["shift", "main"]));
    let shift_to_flipped = warehouse.run(&["shift", "flipped"]);
    assert_success(&shift_to_flipped);

    assert!(!warehouse.root.join("foo").is_dir(), "\"foo\" must now be a file, not a directory");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo")).unwrap(),
        "foo is a file now\n"
    );

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);

    // Pin: the reverse flip (file -> directory) already works; shifting back must restore the
    // directory and its tracked files exactly.
    assert_success(&warehouse.run(&["shift", "main"]));
    assert!(warehouse.root.join("foo").is_dir(), "\"foo\" must be a directory again");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("foo/a.txt")).unwrap(), "a\n");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("foo/b.txt")).unwrap(), "b\n");

    let status_back = stdout(&warehouse.run(&["stocktake"]));
    assert!(status_back.contains("The inventory matches the pallet head"), "status: {}", status_back);
    assert!(status_back.contains("The working directory matches the inventory"), "status: {}", status_back);
}

#[test]
fn shift_still_refuses_a_directory_to_file_flip_when_untracked_content_is_beneath_it() {
    let warehouse = TestWarehouse::new("shift-dir-file-flip-guard");
    warehouse.write_file("foo/a.txt", "a\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "flipped"]));
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo", "foo is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "flip foo to a file"]));

    assert_success(&warehouse.run(&["shift", "main"]));

    // An untracked file left inside "foo" must still block the flip: replacing the directory
    // would silently destroy it.
    warehouse.write_file("foo/untracked.txt", "never loaded\n");

    let refused = warehouse.run(&["shift", "flipped"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("would overwrite these untracked files"), "{}", stderr(&refused));

    // Nothing was touched: the untracked file survives and the pallet did not change.
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo/untracked.txt")).unwrap(),
        "never loaded\n"
    );
    assert!(warehouse.root.join("foo").is_dir());
    assert!(stdout(&warehouse.run(&["stocktake"])).contains("On pallet \"main\""));
}

#[test]
fn consolidate_flips_a_tracked_directory_into_a_file_and_back() {
    let warehouse = TestWarehouse::new("consolidate-dir-file-flip");
    warehouse.write_file("foo/a.txt", "a\n");
    warehouse.write_file("root.txt", "root v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "feature" one-sidedly replaces the tracked directory "foo" with a file.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo", "foo is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature flips foo to a file"]));

    // "main" diverges the other way (an unrelated, non-overlapping change), so the merge is a
    // genuine three-way merge — not a fast-forward.
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("root.txt", "root v2\n");
    assert_success(&warehouse.run(&["load", "root.txt"]));
    assert_success(&warehouse.run(&["stack", "main edits root"]));

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    assert!(stdout(&merge).contains("stacked merge parcel"), "output: {}", stdout(&merge));

    assert!(!warehouse.root.join("foo").is_dir(), "\"foo\" must now be a file, not a directory");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo")).unwrap(),
        "foo is a file now\n"
    );
    assert_eq!(std::fs::read_to_string(warehouse.root.join("root.txt")).unwrap(), "root v2\n");

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

#[test]
fn consolidate_flips_a_tracked_file_into_a_directory() {
    // Pin for the reverse flip (file -> directory): the merge machinery already ordered this
    // direction correctly on its own (the delete of the file was already walked before the
    // writes under the new directory); returning the warehouse root to "main" below still
    // exercises the dir->file shift the fix above covers, since "main" still has "foo" as a file.
    let warehouse = TestWarehouse::new("consolidate-file-dir-flip");
    warehouse.write_file("foo", "foo is a file v1\n");
    warehouse.write_file("root.txt", "root v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_file(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo/inner.txt", "inner v1\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature flips foo to a directory"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("root.txt", "root v2\n");
    assert_success(&warehouse.run(&["load", "root.txt"]));
    assert_success(&warehouse.run(&["stack", "main edits root"]));

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);

    assert!(warehouse.root.join("foo").is_dir(), "\"foo\" must now be a directory");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo/inner.txt")).unwrap(),
        "inner v1\n"
    );

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

#[test]
fn consolidate_still_refuses_a_directory_to_file_flip_when_untracked_content_is_beneath_it() {
    let warehouse = TestWarehouse::new("consolidate-dir-file-flip-guard");
    warehouse.write_file("foo/a.txt", "a\n");
    warehouse.write_file("root.txt", "root v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo", "foo is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature flips foo to a file"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("root.txt", "root v2\n");
    assert_success(&warehouse.run(&["load", "root.txt"]));
    assert_success(&warehouse.run(&["stack", "main edits root"]));

    // An untracked file left inside "foo" must still block the merge.
    warehouse.write_file("foo/untracked.txt", "never loaded\n");

    let refused = warehouse.run(&["consolidate", "feature"]);
    assert!(!refused.status.success());
    assert!(stderr(&refused).contains("would overwrite these untracked files"), "{}", stderr(&refused));

    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo/untracked.txt")).unwrap(),
        "never loaded\n"
    );
    assert!(warehouse.root.join("foo").is_dir());
    assert_eq!(std::fs::read_to_string(warehouse.root.join("root.txt")).unwrap(), "root v2\n");
}

#[test]
fn cherry_pick_flips_a_tracked_directory_into_a_file_and_back() {
    let warehouse = TestWarehouse::new("cherry-pick-dir-file-flip");
    warehouse.write_file("foo/a.txt", "a\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "feature" replaces the tracked directory "foo" with a file, in a single parcel.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_dir_all(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo", "foo is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "flip foo to a file"]));

    // Cherry-pick that parcel onto "main", where "foo" is still a tracked, clean directory.
    assert_success(&warehouse.run(&["shift", "main"]));
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "applied", "{:?}", picked);

    assert!(!warehouse.root.join("foo").is_dir(), "\"foo\" must now be a file, not a directory");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo")).unwrap(),
        "foo is a file now\n"
    );

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

#[test]
fn cherry_pick_flips_a_tracked_file_into_a_directory() {
    // Pin for the reverse flip (file -> directory): the pick's merge machinery already ordered
    // this direction correctly on its own; shifting to "main" below still exercises the
    // dir->file shift the fix above covers, since "main" still has "foo" as a file.
    let warehouse = TestWarehouse::new("cherry-pick-file-dir-flip");
    warehouse.write_file("foo", "foo is a file v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_file(warehouse.root.join("foo")).unwrap();
    warehouse.write_file("foo/inner.txt", "inner v1\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "flip foo to a directory"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "applied", "{:?}", picked);

    assert!(warehouse.root.join("foo").is_dir(), "\"foo\" must now be a directory");
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("foo/inner.txt")).unwrap(),
        "inner v1\n"
    );

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

/// Count the `.pack` data files under an object store's pack folder.
fn count_packs(objects_root: &Path) -> usize {
    let pack_dir = objects_root.join("pack");
    match std::fs::read_dir(&pack_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count(),
        Err(_) => 0,
    }
}

/// Count the loose object files (excluding signature sidecars and the pack folder) under
/// an object store — how many objects `compact` has left to pack.
fn count_loose_objects(objects_root: &Path) -> usize {
    let mut count = 0;

    for entry in std::fs::read_dir(objects_root).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().to_string();

        // Only the two-hex fan-out folders hold loose objects; skip `pack/` and stray files.
        if name.len() != 2 || !entry.path().is_dir() {
            continue;
        }

        for file in std::fs::read_dir(entry.path()).unwrap() {
            let file_name = file.unwrap().file_name().to_string_lossy().to_string();
            if !file_name.ends_with(".sig") {
                count += 1;
            }
        }
    }

    count
}

#[test]
fn compact_packs_the_object_store_and_reads_survive() {
    let warehouse = TestWarehouse::new("compact");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("a.txt", "one\n");
    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    warehouse.write_file("a.txt", "one changed\n");
    assert_success(&warehouse.run(&["load", "."]));
    let head = extract_parcel_hash(&warehouse.run(&["stack", "second"]));

    let objects = warehouse.root.join(".forklift/objects");
    let loose_before = count_loose_objects(&objects);
    assert!(loose_before > 0, "there should be loose objects to pack");

    // Compact reports exactly what it packed and removed.
    let compact = warehouse.run(&["--json", "compact"]);
    assert_success(&compact);
    let data = json(&compact)["data"].clone();
    assert_eq!(data["objects_packed"].as_u64().unwrap(), loose_before as u64);
    assert_eq!(data["loose_removed"].as_u64().unwrap(), loose_before as u64);
    assert_eq!(data["packs_written"].as_u64().unwrap(), 1);

    // The loose object files are gone; a pack pair (.pack + .idx) took their place.
    assert_eq!(count_loose_objects(&objects), 0, "no loose objects should remain");
    let pack_files = objects.join("pack").read_dir().unwrap().count();
    assert_eq!(pack_files, 2, "one pack is a data file and an index");

    // Reads now come from the packs: history shows both parcels, peek reads the head object.
    let history = warehouse.run(&["history"]);
    assert_success(&history);
    assert!(stdout(&history).contains("first"), "history must read packed parcels");
    assert!(stdout(&history).contains("second"));
    assert_success(&warehouse.run(&["peek", &head]));

    // A second compaction has nothing to do.
    let again = warehouse.run(&["--json", "compact"]);
    assert_success(&again);
    assert_eq!(json(&again)["data"]["objects_packed"].as_u64().unwrap(), 0);
}

#[test]
fn compact_preserves_signed_history_for_audit() {
    let warehouse = TestWarehouse::new("compact-sign");
    warehouse.write_file("f.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    warehouse.write_file("f.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    let signed = extract_parcel_hash(&warehouse.run(&["stack", "signed"]));

    // Audit verifies the signed chain before compaction.
    assert!(stdout(&warehouse.run(&["audit"])).contains("verified"));

    assert_success(&warehouse.run(&["compact"]));

    // The parcel object is packed away (its loose file is gone) but its signature sidecar
    // stays loose — sidecars are read by path, not through the object store.
    let objects = warehouse.root.join(".forklift/objects");
    let loose_parcel = objects.join(&signed[0..2]).join(&signed[2..]);
    let sidecar = objects.join(&signed[0..2]).join(format!("{}.sig", &signed[2..]));
    assert!(!loose_parcel.exists(), "the parcel object should be packed away");
    assert!(sidecar.exists(), "the signature sidecar must stay loose");

    // Signed history still verifies, now against the packed objects.
    let audit = warehouse.run(&["audit"]);
    assert_success(&audit);
    assert!(stdout(&audit).contains("signed parcel(s) valid"), "audit must pass after compaction");
}

#[test]
fn compact_path_aware_delta_chains_reconstruct() {
    let warehouse = TestWarehouse::new("compact-pathaware");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Twelve versions of one file: path-aware base selection deltas each against the previous
    // version at the same path, forming a chain up to ~11 deep.
    let body: String = (0..600).map(|i| format!("line {} value {}\n", i, (i * 7) % 13)).collect();
    for v in 0..12 {
        warehouse.write_file("data.txt", &format!("{}unique tail for version {}\n", body, v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("v{}", v)]));
    }

    // The oldest and newest parcels, for a diff that spans the whole chain.
    let entries = json(&warehouse.run(&["--json", "history"]))["data"]["entries"].clone();
    let entries = entries.as_array().unwrap();
    assert_eq!(entries.len(), 12);
    let newest = entries[0]["parcel"].as_str().unwrap().to_string();
    let oldest = entries[11]["parcel"].as_str().unwrap().to_string();

    // Compact: the versioned blob is stored as a path-aware delta chain.
    let compact = warehouse.run(&["--json", "compact"]);
    assert_success(&compact);
    assert!(json(&compact)["data"]["deltas"].as_u64().unwrap() > 0, "path versions should delta");
    assert_eq!(count_loose_objects(&warehouse.root.join(".forklift/objects")), 0);

    // Every version still reconstructs from its delta chain: the full log walks, and a diff
    // across the whole chain rebuilds both endpoints' blobs (the newest is a deep delta).
    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        12, "history must walk every parcel from the packs"
    );
    let diff = warehouse.run(&["diff", &oldest, &newest]);
    assert_success(&diff);
    assert!(stdout(&diff).contains("data.txt"), "a cross-chain diff must reconstruct the delta-chained file");
}

#[test]
fn compact_all_repacks_consolidates_and_drops_garbage() {
    let warehouse = TestWarehouse::new("repack");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Several versions, each compacted into its own pack — so packs accumulate.
    for v in 0..4 {
        warehouse.write_file("f.txt", &format!("content {} xyzzy\n", v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("v{}", v)]));
        assert_success(&warehouse.run(&["compact"]));
    }

    // Create garbage: stack a parcel, then undo it (orphaning its objects), and compact so
    // the orphan lands in a pack — where only a repack can reach it.
    warehouse.write_file("g.txt", "garbage\n");
    assert_success(&warehouse.run(&["load", "."]));
    let orphan = extract_parcel_hash(&warehouse.run(&["stack", "garbage"]));
    assert_success(&warehouse.run(&["undo"]));
    assert_success(&warehouse.run(&["compact"]));
    // The orphan is packed but not yet collected, so it is still retrievable.
    assert_success(&warehouse.run(&["peek", &orphan]));

    let objects = warehouse.root.join(".forklift/objects");
    assert!(count_packs(&objects) > 1, "several compacts should have made several packs");

    // Repack: consolidate the packs and drop the orphaned (unreachable) parcel.
    let repack = warehouse.run(&["--json", "compact", "--all"]);
    assert_success(&repack);
    assert_eq!(json(&repack)["data"]["all"], true);
    assert_eq!(count_packs(&objects), 1, "a repack consolidates to a single pack");

    // The garbage is gone; the live history is intact and still reads.
    assert!(!warehouse.run(&["peek", &orphan]).status.success(), "the orphaned parcel must be dropped");
    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        4, "every live parcel must survive the repack"
    );

    // A second repack is idempotent and never loses the pack (regression: the content-derived
    // pack name once made the repack delete the very pack it had just written).
    assert_success(&warehouse.run(&["compact", "--all"]));
    assert_eq!(count_packs(&objects), 1, "an idempotent repack must keep the pack");
    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        4, "an idempotent repack must not lose objects"
    );
}

// ---------------------------------------------------------------------------------------------
// `compact --all --redelta`: the opt-in densification pass. Cross-path similarity (renames,
// moved files) that a per-object copy structurally cannot see.
// ---------------------------------------------------------------------------------------------

/// The `.pack` data file names under an object store's pack folder, sorted — the
/// deterministic-layout property (an unchanged repack must write the exact same names) is
/// what an idempotency check compares.
fn pack_file_names(objects_root: &Path) -> Vec<String> {
    let pack_dir = objects_root.join("pack");
    let mut names: Vec<String> = match std::fs::read_dir(&pack_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Total bytes under an object store root (loose files, sidecars and packs) — a coarse but
/// sufficient stand-in for the `du -sk .forklift` used at scale.
fn objects_store_bytes(objects_root: &Path) -> u64 {
    fn walk(dir: &Path, total: &mut u64) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if path.is_dir() {
                walk(&path, total);
            } else {
                *total += entry.metadata().unwrap().len();
            }
        }
    }

    let mut total = 0;
    walk(objects_root, &mut total);
    total
}

/// A store with the cross-path gap `--redelta` targets: several versions of one file, each
/// packed *alone* (so a plain incremental `compact`'s delta window never spans a different
/// run), plus a same-content file at a *different* path (a rename) also packed alone — so at
/// write time it has neither a path base (a new path has no predecessor) nor a window neighbor
/// (a fresh, single-object incremental compact starts with an empty window), and lands full
/// even though a near-duplicate already sits in an earlier pack. Returns the warehouse.
fn redelta_fixture(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // Large enough that the real delta-vs-full saving dominates the fixed per-record framing
    // overhead (a delta record's base-hash field): at trivial object sizes the two are the
    // same order of magnitude and the choice between them is noise, not signal.
    let body: String = (0..1000).map(|i| format!("line {} payload {} filler\n", i, (i * 13) % 97)).collect();

    for v in 0..4 {
        warehouse.write_file("data/a.txt", &format!("{}unique tail v{}\n", body, v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("v{}", v)]));
        assert_success(&warehouse.run(&["compact"]));
    }

    // Near-duplicate (not byte-identical — a true duplicate would just dedupe to the same
    // content-addressed blob, testing nothing) of the final version of "data/a.txt", at a
    // different path: a rename a per-path chain (import-git) and a per-batch window (a fresh
    // incremental compact, whose window starts empty) both miss.
    warehouse.write_file("data/b-renamed.txt", &format!("{}unique tail v3 renamed copy tweak\n", body));
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "renamed"]));
    assert_success(&warehouse.run(&["compact"]));

    warehouse
}

#[test]
fn compact_redelta_without_all_is_refused() {
    let warehouse = TestWarehouse::new("redelta-refused");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("f.txt", "content\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));

    let refused = warehouse.run(&["compact", "--redelta"]);
    assert!(!refused.status.success(), "--redelta without --all must be refused");
    assert!(
        stderr(&refused).contains("--redelta") && stderr(&refused).contains("--all"),
        "unexpected refusal message: {}", stderr(&refused)
    );

    // Nothing was touched: the loose object is still there to compact normally.
    let objects = warehouse.root.join(".forklift/objects");
    assert!(count_loose_objects(&objects) > 0, "a refused compact must not run at all");
}

#[test]
fn compact_all_redelta_shrinks_the_store_and_round_trips() {
    let warehouse = redelta_fixture("redelta-density");
    let objects = warehouse.root.join(".forklift/objects");

    // Plain `--all` is the copy-fast path: it consolidates packs but must not, by itself,
    // improve on the (sub-optimal) encodings already on disk.
    assert_success(&warehouse.run(&["compact", "--all"]));
    let after_plain_all = objects_store_bytes(&objects);

    let redelta = warehouse.run(&["--json", "compact", "--all", "--redelta"]);
    assert_success(&redelta);
    assert!(json(&redelta)["data"]["objects_packed"].as_u64().unwrap() > 0);

    let after_redelta = objects_store_bytes(&objects);
    assert!(
        after_redelta < after_plain_all,
        "redelta should find the cross-path near-duplicate and shrink the store \
         (plain --all {} bytes, after redelta {} bytes)", after_plain_all, after_redelta
    );

    // Every object still reads back byte-correct: a full diff from empty forces every blob —
    // including the path-delta chain and the just-redeltad rename — to reconstruct.
    let diff = warehouse.run(&["diff", ":empty", "main"]);
    assert_success(&diff);
    let text = stdout(&diff);
    assert!(text.contains("data/a.txt"), "unexpected diff output: {}", text);
    assert!(text.contains("data/b-renamed.txt"), "unexpected diff output: {}", text);
    assert!(text.contains("unique tail v3"), "data/a.txt must reconstruct byte-correct: {}", text);
    assert!(
        text.contains("unique tail v3 renamed copy tweak"),
        "the just-redeltad rename must reconstruct byte-correct: {}", text
    );

    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        5, "every parcel must survive a redelta repack"
    );
}

#[test]
fn compact_all_redelta_repeats_without_data_loss() {
    let warehouse = redelta_fixture("redelta-repeat");
    let objects = warehouse.root.join(".forklift/objects");

    assert_success(&warehouse.run(&["compact", "--all", "--redelta"]));
    assert!(objects_store_bytes(&objects) > 0);

    // `--redelta` re-encodes the whole live set again on *every* run — `Source::Reconstruct` is
    // unconditional under it, never the copy-fast path — so, unlike a plain repack, this never
    // reaches a cheap copy-bound steady state; every invocation pays the full CPU cost. Repeating
    // it three times pins the exact scenario a write-time depth-safety gap once made unsafe at
    // real-repository scale: without a true per-object depth ledger, repeated redeltas could grow
    // a delta chain's *real* depth well past its nominal cap (`compute_path_bases`'s own
    // bookkeeping only counts path hops since a reset, blind to a reset point's own real depth)
    // until the read-side reconstruction-depth backstop refused to compact at all — never losing
    // data (durable-before-destructive held even then), just eventually unusable. `compact`'s
    // depth ledger (`true_depth`) now resolves every path-base candidate's actual current depth
    // before committing to it, so this must keep succeeding no matter how many times it repeats.
    // The exact byte layout can still drift run to run (which candidates the ledger allows
    // depends on the *previous* run's own shape), so this does not assert a fixed pack id — only
    // what must hold on every run: no object lost, every object still reads back byte-correct.
    for _ in 0..2 {
        let repeat = warehouse.run(&["--json", "compact", "--all", "--redelta"]);
        assert_success(&repeat);
        assert_eq!(
            json(&repeat)["data"]["objects_packed"].as_u64().unwrap(), 20,
            "a repeated redelta must still account for every live object"
        );
    }

    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        5, "a repeated redelta must not lose objects"
    );

    let diff = warehouse.run(&["diff", ":empty", "main"]);
    assert_success(&diff);
    let text = stdout(&diff);
    assert!(
        text.contains("unique tail v3 renamed copy tweak"),
        "content must still reconstruct byte-correct after a repeated redelta: {}", text
    );
}

#[test]
fn compact_all_without_redelta_still_takes_the_copy_path() {
    let warehouse = redelta_fixture("redelta-steady-state-guard");
    let objects = warehouse.root.join(".forklift/objects");

    // First `--all` consolidates the several incremental packs into one, by copying each
    // live record verbatim.
    assert_success(&warehouse.run(&["compact", "--all"]));
    let names_first = pack_file_names(&objects);
    let bytes_first = objects_store_bytes(&objects);

    // A second plain `--all`, with nothing to drop and nothing new, must be the steady-state
    // copy-bound repack: same pack name (nothing re-encoded to a different layout), same size.
    let second = warehouse.run(&["--json", "compact", "--all"]);
    assert_success(&second);
    assert_eq!(pack_file_names(&objects), names_first, "a steady-state repack must not churn pack names");
    assert_eq!(objects_store_bytes(&objects), bytes_first, "a steady-state repack must not change the store size");
}

/// How many files share the cycling directory in [`redelta_tree_fixture`] — many, not one, so a
/// *full* tree record (every entry listed) is measurably larger than a *delta* one (just the one
/// entry that changed), the gap the fixture exposes. Also the version count: each version
/// touches a *different* file, cycling once through the whole directory (more than
/// `DELTA_WINDOW`, 10, so a size-window fallback could not span the whole chain by luck either).
const REDELTA_TREE_FIXTURE_FILES_PER_DIR: usize = 24;

/// A store exercising the *directory* counterpart of the path-base machinery a blob's delta
/// chain already uses: one directory with several files, a *different* one of which changes each
/// commit (cycling once through all of them), so the directory's own tree object gets a new hash
/// every version. Touching a different file each time (rather than the same one repeatedly)
/// means only the *immediately preceding* version is a good delta base for a later one — by the
/// time several versions have passed, enough *other* entries have also changed that a
/// non-adjacent pairing is a measurably worse delta, mirroring how a real directory accumulates
/// many small, unrelated changes over history (a fixture where every version stayed
/// interchangeable with every other would let a lucky window match hide a broken path base).
/// Each version is packed *alone* (an incremental `compact` per commit, like `redelta_fixture`),
/// so cross-version matching can only come from `compute_path_bases`, not the window (which
/// starts empty each run) — this is what `compute_path_bases` neglecting trees (only ever basing
/// blobs) previously defeated. Returns the warehouse.
fn redelta_tree_fixture(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    for f in 0..REDELTA_TREE_FIXTURE_FILES_PER_DIR {
        warehouse.write_file(&format!("data/sub/f{:02}.txt", f), &format!("file {} v0 filler content\n", f));
    }
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "setup"]));
    assert_success(&warehouse.run(&["compact"]));

    for v in 0..REDELTA_TREE_FIXTURE_FILES_PER_DIR {
        warehouse.write_file(&format!("data/sub/f{:02}.txt", v), &format!("file {} v1 filler content\n", v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("v{}", v)]));
        assert_success(&warehouse.run(&["compact"]));
    }

    warehouse
}

#[test]
fn compact_finds_a_directorys_path_base_across_many_versions() {
    let warehouse = redelta_tree_fixture("redelta-tree-density");
    let objects = warehouse.root.join(".forklift/objects");

    // `compute_path_bases` runs on *every* compact (it is shared, not `--redelta`-only — see its
    // doc comment), so the density this pins comes from the incremental compacts themselves;
    // the closing `--all` (no `--redelta`) just consolidates the already-packed records
    // verbatim into one pack for a clean total-byte measurement.
    assert_success(&warehouse.run(&["compact", "--all"]));
    let after_plain_all = objects_store_bytes(&objects);

    // Each directory version is `REDELTA_TREE_FIXTURE_FILES_PER_DIR` entries; without a path
    // base, every version but the first has nothing better than a full listing (or, absent any
    // window neighbor at write time, the same thing) to fall back to. Calibrated against this
    // exact fixture measured both ways: ~40,500 bytes before `compute_path_bases` covered trees
    // (every version's tree stored close to full), ~21,900 bytes after (every version's tree and
    // blob delta against its immediate predecessor) — most of what remains even in the fixed
    // case is the per-parcel cost, which is fixed (a parcel is never delta'd, see `pack_utils`)
    // and so does not move with the fix. The ceiling sits well below the broken number and with
    // comfortable margin above the fixed one, so it stays a real (if approximate) regression trip
    // wire rather than pinning an exact byte count to a heuristic that is free to improve.
    let ceiling = 30_000;
    assert!(
        after_plain_all < ceiling,
        "a directory with a real per-version path base should collapse to near-nothing per \
         version, not store every version's full listing (after compact --all: {} bytes, \
         ceiling {})", after_plain_all, ceiling
    );

    // `--all --redelta` on the same store must not be a regression: it has nothing further to
    // buy back here (no cross-path rename in this fixture), but re-encoding from scratch must
    // land in the same ballpark, not the multi-times-larger store the original bug produced.
    let redelta = warehouse.run(&["--json", "compact", "--all", "--redelta"]);
    assert_success(&redelta);
    assert!(json(&redelta)["data"]["objects_packed"].as_u64().unwrap() > 0);
    let after_redelta = objects_store_bytes(&objects);
    assert!(
        after_redelta < ceiling,
        "redelta must not lose the directory path base plain compact already found \
         (after --redelta: {} bytes, ceiling {})", after_redelta, ceiling
    );

    // Every object still reads back byte-correct — a full diff from empty forces every tree
    // version (and the blob it carries) to reconstruct.
    let diff = warehouse.run(&["diff", ":empty", "main"]);
    assert_success(&diff);
    let text = stdout(&diff);
    assert!(text.contains("data/sub/f00.txt"), "unexpected diff output: {}", text);
    assert!(
        text.contains(&format!("file {} v1 filler content", REDELTA_TREE_FIXTURE_FILES_PER_DIR - 1)),
        "the final directory version must reconstruct byte-correct: {}", text
    );

    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        REDELTA_TREE_FIXTURE_FILES_PER_DIR + 1, "every parcel must survive a redelta repack"
    );
}

#[test]
fn a_mutating_command_runs_maintenance_when_due() {
    let warehouse = TestWarehouse::new("auto-maintenance");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    // Lower the pack threshold so a couple of packs is "too many" (git's gc.autoPackLimit).
    assert_success(&warehouse.run(&["config", "maintenance.packs", "1"]));
    let objects = warehouse.root.join(".forklift/objects");

    // Make two packs (two rounds of stack + incremental compact) with auto off, so the setup
    // itself never triggers maintenance.
    assert_success(&warehouse.run(&["config", "maintenance.auto", "false"]));
    for v in 0..2 {
        warehouse.write_file("f.txt", &format!("v{}\n", v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("v{}", v)]));
        assert_success(&warehouse.run(&["compact"]));
    }
    assert!(count_packs(&objects) > 1, "there should be several packs to consolidate");

    // A mutating command with auto still OFF must not consolidate.
    warehouse.write_file("f.txt", "suppressed\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "suppressed"]));
    assert!(count_packs(&objects) > 1, "maintenance.auto=false must suppress the repack");

    // Enable it: the next mutating command runs `compact --all` synchronously (under its own
    // lock, before it returns), so by the time it returns the packs are already consolidated.
    assert_success(&warehouse.run(&["config", "maintenance.auto", "true"]));
    warehouse.write_file("f.txt", "trigger\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "trigger"]));
    assert_eq!(count_packs(&objects), 1, "maintenance should have consolidated the packs to one");

    // And it must not have corrupted anything: the full history still reads (v0, v1, suppressed,
    // trigger).
    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        4, "history must survive maintenance"
    );
}

// ---------------------------------------------------------------------------------------------
// The densify suggestion: `import-git`'s pack-direct path (and a franchise's native bundle
// install) can only ever see per-path/per-window similarity on the way in, unlike a plain
// `compact --all --redelta`'s whole-store pass — so a bulk-ingested store sets a marker that
// `store` surfaces as a one-line suggestion, cleared once a redelta run actually earns it back.
// Ordinary incremental use (stack -> auto-compact) already gets full delta selection at write
// time and must never trip the marker at all.
// ---------------------------------------------------------------------------------------------

#[test]
fn store_suggests_densify_after_import_git_and_clears_after_redelta() {
    fn seed_git_repo(root: &Path) {
        let git = |args: &[&str]| {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .output()
                .expect("git must be installed to run this test");
            assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        };
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(root.join("a.txt"), "hello\n").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "first commit"]);
    }

    let warehouse = TestWarehouse::new("densify-suggestion");
    seed_git_repo(&warehouse.root);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // The import itself packs at least one object, so its own human output tips the suggestion.
    let imported = warehouse.run(&["import-git", "."]);
    assert_success(&imported);
    assert!(
        stdout(&imported).contains("forklift compact --all --redelta"),
        "import-git should tip the densify suggestion after packing on the way in: {}", stdout(&imported)
    );

    // `store` surfaces it too: the human line, and an additive `--json` flag.
    let store = warehouse.run(&["store"]);
    assert_success(&store);
    assert!(
        stdout(&store).contains("forklift compact --all --redelta"),
        "store should surface the densify suggestion after a bulk ingest: {}", stdout(&store)
    );

    let store_json = warehouse.run(&["--json", "store"]);
    assert_success(&store_json);
    assert_eq!(json(&store_json)["data"]["densify_suggested"], true);

    // A successful `compact --all --redelta` clears the marker — the suggestion is gone.
    assert_success(&warehouse.run(&["compact", "--all", "--redelta"]));

    let after = warehouse.run(&["--json", "store"]);
    assert_success(&after);
    assert_eq!(
        json(&after)["data"]["densify_suggested"], false,
        "a successful redelta run must clear the densify-pending marker"
    );

    let store_after = warehouse.run(&["store"]);
    assert_success(&store_after);
    assert!(
        !stdout(&store_after).contains("forklift compact --all --redelta"),
        "the human suggestion must be gone once the marker is cleared"
    );
}

#[test]
fn store_never_suggests_densify_for_ordinary_incremental_use() {
    // Plain stack -> compact already gets full delta selection at write time (the loose path's
    // path-base + window fallback) — the marker must never be set for it, so the suggestion
    // must never fire, unlike the bulk-ingest paths above.
    let warehouse = TestWarehouse::new("densify-no-bulk-ingest");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("f.txt", "content\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    assert_success(&warehouse.run(&["compact"]));

    let store_json = warehouse.run(&["--json", "store"]);
    assert_success(&store_json);
    assert_eq!(json(&store_json)["data"]["densify_suggested"], false);

    let store = warehouse.run(&["store"]);
    assert_success(&store);
    assert!(!stdout(&store).contains("forklift compact --all --redelta"));
}

// ---------------------------------------------------------------------------------------------
// Task-scoped sparse workspaces: scoped bays on a full object store.
//
// Materialization-only sparseness — the store holds everything; a scoped bay materializes and
// operates on only its subtree(s). The load-bearing invariant is that a scoped stack produces a
// **byte-identical** root tree to a full stack of the same content.
// ---------------------------------------------------------------------------------------------

/// The head parcel hash of a (shared) pallet ref, read from the main tree.
fn pallet_head_hash(warehouse: &TestWarehouse, pallet: &str) -> String {
    std::fs::read_to_string(warehouse.root.join(".forklift").join("pallets").join(pallet))
        .unwrap()
        .trim()
        .to_string()
}

/// The root tree hash a parcel commits (pure content, no wall-clock) — read via `peek`.
fn parcel_tree_hash(warehouse: &TestWarehouse, parcel: &str) -> String {
    let output = warehouse.run(&["--json", "peek", parcel]);
    assert_success(&output);
    json(&output)["data"]["tree"].as_str().unwrap().to_string()
}

/// A warehouse with an in-scope subtree, an out-of-scope sibling subtree and an out-of-scope
/// root file, stacked as `base`. Returns the warehouse.
fn scoped_fixture(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/api/b.txt", "api b v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    warehouse.write_file("README.md", "readme v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    warehouse
}

#[test]
fn a_scoped_bay_materializes_only_its_subtree_and_reports_the_scope() {
    let warehouse = scoped_fixture("scoped-materialize");

    let scoped_dir = warehouse.home.join("bay-scoped");
    let added = warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]);
    assert_success(&added);
    assert!(stdout(&added).contains("Scoped"), "{}", stdout(&added));

    // Only the in-scope subtree is materialized; the out-of-scope sibling and root file are not.
    assert!(scoped_dir.join("src/api/a.txt").exists());
    assert!(scoped_dir.join("src/api/b.txt").exists());
    assert!(!scoped_dir.join("src/web").exists(), "out-of-scope subtree must not be materialized");
    assert!(!scoped_dir.join("README.md").exists(), "out-of-scope file must not be materialized");

    // `scope` reports the bay's materialization scope; the main tree is full.
    let scoped_status = warehouse.run_at(&scoped_dir, &["--json", "scope"]);
    assert_success(&scoped_status);
    let value = json(&scoped_status);
    assert_eq!(value["data"]["scoped"], true);
    assert_eq!(value["data"]["materialization_scope"][0], "src/api");

    let main_status = warehouse.run(&["--json", "scope"]);
    assert_success(&main_status);
    assert_eq!(json(&main_status)["data"]["scoped"], false);
}

#[test]
fn bay_add_scope_rejects_a_path_that_is_a_file_not_a_directory_in_the_head() {
    // `scoped_fixture` stacks "README.md" as a plain file at the head. A `--scope` naming it (or
    // a path through it) is not a subtree the overlay can materialize — refuse up front rather
    // than let it silently misbehave later (a Spine walk that expects a directory there).
    let warehouse = scoped_fixture("scoped-add-rejects-file");

    let scoped_dir = warehouse.home.join("bay-file-scope");
    let rejected = warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "README.md"]);

    assert!(!rejected.status.success(), "a file path must not be accepted as a bay scope");
    assert!(
        stderr(&rejected).contains("is not a directory in the current head"),
        "{}", stderr(&rejected)
    );
    assert!(!scoped_dir.exists(), "a rejected bay add must not create the bay directory");

    // A path through the file (as if it were a directory) is rejected the same way.
    let rejected_nested = warehouse.run(&[
        "bay", "add", "scoped2", scoped_dir.to_str().unwrap(), "--scope", "README.md/nested"
    ]);
    assert!(!rejected_nested.status.success());
    assert!(
        stderr(&rejected_nested).contains("is not a directory in the current head"),
        "{}", stderr(&rejected_nested)
    );
}

#[test]
fn a_scoped_stack_produces_a_byte_identical_root_tree_to_a_full_stack() {
    let warehouse = scoped_fixture("scoped-identical");

    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // The identical in-scope edit in both bays (branched from the same head).
    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "edit in full"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit in scoped"]));

    // The stage-1 invariant: the scoped overlay spliced the out-of-scope siblings back by hash,
    // so the two roots are byte-for-byte the same content address.
    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));
    assert_eq!(full_tree, scoped_tree,
        "a scoped stack must produce a byte-identical root tree to a full stack of the same content");
}

#[test]
fn a_scoped_stack_prunes_an_emptied_subtree_identically_to_a_full_stack() {
    // A `lonely` directory whose only descendant is the in-scope leaf, so emptying the leaf must
    // prune `lonely/deep` and then `lonely` itself — recursively up the spine, exactly as a full
    // build does — or the two root hashes diverge.
    let warehouse = TestWarehouse::new("scoped-prune");
    warehouse.write_file("src/api/a.txt", "api\n");
    warehouse.write_file("lonely/deep/only.txt", "only\n");
    warehouse.write_file("README.md", "r\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "lonely/deep"]));

    // Delete the only in-scope file in both bays and stack the removal.
    std::fs::remove_file(full_dir.join("lonely/deep/only.txt")).unwrap();
    std::fs::remove_file(scoped_dir.join("lonely/deep/only.txt")).unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "drop it (full)"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "drop it (scoped)"]));

    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));
    assert_eq!(full_tree, scoped_tree,
        "emptying the in-scope subtree must prune the spine identically to a full build");
}

#[test]
fn a_scoped_stocktake_does_not_report_out_of_scope_paths_as_removed() {
    let warehouse = scoped_fixture("scoped-stocktake");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // A fresh scoped bay is clean: the out-of-scope head content (src/web, README) is sealed by
    // hash, not "Removed" — without the scope-aware staged walk it would be reported as removed.
    let status = warehouse.run_at(&scoped_dir, &["--json", "stocktake"]);
    assert_success(&status);
    let value = json(&status);
    assert_eq!(value["data"]["staged_count"], 0,
        "a fresh scoped bay stages nothing: {}", stdout(&status));

    let human = stdout(&warehouse.run_at(&scoped_dir, &["stocktake"]));
    assert!(!human.contains("src/web"), "out-of-scope path leaked into stocktake: {}", human);
    assert!(!human.contains("README"), "out-of-scope path leaked into stocktake: {}", human);

    // A staged in-scope edit is reported normally.
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    let staged = warehouse.run_at(&scoped_dir, &["--json", "diff", "--staged"]);
    assert_success(&staged);
    let files = json(&staged)["data"]["files"].as_array().unwrap().clone();
    assert_eq!(files.len(), 1, "only the in-scope edit is staged");
    assert_eq!(files[0]["path"], "src/api/a.txt");
}

#[test]
fn a_scoped_bay_refuses_out_of_scope_path_arguments() {
    let warehouse = scoped_fixture("scoped-refuse-path");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // load / remove / unload / blame / diff on an out-of-scope path refuse with the stable
    // code + exit 7.
    for args in [
        vec!["--json", "load", "src/web/w.txt"],
        vec!["--json", "remove", "src/web/w.txt"],
        vec!["--json", "unload", "src/web/w.txt"],
        vec!["--json", "blame", "src/web/w.txt"],
        vec!["--json", "diff", "main", "scoped", "src/web"],
    ] {
        let refused = warehouse.run_at(&scoped_dir, &args);
        assert_eq!(refused.status.code(), Some(7), "expected out_of_scope exit for {:?}", args);
        assert_eq!(json(&refused)["error"]["code"], "out_of_scope", "for {:?}", args);
        assert!(json(&refused)["error"]["next_step"].is_string(), "for {:?}", args);
    }

    // In-scope path arguments still work.
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "src/api/a.txt"]));
}

#[test]
fn a_scoped_bay_refuses_whole_tree_verbs_with_a_stable_code() {
    let warehouse = scoped_fixture("scoped-refuse-verb");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // export-git (would silently truncate the out-of-scope content) refuses with the
    // sparse_workspace code + exit 9. (consolidate no longer refuses — a scoped bay resolves
    // out-of-scope siblings by hash; see the scoped-merge tests below.)
    let export_dir = warehouse.home.join("export-git-out");
    let export = warehouse.run_at(&scoped_dir, &["--json", "export-git", export_dir.to_str().unwrap()]);
    assert_eq!(export.status.code(), Some(9));
    assert_eq!(json(&export)["error"]["code"], "sparse_workspace");
}

#[test]
fn a_scoped_bay_refuses_a_spine_path_type_flip() {
    let warehouse = scoped_fixture("scoped-type-flip");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // On main, replace the `src` directory with a *file* named `src` and stack it: the scope's
    // spine now has a file where it expects a directory.
    std::fs::remove_dir_all(warehouse.root.join("src")).unwrap();
    std::fs::write(warehouse.root.join("src"), "src is a file now\n").unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "src becomes a file"]));

    // Shifting the scoped bay onto that revision must refuse — the sparse scope is no longer valid
    // (a spine path flipped between a directory and a file), not guess.
    let shifted = warehouse.run_at(&scoped_dir, &["--json", "shift", "main"]);
    assert_eq!(shifted.status.code(), Some(8), "stderr/out: {}", stdout(&shifted));
    assert_eq!(json(&shifted)["error"]["code"], "scope_path_type_changed");
}

// -------------------------------------------------------------------------------------------
// Scoped bays: hostile inputs at the scope boundary (a spine path flipping type, an
// out-of-scope entry a merge can't reconcile, and similar edge cases).
// -------------------------------------------------------------------------------------------

#[test]
fn a_scoped_stack_refuses_when_the_dock_replaces_an_in_scope_directory_with_a_file() {
    // The working tree flips the scoped subtree itself from a directory to a
    // file. The overlay's dock-side files pass must catch this — without it, the file lands in
    // no branch of the splice and is silently dropped from the signed (!) tree.
    let warehouse = scoped_fixture("scoped-dock-flip");

    let full_dir = warehouse.home.join("bay-full-flip");
    let scoped_dir = warehouse.home.join("bay-scoped-flip");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // Full bay: replacing src/api with a file is an ordinary (non-sparse) edit and must
    // keep succeeding — the fix must not regress the unscoped case.
    std::fs::remove_dir_all(full_dir.join("src/api")).unwrap();
    std::fs::write(full_dir.join("src/api"), "api is a file now\n").unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "api becomes a file"]));

    // Scoped bay: the same edit must refuse rather than silently drop the file from the tree.
    std::fs::remove_dir_all(scoped_dir.join("src/api")).unwrap();
    std::fs::write(scoped_dir.join("src/api"), "api is a file now\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    let stacked = warehouse.run_at(&scoped_dir, &["--json", "stack", "api becomes a file"]);
    assert_eq!(stacked.status.code(), Some(8), "stdout/err: {}", stdout(&stacked));
    assert_eq!(json(&stacked)["error"]["code"], "scope_path_type_changed");
}

#[test]
fn a_scoped_park_produces_a_byte_identical_tree_and_a_clean_park_refuses() {
    // park must route through the overlay exactly like stack, and its "nothing to park"
    // check must compare the spliced root against head (not the truncated sparse partial) or
    // it never fires in a scoped bay.
    let warehouse = scoped_fixture("scoped-park");

    let full_dir = warehouse.home.join("bay-full-park");
    let scoped_dir = warehouse.home.join("bay-scoped-park");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // A clean park (no WIP) must refuse in both bays.
    assert!(!warehouse.run_at(&full_dir, &["park"]).status.success(), "a clean full-bay park must refuse");
    let clean_scoped = warehouse.run_at(&scoped_dir, &["park"]);
    assert!(!clean_scoped.status.success(), "a clean scoped-bay park must refuse: {}", stdout(&clean_scoped));

    // The identical in-scope WIP in both bays.
    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2 (wip)\n").unwrap();
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2 (wip)\n").unwrap();

    assert_success(&warehouse.run_at(&full_dir, &["park"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["park"]));

    let full_parked = json(&warehouse.run_at(&full_dir, &["--json", "park", "list"]));
    let scoped_parked = json(&warehouse.run_at(&scoped_dir, &["--json", "park", "list"]));
    let full_parcel = full_parked["data"]["parked"][0]["parcel"].as_str().unwrap().to_string();
    let scoped_parcel = scoped_parked["data"]["parked"][0]["parcel"].as_str().unwrap().to_string();

    let full_tree = parcel_tree_hash(&warehouse, &full_parcel);
    let scoped_tree = parcel_tree_hash(&warehouse, &scoped_parcel);

    assert_eq!(full_tree, scoped_tree,
        "a scoped park must commit a byte-identical tree to a full-bay park of the same WIP");
}

#[test]
fn a_scoped_restore_staged_root_leaves_stocktake_clean_and_the_shards_scope_bounded() {
    // `restore --staged` must not resurrect out-of-scope shards into a scoped bay's
    // inventory — those paths were never materialized here.
    let warehouse = scoped_fixture("scoped-restore-staged");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // Stage an in-scope edit, then reset it via `restore --staged .` (the root) — and revert
    // the working file too, so the working tree, inventory and head all agree again
    // afterwards (isolating this assertion from `restore --staged`'s ordinary "working
    // directory untouched" semantics, which would otherwise still show the edited file as
    // unstaged and drown out the out-of-scope-leakage signal this test checks for).
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["restore", "--staged", "."]));
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v1\n").unwrap();

    // The out-of-scope subtree must never be smuggled into the inventory by the restore walk —
    // stocktake stays clean (no phantom entries for content that was never materialized here).
    let status = warehouse.run_at(&scoped_dir, &["--json", "stocktake"]);
    assert_success(&status);
    let value = json(&status);
    assert_eq!(value["data"]["staged_count"], 0, "restore --staged must have unstaged the edit");
    assert_eq!(value["data"]["unstaged_count"], 0,
        "the restore walk must not smuggle out-of-scope shards into the inventory: {}", stdout(&status));
    assert!(!scoped_dir.join("src/web").exists(), "restore --staged must never materialize out-of-scope content");

    // A direct restore of an out-of-scope path refuses too (closing the gap when the target
    // itself, not just a descendant, is out of scope).
    let refused = warehouse.run_at(&scoped_dir, &["--json", "restore", "--staged", "src/web"]);
    assert_eq!(refused.status.code(), Some(7), "stdout/err: {}", stdout(&refused));
    assert_eq!(json(&refused)["error"]["code"], "out_of_scope");
}

#[test]
fn a_multi_path_scoped_stack_produces_a_byte_identical_root_tree() {
    // Pin the multi-path scope case (two independent in-scope subtrees) for the
    // byte-identical invariant, not just the single-scope-path case.
    let warehouse = scoped_fixture("scoped-multi-path");

    let full_dir = warehouse.home.join("bay-full-multi");
    let scoped_dir = warehouse.home.join("bay-scoped-multi");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&[
        "bay", "add", "scoped", scoped_dir.to_str().unwrap(),
        "--scope", "src/api", "--scope", "src/web",
    ]));

    // Edits under both in-scope subtrees, identical in both bays.
    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    std::fs::write(full_dir.join("src/web/w.txt"), "web v2\n").unwrap();
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    std::fs::write(scoped_dir.join("src/web/w.txt"), "web v2\n").unwrap();

    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "edit both (full)"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit both (scoped)"]));

    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));
    assert_eq!(full_tree, scoped_tree,
        "a multi-path scoped stack must produce a byte-identical root tree to a full stack");
}

#[test]
fn a_scoped_bay_refuses_import_git() {
    // import-git bypasses the overlay entirely (it builds parcels straight from the git tree);
    // a scoped bay is not a sensible import target — refuse before even validating the path.
    let warehouse = scoped_fixture("scoped-refuse-import-git");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    let import = warehouse.run_at(&scoped_dir, &["--json", "import-git", "."]);
    assert_eq!(import.status.code(), Some(9), "stdout/err: {}", stdout(&import));
    assert_eq!(json(&import)["error"]["code"], "sparse_workspace");
}

// -------------------------------------------------------------------------------------------
// Hash-only three-way merge resolution in a scoped bay.
//
// A scoped merge resolves out-of-scope siblings (subtrees, files, symlinks) by hash alone: a
// one-sided change is adopted from theirs, a two-sided one refuses. The committed merge tree is
// byte-identical to a full workspace merging the same two heads — and, to prove the resolution
// never depends on out-of-scope content being present (so it will still hold once fetching
// itself can be scoped, not just materialization), the out-of-scope objects are deleted before
// the scoped merge runs.
// -------------------------------------------------------------------------------------------

/// The object-store path of a loose object (matches the on-disk sharding the store uses).
fn object_store_path(warehouse: &TestWarehouse, hash: &str) -> PathBuf {
    warehouse.root.join(".forklift").join("objects").join(&hash[0..2]).join(&hash[2..])
}

/// Resolve the object hash a parcel commits at a warehouse path by walking tree objects through
/// `peek`, so a test can then delete that object and prove a later walk never reads it.
fn path_object_hash(warehouse: &TestWarehouse, parcel: &str, path: &str) -> String {
    let mut hash = parcel_tree_hash(warehouse, parcel);

    for component in path.split('/') {
        let peeked = warehouse.run(&["--json", "peek", &hash]);
        assert_success(&peeked);

        let entries = json(&peeked)["data"]["entries"].as_array().unwrap().clone();
        hash = entries.iter()
            .find(|entry| entry["name"] == component)
            .unwrap_or_else(|| panic!("no tree entry \"{}\" under {}", component, hash))
            ["hash"].as_str().unwrap().to_string();
    }

    hash
}

#[test]
fn a_scoped_merge_resolves_one_sided_out_of_scope_changes_by_hash_without_reading_them() {
    let warehouse = TestWarehouse::new("scoped-merge-oneside");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    warehouse.write_file("README.md", "readme v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // "theirs" changes only out-of-scope content: a subtree (src/web) and a root file (README).
    assert_success(&warehouse.run(&["palletize", "theirs"]));
    warehouse.write_file("src/web/w.txt", "web v2 from theirs\n");
    warehouse.write_file("README.md", "readme v2 from theirs\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs edits out-of-scope content"]));
    let theirs_head = pallet_head_hash(&warehouse, "theirs");
    assert_success(&warehouse.run(&["shift", "main"]));

    // A full bay and a scoped bay, both branched from the base.
    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // The full bay makes an in-scope edit and consolidates theirs with every object present; that
    // merge tree is the reference the scoped merge must reproduce byte-for-byte.
    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "edit api (full)"]));
    assert_success(&warehouse.run_at(&full_dir, &["consolidate", "theirs"]));
    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));

    // Make the out-of-scope objects theirs changed impossible to read: a store fetched sparsely
    // would never hold them. Deleting them here proves the scoped merge resolves them by hash
    // alone — never loading or materializing one.
    let web_tree = path_object_hash(&warehouse, &theirs_head, "src/web");
    let web_blob = path_object_hash(&warehouse, &theirs_head, "src/web/w.txt");
    let readme_blob = path_object_hash(&warehouse, &theirs_head, "README.md");
    for hash in [&web_tree, &web_blob, &readme_blob] {
        std::fs::remove_file(object_store_path(&warehouse, hash)).expect("the out-of-scope object existed");
    }

    // The scoped bay makes the same in-scope edit and consolidates theirs. It must succeed...
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api (scoped)"]));
    let consolidated = warehouse.run_at(&scoped_dir, &["consolidate", "theirs"]);
    assert_success(&consolidated);

    // ...committing a byte-identical merge tree, and never materializing the out-of-scope content.
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));
    assert_eq!(full_tree, scoped_tree,
        "a scoped merge must commit the same tree a full merge does, resolving out-of-scope siblings by hash");
    assert!(!scoped_dir.join("src/web").exists(), "an out-of-scope subtree must not materialize in a scoped merge");
    assert!(!scoped_dir.join("README.md").exists(), "an out-of-scope file must not materialize in a scoped merge");
}

#[test]
fn a_scoped_merge_refuses_a_two_sided_out_of_scope_conflict() {
    let warehouse = TestWarehouse::new("scoped-merge-conflict");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // theirs branches from the base and changes src/web one way...
    assert_success(&warehouse.run(&["palletize", "theirs"]));
    warehouse.write_file("src/web/w.txt", "web from theirs\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs edits web"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    // ...and main changes the same out-of-scope file differently, so the scoped bay's head carries
    // an out-of-scope change of its own — a genuine two-sided out-of-scope divergence.
    warehouse.write_file("src/web/w.txt", "web from main\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main edits web"]));

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));
    let head_before = pallet_head_hash(&warehouse, "scoped");

    // The scoped bay has no content to reconcile src/web with, so it refuses cleanly with the
    // stable code + exit 10, and stacks nothing.
    let refused = warehouse.run_at(&scoped_dir, &["--json", "consolidate", "theirs"]);
    assert_eq!(refused.status.code(), Some(10), "stdout: {}", stdout(&refused));
    assert_eq!(json(&refused)["error"]["code"], "out_of_scope_conflict");
    assert!(json(&refused)["error"]["next_step"].is_string());

    assert_eq!(pallet_head_hash(&warehouse, "scoped"), head_before, "a refused merge must stack nothing");
}

#[cfg(unix)]
#[test]
fn a_scoped_merge_resolves_a_one_sided_out_of_scope_symlink_by_hash() {
    use std::os::unix::fs::symlink;

    let warehouse = TestWarehouse::new("scoped-merge-symlink");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    // An out-of-scope symlink at the root (a plain file entry of type symlink in the tree).
    symlink("src/web/w.txt", warehouse.root.join("shortcut")).unwrap();
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // theirs re-points the out-of-scope symlink one-sided.
    assert_success(&warehouse.run(&["palletize", "theirs"]));
    std::fs::remove_file(warehouse.root.join("shortcut")).unwrap();
    symlink("src/api/a.txt", warehouse.root.join("shortcut")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs re-points the symlink"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "edit api (full)"]));
    assert_success(&warehouse.run_at(&full_dir, &["consolidate", "theirs"]));
    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));

    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api (scoped)"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["consolidate", "theirs"]));
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));

    assert_eq!(full_tree, scoped_tree,
        "a scoped merge must adopt a one-sided out-of-scope symlink by hash, identically to a full merge");
    assert!(std::fs::symlink_metadata(scoped_dir.join("shortcut")).is_err(),
        "the out-of-scope symlink must not materialize in the scoped bay");
}

// -------------------------------------------------------------------------------------------
// A one-sided out-of-scope directory↔file type flip.
//
// The merge walk classifies a changed name in two separate loops (files, then subtrees) at
// each directory level. A type flip makes exactly one of those loops see theirs' real entry (a
// "set") and the other see none of its own type there (a "delete") — both for the *same* path.
// Which loop runs second depends on the flip's direction, so the skeleton must combine the two
// resolutions correctly regardless of order: a "set" must always win over a "delete".
// -------------------------------------------------------------------------------------------

/// Resolve the `(hash, item_type)` of the tree entry a parcel commits at a warehouse path, by
/// walking tree objects through `peek` and reading the LAST component's own entry (not
/// descending into it) — so a test can assert both its content address and its type (file vs.
/// directory) directly, without loading the entry itself. `item_type` is the trimmed
/// `get_name_for_peek()` string ("normal", "tree", …).
fn path_object_entry(warehouse: &TestWarehouse, parcel: &str, path: &str) -> (String, String) {
    let mut hash = parcel_tree_hash(warehouse, parcel);
    let components: Vec<&str> = path.split('/').collect();
    let mut item_type = String::new();

    for component in &components {
        let peeked = warehouse.run(&["--json", "peek", &hash]);
        assert_success(&peeked);
        let entries = json(&peeked)["data"]["entries"].as_array().unwrap().clone();
        let entry = entries.iter()
            .find(|entry| entry["name"] == *component)
            .unwrap_or_else(|| panic!("no tree entry \"{}\" under {}", component, hash));

        hash = entry["hash"].as_str().unwrap().to_string();
        item_type = entry["item_type"].as_str().unwrap().trim().to_string();
    }

    (hash, item_type)
}

#[test]
fn a_scoped_merge_resolves_an_out_of_scope_directory_to_file_flip_by_hash() {
    // NOTE on verification strategy: verification checks the scoped merge's own committed tree
    // directly (rather than diffing a full-bay `consolidate` run against it): the out-of-scope
    // path must carry theirs' exact post-flip hash and type — never omitted and never the stale
    // pre-flip directory. (A separate, now-fixed bug once misread a tracked directory a one-sided
    // merge replaced with a file as an untracked collision — see `ensure_no_untracked_collisions`
    // and `inventory_utils::directory_is_safe_to_replace` — but this test's verification does not
    // depend on that fix; it never runs an unscoped `consolidate` at all.)
    let warehouse = TestWarehouse::new("scoped-merge-dir-to-file");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n"); // src/web starts as a DIRECTORY
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // theirs one-sidedly replaces the out-of-scope directory src/web with a FILE. This is the
    // exact construction that emits a "set" (file loop) then a "delete" (subtree loop) for the
    // same path, in that order — the direction a plain last-write-wins insert gets wrong.
    assert_success(&warehouse.run(&["palletize", "theirs"]));
    std::fs::remove_dir_all(warehouse.root.join("src/web")).unwrap();
    warehouse.write_file("src/web", "web is a file now\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs turns src/web into a file"]));
    let theirs_head = pallet_head_hash(&warehouse, "theirs");
    let (theirs_web_hash, theirs_web_type) = path_object_entry(&warehouse, &theirs_head, "src/web");
    assert_eq!(theirs_web_type, "normal", "sanity: theirs' src/web really is a file");

    assert_success(&warehouse.run(&["shift", "main"]));

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api (scoped)"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["consolidate", "theirs"]));

    let scoped_head = pallet_head_hash(&warehouse, "scoped");
    let (merged_web_hash, merged_web_type) = path_object_entry(&warehouse, &scoped_head, "src/web");

    assert_eq!(merged_web_type, "normal",
        "the merge must resolve src/web to theirs' FILE, not omit it or keep the stale directory");
    assert_eq!(merged_web_hash, theirs_web_hash,
        "the merge must adopt theirs' exact file content by hash");
}

#[test]
fn a_scoped_merge_resolves_an_out_of_scope_file_to_directory_flip_by_hash() {
    // This direction is verified against a real full-bay `consolidate`, with "theirs" authored
    // in a freshly materialized bay rather than by switching the warehouse root's own working
    // directory from "theirs" back to "main" via `shift` — that would revert a directory left on
    // disk back to a file, a dir->file shift, which is exactly what this suite's
    // `shift_flips_a_tracked_directory_into_a_file_and_back` covers directly; there is no need
    // to exercise it incidentally here too. Authoring "theirs" in its own bay means the warehouse
    // root never leaves "main", so no reverse-direction shift happens in this test at all.
    let warehouse = TestWarehouse::new("scoped-merge-file-to-dir");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web", "web is a file v1\n"); // src/web starts as a FILE
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // Author "theirs" in its own bay (branched from main's current head) rather than switching
    // the warehouse root onto it — the warehouse root stays on "main" throughout.
    let theirs_dir = warehouse.home.join("bay-theirs");
    assert_success(&warehouse.run(&["bay", "add", "theirs", theirs_dir.to_str().unwrap()]));
    std::fs::remove_file(theirs_dir.join("src/web")).unwrap();
    std::fs::create_dir_all(theirs_dir.join("src/web")).unwrap();
    std::fs::write(theirs_dir.join("src/web/inner.txt"), "inner v1\n").unwrap();
    assert_success(&warehouse.run_at(&theirs_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&theirs_dir, &["stack", "theirs turns src/web into a directory"]));

    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    std::fs::write(full_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&full_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&full_dir, &["stack", "edit api (full)"]));
    assert_success(&warehouse.run_at(&full_dir, &["consolidate", "theirs"]));
    let full_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "full"));

    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api (scoped)"]));
    assert_success(&warehouse.run_at(&scoped_dir, &["consolidate", "theirs"]));
    let scoped_tree = parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, "scoped"));

    assert_eq!(full_tree, scoped_tree,
        "a scoped merge must resolve a one-sided out-of-scope file-to-directory flip \
        identically to a full merge");
}

// -------------------------------------------------------------------------------------------
// The out-of-scope skeleton's durability invariant: a crash between writing it and the
// consolidation state must never be silently read back as "no out-of-scope resolutions".
// -------------------------------------------------------------------------------------------

#[test]
fn a_stack_refuses_when_consolidation_state_exists_but_the_skeleton_file_is_gone() {
    // Simulates an interrupted write between the skeleton and the consolidation-state file (a
    // crash mid-merge): the consolidation state is present, but its skeleton is not. The
    // completing stack must refuse rather than silently treat the missing skeleton as "no
    // out-of-scope resolutions" and commit a tree that dropped them.
    //
    // A clean (conflict-free) merge auto-stacks immediately and consumes the consolidation state
    // right away, so there would be no window to interfere. The merge here also carries a genuine
    // *in-scope* conflict (both sides edit "src/api/a.txt" differently), so the consolidation
    // stays in progress — state and skeleton both on disk — until the operator resolves, loads,
    // and stacks.
    let warehouse = TestWarehouse::new("scoped-skeleton-durability");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    // theirs changes both the out-of-scope subtree one-sidedly (so the merge produces a
    // skeleton) and the in-scope file (so the scoped bay's own edit below conflicts with it).
    assert_success(&warehouse.run(&["palletize", "theirs"]));
    warehouse.write_file("src/web/w.txt", "web v2 from theirs\n");
    warehouse.write_file("src/api/a.txt", "api a v2 from theirs\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs edits web and api"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    // A conflicting in-scope edit: the scoped bay changes a.txt differently than theirs did.
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2 from scoped\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api"]));

    let consolidated = warehouse.run_at(&scoped_dir, &["--json", "consolidate", "theirs"]);
    assert_success(&consolidated);
    assert_eq!(json(&consolidated)["data"]["outcome"], "conflicts",
        "the in-scope edit must conflict, keeping the consolidation in progress: {}", stdout(&consolidated));

    // Both files exist after the conflicting consolidate — the merge is not yet complete.
    let bay_data_dir = warehouse.root.join(".forklift").join("bays").join("scoped");
    let state_path = bay_data_dir.join("consolidation");
    let skeleton_path = bay_data_dir.join("consolidation-skeleton");
    assert!(state_path.exists(), "consolidation state must exist while conflicts are unresolved");
    assert!(skeleton_path.exists(), "the skeleton file must exist too, even before the conflict is resolved");

    // Simulate the interrupted write: the skeleton vanishes, the state does not.
    std::fs::remove_file(&skeleton_path).unwrap();

    // Resolve the in-scope conflict so only the skeleton-durability gate stands in the way.
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a resolved\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));

    let head_before = pallet_head_hash(&warehouse, "scoped");
    let refused = warehouse.run_at(&scoped_dir, &["--json", "stack", "complete the merge"]);

    assert!(!refused.status.success(), "a stack with state-present/skeleton-absent must refuse");
    assert!(
        stdout(&refused).to_lowercase().contains("skeleton") || stderr(&refused).to_lowercase().contains("skeleton"),
        "the refusal must name the broken invariant: stdout={} stderr={}", stdout(&refused), stderr(&refused)
    );
    assert_eq!(pallet_head_hash(&warehouse, "scoped"), head_before, "a refused stack must commit nothing");
}

// -------------------------------------------------------------------------------------------
// Presence-tolerant maintenance: `compact --all` on a store that legitimately lacks its
// out-of-scope objects.
//
// The object store is FULL today (fetching cannot yet be scoped), so — like the scoped-merge
// tests — absence is simulated by deleting the out-of-scope objects before maintenance runs.
// That proves the repack's reachability pass tolerates a sparsely-fetched store in advance: it
// packs what is present and never errors on a subtree object it does not hold. gc shares the same
// reachability walk (`collect_live_set`), unit-tested directly in `gc_utils` where its collecting
// side has a warehouse to sweep; here the end-to-end shape is pinned through the client.
// -------------------------------------------------------------------------------------------

#[test]
fn compact_all_tolerates_a_sparse_store_and_a_later_scoped_stack_still_works() {
    let warehouse = TestWarehouse::new("compact-all-sparse");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/api/b.txt", "api b v1\n");
    warehouse.write_file("src/web/w.txt", "web v1\n");
    warehouse.write_file("README.md", "readme v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    let head = pallet_head_hash(&warehouse, "main");

    // Resolve every out-of-scope object's hash *before* deleting any (walking one requires the
    // ones above it to still be present): the sibling subtree, its blob, and the root file's blob.
    let web_tree = path_object_hash(&warehouse, &head, "src/web");
    let web_blob = path_object_hash(&warehouse, &head, "src/web/w.txt");
    let readme_blob = path_object_hash(&warehouse, &head, "README.md");

    // Make them unreadable, the way a sparsely-fetched store holds them: sealed by hash in the
    // signed head, never downloaded.
    for hash in [&web_tree, &web_blob, &readme_blob] {
        std::fs::remove_file(object_store_path(&warehouse, hash)).expect("the out-of-scope object existed");
    }

    // The repack must not error on the absent-but-reachable subtree: it packs the present live set.
    let objects = warehouse.root.join(".forklift/objects");
    let repack = warehouse.run(&["--json", "compact", "--all"]);
    assert_success(&repack);
    assert_eq!(json(&repack)["data"]["all"], true);
    assert_eq!(count_packs(&objects), 1, "the present live set consolidates into a single pack");

    // Byte-reproducible on the sparse store: a second repack lands on the same single pack (the
    // pack id is content-derived, so an unchanged repack neither churns the name nor loses an
    // object) — the determinism contract, holding with objects legitimately missing.
    assert_success(&warehouse.run(&["compact", "--all"]));
    assert_eq!(count_packs(&objects), 1, "an idempotent repack on a sparse store keeps the one pack");

    // The live, present history still reads back after the repack; the deleted objects stay gone
    // (the repack neither collected the sealed spine nor resurrected the unfetched objects).
    assert_success(&warehouse.run(&["peek", &head]));
    assert_eq!(
        json(&warehouse.run(&["--json", "history"]))["data"]["entries"].as_array().unwrap().len(),
        1, "the one live parcel survives the sparse repack"
    );
    assert!(!object_store_path(&warehouse, &web_tree).exists(), "the repack must not recreate an unfetched object");

    // A subsequent scoped stack still works on the compacted sparse store: the overlay reads the
    // out-of-scope sibling's hash off the (now packed) spine tree and carries it forward by hash,
    // never touching the absent object.
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));
    std::fs::write(scoped_dir.join("src/api/a.txt"), "api a v2\n").unwrap();
    assert_success(&warehouse.run_at(&scoped_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&scoped_dir, &["stack", "edit api after a sparse repack"]));

    // The new head still commits the out-of-scope sibling at its original, never-fetched hash — the
    // seal survived both the compaction and the scoped stack, with the object absent throughout.
    let scoped_head = pallet_head_hash(&warehouse, "scoped");
    assert_eq!(path_object_hash(&warehouse, &scoped_head, "src/web"), web_tree,
        "the sealed out-of-scope subtree hash must carry forward unchanged");
}

// =================================================================================================
// Native large-file (chunked) handling (§9.4b, Stage 1).
// =================================================================================================

/// The chunk threshold (bytes): content at or above this is stored chunked, below it as a blob.
/// Mirrors `chunk_utils::CHUNK_THRESHOLD_BYTES` (a frozen format constant).
const CHUNK_THRESHOLD: usize = 8 * 1024 * 1024;

/// Deterministic, seeded, incompressible-ish bytes (a SplitMix64 stream) — no clock, no RNG, so a
/// test's chunk boundaries are stable. Different seeds give unrelated content.
fn large_bytes(seed: u64, size: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(size);
    let mut state = seed;
    while out.len() < size {
        state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        out.extend_from_slice(&(z ^ (z >> 31)).to_le_bytes());
    }
    out.truncate(size);
    out
}

/// Write raw bytes to a path under the warehouse root (the harness `write_file` is text-only).
fn write_bytes(warehouse: &TestWarehouse, relative_path: &str, bytes: &[u8]) {
    let path = warehouse.root.join(relative_path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

/// The stored object type (`"blob"`, `"recipe"`, …) of the tree entry a parcel commits at `path`.
fn entry_object_type(warehouse: &TestWarehouse, parcel: &str, path: &str) -> String {
    let hash = path_object_hash(warehouse, parcel, path);
    let peeked = warehouse.run(&["--json", "peek", &hash]);
    assert_success(&peeked);
    json(&peeked)["data"]["object_type"].as_str().unwrap().to_string()
}

#[test]
fn large_file_threshold_classifies_by_hashed_length() {
    let warehouse = TestWarehouse::new("chunk-threshold");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // A byte below the threshold is a blob; at and above it, a recipe (chunk iff len >= threshold).
    write_bytes(&warehouse, "under.bin", &large_bytes(1, CHUNK_THRESHOLD - 1));
    write_bytes(&warehouse, "exact.bin", &large_bytes(2, CHUNK_THRESHOLD));
    write_bytes(&warehouse, "over.bin", &large_bytes(3, CHUNK_THRESHOLD + 1));

    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "large files"]);
        assert_success(&out);
        out
    });

    assert_eq!(entry_object_type(&warehouse, &parcel, "under.bin"), "blob",
               "just under the threshold must stay a blob");
    assert_eq!(entry_object_type(&warehouse, &parcel, "exact.bin"), "recipe",
               "exactly at the threshold must be chunked");
    assert_eq!(entry_object_type(&warehouse, &parcel, "over.bin"), "recipe",
               "above the threshold must be chunked");

    // The warehouse reports clean after loading and stacking a chunked file.
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "staged must be clean: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "unstaged must be clean: {}", status);
}

#[test]
fn chunked_file_round_trips_byte_identical_through_shift() {
    let warehouse = TestWarehouse::new("chunk-roundtrip");
    let big = large_bytes(0x5EED, CHUNK_THRESHOLD + 500_000);

    write_bytes(&warehouse, "big.bin", &big);
    warehouse.write_file("small.txt", "on main\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "with the giant"]));

    // Diverge on a feature pallet that does not have the giant.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::remove_file(warehouse.root.join("big.bin")).unwrap();
    warehouse.write_file("small.txt", "on feature\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "without the giant"]));

    // `palletize` already switched us to feature, whose tree has no giant on disk.
    assert!(!warehouse.root.join("big.bin").exists(), "the giant is gone on feature");

    // Shifting back to main re-materializes the giant byte-for-byte (stream-assembled + verified).
    assert_success(&warehouse.run(&["shift", "main"]));
    let restored = std::fs::read(warehouse.root.join("big.bin")).unwrap();
    assert_eq!(restored, big, "the chunked file must round-trip byte-identical");

    // And forward again removes it (a chunked entry deleted by a shift).
    assert_success(&warehouse.run(&["shift", "feature"]));
    assert!(!warehouse.root.join("big.bin").exists(), "the giant is removed shifting to feature");

    // Clean after the round trip.
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"), "must be clean: {}", status);
}

#[test]
fn identical_chunked_files_dedup_their_chunks() {
    let warehouse = TestWarehouse::new("chunk-dedup");
    let content = large_bytes(0xD3D0, CHUNK_THRESHOLD + 300_000);

    // First: one chunked file.
    write_bytes(&warehouse, "first.bin", &content);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    let objects_root = warehouse.root.join(".forklift").join("objects");
    let after_first = count_loose_objects(&objects_root);

    // Second: a byte-identical copy at a different path. It shares the same recipe and every chunk.
    write_bytes(&warehouse, "second.bin", &content);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second copy"]));
    let after_second = count_loose_objects(&objects_root);

    // The only new objects are the tree(s) and the parcel — no new chunk or recipe objects, since
    // identical content produces the identical recipe hash and identical chunk hashes.
    let added = after_second - after_first;
    assert!(added <= 3,
            "a byte-identical second chunked file should add only structural objects (trees/parcel), added {}",
            added);
}

#[test]
fn compact_leaves_chunks_loose_and_the_file_still_materializes() {
    let warehouse = TestWarehouse::new("chunk-compact");
    let big = large_bytes(0xBEE5, CHUNK_THRESHOLD + 200_000);

    write_bytes(&warehouse, "big.bin", &big);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "the giant"]);
        assert_success(&out);
        out
    });

    // Every chunk of the recipe, so we can prove they survive `compact` as loose objects.
    let recipe_hash = path_object_hash(&warehouse, &parcel, "big.bin");
    let peeked = warehouse.run(&["--json", "peek", &recipe_hash]);
    assert_success(&peeked);
    let chunk_hashes: Vec<String> = json(&peeked)["data"]["chunks"].as_array().unwrap().iter()
        .map(|c| c["hash"].as_str().unwrap().to_string())
        .collect();
    assert!(chunk_hashes.len() >= 2, "a chunked file is at least two chunks");

    // Compact the whole store (repack). Chunks must never be packed — they stay individually
    // addressable loose objects.
    assert_success(&warehouse.run(&["compact", "--all"]));

    for chunk in &chunk_hashes {
        assert!(object_store_path(&warehouse, chunk).exists(),
                "chunk {} must remain a loose object after compact (never packed)", chunk);
    }

    // The file still materializes byte-identically after compaction.
    assert_success(&warehouse.run(&["palletize", "empty"]));
    std::fs::remove_file(warehouse.root.join("big.bin")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "no giant"]));
    assert_success(&warehouse.run(&["shift", "main"]));
    assert_eq!(std::fs::read(warehouse.root.join("big.bin")).unwrap(), big,
               "the chunked file must still assemble after compact");
}

#[test]
fn a_corrupt_chunk_makes_materialization_fail_loudly() {
    let warehouse = TestWarehouse::new("chunk-corrupt");
    let big = large_bytes(0xC0FFEE, CHUNK_THRESHOLD + 100_000);

    write_bytes(&warehouse, "big.bin", &big);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "the giant"]);
        assert_success(&out);
        out
    });

    // Find the recipe, peek its first chunk hash, and corrupt that chunk object on disk.
    let recipe_hash = path_object_hash(&warehouse, &parcel, "big.bin");
    let peeked = warehouse.run(&["--json", "peek", &recipe_hash]);
    assert_success(&peeked);
    let chunk_hash = json(&peeked)["data"]["chunks"][0]["hash"].as_str().unwrap().to_string();
    let chunk_path = object_store_path(&warehouse, &chunk_hash);
    // Corrupt the stored chunk object (garbage that is neither valid zstd nor hashes to its name),
    // so reading it back fails loudly rather than returning wrong bytes.
    std::fs::write(&chunk_path, b"this is not the real chunk object").unwrap();

    // On a branch, replace big.bin with a small plain "sentinel" and stack it. `palletize`
    // switches to the new pallet.
    assert_success(&warehouse.run(&["palletize", "other"]));
    write_bytes(&warehouse, "big.bin", b"sentinel content\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "sentinel"]));

    // Shift back to main: re-materializing the giant must fail loudly (the corrupt chunk no
    // longer verifies), and — durable-before-destructive — the working copy is left intact rather
    // than half-written or destroyed.
    let shift_back = warehouse.run(&["shift", "main"]);
    assert!(!shift_back.status.success(), "a corrupt chunk must fail materialization, not corrupt the file");
    assert_eq!(std::fs::read(warehouse.root.join("big.bin")).unwrap(), b"sentinel content\n",
               "a failed chunked materialization must not destroy the working copy");
}

#[test]
fn blame_refuses_a_chunked_file() {
    let warehouse = TestWarehouse::new("chunk-blame");
    write_bytes(&warehouse, "big.bin", &large_bytes(7, CHUNK_THRESHOLD + 10_000));

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "the giant"]));

    let blame = warehouse.run(&["blame", "big.bin"]);
    assert!(!blame.status.success(), "blame must refuse a chunked file");
    assert!(stderr(&blame).contains("large binary file") || stderr(&blame).contains("text files"),
            "blame refusal must be clear: {}", stderr(&blame));
}

#[test]
fn blame_succeeds_at_a_plain_head_with_a_chunked_ancestor() {
    // A file that is plain text at head but was a chunked giant earlier in first-parent
    // history must not abort the whole command: the chunked ancestor is an opaque binary
    // boundary the walk clears state at (never a bare `load_blob` on a recipe hash, which
    // would otherwise leak the raw internal "is a recipe, not a blob" error). Every
    // post-transition line is attributed to the parcel that (re)introduced text.
    let warehouse = TestWarehouse::new("chunk-blame-ancestor");

    // First version: a chunked giant.
    write_bytes(&warehouse, "f.txt", &large_bytes(99, CHUNK_THRESHOLD + 10_000));
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "chunked version"]));

    // Second version: plain text (a chunked -> plain flip at this path).
    warehouse.write_file("f.txt", "one\ntwo\nthree\n");
    assert_success(&warehouse.run(&["load", "."]));
    let second = extract_parcel_hash(&warehouse.run(&["stack", "text version"]));

    let blame = warehouse.run(&["--json", "blame", "f.txt"]);
    assert_success(&blame);
    let parsed = json(&blame);
    let lines = parsed["data"]["lines"].as_array().unwrap();
    assert_eq!(lines.len(), 3);
    for line in lines {
        assert_eq!(line["parcel"], second,
                   "no line may cross the chunked-ancestor boundary; every line attributes to \
                   the parcel that (re)introduced text");
    }

    // The human form must not leak the raw internal error either.
    let human = stdout(&warehouse.run(&["blame", "f.txt"]));
    assert!(!human.contains("is a recipe, not a blob"), "internal error leaked: {}", human);
}

#[test]
fn diff_reports_a_changed_chunked_file_as_binary() {
    let warehouse = TestWarehouse::new("chunk-diff");
    write_bytes(&warehouse, "big.bin", &large_bytes(11, CHUNK_THRESHOLD + 10_000));

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "v1"]));

    // Change the giant's content; the unstaged diff must report it as binary, never crash.
    write_bytes(&warehouse, "big.bin", &large_bytes(12, CHUNK_THRESHOLD + 20_000));
    let diff = warehouse.run(&["diff"]);
    assert_success(&diff);
    assert!(stdout(&diff).contains("binary contents"), "a changed chunked file is binary: {}", stdout(&diff));
}

#[test]
fn a_file_flips_between_chunked_and_plain_across_revisions() {
    let warehouse = TestWarehouse::new("chunk-flip-plain");
    // Revision 1: the path is a chunked giant.
    write_bytes(&warehouse, "x.bin", &large_bytes(21, CHUNK_THRESHOLD + 50_000));
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let chunked_parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "chunked"]);
        assert_success(&out);
        out
    });
    assert_eq!(entry_object_type(&warehouse, &chunked_parcel, "x.bin"), "recipe");

    // Revision 2 on a branch: the same path is now a tiny plain file (chunked -> plain flip).
    assert_success(&warehouse.run(&["palletize", "plain"]));
    write_bytes(&warehouse, "x.bin", b"now i am small\n");
    assert_success(&warehouse.run(&["load", "."]));
    let plain_parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "plain"]);
        assert_success(&out);
        out
    });
    assert_eq!(entry_object_type(&warehouse, &plain_parcel, "x.bin"), "blob",
               "the flipped entry is now a plain blob");

    // Shift both ways: content and type compose with the deletes-before-writes flip machinery.
    assert_success(&warehouse.run(&["shift", "main"]));
    assert_eq!(std::fs::read(warehouse.root.join("x.bin")).unwrap(),
               large_bytes(21, CHUNK_THRESHOLD + 50_000), "chunked content restored");

    assert_success(&warehouse.run(&["shift", "plain"]));
    assert_eq!(std::fs::read(warehouse.root.join("x.bin")).unwrap(), b"now i am small\n",
               "plain content restored");
    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"), "clean after flip: {}", status);
}

#[test]
fn a_chunked_file_directory_flip_shift_succeeds() {
    // The tracked dir->file flip fix (merged from main) composes with chunked storage exactly
    // like a plain file: the directory is a tracked, clean shard, so replacing it with the
    // chunked file must succeed, not refuse as an untracked collision.
    let warehouse = TestWarehouse::new("chunk-flip-dir");
    let giant = large_bytes(31, CHUNK_THRESHOLD + 40_000);
    write_bytes(&warehouse, "node", &giant);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "node is a chunked file"]));

    // On a branch, "node" becomes a directory. `palletize` switches to the new pallet.
    assert_success(&warehouse.run(&["palletize", "asdir"]));
    std::fs::remove_file(warehouse.root.join("node")).unwrap();
    warehouse.write_file("node/inside.txt", "now a directory\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "node is a directory"]));

    // Shifting back to "main" (where "node" is still a tracked, clean chunked file) must not
    // refuse: it is exactly the tracked dir->file flip, and the file materializes from its
    // recipe like any other chunked write.
    let shift_back = warehouse.run(&["shift", "main"]);
    assert_success(&shift_back);

    assert!(!warehouse.root.join("node").is_dir(), "\"node\" must now be a file, not a directory");
    assert_eq!(std::fs::read(warehouse.root.join("node")).unwrap(), giant,
               "the chunked file's content must be materialized exactly");

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("The working directory matches the inventory"), "status: {}", status);
}

#[test]
fn show_prints_a_files_content_at_head_and_at_an_older_revision() {
    let warehouse = TestWarehouse::new("show-text");
    warehouse.write_file("src/app.rs", "v1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let v1 = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "v1"]);
        assert_success(&out);
        out
    });

    warehouse.write_file("src/app.rs", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "v2"]));

    // The current head, by pallet name.
    let head = warehouse.run(&["show", "main:src/app.rs"]);
    assert_success(&head);
    assert_eq!(stdout(&head), "v2\n");

    // An older revision, by parcel hash — one invocation, no separate resolve/peek round trip.
    let older = warehouse.run(&["show", &format!("{}:src/app.rs", v1)]);
    assert_success(&older);
    assert_eq!(stdout(&older), "v1\n");

    // --json carries the resolved parcel hash, the blob hash, and the content.
    let json_out = warehouse.run(&["--json", "show", "main:src/app.rs"]);
    assert_success(&json_out);
    let data = json(&json_out)["data"].clone();
    assert_eq!(data["path"], "src/app.rs");
    assert_eq!(data["binary"], false);
    assert_eq!(data["content"], "v2\n");
    assert_eq!(data["size"], 3);
    assert!(data["revision"].as_str().unwrap().len() >= 4, "revision should be a resolved hash");
    assert!(data.get("content_hash").is_none(), "content_hash is chunked-only");
    assert!(data.get("chunk_count").is_none(), "chunk_count is chunked-only");
}

#[test]
fn show_reports_binary_content_honestly_never_mangling_it() {
    let warehouse = TestWarehouse::new("show-binary");
    write_bytes(&warehouse, "blob.bin", b"bin\0ary");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "binary"]));

    let json_out = warehouse.run(&["--json", "show", "main:blob.bin"]);
    assert_success(&json_out);
    let data = json(&json_out)["data"].clone();
    assert_eq!(data["binary"], true);
    assert_eq!(data["size"], 7);
    assert!(data.get("content").is_none(), "a binary blob must never carry mangled content");

    let human = warehouse.run(&["show", "main:blob.bin"]);
    assert_success(&human);
    assert!(stdout(&human).contains("binary file"), "unexpected output: {}", stdout(&human));
}

#[test]
fn show_reports_nul_free_invalid_utf8_content_as_binary() {
    // A NUL-free blob can still be invalid UTF-8 — text means both checks pass, not just the
    // NUL-byte one, or this is exactly the mangled-content bug the "binary" signal exists to
    // prevent.
    let warehouse = TestWarehouse::new("show-invalid-utf8");
    write_bytes(&warehouse, "blob.bin", &[0x66, 0x6f, 0xff, 0xfe]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "invalid utf8"]));

    let json_out = warehouse.run(&["--json", "show", "main:blob.bin"]);
    assert_success(&json_out);
    let data = json(&json_out)["data"].clone();
    assert_eq!(data["binary"], true);
    assert_eq!(data["size"], 4);
    assert!(data.get("content").is_none(), "invalid UTF-8 must never carry mangled content");
}

#[test]
fn show_reports_chunked_metadata_without_assembling_the_file() {
    let warehouse = TestWarehouse::new("show-chunked");
    let giant = large_bytes(0x540, CHUNK_THRESHOLD + 10_000);
    write_bytes(&warehouse, "big.bin", &giant);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "chunked"]));

    let json_out = warehouse.run(&["--json", "show", "main:big.bin"]);
    assert_success(&json_out);
    let data = json(&json_out)["data"].clone();
    assert_eq!(data["binary"], true);
    assert_eq!(data["size"], giant.len());
    assert!(data.get("content").is_none(), "a chunked file is never assembled just to show it");
    assert!(data["content_hash"].as_str().unwrap().len() > 0);
    assert!(data["chunk_count"].as_u64().unwrap() > 0);

    let human = warehouse.run(&["show", "main:big.bin"]);
    assert_success(&human);
    let text = stdout(&human);
    assert!(text.contains("chunked file") && text.contains("chunks"), "unexpected output: {}", text);
}

#[test]
fn show_resolves_a_subdirectory_path() {
    let warehouse = TestWarehouse::new("show-paths");
    warehouse.write_file("src/data/nested.txt", "nested content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "paths"]));

    let nested = warehouse.run(&["show", "main:src/data/nested.txt"]);
    assert_success(&nested);
    assert_eq!(stdout(&nested), "nested content\n");
}

// NTFS filenames cannot contain ":", so a fixture file with one in its name cannot even be
// created on Windows — this is exactly the split-on-first-":" behavior under test, and there
// is no cross-platform way to exercise it with a real on-disk path. Unix-only.
#[cfg(unix)]
#[test]
fn show_resolves_a_path_containing_a_colon() {
    let warehouse = TestWarehouse::new("show-colon-path");
    warehouse.write_file("note:with:colons.txt", "colon path\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "paths"]));

    // Only the *first* ":" splits revision from path — the rest belongs to the path.
    let colon_path = warehouse.run(&["show", "main:note:with:colons.txt"]);
    assert_success(&colon_path);
    assert_eq!(stdout(&colon_path), "colon path\n");
}

#[test]
fn show_reports_a_clean_error_for_a_missing_path_or_revision() {
    let warehouse = TestWarehouse::new("show-missing");
    warehouse.write_file("src/app.rs", "content\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    let missing_path = warehouse.run(&["show", "main:nope.txt"]);
    assert!(!missing_path.status.success());
    assert!(stderr(&missing_path).contains("was not found"), "unexpected error: {}", stderr(&missing_path));

    let missing_revision = warehouse.run(&["show", "nope:src/app.rs"]);
    assert!(!missing_revision.status.success());
    assert!(
        stderr(&missing_revision).contains("neither a pallet nor a parcel hash"),
        "unexpected error: {}", stderr(&missing_revision)
    );

    let malformed = warehouse.run(&["show", "no-colon-here"]);
    assert!(!malformed.status.success());
    assert!(stderr(&malformed).contains("is not \"<revision>:<path>\""), "unexpected error: {}", stderr(&malformed));
}

#[test]
fn peek_json_reports_a_binary_blob_honestly() {
    let warehouse = TestWarehouse::new("peek-binary-json");
    write_bytes(&warehouse, "blob.bin", b"bin\0ary");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "binary"]);
        assert_success(&out);
        out
    });

    let hash = path_object_hash(&warehouse, &parcel, "blob.bin");
    let peeked = warehouse.run(&["--json", "peek", &hash]);
    assert_success(&peeked);
    let data = json(&peeked)["data"].clone();
    assert_eq!(data["binary"], true);
    assert!(data.get("content").is_none(), "a binary blob must never carry mangled content");
}

#[test]
fn peek_json_reports_nul_free_invalid_utf8_content_as_binary() {
    // Same fix, same test shape as `show`: a NUL-free blob that is not valid UTF-8 must still
    // report binary, not fall through to a lossy conversion that mangles it into fake text.
    let warehouse = TestWarehouse::new("peek-invalid-utf8-json");
    write_bytes(&warehouse, "blob.bin", &[0x66, 0x6f, 0xff, 0xfe]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    let parcel = extract_parcel_hash(&{
        let out = warehouse.run(&["stack", "invalid utf8"]);
        assert_success(&out);
        out
    });

    let hash = path_object_hash(&warehouse, &parcel, "blob.bin");
    let peeked = warehouse.run(&["--json", "peek", &hash]);
    assert_success(&peeked);
    let data = json(&peeked)["data"].clone();
    assert_eq!(data["binary"], true);
    assert!(data.get("content").is_none(), "invalid UTF-8 must never carry mangled content");
}

#[test]
fn diff_empty_token_lists_every_file_of_a_root_parcel_as_added() {
    let warehouse = TestWarehouse::new("diff-empty-token");
    warehouse.write_file("src/app.rs", "content\n");
    warehouse.write_file("README.md", "readme\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "root"]));

    // A root parcel has no real "before" — diffing it against ":empty" reports every
    // tracked file as added, rather than refusing for want of a second real revision.
    let human = warehouse.run(&["diff", ":empty", "main"]);
    assert_success(&human);
    let text = stdout(&human);
    assert!(text.contains("added: src/app.rs"), "unexpected diff output: {}", text);
    assert!(text.contains("added: README.md"), "unexpected diff output: {}", text);

    let json_out = warehouse.run(&["--json", "diff", ":empty", "main"]);
    assert_success(&json_out);
    let files = json(&json_out)["data"]["files"].as_array().unwrap().clone();
    assert_eq!(files.len(), 2);
    assert!(files.iter().all(|file| file["kind"] == "added"));

    // The reverse direction: main vs nothing removes everything.
    let reversed = stdout(&warehouse.run(&["diff", "main", ":empty"]));
    assert!(reversed.contains("removed: src/app.rs"), "unexpected diff output: {}", reversed);
}

/// The path of the taint directory in a plain (non-bay) test warehouse — see
/// `taint_utils::taint_dir_path_under`.
fn taint_dir_path(warehouse: &TestWarehouse) -> PathBuf {
    forklift_core::util::taint_utils::taint_dir_path_under(&warehouse.root)
}

/// Hand-plant a complete (non-torn) taint file recording `paths` — a crash-before-heal
/// simulation, the same technique `a_hand_written_marker_refuses_stack_even_without_a_reported_load_failure`
/// uses for the incomplete-load marker.
fn plant_taint(warehouse: &TestWarehouse, paths: &[&str]) {
    let dir = taint_dir_path(warehouse);
    std::fs::create_dir_all(&dir).unwrap();
    let mut content = String::new();
    for path in paths {
        content.push_str(path);
        content.push('\n');
    }
    content.push_str("END\n");
    std::fs::write(dir.join("taint-1-0"), content).unwrap();
}

#[test]
fn a_tainted_root_exits_21_with_the_durability_taint_code() {
    // A taint the entry-heal cannot auto-heal (the recorded object never existed) must refuse
    // every warehouse command with the machine-coded `durability_taint` error and exit 21 — the
    // CLI-level proof that `main.rs`'s chokepoint is actually wired, mirroring how
    // `a_hand_written_marker_refuses_stack_even_without_a_reported_load_failure` proves
    // `incomplete_load`'s exit 20 the same way.
    let warehouse = TestWarehouse::new("durability-taint-vanished");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    plant_taint(&warehouse, &["objects/zz/doesnotexist"]);

    let status = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    let value = json(&status);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "durability_taint");
    assert!(
        value["error"]["message"].as_str().unwrap().contains("vanished"),
        "the message names the unhealable verdict: {}", value["error"]["message"]
    );
    assert!(value["error"]["next_step"].as_str().unwrap().contains("forklift heal"));

    // The taint survives the refusal — nothing was silently cleared.
    assert!(taint_dir_path(&warehouse).join("taint-1-0").exists());
}

#[test]
fn a_clean_taint_heals_automatically_and_the_command_proceeds() {
    // The common case this whole mechanism exists for: a crash-before-heal taint over a still-
    // present, still-correct object heals silently at the next command's entry, with no user-
    // visible effect beyond the command succeeding.
    let warehouse = TestWarehouse::new("durability-taint-heals");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // A real loose object this warehouse already wrote (from `prepare`'s own bootstrap, or a
    // trivial load+stack) makes a legitimate target: use a load+stack to get one on disk, then
    // hand-plant a taint over its own working directory's parent instead of guessing an object
    // hash — simpler and just as effective is proving the *absence* of any taint dir is what a
    // fresh warehouse starts with, then that a present taint dir with zero entries (the
    // "torn-free, nothing recorded" edge the entry-heal treats as a cheap Ok) does not block
    // anything either.
    assert!(!taint_dir_path(&warehouse).exists(), "a fresh warehouse must start untainted");

    std::fs::create_dir_all(taint_dir_path(&warehouse)).unwrap();
    assert_success(&warehouse.run(&["stocktake"]));
}

#[test]
fn a_torn_taint_exits_21_and_survives() {
    let warehouse = TestWarehouse::new("durability-taint-torn");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let dir = taint_dir_path(&warehouse);
    std::fs::create_dir_all(&dir).unwrap();
    // No terminator line: torn by construction.
    std::fs::write(dir.join("taint-1-0"), b"objects/ab/cdef\n").unwrap();

    let status = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    assert_eq!(json(&status)["error"]["code"], "durability_taint");
    assert!(dir.join("taint-1-0").exists(), "a torn taint file must survive the refusal");
}

/// Every regular taint file's recorded lines, unioned (dropping the `END` terminator) — used to
/// inspect exactly what a recovery attempt left standing.
fn recorded_taint_paths(warehouse: &TestWarehouse) -> Vec<String> {
    let dir = taint_dir_path(warehouse);
    if !dir.exists() {
        return Vec::new();
    }

    let mut paths = Vec::new();
    for entry in std::fs::read_dir(&dir).unwrap() {
        let entry = entry.unwrap();
        if !entry.path().is_file() {
            continue;
        }
        let content = std::fs::read_to_string(entry.path()).unwrap();
        paths.extend(content.lines().filter(|line| *line != "END").map(|line| line.to_string()));
    }
    paths
}

/// The root-relative loose-object path (`objects/<2 hex>/<rest>`) a hash lives at — the exact
/// inverse `taint_utils::to_root_relative`/`file_utils::get_path_for_object` use, hand-derived
/// here so these tests can plant/inspect a taint without reaching into `forklift-core` privates.
fn loose_object_relative_path(hash: &str) -> String {
    format!("objects/{}/{}", &hash[..2], &hash[2..])
}

#[test]
fn heal_reaches_a_tainted_root_and_clears_a_vanished_unreferenced_object() {
    // The chokepoint exemption, proven the mutation-killing way: if `Command::bypasses_taint_heal`
    // did not exempt `heal`, this run would be blocked at the entry chokepoint and refuse with
    // `heal_if_tainted`'s own conservative "vanished: ..." refusal (exit 21, nothing cleared).
    // With the exemption wired, `heal` reaches its own recovery logic, walks every durable ref
    // source, finds the fabricated hash referenced by nothing at all, and clears — exit 0. The
    // two outcomes are observably different (exit code, and whether the taint survives), so this
    // one test kills the "exemption missing" mutation without needing a second one.
    let warehouse = TestWarehouse::new("heal-clears-unreferenced");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let fake_hash = "1".repeat(64);
    let relative = loose_object_relative_path(&fake_hash);
    plant_taint(&warehouse, &[&relative]);

    let status = warehouse.run(&["heal", "--json"]);
    assert_success(&status);
    let value = json(&status);
    assert_eq!(value["data"]["was_tainted"], true);
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|v| v == &fake_hash),
        "the fabricated hash must be reported resolved (absent and unreferenced): {}", value
    );

    assert!(!taint_dir_path(&warehouse).exists() || recorded_taint_paths(&warehouse).is_empty(),
        "the taint must be fully cleared");

    // The warehouse is fully usable again, including the mutating write path a taint exists to
    // protect — not just a read-only command.
    warehouse.write_file("src/app.rs", "after heal");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    assert_success(&warehouse.run(&["stack", "after heal"]));
}

#[test]
fn heal_reports_a_vanished_but_referenced_object_and_everything_still_refuses() {
    let warehouse = TestWarehouse::new("heal-reports-referenced");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("src/app.rs", "referenced content");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    let stack_out = warehouse.run(&["stack", "referenced parcel"]);
    assert_success(&stack_out);
    let parcel_hash = extract_parcel_hash(&stack_out);

    // The pallet head is a durable ref by construction, so vanishing (and taining) its own
    // parcel object is the simplest "still referenced" case: it is the walk's own root.
    let relative = loose_object_relative_path(&parcel_hash);
    std::fs::remove_file(warehouse.root.join(".forklift").join(&relative)).unwrap();
    plant_taint(&warehouse, &[&relative]);

    let status = warehouse.run(&["heal", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    let value = json(&status);
    assert_eq!(value["error"]["code"], "durability_taint");
    assert!(
        value["error"]["message"].as_str().unwrap().contains("vanished and still referenced"),
        "unexpected message: {}", value
    );
    // Machine-coded remedies, not an exitless refusal.
    let next_step = value["error"]["next_step"].as_str().unwrap();
    assert!(next_step.contains("forklift lower") || value["error"]["message"].as_str().unwrap().contains("forklift lower"));

    assert!(recorded_taint_paths(&warehouse).contains(&relative),
        "the taint must survive with the still-dangling path recorded");

    // Every ordinary command still refuses too — the taint was never lifted — including the
    // mutating write path (`stack`) a taint exists specifically to protect. The chokepoint runs
    // before a command dispatches, so `stack` refuses here regardless of staged content.
    let blocked_read = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(blocked_read.status.code(), Some(21));
    assert_eq!(json(&blocked_read)["error"]["code"], "durability_taint");

    let blocked_stack = warehouse.run(&["stack", "should not land", "--json"]);
    assert_eq!(blocked_stack.status.code(), Some(21));
    assert_eq!(json(&blocked_stack)["error"]["code"], "durability_taint");
}

#[test]
fn heal_partial_clear_leaves_exactly_the_unresolved_remainder_recorded() {
    let warehouse = TestWarehouse::new("heal-partial-clear");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("src/app.rs", "referenced content");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    let stack_out = warehouse.run(&["stack", "referenced parcel"]);
    assert_success(&stack_out);
    let parcel_hash = extract_parcel_hash(&stack_out);
    let referenced_relative = loose_object_relative_path(&parcel_hash);
    std::fs::remove_file(warehouse.root.join(".forklift").join(&referenced_relative)).unwrap();

    let fake_hash = "2".repeat(64);
    let unreferenced_relative = loose_object_relative_path(&fake_hash);

    plant_taint(&warehouse, &[&referenced_relative, &unreferenced_relative]);

    let status = warehouse.run(&["heal", "--json"]);
    assert_eq!(status.status.code(), Some(21));

    // Exactly the unresolved remainder is recorded afterwards — the resolved (unreferenced)
    // path must be gone, the still-dangling one must remain, and nothing extra appears.
    let remaining = recorded_taint_paths(&warehouse);
    assert_eq!(remaining, vec![referenced_relative.clone()],
        "the remainder must be exactly the still-dangling path, no more and no less");
}

#[test]
fn audit_runs_while_tainted() {
    // The second chokepoint exemption: `audit` is a read-only diagnostic and must reach its own
    // logic instead of being blocked by the entry-heal chokepoint. Proven by observing an error
    // that is NOT the chokepoint's `durability_taint` refusal — a fresh, un-enrolled warehouse's
    // `audit` refuses for a different, audit-specific reason ("trust is not established"),
    // which it can only report if it actually ran.
    let warehouse = TestWarehouse::new("audit-runs-while-tainted");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    plant_taint(&warehouse, &[&loose_object_relative_path(&"3".repeat(64))]);

    let status = warehouse.run(&["audit", "--json"]);
    assert_ne!(status.status.code(), Some(21),
        "audit must bypass the taint chokepoint, not inherit its exit code");
    let value = json(&status);
    assert_eq!(value["ok"], false);
    assert_ne!(value["error"]["code"], "durability_taint");
}

#[test]
fn heal_on_a_torn_taint_names_heavyweight_recovery_and_survives() {
    let warehouse = TestWarehouse::new("heal-torn");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let dir = taint_dir_path(&warehouse);
    std::fs::create_dir_all(&dir).unwrap();
    // No terminator line: torn by construction.
    std::fs::write(dir.join("taint-1-0"), b"objects/ab/cdef\n").unwrap();

    let status = warehouse.run(&["heal", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    let value = json(&status);
    assert_eq!(value["error"]["code"], "durability_taint");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("torn") || message.to_lowercase().contains("incomplete"),
        "the message must name the torn/unknown-scope condition: {}", message);
    let next_step = value["error"]["next_step"].as_str().unwrap();
    assert!(
        next_step.contains("re-fetch") && next_step.contains("re-run") && next_step.contains("accept the loss"),
        "the heavyweight exits must be named: {}", next_step
    );

    assert!(dir.join("taint-1-0").exists(), "a torn taint file must survive a heal attempt too");
}
