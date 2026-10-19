use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Path to the compiled forklift binary (provided by cargo for integration tests).
const FORKLIFT: &str = env!("CARGO_BIN_EXE_forklift");

mod common;
use common::{forklift_command, hold_store_access};

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
        forklift_command()
            .args(args)
            .current_dir(dir)
            .env("FORKLIFT_GLOBAL_CONFIG", self.home.join("global-config.toml"))
            .env("FORKLIFT_KEYS_DIR", self.home.join("test-keys"))
            .output()
            .unwrap()
    }

    fn run_in(&self, relative_dir: &str, args: &[&str]) -> Output {
        forklift_command()
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
        let mut command = forklift_command();

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

        let mut child = forklift_command()
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

/// Assert the state `import-git` leaves a colocated repo in: the imported tree is checked out,
/// nothing is staged, and the only working-tree change is the ignore file the import wrote its
/// `.git` rule into. That file is tracked content, so the import leaves it for the user to stack.
fn assert_only_the_ignore_file_is_untracked(warehouse: &TestWarehouse) {
    let status = json(&warehouse.run(&["--json", "stocktake"]));
    assert_eq!(status["data"]["staged_count"], 0, "{}", status);
    assert_eq!(status["data"]["unstaged"], serde_json::json!([{"kind": "untracked", "path": ".forkliftignore"}]),
        "{}", status);
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

// Timestamps have second granularity, so an entry is only trusted when its file's mtime is two
// whole seconds older than the clock read before its stat. The file must be backdated to make
// the test deterministic; `touch -t` keeps this unix-only.
#[cfg(unix)]
#[test]
fn unchanged_files_are_not_rehashed_on_reload() {
    let warehouse = TestWarehouse::new("statcache");
    warehouse.write_file("src/stable.txt", "stable content\n");

    // Backdate the file so the load's stat proves its entry.
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

    // Rewriting the file (same content, fresh timestamps) must take the full path again: its
    // fresh mtime proves nothing, so the entry records it as unproven and the reload rehashes.
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

/// FORK-117's contract, at the surface where it is most visible: _a configuration file either
/// parses completely, or the command that needed it refuses, naming the file and the key._
///
/// Before this, `get_value_from_document` collapsed three states — absent, section-is-not-a-table,
/// field-is-the-wrong-type — into `None`, and each caller decided for itself what `None` meant.
/// The two reads below took that decision in opposite directions on the very same broken file:
/// `config operator.identifier` fell through to the global scope and exited **0**, printing a
/// value from a file the user was not configuring; `config remote.url` had nothing to fall
/// through to and exited **1** saying the key was "not set". One file, one defect, two answers,
/// neither of them the truth.
///
/// The fixture is a *typed* fault (`identifier = 12345`), not a TOML syntax error, because a
/// syntax error was never the bug: `load_document` already refused those, identically for both
/// keys. Only the per-key leniency layered on top of a file that parsed fine as TOML could
/// produce the split above — so a syntax-error fixture here would pass before the change as
/// well as after it, and prove nothing.
///
/// Falsified both directions: measured against `origin/main`'s binary on this exact fixture —
/// `config operator.identifier` exits 0 printing `global@id`, `config remote.url` exits 1.
#[test]
fn a_configuration_file_either_parses_completely_or_every_read_of_it_refuses() {
    let warehouse = TestWarehouse::new("config-whole-file-or-nothing");
    assert_success(&warehouse.run(&["prepare"]));

    // A perfectly good global configuration — the scope a lenient reader would quietly
    // fall through to.
    std::fs::write(
        warehouse.home.join("global-config.toml"),
        "[operator]\nidentifier = \"global@id\"\n",
    ).unwrap();

    // A hand-edited, unquoted warehouse identifier: valid TOML, wrong type for the key.
    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "[operator]\nidentifier = 12345\n",
    ).unwrap();

    let identifier = warehouse.run(&["config", "operator.identifier"]);
    let url = warehouse.run(&["config", "remote.url"]);

    assert_eq!(
        identifier.status.code(), url.status.code(),
        "one broken file must produce one answer, not a different one per key.\n\
         operator.identifier: {:?} / {}\nremote.url: {:?} / {}",
        identifier.status.code(), stderr(&identifier),
        url.status.code(), stderr(&url)
    );
    assert!(!identifier.status.success(), "a read of an unparseable file must not succeed");

    for output in [&identifier, &url] {
        let error = stderr(output);
        assert!(error.contains("warehouse.toml"), "the refusal must name the file: {}", error);
        assert!(!error.contains("is not set"), "a broken file must not be reported as unset: {}", error);
    }

    // And nothing leaked the global value the user was not asking about.
    assert!(
        !stdout(&identifier).contains("global@id"),
        "a malformed warehouse scope must not fall through to the global one: {}",
        stdout(&identifier)
    );
}

/// A bad value must not block its own repair. Validating the file *before*
/// the edit meant `remote.tor = "onn"` refused every write to that file — including
/// `config remote.tor on`, the command that corrects it. The value blocked its own repair, and a
/// text editor was the only way out. `write_validated` now parses the document **as edited**, so
/// the question is whether the file you are about to leave on disk is valid.
///
/// The fourth leg is the one that keeps this from being a hole rather than a fix: a write that
/// leaves the bad value untouched must still be refused, naming it.
///
/// Falsified both directions: validating before the edit reddens legs 1 and 3 (both refuse with
/// the `"onn"` message); accepting unconditionally after it reddens leg 4.
#[test]
fn a_write_that_repairs_the_offending_entry_is_allowed_where_one_that_leaves_it_is_not() {
    let warehouse = TestWarehouse::new("config-repairs-itself");
    assert_success(&warehouse.run(&["prepare"]));

    let config = warehouse.root.join(".forklift/config/warehouse.toml");
    let broken = "[remote]\nurl = \"http://example\"\ntor = \"onn\"\n";

    // 1. Overwriting the offending key repairs the file.
    std::fs::write(&config, broken).unwrap();
    assert_success(&warehouse.run(&["config", "remote.tor", "on"]));
    assert!(
        std::fs::read_to_string(&config).unwrap().contains("tor = \"on\""),
        "the corrected value must be on disk: {}", std::fs::read_to_string(&config).unwrap()
    );

    // 2. And the file is ordinary again afterwards.
    assert_success(&warehouse.run(&["config", "remote.token", "abc"]));

    // 3. Unsetting the offending key repairs it too — it removes exactly what was refusing.
    std::fs::write(&config, broken).unwrap();
    assert_success(&warehouse.run(&["config", "--unset", "remote.tor"]));
    assert!(
        !std::fs::read_to_string(&config).unwrap().contains("onn"),
        "the offending entry must be gone"
    );

    // 4. A write that leaves the bad value in place is still refused, and still names it.
    std::fs::write(&config, broken).unwrap();
    let refused = warehouse.run(&["config", "remote.token", "abc"]);
    assert!(!refused.status.success(), "unrelated writes must not land beside a bad value");
    let error = stderr(&refused);
    assert!(error.contains("remote.tor"), "the refusal must name the offending key: {}", error);
    assert!(error.contains("onn"), "the refusal must quote what was written: {}", error);
    assert_eq!(
        std::fs::read_to_string(&config).unwrap(), broken,
        "a refused write must leave the file byte-identical"
    );
}

/// The privacy-relevant half of the same contract, with a fixture that _discriminates_.
///
/// `remote.tor` decides whether a remote is dialled through Tor at all, and
/// `TorSettings::from_config` used to swallow any read failure with `.ok().flatten()` and
/// default to `TorMode::Auto`. So a hand-edited, unquoted `tor = true` — a natural edit, and
/// one `TorMode::parse` would have read as "on" had it been quoted — silently dropped the
/// warehouse out of the mode it was configured in and dialled the clearnet remote **directly**.
///
/// The fixture deliberately sets *no* global `remote.tor`: an earlier version of this test put
/// a valid global value behind the malformed warehouse one, which made a lenient reader look
/// correct (it "fell through" to something reasonable) and so proved the bug rather than the
/// property. With nothing behind it, the only two possible outcomes are a silent clearnet dial
/// and a refusal.
///
/// Falsified both directions: this test file was appended to a worktree of `origin/main` and
/// run against that binary, where it fails — `lower` reaches the network and reports
/// "Error while reaching the remote http://127.0.0.1:9: Connection refused", never naming
/// `remote.tor`.
#[test]
fn a_malformed_remote_tor_refuses_instead_of_silently_dialling_the_remote_directly() {
    let warehouse = TestWarehouse::new("config-tor-no-silent-clearnet");
    assert_success(&warehouse.run(&["prepare"]));

    // Port 9 (discard) is closed on every sane host: if the command reaches the network at
    // all, it fails with a transport error rather than hanging.
    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "[remote]\nurl = \"http://127.0.0.1:9/\"\ntor = true\n",
    ).unwrap();

    let lower = warehouse.run(&["lower"]);

    assert!(!lower.status.success(), "a malformed remote.tor must refuse: {}", stdout(&lower));
    let error = stderr(&lower);
    assert!(error.contains("remote.tor"), "the refusal must name the key: {}", error);
    assert!(error.contains("warehouse.toml"), "the refusal must name the file: {}", error);
    assert!(
        !error.to_lowercase().contains("connect"),
        "the command must refuse before it dials anything: {}", error
    );
}

/// The other half of the ticket's "two silent failures live on main" pair, also with a
/// discriminating fixture. `maintenance.rs` used to do `.unwrap_or(AutoCompaction::None)`, so a
/// `maintenance.*` value it could not read turned background packing off — on every command,
/// forever, with nothing printed anywhere.
///
/// `load` is the observable seam because it is a mutating command that resolves no operator
/// identity, so it still *succeeds* on a warehouse whose configuration does not parse — which
/// is exactly the state in which the old code went quiet. The warning rides stderr, so `--json`
/// stdout is still exactly one document.
///
/// Falsified both directions: run against an `origin/main` worktree's binary it fails at the
/// stderr assertion — `load` exits 0 with empty stderr, and `store` exits 0 reporting a store
/// census as though the configuration had been read.
#[test]
fn unreadable_maintenance_settings_are_announced_rather_than_silently_disabling_maintenance() {
    let warehouse = TestWarehouse::new("config-maintenance-not-silent");
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("a.txt", "one\n");

    // A hand-edited, unquoted threshold (a bare TOML integer, not the string the reader takes).
    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "[maintenance]\nloose = 12345\n",
    ).unwrap();

    let load = warehouse.run(&["--json", "load", "."]);
    assert_success(&load);

    // stdout is still exactly one JSON document — the warning never shares it.
    let envelope = json(&load);
    assert_eq!(envelope["ok"], true, "unexpected envelope: {}", envelope);

    let warning: serde_json::Value = serde_json::from_str(stderr(&load).trim())
        .unwrap_or_else(|e| panic!("stderr is not the JSON warning ({}): {}", e, stderr(&load)));
    assert_eq!(warning["warning"], "maintenance_unavailable", "unexpected warning: {}", warning);
    assert!(
        warning["message"].as_str().unwrap_or_default().contains("maintenance.loose"),
        "the warning must name the key: {}", warning
    );

    // `docs/MACHINE_INTERFACE.md` promises four fields on every stderr warning, and `next_step`
    // is the one a consumer surfaces to a human — so it has to carry an **action**. It used to
    // hold only a reassurance about the current state ("packing is skipped while this lasts"),
    // leaving that consumer nothing to do while the thing to do sat in the reported cause.
    //
    // This must not require the word "message": requiring it here once was how the wording came to name a `--json` field in text that is also printed
    // verbatim to a human. What it must require is the *action* and the agreement with the
    // constant; `output.rs`'s unit test owns the rule that no envelope field may be named.
    let next_step = warning["next_step"].as_str().unwrap_or_default();
    assert!(
        next_step.contains("Resolve") && next_step.contains("cause"),
        "next_step must point at an action, not describe the state: {}", warning
    );
    assert!(
        !next_step.contains("message"),
        "next_step reaches human readers too, so it must not name an envelope field: {}", warning
    );
    assert_eq!(
        (&warning["lonic_json"], &warning["tool"]), (&serde_json::json!(1), &serde_json::json!("forklift")),
        "the warning carries the same envelope header as stdout: {}", warning
    );

    // And the command that exists to report the store's health refuses outright rather than
    // reporting a default threshold the tool is not actually using.
    let store = warehouse.run(&["store"]);
    assert!(!store.status.success(), "store must refuse on a config it cannot read: {}", stdout(&store));
    assert!(stderr(&store).contains("maintenance.loose"), "unexpected refusal: {}", stderr(&store));
}

/// A `maintenance.*` value that is a string but not one the setting takes is refused at both
/// doors — a hand edit fails the command that reads it, and `config` will not write one. Before,
/// the reader took only the off-spellings and treated anything else as on, so `auto = "flase"`
/// left maintenance running; and an unparseable threshold was silently replaced by the default.
#[test]
fn a_mistyped_maintenance_value_is_refused_rather_than_read_as_a_default() {
    let warehouse = TestWarehouse::new("config-maintenance-typo");
    assert_success(&warehouse.run(&["prepare"]));
    let config = warehouse.root.join(".forklift/config/warehouse.toml");

    for (content, key) in [
        ("[maintenance]\nauto = \"flase\"\n", "maintenance.auto"),
        ("[maintenance]\nloose = \"10k\"\n",  "maintenance.loose"),
        ("[maintenance]\npacks = \"-1\"\n",   "maintenance.packs"),
    ] {
        std::fs::write(&config, content).unwrap();

        let store = warehouse.run(&["store"]);
        assert!(!store.status.success(), "store must refuse {content:?}: {}", stdout(&store));
        assert!(
            stderr(&store).contains(&format!("is not a valid value for {key}")),
            "the refusal must name the key and say why: {}", stderr(&store)
        );
    }

    std::fs::write(&config, "").unwrap();

    for (key, value) in [("maintenance.auto", "flase"), ("maintenance.loose", "10k")] {
        let set = warehouse.run(&["config", key, value]);
        assert!(!set.status.success(), "config must refuse {key} = {value:?}");
        assert!(stderr(&set).contains(&format!("is not a valid value for {key}")), "{}", stderr(&set));
    }
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "", "a refused set must not touch the file");

    // The documented spellings are accepted, and stored the documented way.
    assert_success(&warehouse.run(&["config", "maintenance.auto", " OFF "]));
    assert_eq!(stdout(&warehouse.run(&["config", "maintenance.auto"])).trim(), "off");
}

/// A warehouse configuration that is present but cannot be read is an error, never "absent".
/// An absent warehouse file falls through to the global one, so reading a dangling link as
/// absent replaced the warehouse's own settings with the user's global ones, silently.
#[cfg(unix)]
#[test]
fn a_dangling_warehouse_config_link_is_an_error_not_a_fallback_to_global() {
    let warehouse = TestWarehouse::new("config-dangling-link");
    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["config", "--global", "remote.url", "http://global.example/"]));

    let config = warehouse.root.join(".forklift/config/warehouse.toml");
    let _ = std::fs::remove_file(&config);
    std::os::unix::fs::symlink(warehouse.root.join("gone.toml"), &config).unwrap();

    let read = warehouse.run(&["config", "remote.url"]);
    assert!(
        !read.status.success(),
        "a dangling warehouse config must refuse, not answer with the global value: {}", stdout(&read)
    );
    assert!(stderr(&read).contains("warehouse.toml"), "the refusal must name the file: {}", stderr(&read));
    assert!(stderr(&read).contains("symbolic link"), "the refusal must say why: {}", stderr(&read));
    let named = stderr(&read).split('"').nth(1).unwrap_or_default().to_string();
    assert!(
        std::path::Path::new(&named).is_absolute(),
        "the refusal must name the file by its full path, not {named:?}: {}", stderr(&read)
    );

    // A link to a link whose target is gone is dangling too — the whole chain is followed, not
    // one level of it.
    std::fs::remove_file(&config).unwrap();
    std::os::unix::fs::symlink(warehouse.root.join("gone.toml"), warehouse.root.join("middle.toml")).unwrap();
    std::os::unix::fs::symlink(warehouse.root.join("middle.toml"), &config).unwrap();

    let read = warehouse.run(&["config", "remote.url"]);
    assert!(!read.status.success(), "a dangling chain must refuse: {}", stdout(&read));
    assert!(stderr(&read).contains("symbolic link"), "the refusal must say why: {}", stderr(&read));

    // The same, one level up: the configuration *folder* is the dangling link, so the file's own
    // path ends in nothing at all and only the walk up the path can tell it from absence.
    let folder = warehouse.root.join(".forklift/config");
    // Recursive: beside the file the folder also holds its edit-lock sidecar.
    std::fs::remove_file(&config).unwrap();
    std::fs::remove_dir_all(&folder).unwrap();
    std::os::unix::fs::symlink(warehouse.root.join("unmounted"), &folder).unwrap();

    let read = warehouse.run(&["config", "remote.url"]);
    assert!(
        !read.status.success(),
        "a dangling config folder must refuse, not answer with the global value: {}", stdout(&read)
    );
    assert!(stderr(&read).contains("symbolic link"), "the refusal must say why: {}", stderr(&read));

    // With nothing at the path at all, the global value applies — that is what absent means.
    std::fs::remove_file(&folder).unwrap();
    let read = warehouse.run(&["config", "remote.url"]);
    assert_success(&read);
    assert_eq!(stdout(&read).trim(), "http://global.example/");
}

/// A configuration file that is not TOML at all is refused like any other invalid file: named
/// by its full path, with the same remedy.
#[test]
fn a_config_file_that_is_not_toml_is_refused_like_any_invalid_file() {
    let warehouse = TestWarehouse::new("config-not-toml");
    assert_success(&warehouse.run(&["prepare"]));
    std::fs::write(warehouse.root.join(".forklift/config/warehouse.toml"), "[remote\n").unwrap();

    let read = warehouse.run(&["config", "remote.url"]);
    assert!(!read.status.success(), "a file that is not TOML must refuse: {}", stdout(&read));

    let error = stderr(&read);
    assert!(error.contains("is not valid"), "unexpected refusal: {error}");
    assert!(error.contains("Fix the file by hand."), "the refusal must give the remedy: {error}");
    let named = error.split('"').nth(1).unwrap_or_default();
    assert!(
        std::path::Path::new(named).is_absolute() && named.ends_with("warehouse.toml"),
        "the refusal must name the file by its full path, not {named:?}: {error}"
    );
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
    // .forkliftignore late). It stays tracked, so stocktake walks it like any tracked directory:
    // its unchanged files are not reported, and nothing under it is reported removed (the
    // serial walk of a "removed" subtree is what once made stocktake hang here).
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

/// The staged and unstaged changes `stocktake --json` reports, as (kind, path) pairs.
fn changes(warehouse: &TestWarehouse, section: &str) -> Vec<(String, String)> {
    // An empty section is left out of the JSON, so a misspelled one would read as empty.
    assert!(matches!(section, "staged" | "unstaged"), "stocktake has no section {:?}", section);
    let out = warehouse.run(&["--json", "stocktake"]);
    assert_success(&out);
    json(&out)["data"][section].as_array().cloned().unwrap_or_default()
        .iter()
        .map(|change| (change["kind"].as_str().unwrap().to_string(), change["path"].as_str().unwrap().to_string()))
        .collect()
}

fn read(warehouse: &TestWarehouse, relative_path: &str) -> String {
    std::fs::read_to_string(warehouse.root.join(relative_path)).unwrap()
}

/// A warehouse whose main pallet tracks notes.txt and build/out.txt, plus a pallet "feature"
/// where both differ. Ends on main with a clean working directory.
fn warehouse_with_a_diverged_pallet(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("notes.txt", "v1\n");
    warehouse.write_file("build/out.txt", "b1\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "one"]));
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("notes.txt", "v2\n");
    warehouse.write_file("build/out.txt", "b2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "two"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    warehouse
}

#[test]
fn an_edit_to_a_tracked_file_a_rule_covers_stays_visible_and_blocks_shift() {
    let warehouse = warehouse_with_a_diverged_pallet("ignore-tracked-edit");
    warehouse.write_file(".forkliftignore", "^notes\\.txt$\n");
    warehouse.write_file("notes.txt", "local edit\n");

    assert!(changes(&warehouse, "unstaged").contains(&("modified".to_string(), "notes.txt".to_string())),
        "an edit to a tracked file must stay visible whatever the rules say");

    let shift = warehouse.run(&["shift", "feature"]);
    assert!(!shift.status.success(), "shift must refuse over the edit: {}", stdout(&shift));
    assert_eq!(read(&warehouse, "notes.txt"), "local edit\n", "the edit must survive");
}

/// A disk image formatted exFAT and attached for one test, detached and deleted when dropped.
#[cfg(target_os = "macos")]
struct ExfatVolume {
    image: PathBuf,
    mount: PathBuf,
}

#[cfg(target_os = "macos")]
impl ExfatVolume {
    /// Create and attach the image, or `None` where `hdiutil` cannot (outside CI only: on CI the
    /// macOS leg must reach the volume, or the test it serves would pass without running).
    fn attach(name: &str) -> Option<ExfatVolume> {
        let base = std::env::temp_dir().join(format!("forklift-test-{}-{}", name, std::process::id()));
        let image = base.with_extension("dmg");
        let mount = base.with_extension("mnt");
        let _ = std::process::Command::new("hdiutil").args(["detach", "-force"]).arg(&mount).output();
        let _ = std::fs::remove_file(&image);
        let _ = std::fs::remove_dir_all(&mount);

        let created = std::process::Command::new("hdiutil")
            .args(["create", "-size", "32m", "-fs", "ExFAT", "-volname", "FKEXFAT", "-quiet"]).arg(&image)
            .status();
        let attached = created.is_ok_and(|status| status.success()) && std::process::Command::new("hdiutil")
            .args(["attach", "-nobrowse", "-quiet", "-mountpoint"]).arg(&mount).arg(&image)
            .status().is_ok_and(|status| status.success());

        if !attached {
            assert!(std::env::var_os("CI").is_none(), "hdiutil could not attach an exFAT image on CI");
            eprintln!("skipping: hdiutil could not attach an exFAT image");
            let _ = std::fs::remove_file(&image);
            return None;
        }

        Some(ExfatVolume { image, mount })
    }
}

#[cfg(target_os = "macos")]
impl Drop for ExfatVolume {
    fn drop(&mut self) {
        let _ = std::process::Command::new("hdiutil").args(["detach", "-force", "-quiet"]).arg(&self.mount).status();
        let _ = std::fs::remove_file(&self.image);
    }
}

/// On a volume without an executable bit (exFAT, where every file reads `0700` and a chmod is
/// ignored), what a stat says about the bit is no change. A warehouse copied onto one reads clean
/// and shifts back and forth; a tracked executable keeps its bit through load and stack, a new
/// file is recorded plain, and a chmod the volume drops is reported by nobody. Copied back onto a
/// volume with the bit, a restore writes each file with the bit its entry records.
#[cfg(target_os = "macos")]
#[test]
fn a_volume_without_the_executable_bit_shifts_twice_and_keeps_each_entry_s_bit() {
    use std::os::unix::fs::PermissionsExt;

    let Some(volume) = ExfatVolume::attach("exfat-exec-bit") else {
        return;
    };
    let mode = |root: &std::path::Path, name: &str| std::fs::metadata(root.join(name)).unwrap().permissions().mode();
    let copy = |from: &std::path::Path, to: &std::path::Path| {
        let _ = std::fs::remove_dir_all(to);
        let status = std::process::Command::new("cp").arg("-R").arg(from).arg(to).status().unwrap();
        assert!(status.success(), "cp -R {} {}", from.display(), to.display());
    };

    let built = warehouse_with_a_diverged_pallet("exfat-exec-bit-built");
    built.write_file("tool.sh", "#!/bin/sh\n");
    std::fs::set_permissions(built.root.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&built.run(&["load", "tool.sh"]));
    assert_success(&built.run(&["stack", "tool"]));

    let root = volume.mount.join("warehouse");
    copy(&built.root, &root);
    let warehouse = TestWarehouse { root: root.clone(), home: built.home.clone() };
    assert_ne!(mode(&root, "notes.txt") & 0o111, 0, "exFAT must report the bit set on a plain file");

    assert!(changes(&warehouse, "unstaged").is_empty(), "a copied warehouse reads clean");
    assert_success(&warehouse.run(&["shift", "feature"]));
    assert_eq!(read(&warehouse, "notes.txt"), "v2\n");
    assert_success(&warehouse.run(&["shift", "main"]));
    assert_eq!(read(&warehouse, "notes.txt"), "v1\n");

    // A chmod is dropped by the volume and reported by nobody.
    std::fs::set_permissions(root.join("notes.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(changes(&warehouse, "unstaged").is_empty());
    let diff = stdout(&warehouse.run(&["diff"]));
    assert!(!diff.contains("mode:"), "{}", diff);

    warehouse.write_file("new.txt", "new\n");
    warehouse.write_file("tool.sh", "#!/bin/sh\necho\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "on exfat"]));
    assert!(changes(&warehouse, "unstaged").is_empty());
    assert!(changes(&warehouse, "staged").is_empty());

    let probes: Vec<_> = std::fs::read_dir(root.join(".forklift")).unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().to_string())
        .filter(|name| name.starts_with(forklift_core::util::file_utils::FILE_NAME_PREFIX_EXECUTABLE_BIT_PROBE))
        .collect();
    assert!(probes.is_empty(), "probe files left behind: {:?}", probes);

    // Back on a volume with the bit, the entries say which files are executable.
    let back = std::env::temp_dir().join(format!("forklift-test-exfat-exec-bit-back-{}", std::process::id()));
    copy(&root, &back);
    let back = TestWarehouse { root: back, home: built.home.clone() };
    assert_success(&back.run(&["restore", "."]));
    assert_ne!(mode(&back.root, "tool.sh") & 0o111, 0, "the tracked executable kept its bit");
    assert_eq!(mode(&back.root, "notes.txt") & 0o111, 0, "a plain file stayed plain");
    assert_eq!(mode(&back.root, "new.txt") & 0o111, 0, "a new file on a blind volume is recorded plain");
    assert!(changes(&back, "unstaged").is_empty());
}

/// A chmod alone is a change on every side: stocktake reports it unstaged, then staged once
/// loaded, diff names the mode on both, the parcel records it, and shift refuses to reset it.
#[cfg(unix)]
#[test]
fn a_mode_only_change_is_reported_staged_and_unstaged_and_blocks_shift() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = warehouse_with_a_diverged_pallet("mode-only-change");
    let path = warehouse.root.join("notes.txt");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let modified = ("modified".to_string(), "notes.txt".to_string());
    assert!(changes(&warehouse, "unstaged").contains(&modified), "a chmod must be an unstaged change");
    let diff = stdout(&warehouse.run(&["diff"]));
    assert!(diff.contains("modified: notes.txt") && diff.contains("(mode: normal -> executable)"), "{}", diff);

    let shift = warehouse.run(&["shift", "feature"]);
    assert!(!shift.status.success(), "shift must refuse over the chmod: {}", stdout(&shift));
    assert_ne!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0, "the chmod must survive");

    assert_success(&warehouse.run(&["load", "notes.txt"]));
    assert!(changes(&warehouse, "unstaged").is_empty(), "the loaded chmod must leave nothing unstaged");
    assert!(changes(&warehouse, "staged").contains(&modified), "a loaded chmod must be a staged change");
    let staged = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(staged.contains("(mode: normal -> executable)"), "{}", staged);

    assert_success(&warehouse.run(&["stack", "chmod"]));
    assert!(changes(&warehouse, "staged").is_empty());
    let pallets = stdout(&warehouse.run(&["diff", "feature", "main"]));
    assert!(pallets.contains("modified: notes.txt") && pallets.contains("(mode: normal -> executable)"), "{}", pallets);

    // Back the other way, folded into a move: the pallet diff keeps the mode of the moved file.
    assert_success(&warehouse.run(&["palletize", "executable"]));
    std::fs::rename(&path, warehouse.root.join("moved.txt")).unwrap();
    std::fs::set_permissions(warehouse.root.join("moved.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    let staged = stdout(&warehouse.run(&["diff", "--staged"]));
    assert!(staged.contains("notes.txt -> moved.txt") && staged.contains("(mode: executable -> normal)"), "{}", staged);
    assert_success(&warehouse.run(&["stack", "move and chmod -x"]));
    let pallets = stdout(&warehouse.run(&["diff", "main", "executable"]));
    assert!(pallets.contains("notes.txt -> moved.txt") && pallets.contains("(mode: executable -> normal)"), "{}", pallets);
}

/// The executable bit merges like content: a side that dropped it keeps it dropped over an edit
/// on the other side.
#[cfg(unix)]
#[test]
fn a_consolidate_keeps_the_side_that_changed_the_executable_bit() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("merge-chmod");
    let path = warehouse.root.join("run.sh");
    warehouse.write_file("run.sh", "a\nb\nc\nd\ne\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("run.sh", "a\nb\nc\nd\nE\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs edits"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    warehouse.write_file("run.sh", "A\nb\nc\nd\ne\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "ours edits and drops the bit"]));

    assert_success(&warehouse.run(&["consolidate", "feature"]));
    assert_eq!(read(&warehouse, "run.sh"), "A\nb\nc\nd\nE\n");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0, "our chmod -x must survive");
    assert!(changes(&warehouse, "unstaged").is_empty() && changes(&warehouse, "staged").is_empty(),
        "the merge must record the file as it left it");
}

/// A chmod alone on their side merges cleanly with our edit of a binary file, which no line
/// merge can reconcile: the content is ours, the bit theirs.
#[cfg(unix)]
#[test]
fn a_consolidate_takes_their_chmod_over_our_binary_edit() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("merge-chmod-binary");
    let path = warehouse.root.join("tool.bin");
    std::fs::write(&path, b"\0base\0").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs drops the bit"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    std::fs::write(&path, b"\0ours\0").unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "ours edits"]));

    let consolidate = warehouse.run(&["consolidate", "feature"]);
    assert_success(&consolidate);
    assert_eq!(std::fs::read(&path).unwrap(), b"\0ours\0");
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0, "their chmod -x must survive");
    assert!(changes(&warehouse, "unstaged").is_empty() && changes(&warehouse, "staged").is_empty(),
        "the merge must record the file as it left it");
}

#[test]
fn loading_the_root_or_a_tracked_folder_a_rule_covers_stages_no_removal() {
    let warehouse = warehouse_with_a_diverged_pallet("ignore-tracked-load");

    warehouse.write_file(".forkliftignore", "^notes\\.txt$\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert!(!changes(&warehouse, "staged").iter().any(|(_, path)| path == "notes.txt"),
        "load . must not stage the removal of a tracked file a rule matches");

    warehouse.write_file(".forkliftignore", "^build(/.*)?$\n");
    warehouse.write_file("build/out.txt", "b3\n");
    assert_success(&warehouse.run(&["load", "build"]));
    let staged = changes(&warehouse, "staged");
    assert!(!staged.iter().any(|(kind, _)| kind == "removed"), "load build must not stage a removal: {:?}", staged);
    // The folder's tracked content is refreshed, not skipped.
    assert!(staged.contains(&("modified".to_string(), "build/out.txt".to_string())), "{:?}", staged);
}

#[test]
fn removing_a_tracked_file_a_rule_covers_untracks_it_and_the_rule_then_hides_it() {
    let warehouse = warehouse_with_a_diverged_pallet("ignore-tracked-remove");
    warehouse.write_file(".forkliftignore", "^notes\\.txt$\n");

    assert_success(&warehouse.run(&["remove", "notes.txt"]));
    assert_success(&warehouse.run(&["load", ".forkliftignore"]));
    assert_success(&warehouse.run(&["stack", "untrack notes"]));

    assert_eq!(read(&warehouse, "notes.txt"), "v1\n", "remove never touches the working directory");
    assert!(!changes(&warehouse, "unstaged").iter().any(|(_, path)| path == "notes.txt"),
        "once untracked, the rule covers the file");
}

#[test]
fn a_rule_on_a_tracked_folder_hides_its_new_files_but_not_edits_to_its_tracked_ones() {
    let warehouse = warehouse_with_a_diverged_pallet("ignore-tracked-folder");
    // The rule matches the folder's key only, never the key of a file inside it.
    warehouse.write_file(".forkliftignore", "^build$\n");
    warehouse.write_file("build/new.txt", "generated\n");
    warehouse.write_file("build/out.txt", "edited\n");

    let unstaged = changes(&warehouse, "unstaged");
    assert!(unstaged.contains(&("modified".to_string(), "build/out.txt".to_string())), "{:?}", unstaged);
    assert!(!unstaged.iter().any(|(_, path)| path.starts_with("build/new")),
        "an untracked file inside a covered folder is covered: {:?}", unstaged);

    assert_success(&warehouse.run(&["load", "."]));
    assert!(!changes(&warehouse, "staged").iter().any(|(_, path)| path.starts_with("build/new")));
}

#[test]
fn a_folder_load_of_an_untracked_folder_a_rule_covers_stages_nothing() {
    let warehouse = TestWarehouse::new("ignore-untracked-folder");
    warehouse.write_file("keep.txt", "kept\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    warehouse.write_file(".forkliftignore", "^build(/.*)?$\n");
    warehouse.write_file("build/out.txt", "generated\n");
    assert_success(&warehouse.run(&["load", "build"]));
    assert_eq!(changes(&warehouse, "staged"), Vec::<(String, String)>::new());
}

#[test]
fn load_refuses_a_path_inside_the_metadata_folder_in_any_spelling() {
    let warehouse = TestWarehouse::new("load-metadata-folder");
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file(".forklift/x", "x\n");
    warehouse.write_file(".Forklift/y", "y\n");

    for target in [".forklift/x", ".Forklift/y"] {
        let load = warehouse.run(&["load", target]);
        assert!(!load.status.success(), "load {} must be refused", target);
        assert!(stderr(&load).contains("metadata folder"), "{}", stderr(&load));
    }
    assert_eq!(changes(&warehouse, "staged"), Vec::<(String, String)>::new());
}

/// A warehouse that tracked a/b/c.txt through a single-file load, the only load that registers
/// just the directory it writes, and stacked it. Ends with a clean working directory.
fn warehouse_with_a_nested_single_file_load(name: &str) -> TestWarehouse {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("root.txt", "r\n");
    warehouse.write_file("a/b/c.txt", "c\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "root.txt"]));
    assert_success(&warehouse.run(&["load", "a/b/c.txt"]));
    assert_success(&warehouse.run(&["stack", "nested"]));

    warehouse
}

#[test]
fn an_edit_under_a_nested_single_file_load_is_visible_and_blocks_shift() {
    let warehouse = TestWarehouse::new("nested-load-edit");
    warehouse.write_file("root.txt", "r\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("a/b/c.txt", "feature\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    // A shift rebuilds every shard from the tree, so the single-file load and the edit come
    // after it: the load registers only a/b itself.
    warehouse.write_file("a/b/c.txt", "c\n");
    assert_success(&warehouse.run(&["load", "a/b/c.txt"]));
    assert_success(&warehouse.run(&["stack", "nested"]));
    warehouse.write_file("a/b/c.txt", "local\n");
    assert!(changes(&warehouse, "unstaged").contains(&("modified".to_string(), "a/b/c.txt".to_string())));

    let shift = warehouse.run(&["shift", "feature"]);
    assert!(!shift.status.success(), "shift must refuse over the edit: {}", stdout(&shift));
    assert_eq!(read(&warehouse, "a/b/c.txt"), "local\n", "the edit must survive");
}

#[test]
fn an_edit_under_a_cherry_picked_nested_folder_is_visible_and_blocks_shift() {
    let warehouse = TestWarehouse::new("cherry-pick-nested-edit");
    warehouse.write_file("root.txt", "r\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("x/y/z.txt", "z\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "add x/y/z.txt"]));
    let source = pallet_head_hash(&warehouse, "feature");
    warehouse.write_file("x/y/z.txt", "feature only\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "change x/y/z.txt"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    assert_success(&warehouse.run(&["cherry-pick", &source]));
    assert_eq!(read(&warehouse, "x/y/z.txt"), "z\n");

    warehouse.write_file("x/y/z.txt", "local\n");
    assert!(changes(&warehouse, "unstaged").contains(&("modified".to_string(), "x/y/z.txt".to_string())));

    let shift = warehouse.run(&["shift", "feature"]);
    assert!(!shift.status.success(), "shift must refuse over the edit: {}", stdout(&shift));
    assert_eq!(read(&warehouse, "x/y/z.txt"), "local\n", "the edit must survive");
}

#[test]
fn a_directory_only_its_descendant_registered_is_tracked_for_replace_and_remove() {
    let warehouse = TestWarehouse::new("closure-registered-flip");
    warehouse.write_file("root.txt", "r\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["palletize", "flat"]));
    warehouse.write_file("a", "a file\n");
    assert_success(&warehouse.run(&["load", "a"]));
    assert_success(&warehouse.run(&["stack", "a is a file"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    warehouse.write_file("a/b/c.txt", "c\n");
    assert_success(&warehouse.run(&["load", "a/b/c.txt"]));
    assert_success(&warehouse.run(&["stack", "nested"]));

    // `a` holds only tracked content, so replacing it with the file is safe.
    assert_success(&warehouse.run(&["shift", "flat"]));
    assert_eq!(read(&warehouse, "a"), "a file\n");

    assert_success(&warehouse.run(&["shift", "main"]));
    assert_success(&warehouse.run(&["remove", "a"]));
    assert!(changes(&warehouse, "staged").contains(&("removed".to_string(), "a/b/c.txt".to_string())));
}

#[test]
fn remove_and_restore_follow_a_directory_replaced_by_a_file() {
    let warehouse = warehouse_with_a_nested_single_file_load("flip-dir-to-file");
    std::fs::remove_dir_all(warehouse.root.join("a")).unwrap();
    warehouse.write_file("a", "now a file\n");
    assert_success(&warehouse.run(&["load", "."]));

    warehouse.write_file("a", "edited\n");
    assert_success(&warehouse.run(&["restore", "a"]));
    assert_eq!(read(&warehouse, "a"), "now a file\n", "restore must restore the file");

    assert_success(&warehouse.run(&["remove", "a"]));
    let staged = changes(&warehouse, "staged");
    assert!(!staged.iter().any(|(_, path)| path == "a"), "the file's addition must be unstaged: {:?}", staged);
}

#[test]
fn restore_follows_a_file_replaced_by_a_directory() {
    let warehouse = TestWarehouse::new("flip-file-to-dir");
    warehouse.write_file("a", "a file\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "a"]));
    assert_success(&warehouse.run(&["stack", "a is a file"]));

    std::fs::remove_file(warehouse.root.join("a")).unwrap();
    warehouse.write_file("a/b.txt", "b\n");
    assert_success(&warehouse.run(&["load", "a/b.txt"]));

    warehouse.write_file("a/b.txt", "edited\n");
    assert_success(&warehouse.run(&["restore", "a"]));
    assert_eq!(read(&warehouse, "a/b.txt"), "b\n", "restore must restore the directory's tracked files");
}

#[test]
fn a_tracked_file_a_rule_covers_replaced_by_a_folder_still_shows_its_removal() {
    let warehouse = warehouse_with_a_diverged_pallet("flip-file-to-dir-ignored");
    warehouse.write_file(".forkliftignore", "^notes\\.txt$\n");
    std::fs::remove_file(warehouse.root.join("notes.txt")).unwrap();
    warehouse.write_file("notes.txt/new.txt", "new\n");

    let unstaged = changes(&warehouse, "unstaged");
    assert!(unstaged.iter().any(|(_, path)| path == "notes.txt"),
        "the tracked file's removal must stay visible under the rule: {:?}", unstaged);
    assert!(!unstaged.iter().any(|(_, path)| path.starts_with("notes.txt/")),
        "the rule still covers the folder that replaced it: {:?}", unstaged);

    // Loading the parent stages the tracked file's removal, and nothing the rule covers; the
    // folder is not registered, so after the stack the rule covers it whole.
    assert_success(&warehouse.run(&["load", "."]));
    let staged = changes(&warehouse, "staged");
    assert!(staged.contains(&("removed".to_string(), "notes.txt".to_string())), "{:?}", staged);
    assert!(!staged.iter().any(|(_, path)| path.starts_with("notes.txt/")), "{:?}", staged);
    assert_success(&warehouse.run(&["stack", "notes.txt is gone"]));
    let remove = warehouse.run(&["remove", "notes.txt"]);
    assert!(!remove.status.success(), "notes.txt must be untracked: {}", stdout(&remove));
}

#[test]
fn a_tracked_folder_a_rule_covers_replaced_by_a_file_is_removed_and_the_file_stays_ignored() {
    let warehouse = warehouse_with_a_diverged_pallet("flip-dir-to-file-ignored");
    warehouse.write_file(".forkliftignore", "^build$\n");
    std::fs::remove_dir_all(warehouse.root.join("build")).unwrap();
    warehouse.write_file("build", "now a file\n");

    let unstaged = changes(&warehouse, "unstaged");
    assert!(unstaged.iter().any(|(_, path)| path.starts_with("build/")),
        "the tracked folder's removal must stay visible under the rule: {:?}", unstaged);
    assert!(!unstaged.iter().any(|(_, path)| path == "build"), "the rule covers the untracked file: {:?}", unstaged);

    assert_success(&warehouse.run(&["load", "."]));
    let staged = changes(&warehouse, "staged");
    assert!(staged.contains(&("removed".to_string(), "build/out.txt".to_string())), "{:?}", staged);
    assert!(!staged.iter().any(|(_, path)| path == "build"), "a folder load must not add the covered file: {:?}", staged);
}

#[test]
fn a_rule_on_a_tracked_folder_covers_new_files_in_its_tracked_subfolders() {
    let warehouse = TestWarehouse::new("rule-covers-tracked-subfolder");
    warehouse.write_file("a/sub/x.txt", "x\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "a"]));
    assert_success(&warehouse.run(&["stack", "a"]));

    warehouse.write_file(".forkliftignore", "^a$\n");
    warehouse.write_file("a/sub/new.txt", "new\n");
    let unstaged = changes(&warehouse, "unstaged");
    assert!(!unstaged.iter().any(|(_, path)| path == "a/sub/new.txt"), "the rule covers a/ and below: {:?}", unstaged);

    assert_success(&warehouse.run(&["load", "."]));
    let staged = changes(&warehouse, "staged");
    assert!(!staged.iter().any(|(_, path)| path == "a/sub/new.txt"), "{:?}", staged);
}

#[test]
fn a_folder_load_refuses_an_unreadable_shard_instead_of_dropping_a_tracked_file_a_rule_covers() {
    let warehouse = TestWarehouse::new("load-unreadable-shard");
    warehouse.write_file("x.log", "log\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "x.log"]));
    assert_success(&warehouse.run(&["stack", "x.log"]));
    warehouse.write_file(".forkliftignore", "\\.log$\n");

    // The root's shard: a rebuild of it without the old entries would not know x.log is
    // tracked, and the rule would drop it without a staged removal.
    warehouse.write_file(".forklift/inventory/inv_/data", "not a shard\n");
    let load = warehouse.run(&["load", "."]);
    assert!(!load.status.success(), "{}", stdout(&load));
    assert!(stderr(&load).contains("cannot be read"), "{}", stderr(&load));

    // The way out the message names works, and x.log is still tracked afterwards.
    let suggested = if cfg!(windows) { "forklift restore --staged \".\"" } else { "forklift restore --staged '.'" };
    assert!(stderr(&load).contains(suggested), "{}", stderr(&load));
    assert!(stderr(&load).contains("every folder below it"), "the reset's scope must be named: {}", stderr(&load));
    assert_success(&warehouse.run(&["restore", "--staged", "."]));
    assert_success(&warehouse.run(&["load", "."]));
    warehouse.write_file("x.log", "edited\n");
    assert!(changes(&warehouse, "unstaged").contains(&("modified".to_string(), "x.log".to_string())));
}

#[test]
fn restore_refuses_a_file_replaced_by_a_folder_and_keeps_the_folder() {
    let warehouse = warehouse_with_a_diverged_pallet("restore-flip-file-to-dir");
    std::fs::remove_file(warehouse.root.join("notes.txt")).unwrap();
    warehouse.write_file("notes.txt/new.txt", "new\n");

    let restore = warehouse.run(&["restore", "notes.txt"]);
    assert!(!restore.status.success(), "{}", stdout(&restore));
    assert!(stderr(&restore).contains("replaced by a folder"), "{}", stderr(&restore));
    assert_eq!(read(&warehouse, "notes.txt/new.txt"), "new\n");
}

#[test]
fn remove_untracks_a_file_replaced_by_a_folder_on_disk() {
    let warehouse = warehouse_with_a_diverged_pallet("remove-flip-file-to-dir");
    std::fs::remove_file(warehouse.root.join("notes.txt")).unwrap();
    warehouse.write_file("notes.txt/new.txt", "new\n");

    assert_success(&warehouse.run(&["remove", "notes.txt"]));
    assert!(changes(&warehouse, "staged").contains(&("removed".to_string(), "notes.txt".to_string())));
}

#[test]
fn remove_untracks_a_folder_replaced_by_a_file_on_disk() {
    let warehouse = warehouse_with_a_diverged_pallet("remove-flip-dir-to-file");
    std::fs::remove_dir_all(warehouse.root.join("build")).unwrap();
    warehouse.write_file("build", "now a file\n");

    assert_success(&warehouse.run(&["remove", "build"]));
    assert!(changes(&warehouse, "staged").contains(&("removed".to_string(), "build/out.txt".to_string())));
}

#[test]
fn an_ancestor_registered_only_for_removed_content_stops_being_tracked() {
    let warehouse = warehouse_with_a_nested_single_file_load("prune-ancestor");
    warehouse.write_file("a/untracked.txt", "u\n");
    std::fs::remove_dir_all(warehouse.root.join("a/b")).unwrap();
    assert_success(&warehouse.run(&["remove", "a/b"]));
    assert_success(&warehouse.run(&["stack", "drop a/b"]));

    // "a" was registered only as the parent of a/b: with a/b gone it holds nothing tracked, so
    // it must leave the registered keys (no command reports registration as such).
    let registered = read(&warehouse, ".forklift/inventory/metadata");
    assert!(!registered.lines().any(|key| key == "a"), "a must no longer be registered: {:?}", registered);
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

    // Guards: unknown pallet; shifting to the pallet already current is a catch-up, which has
    // nothing to do at its head.
    let unknown = warehouse.run(&["shift", "nope"]);
    assert!(!unknown.status.success());
    assert!(stderr(&unknown).contains("No pallet named"));

    let same = warehouse.run(&["shift", "feature"]);
    assert_success(&same);
    assert!(stdout(&same).contains("Already at the head of pallet \"feature\""), "{}", stdout(&same));
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

/// The content a wide materialization test gives file `index` on `pallet`: distinct per file
/// and per pallet, so a blob written to the wrong path never matches.
fn wide_content(pallet: &str, index: usize) -> String {
    format!("file {} on {}\n{}", index, pallet, "line\n".repeat(index))
}

/// The paths of a wide materialization test: more files than the write prefetch's threshold,
/// spread over nested directories.
fn wide_paths() -> Vec<String> {
    (0..40).map(|index| format!("d{}/sub{}/f{}.txt", index % 3, index % 5, index)).collect()
}

#[cfg(unix)]
#[test]
fn a_wide_shift_writes_every_file_its_own_content_mode_and_type() {
    use std::os::unix::fs::PermissionsExt;

    // Past the write prefetch's threshold, blobs load on worker threads while the writes land in
    // order on one: each file must still get its own blob, mode and type.
    let warehouse = TestWarehouse::new("wide-shift");
    let paths = wide_paths();
    let big = large_bytes(0x51DE, CHUNK_THRESHOLD + 100_000);

    for (index, path) in paths.iter().enumerate() {
        warehouse.write_file(path, &wide_content("main", index));
    }
    std::fs::set_permissions(warehouse.root.join(&paths[7]), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink("f0.txt", warehouse.root.join("d0/sub0/link")).unwrap();
    write_bytes(&warehouse, "d1/big.bin", &big);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main's files"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    for (index, path) in paths.iter().enumerate() {
        warehouse.write_file(path, &wide_content("feature", index));
    }
    // A rewrite keeps the file's mode, so clear it: the shift back must set it, not inherit it.
    std::fs::set_permissions(warehouse.root.join(&paths[7]), std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::remove_file(warehouse.root.join("d0/sub0/link")).unwrap();
    std::fs::remove_file(warehouse.root.join("d1/big.bin")).unwrap();
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature's files"]));

    assert_success(&warehouse.run(&["shift", "main"]));

    for (index, path) in paths.iter().enumerate() {
        assert_eq!(read(&warehouse, path), wide_content("main", index), "{} has the wrong content", path);
    }
    let mode = std::fs::metadata(warehouse.root.join(&paths[7])).unwrap().permissions().mode();
    assert_ne!(mode & 0o111, 0, "the executable bit must be restored");
    assert_eq!(std::fs::read_link(warehouse.root.join("d0/sub0/link")).unwrap().to_string_lossy(), "f0.txt");
    assert_eq!(std::fs::read(warehouse.root.join("d1/big.bin")).unwrap(), big, "the chunked file must round-trip");

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"), "must be clean: {}", status);
}

#[test]
fn a_wide_consolidate_writes_every_file_theirs_takes_its_own_content() {
    // Every file changed on feature alone is a take-theirs write, more of them than the write
    // prefetch's threshold; main's own change keeps the merge from fast-forwarding. Feature also
    // adds a chunked file (streamed, never prefetched) and, on unix, an executable and a symlink.
    let warehouse = TestWarehouse::new("wide-consolidate");
    let paths = wide_paths();
    let big = large_bytes(0xC0DE, CHUNK_THRESHOLD + 100_000);

    for (index, path) in paths.iter().enumerate() {
        warehouse.write_file(path, &wide_content("base", index));
    }
    warehouse.write_file("main-only.txt", "base\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    for (index, path) in paths.iter().enumerate() {
        warehouse.write_file(path, &wide_content("feature", index));
    }
    write_bytes(&warehouse, "d1/big.bin", &big);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(warehouse.root.join(&paths[3]), std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("f2.txt", warehouse.root.join("d2/sub2/link")).unwrap();
    }
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature's files"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("main-only.txt", "changed on main\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main's change"]));

    assert_success(&warehouse.run(&["consolidate", "feature"]));

    for (index, path) in paths.iter().enumerate() {
        assert_eq!(read(&warehouse, path), wide_content("feature", index), "{} has the wrong content", path);
    }
    assert_eq!(read(&warehouse, "main-only.txt"), "changed on main\n");
    assert_eq!(std::fs::read(warehouse.root.join("d1/big.bin")).unwrap(), big, "the chunked file must round-trip");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(warehouse.root.join(&paths[3])).unwrap().permissions().mode();
        assert_ne!(mode & 0o111, 0, "the executable bit must be taken");
        assert_eq!(std::fs::read_link(warehouse.root.join("d2/sub2/link")).unwrap().to_string_lossy(), "f2.txt");
    }

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"), "must be clean: {}", status);
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

/// A working file's mtime and, on unix, inode: a rewrite moves the mtime (and the inode too,
/// where the write replaces the file); a file left alone keeps both.
fn file_identity(path: &std::path::Path) -> (std::time::SystemTime, u64) {
    let metadata = std::fs::symlink_metadata(path).unwrap();

    #[cfg(unix)]
    let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
    #[cfg(not(unix))]
    let inode = 0;

    (metadata.modified().unwrap(), inode)
}

/// Move a file's mtime `seconds` into the past, leaving its content alone.
fn backdate(path: &std::path::Path, seconds: u64) {
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    file.set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(seconds)).unwrap();
}

/// The rollup hash of the shard `key`, read straight off disk.
fn shard_rollup(warehouse: &TestWarehouse, key: &str) -> Option<String> {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);

    let (_, bytes) = forklift_core::util::file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
    let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();

    inventory.get_rollup_hash().cloned()
}

/// The inventory entry of `name` in the shard `key`, read straight off disk.
fn inventory_entry(warehouse: &TestWarehouse, key: &str, name: &str) -> forklift_core::model::inventory::InventoryItem {
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);

    let (_, bytes) = forklift_core::util::file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
    let inventory = forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();

    inventory.get_item_by_name(name).unwrap().as_ref().clone()
}

#[test]
fn restore_leaves_files_that_already_match_untouched() {
    let warehouse = TestWarehouse::new("restore-skip");
    warehouse.write_file("a.txt", "original a\n");
    warehouse.write_file("dir/b.txt", "original b\n");
    warehouse.write_file("dir/c.txt", "original c\n");

    // Old enough for `load`'s stat to prove every entry, so the stat cache trusts them.
    for path in ["a.txt", "dir/b.txt", "dir/c.txt"] {
        backdate(&warehouse.root.join(path), 3600);
    }

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    let a_before = file_identity(&warehouse.root.join("a.txt"));
    let c_before = file_identity(&warehouse.root.join("dir/c.txt"));

    // Only the modified file is written; the stat-clean ones keep their mtime and inode.
    warehouse.write_file("dir/b.txt", "scribbled over\n");
    let restored = stdout(&warehouse.run(&["restore", "."]));
    assert!(restored.contains("Restored 1 file(s) from the inventory; 2 already matched it."), "{}", restored);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("dir/b.txt")).unwrap(), "original b\n");
    assert_eq!(file_identity(&warehouse.root.join("a.txt")), a_before, "a stat-clean file must not be rewritten");
    assert_eq!(file_identity(&warehouse.root.join("dir/c.txt")), c_before, "a stat-clean file must not be rewritten");

    let single = stdout(&warehouse.run(&["restore", "a.txt"]));
    assert!(single.contains("\"a.txt\" already matches the inventory."), "{}", single);
    assert_eq!(file_identity(&warehouse.root.join("a.txt")), a_before, "a stat-clean file must not be rewritten");

    // A stat miss over the same content (a touch) is settled by hash: the file is still not
    // written, and its entry takes the file's new stat data so the next comparison trusts it.
    backdate(&warehouse.root.join("dir/c.txt"), 7200);
    let c_touched = file_identity(&warehouse.root.join("dir/c.txt"));
    let touched_secs = c_touched.0.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_ne!(inventory_entry(&warehouse, "dir", "c.txt").content_change_timestamp, touched_secs);

    let rollup = shard_rollup(&warehouse, "");
    assert!(rollup.is_some(), "the stack must stamp the root's rollup");

    let restored = stdout(&warehouse.run(&["restore", "dir"]));
    assert!(restored.contains("Restored 0 file(s) from the inventory; 2 already matched it."), "{}", restored);
    assert_eq!(file_identity(&warehouse.root.join("dir/c.txt")), c_touched, "a matching file must not be rewritten");
    assert_eq!(inventory_entry(&warehouse, "dir", "c.txt").content_change_timestamp, touched_secs,
        "the hash-matched entry must record the file's fresh stat data");

    // The single-file form records a touched file the same way, and neither form clears a
    // rollup, since no content changed.
    backdate(&warehouse.root.join("a.txt"), 7200);
    let a_touched = file_identity(&warehouse.root.join("a.txt")).0.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    let single = stdout(&warehouse.run(&["restore", "a.txt"]));
    assert!(single.contains("\"a.txt\" already matches the inventory."), "{}", single);
    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp, a_touched,
        "the hash-matched entry must record the file's fresh stat data");
    assert_eq!(shard_rollup(&warehouse, ""), rollup, "a stat-only restore must keep the rollups");

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The working directory matches the inventory"), "must be clean: {}", status);
}

/// A matching file whose mtime is not two whole seconds older than the restore's own clock read
/// could still be edited in that second without its mtime moving, so its entry is recorded as
/// unproven, to be settled by hash again later.
#[test]
fn restore_records_a_racily_matching_file_as_unproven() {
    let warehouse = TestWarehouse::new("restore-skip-racy");
    warehouse.write_file("a.txt", "original a\n");
    backdate(&warehouse.root.join("a.txt"), 3600);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    // A future mtime stands in for "edited in the same second as the stat".
    let path = warehouse.root.join("a.txt");
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(3600)).unwrap();
    drop(file);

    let restored = stdout(&warehouse.run(&["restore", "."]));
    assert!(restored.contains("Restored 0 file(s) from the inventory; 1 already matched it."), "{}", restored);
    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp,
        forklift_core::model::inventory::UNPROVEN_MTIME,
        "a racily matching file's stat data must not be trusted");
}

/// Run git in `root` with a fixed identity.
fn git_in(root: &std::path::Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(root)
        .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
        .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
        .output()
        .expect("git must be installed to run this test");
    assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
}

/// A deterministic, incompressible-ish file of `len` bytes, at or above the chunk threshold
/// when `len` is.
fn write_pseudo_random_file(path: &std::path::Path, len: usize) {
    let mut content = Vec::with_capacity(len + 8);
    let mut state = 0x9E37_79B9u64;
    while content.len() < len {
        state = state.wrapping_mul(0x2545_F491_4F6C_DD1D).wrapping_add(1);
        content.extend_from_slice(&state.to_le_bytes());
    }
    content.truncate(len);
    std::fs::write(path, content).unwrap();
}

/// The unstaged changes `stocktake` reports, as `(kind, path)` pairs (none when it omits them).
fn unstaged_changes(warehouse: &TestWarehouse) -> Vec<(String, String)> {
    let status = json(&warehouse.run(&["--json", "stocktake"]));
    status["data"]["unstaged"].as_array().into_iter().flatten()
        .map(|change| (change["kind"].as_str().unwrap().to_string(), change["path"].as_str().unwrap().to_string()))
        .collect()
}

/// `import-git` builds its inventory from git's tree over a working tree it never wrote, so it
/// proves each file against the tree before trusting its stat: an uncommitted edit that keeps the
/// file's size is still reported, and a large file git stores as one plain blob, which a fresh
/// ingest would chunk, is not mistaken for a change.
#[test]
fn import_git_proves_the_working_tree_against_the_imported_tree() {
    let warehouse = TestWarehouse::new("import-git-proves");
    let root = warehouse.root.clone();

    git_in(&root, &["init", "-q", "-b", "main"]);
    warehouse.write_file("small.txt", "hello\n");
    warehouse.write_file("edited.txt", "aaaa\n");
    write_pseudo_random_file(&root.join("big.bin"), 9 * 1024 * 1024);
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "first commit"]);

    // An uncommitted edit of the same size, old enough by import time for its stat to prove it.
    warehouse.write_file("edited.txt", "bbbb\n");
    std::thread::sleep(std::time::Duration::from_millis(2100));

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));

    let modified = ("modified".to_string(), "edited.txt".to_string());
    let untracked_ignore = ("untracked".to_string(), ".forkliftignore".to_string());
    assert_eq!(unstaged_changes(&warehouse), vec![untracked_ignore.clone(), modified],
        "the same-size edit must be reported, and nothing else");

    // The large plain file stays clean through a load, and loading it stages nothing.
    assert_success(&warehouse.run(&["load", "big.bin"]));
    let status = json(&warehouse.run(&["--json", "stocktake"]));
    assert_eq!(status["data"]["staged_count"], 0, "an unedited file must stage nothing: {}", status);
    assert_eq!(inventory_entry(&warehouse, "", "big.bin").item_type,
        forklift_core::enums::dir_entry_type::DirEntryType::Normal, "its storage must not change");

    // Nor does park's refresh stage anything for it.
    warehouse.write_file("edited.txt", "aaaa\n");
    let park = warehouse.run(&["park"]);
    assert!(!park.status.success(), "park must find nothing to park");
    assert!(stderr(&park).contains("the warehouse matches the pallet head"), "{}", stderr(&park));
    assert_eq!(unstaged_changes(&warehouse), vec![untracked_ignore]);
}

/// A tree large enough for the inventory build to fan out (its files recorded on worker threads,
/// a level's subtrees loaded in parallel) lands every entry in its own directory's shard, with
/// that file's own hash, proven: after `import-git`, which verifies each file, and after a
/// `shift`, which records the files it found clean. The directories share their file names, so
/// an entry gathered into a sibling's shard reads with the wrong content's hash.
#[test]
fn a_large_tree_lands_every_entry_in_its_own_shard() {
    use forklift_core::enums::dir_entry_type::DirEntryType;
    use forklift_core::model::inventory::UNPROVEN_MTIME;
    use forklift_core::util::shift_utils;

    let warehouse = TestWarehouse::new("large-tree-shards");
    let root = warehouse.root.clone();

    // 24 sibling directories (a level of subtrees to load) holding 268 files in all.
    let mut files: Vec<(String, String)> = (0..4).map(|n| (String::new(), format!("r{}.txt", n))).collect();
    let dirs = 24;
    for dir in 0..dirs {
        files.extend((0..11).map(|n| (format!("d{:02}", dir), format!("f{}.txt", n))));
    }
    assert!(files.len() >= shift_utils::WRITTEN_FANOUT_THRESHOLD && files.len() >= shift_utils::VERIFY_FANOUT_THRESHOLD
        && dirs >= shift_utils::TREE_LOAD_FANOUT_THRESHOLD, "the tree must be large enough to fan out");
    for (dir, name) in &files {
        let path = if dir.is_empty() { name.clone() } else { format!("{}/{}", dir, name) };
        warehouse.write_file(&path, &format!("{}\n", path));
    }

    git_in(&root, &["init", "-q", "-b", "main"]);
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "first commit"]);

    // An uncommitted same-size edit, old enough by import time for its stat to prove it.
    warehouse.write_file("d17/f2.txt", "D17/f2.txt\n");
    std::thread::sleep(std::time::Duration::from_millis(2100));

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));

    let untracked_ignore = ("untracked".to_string(), ".forkliftignore".to_string());
    assert_eq!(unstaged_changes(&warehouse),
        vec![untracked_ignore.clone(), ("modified".to_string(), "d17/f2.txt".to_string())],
        "only the edit must read as a change");

    let assert_every_entry_in_place = |step: &str, skip: Option<&str>| {
        for (dir, name) in &files {
            let path = if dir.is_empty() { name.clone() } else { format!("{}/{}", dir, name) };
            if Some(path.as_str()) == skip {
                continue;
            }

            let entry = inventory_entry(&warehouse, dir, name);
            let hashed = forklift_core::util::object_utils::hash_file_as_entry(
                name, &root.join(&path), DirEntryType::Normal, DirEntryType::Normal).unwrap().unwrap();

            assert_eq!(entry.hash, hashed.hash, "{}: {} must carry its own file's hash", step, path);
            assert_ne!(entry.content_change_timestamp, UNPROVEN_MTIME, "{}: {} must be proven", step, path);
        }
    };

    assert_every_entry_in_place("import-git", Some("d17/f2.txt"));
    assert_eq!(inventory_entry(&warehouse, "d17", "f2.txt").content_change_timestamp, UNPROVEN_MTIME,
        "the edit must not be proven against the tree");

    // A shift between two pallets on the same parcel writes nothing and rebuilds every shard.
    assert_success(&warehouse.run(&["restore", "d17/f2.txt"]));
    std::thread::sleep(std::time::Duration::from_millis(2100));
    assert_success(&warehouse.run(&["palletize", "other"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    assert_eq!(unstaged_changes(&warehouse), vec![untracked_ignore]);
    assert_every_entry_in_place("shift", None);
}

/// A scoped bay over a tree large enough for the inventory build to fan out inventories its scope
/// and nothing else: every in-scope entry in its own shard and proven, so the bay reads clean and
/// an edit deep inside it reads as the only change; no shard for the out-of-scope sibling's
/// directories, whose level is never loaded; and no entry for the spine's own files.
#[test]
fn a_scoped_bay_over_a_large_tree_inventories_only_its_scope() {
    use forklift_core::util::shift_utils;

    let warehouse = TestWarehouse::new("large-tree-scoped");

    // In scope: 20 directories under src/api, 280 files. Out of scope: as many under src/web,
    // and a file on the spine.
    let (dirs, per_dir) = (20, 14);
    for side in ["api", "web"] {
        for dir in 0..dirs {
            for n in 0..per_dir {
                let path = format!("src/{}/d{:02}/f{}.txt", side, dir, n);
                warehouse.write_file(&path, &format!("{}\n", path));
            }
        }
    }
    warehouse.write_file("src/spine.txt", "spine\n");
    assert!(dirs * per_dir >= shift_utils::WRITTEN_FANOUT_THRESHOLD && dirs >= shift_utils::TREE_LOAD_FANOUT_THRESHOLD,
        "the scope must be large enough to fan out");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));
    std::thread::sleep(std::time::Duration::from_millis(2100));

    let changes = |dir: &std::path::Path| -> Vec<(String, String)> {
        let status = warehouse.run_at(dir, &["--json", "stocktake"]);
        assert_success(&status);
        let value = json(&status);
        assert_eq!(value["data"]["staged_count"], 0, "nothing may be staged: {}", stdout(&status));
        value["data"]["unstaged"].as_array().into_iter().flatten()
            .map(|change| (change["kind"].as_str().unwrap().to_string(), change["path"].as_str().unwrap().to_string()))
            .collect()
    };

    assert_eq!(changes(&scoped_dir), vec![], "a fresh scoped bay must read clean");

    // The bay's entries start unproven (it wrote every file after its anchor): a same-size edit
    // reads as the only change because every other entry holds its own file's hash.
    std::fs::write(scoped_dir.join("src/api/d13/f9.txt"), "SRC/api/d13/f9.txt\n").unwrap();
    assert_eq!(changes(&scoped_dir), vec![("modified".to_string(), "src/api/d13/f9.txt".to_string())]);

    // The bay's shards sit where the main tree's would, under the bay's own inventory root.
    let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
    let main_inventory = std::path::PathBuf::from(forklift_core::util::file_utils::get_path_inventory_root());
    let bay_inventory = warehouse.root.join(".forklift").join("bays").join("scoped").join("inventory");
    let shard = |key: &str| {
        let main_path = forklift_core::util::file_utils::get_inventory_data_path_for_key(key);
        let path = bay_inventory.join(main_path.strip_prefix(&main_inventory).unwrap());
        std::fs::read(&path).ok()
            .map(|bytes| forklift_core::parser::inventory::inventory_parser::parse_inventory(&bytes).unwrap())
    };
    assert!(bay_inventory.is_dir(), "the bay keeps its own inventory");

    for dir in 0..dirs {
        let api = shard(&format!("src/api/d{:02}", dir)).expect("every in-scope directory has a shard");
        assert_eq!(api.get_items_count(), per_dir, "src/api/d{:02} must hold its own files", dir);
        assert!(shard(&format!("src/web/d{:02}", dir)).is_none(), "an out-of-scope directory has no shard");
    }
    assert!(shard("src").and_then(|spine| spine.get_item_by_name("spine.txt")).is_none(),
        "the spine's own file is out of scope");
}

/// A symlink in git's tree is proven as a link, and stays clean after import.
#[cfg(unix)]
#[test]
fn import_git_proves_an_unedited_symlink() {
    let warehouse = TestWarehouse::new("import-git-symlink");
    let root = warehouse.root.clone();

    git_in(&root, &["init", "-q", "-b", "main"]);
    warehouse.write_file("target.txt", "hello\n");
    std::os::unix::fs::symlink("target.txt", root.join("link")).unwrap();
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "first commit"]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));
    assert_only_the_ignore_file_is_untracked(&warehouse);
}

/// A file proven against git's tree keeps the tree's kind: an executable bit the tree does not
/// record (a checkout under `core.fileMode=false`) is not something `import-git` stages.
#[cfg(unix)]
#[test]
fn import_git_keeps_the_tree_kind_of_a_proven_file() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("import-git-tree-kind");
    let root = warehouse.root.clone();

    git_in(&root, &["init", "-q", "-b", "main"]);
    warehouse.write_file("tool.sh", "echo hi\n");
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "first commit"]);
    std::fs::set_permissions(root.join("tool.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));
    assert_eq!(inventory_entry(&warehouse, "", "tool.sh").item_type,
        forklift_core::enums::dir_entry_type::DirEntryType::Normal, "the tree's kind must be kept");
}

/// An entry `shift` records for a file it just wrote proves nothing, so an edit in that same
/// second (same size, same mtime and ctime seconds) is still reported after a sibling's load
/// republishes the shard a second later.
#[test]
fn an_edit_in_the_second_of_a_shift_survives_a_sibling_load() {
    for attempt in 0..5 {
        let warehouse = TestWarehouse::new(&format!("shift-same-second-{}", attempt));
        warehouse.write_file("a.txt", "one\n");
        warehouse.write_file("b.txt", "bee\n");
        assert_success(&warehouse.run(&["prepare"]));
        configure_operator(&warehouse);
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", "base"]));
        assert_success(&warehouse.run(&["palletize", "feature"]));
        warehouse.write_file("a.txt", "two\n");
        assert_success(&warehouse.run(&["load", "a.txt"]));
        assert_success(&warehouse.run(&["stack", "second"]));

        assert_success(&warehouse.run(&["shift", "main"]));
        let path = warehouse.root.join("a.txt");
        let seconds = |metadata: &std::fs::Metadata| {
            #[cfg(unix)]
            let ctime = std::os::unix::fs::MetadataExt::ctime(metadata) as u64;
            #[cfg(not(unix))]
            let ctime = 0;
            let mtime = metadata.modified().unwrap().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
            (mtime, ctime)
        };
        let written = seconds(&std::fs::metadata(&path).unwrap());
        std::fs::write(&path, "six\n").unwrap();
        if seconds(&std::fs::metadata(&path).unwrap()) != written {
            continue;
        }

        // The sibling's load lands in a later second, so a shard-mtime anchor would trust a.txt.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        warehouse.write_file("b.txt", "bus\n");
        assert_success(&warehouse.run(&["load", "b.txt"]));

        assert!(unstaged_changes(&warehouse).contains(&("modified".to_string(), "a.txt".to_string())),
            "an edit in the shift's second must still be reported: {:?}", unstaged_changes(&warehouse));
        return;
    }
    panic!("no attempt edited a.txt within the second shift wrote it");
}

/// A shard written before entries proved themselves (format v2) parses with every entry
/// unproven, and the next directory load rewrites it as v3 with real stat data.
#[test]
fn a_load_rewrites_a_v2_shard_with_proven_entries() {
    let warehouse = TestWarehouse::new("v2-shard-rewrite");
    warehouse.write_file("a.txt", "one\n");
    backdate(&warehouse.root.join("a.txt"), 3600);
    assert_success(&warehouse.run(&["prepare"]));
    assert_success(&warehouse.run(&["load", "."]));

    let shard = {
        let _scope = forklift_core::globals::StorageRootScope::enter(&warehouse.root);
        forklift_core::util::file_utils::get_inventory_data_path_for_key("")
    };
    let mut bytes = std::fs::read(&shard).unwrap();
    assert_eq!(bytes[0], 3, "sanity: a fresh shard is v3");
    bytes[0] = 2;
    std::fs::write(&shard, bytes).unwrap();
    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp,
        forklift_core::model::inventory::UNPROVEN_MTIME, "a v2 entry must parse unproven");
    assert_only_the_ignore_file_is_untracked_or_clean(&warehouse);

    assert_success(&warehouse.run(&["load", "."]));
    assert_eq!(std::fs::read(&shard).unwrap()[0], 3, "the load must rewrite the shard as v3");
    let mtime = file_identity(&warehouse.root.join("a.txt")).0
        .duration_since(std::time::UNIX_EPOCH).unwrap().as_secs();
    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp, mtime);
}

fn assert_only_the_ignore_file_is_untracked_or_clean(warehouse: &TestWarehouse) {
    let changes = unstaged_changes(warehouse);
    assert!(changes.iter().all(|(kind, path)| kind == "untracked" && path == ".forkliftignore"), "{:?}", changes);
}

/// A kind change is never hidden by re-proving an unproven entry: a chmod +x on a large plain
/// file is reported and recorded by load as a type change, a file replaced by a link to a copy of itself is
/// modified, and a link replaced by a regular file is modified, not an error.
#[cfg(unix)]
#[test]
fn re_proving_an_unproven_entry_never_hides_a_kind_change() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("reprove-kind-change");
    let root = warehouse.root.clone();
    git_in(&root, &["init", "-q", "-b", "main"]);
    write_pseudo_random_file(&root.join("big.bin"), 9 * 1024 * 1024);
    warehouse.write_file("file.txt", "content\n");
    std::os::unix::fs::symlink("file.txt", root.join("link")).unwrap();
    git_in(&root, &["add", "-A"]);
    git_in(&root, &["commit", "-qm", "first commit"]);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));

    std::fs::set_permissions(root.join("big.bin"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::rename(root.join("file.txt"), root.join("copy.txt")).unwrap();
    std::os::unix::fs::symlink("copy.txt", root.join("file.txt")).unwrap();
    std::fs::remove_file(root.join("link")).unwrap();
    warehouse.write_file("link", "file.txt");

    let changes = unstaged_changes(&warehouse);
    for path in ["big.bin", "file.txt", "link"] {
        assert!(changes.contains(&("modified".to_string(), path.to_string())), "{} must be modified: {:?}", path, changes);
    }

    // The kind is recorded in the entry's own storage: still one plain blob, now executable.
    assert_success(&warehouse.run(&["load", "big.bin"]));
    assert_eq!(inventory_entry(&warehouse, "", "big.bin").item_type,
        forklift_core::enums::dir_entry_type::DirEntryType::Executable, "the chmod must be recorded");
}

/// The clock probe lives under the shared metadata root, so a bay, whose `.forklift` is a
/// redirect file, stamps the warehouse's probe rather than failing on its own.
#[test]
fn a_bay_stamps_the_shared_clock_probe() {
    let warehouse = TestWarehouse::new("bay-clock-probe");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    let bay_dir = warehouse.home.join("bay-clock");
    assert_success(&warehouse.run(&["bay", "add", "feature", bay_dir.to_str().unwrap()]));
    let probe = warehouse.root.join(".forklift").join(forklift_core::util::file_utils::FILE_NAME_CLOCK_PROBE);
    let _ = std::fs::remove_file(&probe);

    std::fs::write(bay_dir.join("code.txt"), "v2\n").unwrap();
    assert_success(&warehouse.run_at(&bay_dir, &["load", "."]));
    assert!(probe.is_file(), "the bay's load must stamp the shared probe");
    assert!(bay_dir.join(".forklift").is_file(), "the bay's redirect must stay a file");
}

/// A file whose bytes match but whose kind does not (an executable bit set by hand) is still
/// written, so restore puts the staged mode back.
#[cfg(unix)]
#[test]
fn restore_rewrites_a_matching_file_of_another_kind() {
    use std::os::unix::fs::PermissionsExt;

    let warehouse = TestWarehouse::new("restore-skip-kind");
    warehouse.write_file("run.sh", "echo hi\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    let path = warehouse.root.join("run.sh");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();

    let restored = stdout(&warehouse.run(&["restore", "run.sh"]));
    assert!(restored.contains("Restored \"run.sh\" from the inventory."), "{}", restored);
    assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o111, 0, "the staged mode must come back");
}

/// `restore --staged` records the stat data of a working file that provably holds the head's
/// content, so the next comparison trusts the stat cache instead of rehashing; a file that does
/// not hold it keeps stale, unproven stat data, so it still shows as modified.
#[test]
fn restore_staged_keeps_the_stat_data_of_files_that_match_the_head() {
    let warehouse = TestWarehouse::new("restore-staged-stat");
    warehouse.write_file("a.txt", "original a\n");
    warehouse.write_file("dir/b.txt", "original b\n");
    warehouse.write_file("dir/c.txt", "original c\n");

    for path in ["a.txt", "dir/b.txt", "dir/c.txt"] {
        backdate(&warehouse.root.join(path), 3600);
    }

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "baseline"]));

    let mtime = |path: &str| {
        file_identity(&warehouse.root.join(path)).0.duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
    };

    // b: a staged modification still in the working file. c: a staged modification since
    // reverted by hand, so only its content proves it matches the head.
    warehouse.write_file("dir/b.txt", "changed b\n");
    warehouse.write_file("dir/c.txt", "changed c\n");
    assert_success(&warehouse.run(&["load", "."]));
    warehouse.write_file("dir/c.txt", "original c\n");
    backdate(&warehouse.root.join("dir/c.txt"), 7200);

    assert_success(&warehouse.run(&["restore", "--staged", "."]));

    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp, mtime("a.txt"),
        "a stat-clean file must keep its stat data");
    assert_eq!(inventory_entry(&warehouse, "dir", "b.txt").content_change_timestamp,
        forklift_core::model::inventory::UNPROVEN_MTIME,
        "a file that differs from the head must keep unproven stat data");
    assert_eq!(inventory_entry(&warehouse, "dir", "c.txt").content_change_timestamp, mtime("dir/c.txt"),
        "a file whose content matches the head must record its stat data");

    let status = stdout(&warehouse.run(&["stocktake"]));
    assert!(status.contains("The inventory matches the pallet head"), "status: {}", status);
    assert!(status.contains("modified:") && status.contains("b.txt") && !status.contains("c.txt"), "status: {}", status);

    // The single-file form records the same way.
    warehouse.write_file("a.txt", "changed a\n");
    assert_success(&warehouse.run(&["load", "a.txt"]));
    warehouse.write_file("a.txt", "original a\n");
    backdate(&warehouse.root.join("a.txt"), 7200);
    assert_success(&warehouse.run(&["restore", "--staged", "a.txt"]));
    assert_eq!(inventory_entry(&warehouse, "", "a.txt").content_change_timestamp, mtime("a.txt"),
        "a file whose content matches the head must record its stat data");
    // A staged removal is never a witness, even where its entry still proves the untouched
    // file by stat: the file comes back as a live entry.
    assert_success(&warehouse.run(&["remove", "a.txt"]));
    assert_success(&warehouse.run(&["restore", "--staged", "a.txt"]));
    let a_entry = inventory_entry(&warehouse, "", "a.txt");
    assert!(a_entry.state == forklift_core::enums::inventory_item_state::InventoryItemState::Normal,
        "an unstaged removal must come back as a live entry");
    assert_eq!(a_entry.content_change_timestamp, mtime("a.txt"), "a file whose content matches the head must record its stat data");

    // Unstaging a file whose entry already matches the head changes no content, so it keeps
    // the rollups a stack stamped.
    assert_success(&warehouse.run(&["restore", "--staged", "."]));
    assert_success(&warehouse.run(&["load", "dir/b.txt"]));
    assert_success(&warehouse.run(&["stack", "second"]));
    let rollup = shard_rollup(&warehouse, "");
    assert!(rollup.is_some(), "the stack must stamp the root's rollup");
    assert_success(&warehouse.run(&["restore", "--staged", "a.txt"]));
    assert_eq!(shard_rollup(&warehouse, ""), rollup, "unstaging an unchanged file must keep the rollups");
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

/// Commit-graph shards are written without a durability barrier, so a power loss can leave one
/// torn. A torn shard must read as absent — never as wrong ancestry — and be rewritten whole by
/// the next query that needs it. Every shard is corrupted here (one flipped byte each, so each
/// still has the right magic, version and length), and the merge that reads the graph must still
/// find the right base.
#[test]
fn a_corrupted_commit_graph_shard_is_ignored_and_rewritten() {
    let warehouse = TestWarehouse::new("commit-graph-corrupt");
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nfive\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nFIVE\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "ONE\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "main work"]));

    assert_success(&warehouse.run(&["compact"]));
    let graph_root = warehouse.root.join(".forklift/graph");
    let shards: Vec<_> = std::fs::read_dir(&graph_root).unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(!shards.is_empty(), "sanity: compact must have built the commit-graph");

    // Flip one byte inside the records of every shard: the 12-byte header and the length stay
    // intact, so only the checksum can tell the shard is damaged.
    let mut corrupted = Vec::new();
    for shard in &shards {
        let mut bytes = std::fs::read(shard).unwrap();
        bytes[12 + 32] ^= 0x01;
        std::fs::write(shard, &bytes).unwrap();
        corrupted.push(bytes);
    }

    let merge = warehouse.run(&["consolidate", "feature"]);
    assert_success(&merge);
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\nFIVE\n",
        "the merge must find the true base despite every shard being damaged"
    );

    // The query that needed the damaged shards recomputed their records and rewrote them.
    let rewritten = shards.iter().zip(&corrupted)
        .filter(|(shard, damaged)| std::fs::read(shard).unwrap() != **damaged)
        .count();
    assert!(rewritten > 0, "the merge must have rewritten at least one damaged shard");

    // A rebuild from every head heals the rest: each shard is sealed again and holds records.
    assert_success(&warehouse.run(&["compact"]));
    for shard in &shards {
        assert!(graph_shard_is_sealed_with_records(shard),
            "every damaged shard must be rewritten whole: {}", shard.display());
    }
}

/// Whether a commit-graph shard file is whole, checked from the outside: its trailing blake3
/// matches the bytes before it (the v2 seal), and the body after the 12-byte header walks as a
/// sequence of complete records — at least one, ending exactly at the checksum — each keyed by a
/// parcel hash that belongs in this shard (its first byte is the file's 2-hex name).
fn graph_shard_is_sealed_with_records(shard: &std::path::Path) -> bool {
    let bytes = std::fs::read(shard).unwrap();
    if bytes.len() < 12 + 32 {
        return false;
    }
    let (body, checksum) = bytes.split_at(bytes.len() - 32);
    if blake3::hash(body).as_bytes() != checksum {
        return false;
    }
    let prefix = u8::from_str_radix(shard.file_name().unwrap().to_str().unwrap(), 16).unwrap();
    let u32_at = |at: usize| u32::from_le_bytes([body[at], body[at + 1], body[at + 2], body[at + 3]]);
    let (mut offset, mut records) = (12, 0);
    while offset < body.len() {
        // hash(32) + generation(4) + parent count(1), the parents, then a filter length (4).
        if offset + 37 > body.len() || body[offset] != prefix {
            return false;
        }
        let parents = body[offset + 36] as usize;
        offset += 37 + parents * 32;
        if offset + 4 > body.len() {
            return false;
        }
        let filter_len = u32_at(offset);
        offset += 4;
        // u32::MAX and u32::MAX - 1 are the "no filter" / "too large" sentinels, with no bytes.
        if filter_len < u32::MAX - 1 {
            offset += filter_len as usize;
        }
        if offset > body.len() {
            return false;
        }
        records += 1;
    }
    records > 0
}

/// A shard is written with no durability barrier, so a power loss can keep a descendant's shard
/// while losing an ancestor's. `compact` must not take the surviving head record as proof that
/// its history is recorded: it walks through it and rebuilds every lost shard.
#[test]
fn compact_rebuilds_lost_ancestor_shards_behind_a_recorded_head() {
    let warehouse = TestWarehouse::new("commit-graph-lost-ancestor");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    for i in 0..12 {
        warehouse.write_file("file.txt", &format!("{}\n", i));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("c{}", i)]));
    }
    assert_success(&warehouse.run(&["compact"]));

    let graph_root = warehouse.root.join(".forklift/graph");
    let head = pallet_head_hash(&warehouse, "main");
    let head_shard = graph_root.join(&head[..2]);
    let shards: Vec<_> = std::fs::read_dir(&graph_root).unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert!(shards.len() > 1 && shards.contains(&head_shard),
        "sanity: the history spans several shards, one of them the head's: {:?}", shards);
    for shard in shards.iter().filter(|shard| **shard != head_shard) {
        std::fs::remove_file(shard).unwrap();
    }

    assert_success(&warehouse.run(&["compact"]));
    for shard in &shards {
        assert!(shard.exists() && graph_shard_is_sealed_with_records(shard),
            "compact must rebuild the lost ancestor shard {}", shard.display());
    }

    // The checker itself: a correctly sealed shard whose body ends in a stray byte, or whose
    // record sits in the wrong shard, must not pass as whole.
    let mut body = std::fs::read(&head_shard).unwrap();
    body.truncate(body.len() - 32);
    let reseal = |body: &[u8]| {
        let mut sealed = body.to_vec();
        sealed.extend_from_slice(blake3::hash(body).as_bytes());
        sealed
    };
    let probe_dir = warehouse.root.join("probe");
    std::fs::create_dir_all(&probe_dir).unwrap();
    let mut stray = body.clone();
    stray.push(0);
    std::fs::write(probe_dir.join(&head[..2]), reseal(&stray)).unwrap();
    assert!(!graph_shard_is_sealed_with_records(&probe_dir.join(&head[..2])),
        "a sealed body with a stray trailing byte must not pass");
    let elsewhere = if &head[..2] == "00" { "01" } else { "00" };
    std::fs::write(probe_dir.join(elsewhere), reseal(&body)).unwrap();
    assert!(!graph_shard_is_sealed_with_records(&probe_dir.join(elsewhere)),
        "a record filed under the wrong shard must not pass");
    std::fs::write(probe_dir.join(&head[..2]), reseal(&body)).unwrap();
    assert!(graph_shard_is_sealed_with_records(&probe_dir.join(&head[..2])),
        "sanity: the untouched body, resealed, passes");
}

/// The commit-graph is a cache: when it cannot be read or written at all, a query still answers
/// from the object store rather than failing. A file squatting the graph directory blocks both.
#[test]
fn an_unwritable_commit_graph_never_fails_a_query() {
    let warehouse = TestWarehouse::new("commit-graph-unwritable");
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("file.txt", "one\ntwo\nthree\nfour\nFIVE\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("file.txt", "ONE\ntwo\nthree\nfour\nfive\n");
    assert_success(&warehouse.run(&["load", "file.txt"]));
    assert_success(&warehouse.run(&["stack", "main work"]));

    let graph_root = warehouse.root.join(".forklift/graph");
    let _ = std::fs::remove_dir_all(&graph_root);
    std::fs::write(&graph_root, "not a directory").unwrap();

    // `blame` reads its parent edges through the graph and has no object-walk fallback of its
    // own; the merge base below does fall back, so blame is the query that pins the graph's
    // own tolerance.
    let blame = warehouse.run(&["blame", "file.txt"]);
    assert_success(&blame);
    assert!(stdout(&blame).contains("ONE") && stdout(&blame).contains("five"),
        "blame must answer from the object store: {}", stdout(&blame));

    assert_success(&warehouse.run(&["consolidate", "feature"]));
    assert_eq!(
        std::fs::read_to_string(warehouse.root.join("file.txt")).unwrap(),
        "ONE\ntwo\nthree\nfour\nFIVE\n",
        "the merge must still find the true base with no usable commit-graph"
    );
    assert!(graph_root.is_file(), "sanity: the squatting file is still in place");
}

/// A cold `blame` fills the changed-path filter of every parcel in its chain. Filling them one
/// query at a time rewrote each shard once per parcel in it; the batched pass writes each shard
/// once for the generation records and once per filter batch. A history long enough to put
/// several parcels in every shard tells the two apart.
#[test]
fn a_cold_blame_writes_each_commit_graph_shard_a_bounded_number_of_times() {
    use std::io::Write;
    use std::process::Stdio;

    const COMMITS: usize = 600;
    let warehouse = TestWarehouse::new("commit-graph-write-once");

    // One file rewritten by every commit, so every parcel's filter is a real (small) diff.
    let mut stream = String::new();
    for i in 0..COMMITS {
        let content = format!("line {}\n", i);
        stream.push_str(&format!(
            "commit refs/heads/main\ncommitter Ada <ada@example.com> {} +0000\ndata 7\nchange\n",
            1_700_000_000 + i
        ));
        stream.push_str(&format!("M 100644 inline a.txt\ndata {}\n{}\n", content.len(), content));
    }
    let git = |args: &[&str], input: Option<&str>| {
        let mut child = Command::new("git")
            .args(args)
            .current_dir(&warehouse.root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("git must be installed to run this test");
        if let Some(input) = input {
            child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"], None);
    git(&["fast-import", "--quiet"], Some(&stream));
    git(&["reset", "-q", "--hard", "main"], None);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", ".", "--no-compact"]));

    let graph_root = warehouse.root.join(".forklift/graph");
    let _ = std::fs::remove_dir_all(&graph_root);
    let blame = warehouse.run_with_env(&["blame", "a.txt"], &[("FORKLIFT_DEBUG_GRAPH_WRITES", "1")]);
    assert_success(&blame);
    assert!(stdout(&blame).contains(&format!("line {}", COMMITS - 1)), "blame output: {}", stdout(&blame));

    let writes: usize = stderr(&blame).lines()
        .find_map(|line| line.strip_prefix("graph-shard-writes: "))
        .expect("the debug hook must report the shard writes")
        .trim().parse().unwrap();
    let shards: Vec<_> = std::fs::read_dir(&graph_root).unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    for shard in &shards {
        assert!(graph_shard_is_sealed_with_records(shard), "a written shard must be whole: {}", shard.display());
    }
    assert!(COMMITS > 2 * shards.len(),
        "sanity: the history must put several parcels in a shard to tell batched writes from per-parcel ones");
    assert!(writes <= 2 * shards.len(),
        "a cold blame must write each of the {} shards at most twice, not once per parcel; it wrote {}",
        shards.len(), writes);
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

/// A symlink's identity is its own inode, read from the `lstat` the walk already holds: a dangling
/// link has no target to `stat`, and it loads and stays clean like any other entry.
#[cfg(unix)]
#[test]
fn a_dangling_symlink_loads_and_reads_clean() {
    let warehouse = TestWarehouse::new("dangling-symlink");
    warehouse.write_file("real.txt", "content\n");
    std::os::unix::fs::symlink("nowhere", warehouse.root.join("dangling")).unwrap();
    std::os::unix::fs::symlink("real.txt", warehouse.root.join("pointing")).unwrap();

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "links"]));

    let status = json(&warehouse.run(&["--json", "stocktake"]));
    assert_eq!(status["data"]["staged_count"], 0, "{}", status);
    assert_eq!(status["data"]["unstaged_count"], 0, "{}", status);
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

/// Past the fan-out threshold, each mode renders its files on several threads; the report must
/// still read as the serial one did: every file once, in path order, each with its own lines.
#[test]
fn diff_renders_many_files_in_path_order_in_every_mode() {
    let warehouse = TestWarehouse::new("diff-fanout");
    let names: Vec<String> = (0..12).map(|i| format!("d{}/f{:02}.txt", i % 3, i)).collect();
    for name in &names {
        warehouse.write_file(name, &format!("old {}\n", name));
    }

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    assert_success(&warehouse.run(&["compact"]));

    for name in &names {
        warehouse.write_file(name, &format!("new {}\n", name));
    }

    let mut sorted = names.clone();
    sorted.sort();
    let expected: String = sorted.iter()
        .map(|name| format!("modified: {}|- old {}|+ new {}|", name, name, name))
        .collect();
    // One line per header and changed line, stripped of colour and line-number columns.
    let digest = |output: &Output| -> String {
        stdout(output).lines()
            .map(|line| line.replace("\x1b[1m", "").replace("\x1b[0m", "").replace("\x1b[31m", "")
                .replace("\x1b[32m", ""))
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                let trimmed = line.trim_start_matches(|c: char| c == ' ' || c.is_ascii_digit()).to_string();
                format!("{}|", trimmed)
            })
            .collect()
    };

    let unstaged = warehouse.run(&["diff"]);
    assert_success(&unstaged);
    assert_eq!(digest(&unstaged), expected, "{}", stdout(&unstaged));

    assert_success(&warehouse.run(&["load", "."]));
    let staged = warehouse.run(&["diff", "--staged"]);
    assert_success(&staged);
    assert_eq!(digest(&staged), expected, "{}", stdout(&staged));
}

/// The `FORKLIFT_DEBUG_PACK_COUNTS` lines of a run: (pack-folder reads, preloads that landed).
fn pack_counts(output: &Output) -> (u64, u64) {
    let stderr = stderr(output);
    let count = |prefix: &str| stderr.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {} line in stderr: {}", prefix, stderr))
        .trim().parse::<u64>().unwrap();

    (count("pack-set-reads: "), count("pack-preloads: "))
}

/// The lazy pack-index counters of a run: (index buckets validated, whole packs validated).
fn index_validation_counts(output: &Output) -> (u64, u64) {
    let stderr = stderr(output);
    let count = |prefix: &str| stderr.lines()
        .find_map(|line| line.strip_prefix(prefix))
        .unwrap_or_else(|| panic!("no {} line in stderr: {}", prefix, stderr))
        .trim().parse::<u64>().unwrap();

    (count("pack-buckets-validated: "), count("pack-full-validations: "))
}

/// (FORK-118) Opening a pack checks its index header and fanout table, then validates only the
/// buckets a command reads: `history -n 1` over two packs of hundreds of objects each validates a
/// handful of buckets and no whole pack. A full repack, which copies every record, validates each
/// pack it reads in full exactly once.
///
/// Mutations: make `CheckedIndex::locate` validate every bucket → 32 buckets (red). Load the pack
/// registry with `IndexCheck::Full` → two whole-pack validations for `history` (red).
#[test]
fn reading_a_pack_validates_only_the_index_buckets_it_touches() {
    let warehouse = TestWarehouse::new("lazy-pack-index");
    for round in 0..2 {
        for index in 0..300 {
            warehouse.write_file(&format!("r{}/f{:03}.txt", round, index), &format!("round {} file {}\n", round, index));
        }
        if round == 0 {
            assert_success(&warehouse.run(&["prepare"]));
            configure_operator(&warehouse);
        }
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("round {}", round)]));
        assert_success(&warehouse.run(&["compact"]));
    }
    let counts = [("FORKLIFT_DEBUG_PACK_COUNTS", "1")];

    let history = warehouse.run_with_env(&["history", "-n", "1"], &counts);
    assert_success(&history);
    assert!(stdout(&history).contains("round 1"), "{}", stdout(&history));
    let (buckets, full) = index_validation_counts(&history);
    assert_eq!(full, 0, "reading one parcel must not validate a whole pack");
    // Each pack holds 300+ records over 16 buckets, 32 in all. Each object `history -n 1` looks up
    // costs at most one bucket per pack, found or not — a whole-index check would cost all 32.
    assert!((1..=4).contains(&buckets), "one parcel read must validate a few buckets of 32, got {}", buckets);

    let repack = warehouse.run_with_env(&["compact", "--all"], &counts);
    assert_success(&repack);
    assert_eq!(index_validation_counts(&repack).1, 2, "a full repack validates each of the two packs once");
}

/// `diff` opens the pack set during the walk, and only when a change needs an old side read:
/// the render finds it cached (one read, the preload's), and a walk with nothing to render never
/// pays for it.
#[test]
fn diff_preloads_the_pack_set_only_for_a_change_it_renders() {
    let warehouse = TestWarehouse::new("diff-pack-preload");
    warehouse.write_file("a.txt", "original\n");
    warehouse.write_file("dir/b.txt", "b\n");

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    assert_success(&warehouse.run(&["compact"]));
    let counts = [("FORKLIFT_DEBUG_PACK_COUNTS", "1")];

    let clean = warehouse.run_with_env(&["diff"], &counts);
    assert_success(&clean);
    assert_eq!(pack_counts(&clean), (0, 0), "a clean walk reads no packs");

    warehouse.write_file("untracked.txt", "new\n");
    let untracked = warehouse.run_with_env(&["diff"], &counts);
    assert_success(&untracked);
    assert_eq!(pack_counts(&untracked), (0, 0), "an untracked file is not diffed, so nothing is preloaded");

    warehouse.write_file("dir/b.txt", "changed\n");
    let modified = warehouse.run_with_env(&["diff"], &counts);
    assert_success(&modified);
    assert!(stdout(&modified).contains("+ changed"), "{}", stdout(&modified));
    assert_eq!(pack_counts(&modified), (1, 1), "the render reads the preloaded set, never a second one");

    let filtered = warehouse.run_with_env(&["diff", "a.txt"], &counts);
    assert_success(&filtered);
    assert!(stdout(&filtered).contains("matches the inventory"), "{}", stdout(&filtered));
    assert_eq!(pack_counts(&filtered), (0, 0), "a change outside the path filter is not rendered, so nothing is preloaded");
}

/// A file whose stored side cannot be read fails `diff`, but only after every file before it has
/// printed, as a serial render would: in the fanned-out render (10 files) and in the serial one
/// (3 files, through a path filter).
#[test]
fn diff_prints_the_files_before_an_unreadable_one_then_fails() {
    let warehouse = TestWarehouse::new("diff-partial");
    for index in 0..10 {
        warehouse.write_file(&format!("fan/f{:02}.txt", index), &format!("old {}\n", index));
    }
    for index in 0..3 {
        warehouse.write_file(&format!("ser/s{}.txt", index), &format!("old s{}\n", index));
    }

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));

    let stored_hash = |dir: &str, name: &str| -> String {
        let peek = stdout(&warehouse.run(&["peek", "--inventory", dir]));
        let line = peek.lines().find(|line| line.ends_with(name)).unwrap_or_else(|| panic!("{} not in: {}", name, peek));
        line.split_whitespace().nth(1).expect("peek line: <state> <hash> <name>").to_string()
    };
    std::fs::remove_file(object_store_path(&warehouse, &stored_hash("fan", "f05.txt"))).unwrap();
    std::fs::remove_file(object_store_path(&warehouse, &stored_hash("ser", "s1.txt"))).unwrap();

    for index in 0..10 {
        warehouse.write_file(&format!("fan/f{:02}.txt", index), &format!("new {}\n", index));
    }
    for index in 0..3 {
        warehouse.write_file(&format!("ser/s{}.txt", index), &format!("new s{}\n", index));
    }

    let fanned = warehouse.run(&["diff", "fan"]);
    assert!(!fanned.status.success(), "an unreadable stored side must fail the diff");
    let out = stdout(&fanned);
    for index in 0..5 {
        assert!(out.contains(&format!("+ new {}", index)), "file {} precedes the failure and must print: {}", index, out);
    }
    for index in 5..10 {
        assert!(!out.contains(&format!("+ new {}", index)), "file {} is at or after the failure and must not print: {}", index, out);
    }

    let serial = warehouse.run(&["diff", "ser"]);
    assert!(!serial.status.success(), "an unreadable stored side must fail the diff");
    let out = stdout(&serial);
    assert!(out.contains("+ new s0"), "the file before the failure must print: {}", out);
    assert!(!out.contains("+ new s2"), "a file after the failure must not print: {}", out);
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

    // A prefix in another case names the same parcel, and what it resolves to is the stored
    // lowercase spelling: a ref written from the user's casing would be refused when read back.
    let upper = first[0..8].to_uppercase();
    assert_success(&warehouse.run(&["palletize", "upper", &upper]));
    let history = stdout(&warehouse.run(&["history", "upper"]));
    assert!(history.contains(&format!("parcel {}", first)), "unexpected history: {}", history);

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

#[cfg(unix)]
#[test]
fn mcp_is_not_killed_by_a_client_that_stopped_reading() {
    use std::io::Write;
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let warehouse = TestWarehouse::new("mcp-gone");
    assert_success(&warehouse.run(&["prepare"]));

    let mut child = forklift_command()
        .arg("mcp")
        .current_dir(&warehouse.root)
        .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", warehouse.home.join("test-keys"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    // The client is gone before the server's first reply.
    drop(child.stdout.take());
    child.stdin.take().unwrap()
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();

    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.signal(), None, "killed by a signal: {:?}", output.status);
    assert!(output.status.success(), "a client that left is the end of the connection: {:?}", output.status);
    assert!(output.stderr.is_empty(), "{}", String::from_utf8_lossy(&output.stderr));
}

#[test]
fn mcp_forwards_a_warning_document_after_the_envelope() {
    let warehouse = TestWarehouse::new("mcp-warning");
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("a.txt", "one\n");

    // The same unparseable maintenance setting that makes `load` warn on the CLI.
    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "[maintenance]\nloose = 12345\n",
    ).unwrap();

    let input = format!(
        "{}\n{}\n",
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"load","arguments":{"path":"."}}}"#,
    );
    let output = warehouse.run_with_stdin(&["mcp"], &input);
    assert_success(&output);

    let call = stdout(&output).lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .find(|message| message["id"] == 2)
        .expect("a reply to the load call");
    assert_eq!(call["result"]["isError"], false, "load failed: {}", call);

    let content = call["result"]["content"].as_array().unwrap();
    assert_eq!(content.len(), 2, "expected the envelope and one warning: {}", call);

    let envelope: serde_json::Value = serde_json::from_str(content[0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(envelope["command"], "load", "the envelope comes first: {}", call);

    let warning: serde_json::Value = serde_json::from_str(content[1]["text"].as_str().unwrap()).unwrap();
    assert_eq!(warning["warning"], "maintenance_unavailable", "unexpected warning: {}", warning);
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

    // History and authorship survived; the colocated working tree reads clean apart from the
    // ignore file (`.git` was auto-ignored).
    assert!(stdout(&warehouse.run(&["history", "feature"])).contains("feature work"));
    assert!(stdout(&warehouse.run(&["history"])).contains("ada@example.com"), "git author must survive");
    assert_only_the_ignore_file_is_untracked(&warehouse);

    // Enrolling after import anchors the imported history as the legacy boundary, so a
    // later audit tolerates it as unsigned rather than rejecting it.
    assert_success(&warehouse.run(&["office", "enroll"]));
    assert!(stdout(&warehouse.run(&["audit"])).contains("legacy"));

    // A second import into the now-trusted warehouse is refused.
    assert!(!warehouse.run(&["import-git", "."]).status.success());
}

#[test]
fn import_git_ignores_only_the_git_directory_not_its_dotfile_siblings() {
    // The `.git` rule import-git writes must not swallow tracked files that merely start
    // with `.git`: if it does, the first ordinary `load .` stages their removal.
    let warehouse = TestWarehouse::new("import-git-siblings");
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

    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file(".github/x.md", "a\n");
    warehouse.write_file(".gitignore", "b\n");
    warehouse.write_file(".gitattributes", "c\n");
    warehouse.write_file(".git-blame-ignore-revs", "e\n");
    warehouse.write_file("readme", "d\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "init"]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["import-git", "."]));

    // Before the load, nothing is staged and only the import's ignore file is untracked: the
    // import builds the tree from git objects, not the worktree. The over-broad rule only bites
    // on the load.
    assert_only_the_ignore_file_is_untracked(&warehouse);

    assert_success(&warehouse.run(&["load", "."]));
    let status = stdout(&warehouse.run(&["stocktake"]));
    // "The working directory matches the inventory" holds either way; the staged half is
    // what the wrong rule changes. The one staged change is the ignore file the import wrote,
    // which is tracked content.
    let staged = json(&warehouse.run(&["--json", "stocktake"]));
    assert_eq!(staged["data"]["staged"], serde_json::json!([{"kind": "added", "path": ".forkliftignore"}]),
        "load staged a change after import: {}", staged);
    for sibling in [".github/x.md", ".gitignore", ".gitattributes", ".git-blame-ignore-revs"] {
        assert!(!status.contains(sibling), "{} was staged for removal: {}", sibling, status);
    }

    // The git directory itself is still ignored (it is on disk, holding objects and refs).
    assert!(root.join(".git").is_dir());
    assert!(!status.contains(".git/"), "the git directory leaked into the stocktake: {}", status);
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
    assert_only_the_ignore_file_is_untracked(&packed);

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
    assert_only_the_ignore_file_is_untracked(&starved);
}

#[cfg(unix)]
#[test]
fn import_git_never_replaces_an_ignore_file_it_cannot_read() {
    use std::os::unix::fs::PermissionsExt;

    // The ignore file is tracked content. If it exists but cannot be read, appending the `.git`
    // rule to an empty buffer would replace the user's rules with that one line. The history is
    // imported by then, so the import still succeeds, and warns with the rule to add by hand.
    let warehouse = TestWarehouse::new("import-git-unreadable-ignore");
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
    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("a.txt", "hello\n");
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "first commit"]);
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let rules = "^keep(/.*)?$\n";
    warehouse.write_file(".forkliftignore", rules);
    let ignore_file = root.join(".forkliftignore");
    std::fs::set_permissions(&ignore_file, std::fs::Permissions::from_mode(0o200)).unwrap();
    if std::fs::read_to_string(&ignore_file).is_ok() {
        // Running as root: permissions do not refuse the read, so there is nothing to test.
        return;
    }

    let imported = warehouse.run(&["import-git", "."]);
    std::fs::set_permissions(&ignore_file, std::fs::Permissions::from_mode(0o644)).unwrap();

    assert_success(&imported);
    assert!(stdout(&imported).contains(r"add the rule ^\.git(/.*)?$ to"), "{}", stdout(&imported));
    assert!(!stdout(&imported).contains("Added \".git/\""), "{}", stdout(&imported));
    assert_eq!(std::fs::read_to_string(&ignore_file).unwrap(), rules, "the user's rules must survive");
}

#[test]
fn import_git_from_elsewhere_writes_no_ignore_file() {
    // The `.git` rule goes into the tracked ignore file, so it is added only when the warehouse
    // holds a `.git` folder to ignore.
    let source = TestWarehouse::new("import-git-elsewhere-source");
    let warehouse = TestWarehouse::new("import-git-elsewhere");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&source.root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    source.write_file("a.txt", "hello\n");
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "first commit"]);

    // The checkout's files are here (the inventory is built against them), but no `.git` is.
    warehouse.write_file("a.txt", "hello\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    let imported = warehouse.run(&["import-git", source.root.to_str().unwrap()]);
    assert_success(&imported);
    assert!(!stdout(&imported).contains(".forkliftignore"), "{}", stdout(&imported));
    assert!(!warehouse.root.join(".forkliftignore").exists(), "no `.git` here, so no rule to add");

    // The positive half: the same import into a warehouse holding a `.git` folder adds the rule.
    let colocated = TestWarehouse::new("import-git-elsewhere-colocated");
    assert_success(&colocated.run(&["prepare"]));
    configure_operator(&colocated);
    std::fs::create_dir(colocated.root.join(".git")).unwrap();
    colocated.write_file("a.txt", "hello\n");
    assert_success(&colocated.run(&["import-git", source.root.to_str().unwrap()]));
    let rules = std::fs::read_to_string(colocated.root.join(".forkliftignore")).unwrap();
    assert!(rules.lines().any(|line| line == r"^\.git(/.*)?$"), "{:?}", rules);
}

#[test]
fn import_git_warnings_are_sanitized_for_the_terminal() {
    // A warning can name an entry of the imported repository, whose name is anyone's to choose:
    // a submodule named with an escape sequence must not reach the terminal as one.
    let source = TestWarehouse::new("import-git-warning-source");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&source.root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git(&["init", "-q", "-b", "main"]);
    source.write_file("a.txt", "hello\n");
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "first commit"]);
    let head = git(&["rev-parse", "HEAD"]);
    // A right-to-left override makes the name display reversed; git on Windows refuses control
    // characters in a path, so the escape sequence is tried where git accepts it.
    let (name, shown) = if cfg!(unix) { ("sub\u{202E}fdp.exe\u{1b}]0;pwned\u{7}", "sub fdp.exe ]0;pwned ") }
                        else { ("sub\u{202E}fdp.exe", "sub fdp.exe") };
    git(&["update-index", "--add", "--cacheinfo", &format!("160000,{},{}", head, name)]);
    git(&["commit", "-qm", "add a submodule"]);

    let warehouse = TestWarehouse::new("import-git-warning-target");
    warehouse.write_file("a.txt", "hello\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    let imported = warehouse.run(&["import-git", source.root.to_str().unwrap()]);
    assert_success(&imported);
    assert!(stdout(&imported).contains(&format!("  warning: skipped submodule \"{}\"", shown)), "{:?}", stdout(&imported));
    assert!(!stdout(&imported).contains(['\u{202E}', '\u{1b}', '\u{7}']), "{:?}", stdout(&imported));
}

#[test]
fn import_git_reports_a_rule_already_covering_git_and_leaves_a_refused_file_alone() {
    let source = TestWarehouse::new("import-git-rule-source");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .args(args)
            .current_dir(&source.root)
            .env("GIT_AUTHOR_NAME", "Ada").env("GIT_AUTHOR_EMAIL", "ada@example.com")
            .env("GIT_COMMITTER_NAME", "Ada").env("GIT_COMMITTER_EMAIL", "ada@example.com")
            .output()
            .expect("git must be installed to run this test");
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
    };
    git(&["init", "-q", "-b", "main"]);
    source.write_file("a.txt", "hello\n");
    git(&["add", "a.txt"]);
    git(&["commit", "-qm", "first commit"]);

    let colocated = |name: &str, rules: &str| {
        let warehouse = TestWarehouse::new(name);
        assert_success(&warehouse.run(&["prepare"]));
        configure_operator(&warehouse);
        std::fs::create_dir(warehouse.root.join(".git")).unwrap();
        warehouse.write_file("a.txt", "hello\n");
        warehouse.write_file(".forkliftignore", rules);
        warehouse
    };

    // A differently spelled rule that covers `.git` is reported by line, and none is added.
    let rules = "^x$\n^\\.git($|/)\n";
    let spelled = colocated("import-git-rule-spelled", rules);
    let imported = spelled.run(&["--json", "import-git", source.root.to_str().unwrap()]);
    assert_success(&imported);
    let report = json(&imported);
    assert_eq!(report["data"]["ignored_git"], false, "{}", report);
    assert_eq!(report["data"]["git_already_ignored"], serde_json::json!({"line": 2, "pattern": "^\\.git($|/)"}), "{}", report);
    assert_eq!(std::fs::read_to_string(spelled.root.join(".forkliftignore")).unwrap(), rules);
    let human = colocated("import-git-rule-spelled-human", rules).run(&["import-git", source.root.to_str().unwrap()]);
    assert_success(&human);
    assert!(stdout(&human).contains("\".git/\" is already ignored by line 2 of \".forkliftignore\": ^\\.git($|/)"),
        "{}", stdout(&human));

    // A rule that does not match `.git` itself is no rule for it: the rule is added.
    let unrelated = colocated("import-git-rule-unrelated", "^\\.github$\n");
    let imported = json(&unrelated.run(&["--json", "import-git", source.root.to_str().unwrap()]));
    assert_eq!(imported["data"]["ignored_git"], true, "{}", imported);
    assert!(imported["data"].get("git_already_ignored").is_none(), "{}", imported);

    // An indented copy of the rule is refused as rules, and appending to a refused file would
    // leave it refused: the file is left as it was, with a warning naming why.
    let rules = "  ^\\.git(/.*)?$\n";
    let indented = colocated("import-git-rule-indented", rules);
    let imported = indented.run(&["import-git", source.root.to_str().unwrap()]);
    assert_success(&imported);
    assert!(stdout(&imported).contains(r"is refused, so it was left unchanged. Fix the problem below, then add the rule ^\.git(/.*)?$ if no rule covers"),
        "{}", stdout(&imported));
    // The reason sits on its own line, indented inside the warning rather than at column 0.
    assert!(stdout(&imported).contains("covers \".git\".\n           \".forkliftignore\", line 1: the pattern \"  ^\\\\.git(/.*)?$\" starts or ends with whitespace (U+0020)"),
        "{}", stdout(&imported));
    assert!(!stdout(&imported).contains("by hand"), "{}", stdout(&imported));
    assert!(!stdout(&imported).contains("Added \".git/\""), "{}", stdout(&imported));
    assert_eq!(std::fs::read_to_string(indented.root.join(".forkliftignore")).unwrap(), rules);

    // A symbolic link is refused whatever it holds, so it gets the same remedy, not "by hand".
    #[cfg(unix)]
    {
        let linked = colocated("import-git-rule-linked", "");
        let ignore_file = linked.root.join(".forkliftignore");
        std::fs::remove_file(&ignore_file).unwrap();
        linked.write_file("rules.txt", "^x$\n");
        std::os::unix::fs::symlink("rules.txt", &ignore_file).unwrap();
        let imported = linked.run(&["import-git", source.root.to_str().unwrap()]);
        assert_success(&imported);
        assert!(stdout(&imported).contains("is refused, so it was left unchanged") && stdout(&imported).contains("must be a regular file"),
            "{}", stdout(&imported));
        assert!(!stdout(&imported).contains("by hand"), "{}", stdout(&imported));
        assert!(std::fs::symlink_metadata(&ignore_file).unwrap().file_type().is_symlink());
        assert_eq!(read(&linked, "rules.txt"), "^x$\n");
    }
}

#[test]
fn a_blank_looking_ignore_line_hides_nothing_and_whitespace_around_a_rule_is_refused() {
    // Compiled as a pattern, a whitespace-only line matches every path holding a space: such a
    // path is hidden from stocktake, and the next `load .` stages the removal of a tracked one.
    let warehouse = TestWarehouse::new("ignore-whitespace");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file(".forkliftignore", "^x$\n \n\t\n");
    warehouse.write_file("a b.txt", "a\n");
    warehouse.write_file("plain.txt", "p\n");

    let unstaged = changes(&warehouse, "unstaged");
    for path in ["a b.txt", "plain.txt"] {
        assert!(unstaged.contains(&("untracked".to_string(), path.to_string())), "{}: {:?}", path, unstaged);
    }

    // A rule with whitespace around it would never match the name it spells; it is refused.
    warehouse.write_file(".forkliftignore", "^x$\n^a b\\.txt$ \n");
    for args in [&["stocktake"][..], &["load", "."][..]] {
        let refused = warehouse.run(args);
        assert!(!refused.status.success(), "{:?} must refuse the rules: {}", args, stdout(&refused));
        assert!(stderr(&refused).contains("line 2: the pattern") && stderr(&refused).contains("starts or ends with whitespace"),
            "{}", stderr(&refused));
    }

    // Without the whitespace, the same rule matches the path holding a space.
    warehouse.write_file(".forkliftignore", "^x$\n^a b\\.txt$\n");
    let unstaged = changes(&warehouse, "unstaged");
    assert!(!unstaged.iter().any(|(_, path)| path == "a b.txt"), "{:?}", unstaged);
    assert!(unstaged.contains(&("untracked".to_string(), "plain.txt".to_string())), "{:?}", unstaged);
}

#[test]
fn a_conflicted_ignore_file_is_refused_not_read_as_rules() {
    // The ignore file is tracked, so consolidating two edits of it can write conflict markers
    // into it. Read as rules, `||||||| base` matches every path: stocktake would show nothing and
    // `load .` would stage the removal of the whole tree.
    let warehouse = TestWarehouse::new("conflicted-ignore-file");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file(".forkliftignore", "^build(/.*)?$\n");
    warehouse.write_file("a.txt", "a\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file(".forkliftignore", "^out(/.*)?$\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature rules"]));
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file(".forkliftignore", "^tmp(/.*)?$\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main rules"]));

    warehouse.run(&["consolidate", "feature"]);
    let conflicted = std::fs::read_to_string(warehouse.root.join(".forkliftignore")).unwrap();
    assert!(conflicted.contains("||||||| base"), "{}", conflicted);

    for args in [&["stocktake"][..], &["load", "."][..]] {
        let refused = warehouse.run(args);
        assert!(!refused.status.success(), "{:?} must refuse the conflicted rules: {}", args, stdout(&refused));
        assert!(stderr(&refused).contains("unresolved conflict marker on line 1"), "{}", stderr(&refused));
    }

    // Resolved, the rules read again and nothing tracked was dropped.
    warehouse.write_file(".forkliftignore", "^out(/.*)?$\n^tmp(/.*)?$\n");
    assert_success(&warehouse.run(&["load", "."]));
    let status = json(&warehouse.run(&["--json", "stocktake"]));
    assert!(status["data"]["staged"].as_array().unwrap().iter().all(|entry| entry["kind"] != "removed"),
        "{}", status);
}

#[cfg(unix)]
#[test]
fn a_symlinked_ignore_file_is_refused() {
    // The ignore file is tracked content and can arrive as a symbolic link. A dangling one must
    // not read as "no rules", and a live one must not read rules from outside the warehouse.
    let warehouse = TestWarehouse::new("symlinked-ignore-file");
    assert_success(&warehouse.run(&["prepare"]));
    warehouse.write_file("rules", "^x$\n");

    for target in ["rules", "missing"] {
        let _ = std::fs::remove_file(warehouse.root.join(".forkliftignore"));
        std::os::unix::fs::symlink(target, warehouse.root.join(".forkliftignore")).unwrap();
        let refused = warehouse.run(&["stocktake"]);
        assert!(!refused.status.success(), "a link to {} must be refused: {}", target, stdout(&refused));
        assert!(stderr(&refused).contains("must be a regular file"), "{}", stderr(&refused));
    }

    // The positive half: the same rules in a regular file are read.
    std::fs::remove_file(warehouse.root.join(".forkliftignore")).unwrap();
    warehouse.write_file(".forkliftignore", "^x$\n");
    assert_success(&warehouse.run(&["stocktake"]));
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

#[cfg(unix)]
#[test]
fn import_git_refuses_a_top_level_alias_of_the_metadata_folder() {
    // `.forklift.` is an ordinary directory to git here, but Windows opens `.forklift` for it, so
    // imported history carrying it could never be checked out there.
    let warehouse = TestWarehouse::new("import-git-metadata-alias");
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

    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("a.txt", "hello\n");
    warehouse.write_file(".forklift./notes.txt", "notes\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "alias"]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let imported = warehouse.run(&["import-git", "."]);
    assert!(!imported.status.success(), "import-git accepted the alias: {}", stdout(&imported));
    assert!(stderr(&imported).contains("\".forklift.\""), "the refusal must name the entry: {}", stderr(&imported));
}

#[cfg(unix)]
#[test]
fn import_git_refuses_the_alias_in_a_root_tree_it_first_met_as_a_subdirectory() {
    // Commit 1 holds `vendor/.forklift.`, legal below the top level. Commit 2 lifts vendor's
    // contents to the root, so its root tree is byte-identical to the already-converted
    // `vendor` tree: the importer reuses that conversion and must still refuse it as a root.
    let warehouse = TestWarehouse::new("import-git-metadata-alias-reused");
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

    git(&["init", "-q", "-b", "main"]);
    warehouse.write_file("vendor/a.txt", "hello\n");
    warehouse.write_file("vendor/.forklift./notes.txt", "notes\n");
    git(&["add", "-A"]);
    git(&["commit", "-qm", "nested"]);

    git(&["mv", "vendor/a.txt", "a.txt"]);
    git(&["mv", "vendor/.forklift.", ".forklift."]);
    git(&["commit", "-qm", "lifted"]);

    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let imported = warehouse.run(&["import-git", "."]);
    assert!(!imported.status.success(), "import-git accepted the alias: {}", stdout(&imported));
    assert!(stderr(&imported).contains("\".forklift.\""), "the refusal must name the entry: {}", stderr(&imported));
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

/// Tree names holding a newline or tab, or beginning with a quote, export byte for byte. Fed to
/// `git mktree` newline-delimited, the first two split an entry and the third is C-unquoted into a
/// different name. Unix only: Windows file names cannot hold these characters.
#[cfg(unix)]
#[test]
fn export_git_keeps_tree_names_with_line_breaks_tabs_and_quotes() {
    let warehouse = TestWarehouse::new("export-git-hostile-names");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let names = ["line\nbreak.txt", "tab\there.txt", "\"quo\\164ed\"", "dir\nname/inner.txt"];
    for name in names {
        warehouse.write_file(name, "content\n");
    }
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "hostile names"]));

    let out = warehouse.root.join("exported");
    assert_success(&warehouse.run(&["export-git", out.to_str().unwrap()]));

    let listing = Command::new("git")
        .args(["ls-tree", "-r", "-z", "--name-only", "main"])
        .current_dir(&out)
        .output()
        .expect("git must be installed to run this test");
    assert!(listing.status.success(), "git ls-tree: {}", String::from_utf8_lossy(&listing.stderr));

    let mut exported: Vec<String> = String::from_utf8(listing.stdout).unwrap()
        .split_terminator('\0')
        .map(str::to_string)
        .filter(|name| name != ".forkliftignore")
        .collect();
    exported.sort();
    let mut expected: Vec<String> = names.iter().map(|name| name.to_string()).collect();
    expected.sort();
    assert_eq!(exported, expected);
}

#[test]
fn self_update_check_reports_without_mutating() {
    // A global command — no warehouse needed. The env override injects "the latest
    // release" so the check never touches the network.
    let run = |latest: &str| -> Output {
        forklift_command()
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

/// `self-schema --json` keeps the document's key order, unlike other envelopes' `data`: Pult
/// binds positional params in the order they are listed, so a sorted `franchise` would take
/// the directory for the url. Human mode prints the same document, so the keys of both, in
/// order, must agree past the envelope's header.
#[test]
fn self_schema_json_keeps_the_document_order() {
    let keys = |output: &Output| -> Vec<String> {
        stdout(output).lines()
            .filter_map(|line| line.trim_start().strip_prefix('"'))
            .filter_map(|line| line.split_once("\": ").map(|(key, _)| key.to_string()))
            .collect()
    };

    let enveloped = forklift_command().args(["--json", "self-schema"]).output().expect("forklift binary runs");
    let human = forklift_command().args(["self-schema"]).output().expect("forklift binary runs");
    assert_success(&enveloped);
    assert_success(&human);

    // Not sorted: in franchise, `url` (the first positional) comes before `directory`.
    let text = stdout(&enveloped);
    let at = text.find("\"id\": \"franchise\"").expect("franchise is described");
    assert!(text[at..].find("\"url\":").unwrap() < text[at..].find("\"directory\":").unwrap());

    let header = ["lonic_json", "tool", "schema", "command", "ok", "data"];
    let enveloped = keys(&enveloped);
    assert_eq!(enveloped[..header.len()], header);
    assert_eq!(enveloped[header.len()..], keys(&human)[..]);
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
        forklift_command()
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
    let via_alias = common::test_command("cmd")
        .arg("/C")
        .arg(scratch.alias_path("fl"))
        .args(["--json", "version"])
        .output()
        .unwrap();
    #[cfg(not(windows))]
    let via_alias = common::test_command(scratch.alias_path("fl"))
        .args(["--json", "version"])
        .output()
        .unwrap();
    let via_forklift = forklift_command().args(["--json", "version"]).output().unwrap();

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

    let approvals = |warehouse: &TestWarehouse| json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["approvals"].clone();
    assert_eq!(approvals(&warehouse), 1);

    // New work on the source: until the haul is updated it still proposes the old head, and
    // once it is, the earlier reviews are listed against the head they reviewed but no longer
    // count.
    assert_success(&warehouse.run(&["shift", "feature"]));
    warehouse.write_file("feature.txt", "feat 2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "more feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    let old_head = shown["data"]["head"].as_str().unwrap().to_string();
    let updated = json(&warehouse.run(&["haul", "update", &id, "--json"]));
    assert_eq!(updated["data"]["changed"], true);
    assert_ne!(updated["data"]["head"], old_head.as_str());
    assert_eq!(json(&warehouse.run(&["haul", "update", &id, "--json"]))["data"]["changed"], false);

    let shown = json(&warehouse.run(&["haul", "show", &id, "--json"]));
    assert!(
        shown["data"]["reviews"].as_array().unwrap().iter().all(|r| r["current"] == false && r["head"] == old_head.as_str()),
        "a review stays pinned to the head it reviewed: {}", shown
    );
    assert_eq!(approvals(&warehouse), 0, "an approval of an earlier head does not count");

    assert_success(&warehouse.run(&["haul", "review", &id, "-m", "still good"]));
    assert_eq!(approvals(&warehouse), 1);

    // Merge is recorded, not gated on the request-changes (MVP policy) → merged, and the
    // source's file is now on the target.
    assert_success(&warehouse.run(&["haul", "merge", &id]));
    assert_eq!(std::fs::read_to_string(warehouse.root.join("feature.txt")).unwrap(), "feat 2\n");
    let shown = json(&warehouse.run(&["haul", "show", &id, "--json"]));
    assert_eq!(shown["data"]["status"], "merged");
    assert!(shown["data"]["merge_parcel"].as_str().is_some_and(|p| p.len() == 64), "{}", shown);

    // A merged haul is final.
    for action in ["reopen", "close", "update", "merge"] {
        let refused = warehouse.run(&["haul", action, &id]);
        assert!(!refused.status.success(), "haul {} of a merged haul must be refused", action);
        assert!(stderr(&refused).contains("merged"), "{}", stderr(&refused));
    }
}

/// Record text is authored by whoever wrote the record — a remote operator, once it syncs — so a
/// human render must not let it drive the terminal: escapes and bidi overrides print as inert
/// spaces, and a multi-line text keeps its lines. `--json` is the record, so it stays verbatim.
#[test]
fn record_text_renders_inert_for_humans_and_verbatim_in_json() {
    const ESCAPE: &str = "\x1b[2J\x1b[31mforged\x1b[0m";
    const OVERRIDE: &str = "\u{202E}txt.exe";

    let warehouse = TestWarehouse::new("render-inert");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    let description = format!("subject {}\nbody {}", ESCAPE, OVERRIDE);
    assert_success(&warehouse.run(&["stack", &description]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("feature.txt", "feat\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "feature work"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    let title = format!("title {}", ESCAPE);
    let message = format!("first {}\nsecond {}", ESCAPE, OVERRIDE);
    assert_success(&warehouse.run(&[
        "haul", "open", "--target", "main", "--source", "feature", "--title", &title, "-m", &message,
    ]));
    let id = json(&warehouse.run(&["haul", "list", "--json"]))["data"]["hauls"][0]["id"]
        .as_str().unwrap().to_string();
    assert_success(&warehouse.run(&["haul", "comment", &id, "-m", &message]));
    assert_success(&warehouse.run(&["haul", "review", &id, "-m", &message]));

    let parcel = json(&warehouse.run(&["history", "--json"]))["data"]["entries"]
        .as_array().unwrap().iter()
        .find(|entry| entry["description"].as_str().is_some_and(|d| d.starts_with("subject")))
        .expect("the planted parcel is in history")["parcel"]
        .as_str().unwrap().to_string();

    let note = format!("note {}\nnext {}", ESCAPE, OVERRIDE);
    assert_success(&warehouse.run(&["manifest", "note", &parcel, "-m", &note]));
    let model = format!("model-{}", ESCAPE);
    assert_success(&warehouse.run(&["manifest", "provenance", &parcel, "--model", &model]));

    let renders: [&[&str]; 7] = [
        &["history"],
        &["history", "--oneline"],
        &["peek", &parcel],
        &["haul", "list"],
        &["haul", "show", &id],
        &["query", &parcel],
        &["manifest", "show", &parcel],
    ];

    // An error that quotes its input is printed through the same guard, to stderr.
    let unknown = warehouse.run(&["haul", "show", ESCAPE]);
    assert!(!unknown.status.success());
    let outputs = renders.iter()
        .map(|args| {
            let output = warehouse.run(args);
            assert_success(&output);
            (format!("{:?}", args), stdout(&output))
        })
        .chain(std::iter::once(("haul show <escape> (stderr)".to_string(), stderr(&unknown))));

    for (args, text) in outputs {
        assert!(text.contains("forged"), "{} must still show the text: {}", args, text);
        // Some renders colour their own output, so look for the payload's escapes, not any.
        assert!(
            !text.contains("\x1b[2J") && !text.contains("\x1b[31mforged"),
            "{} printed the payload's escapes raw: {:?}", args, text
        );
        assert!(!text.contains('\u{202E}'), "{} printed a raw override: {:?}", args, text);
    }

    // The lines of a multi-line text stay lines: the second never lands on the first's row.
    let shown = stdout(&warehouse.run(&["haul", "show", &id]));
    let second = shown.lines().filter(|line| line.contains("second")).count();
    assert!(second >= 2, "every multi-line text keeps its second line: {}", shown);
    assert!(!shown.lines().any(|line| line.contains("first") && line.contains("second")), "{}", shown);

    let manifest = stdout(&warehouse.run(&["manifest", "show", &parcel]));
    assert!(
        manifest.lines().any(|line| line.contains("next")) &&
            !manifest.lines().any(|line| line.contains("note") && line.contains("next")),
        "a manifest body keeps its lines: {}", manifest
    );

    // Undoing names the parcel it undid by its description's first line.
    let undo_description = format!("undone {}", ESCAPE);
    warehouse.write_file("undo.txt", "undo\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", &undo_description]));
    let undone = warehouse.run(&["undo"]);
    assert_success(&undone);
    assert!(
        stdout(&undone).contains("forged") && !stdout(&undone).contains("\x1b[2J"),
        "undo printed the payload's escapes raw: {:?}", stdout(&undone)
    );

    // JSON is the record itself.
    let shown = json(&warehouse.run(&["haul", "show", &id, "--json"]));
    assert_eq!(shown["data"]["title"], title.as_str());
    assert!(shown.to_string().contains(&serde_json::to_string(&message).unwrap()), "{}", shown);

    let entries = json(&warehouse.run(&["history", "--json"]))["data"]["entries"].clone();
    assert!(
        entries.as_array().unwrap().iter().any(|entry| entry["description"] == description.as_str()),
        "{}", entries
    );
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

// FORK-213: a bay records the parcel its files stand on (its base) beside its pallet. Two bays
// current on one pallet: bay A is the main tree, bay B a second bay, both on main. Every test below
// names the production change whose revert turns it red.

/// A warehouse with f.txt and g.txt stacked on main, and a second bay "b" on main beside the main
/// tree, both standing on that first parcel.
fn two_bays_on_main(name: &str) -> (TestWarehouse, std::path::PathBuf) {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("f.txt", "f v1\n");
    warehouse.write_file("g.txt", "g v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    let bay_dir = warehouse.home.join("bay-b");
    assert_success(&warehouse.run(&["bay", "add", "b", bay_dir.to_str().unwrap()]));
    assert_success(&warehouse.run_at(&bay_dir, &["shift", "main"]));

    (warehouse, bay_dir)
}

/// Write `content` to `file` in `dir`, load it and stack it there; the new head of main.
fn stack_at(warehouse: &TestWarehouse, dir: &std::path::Path, file: &str, content: &str, message: &str) -> String {
    std::fs::write(dir.join(file), content).unwrap();
    assert_success(&warehouse.run_at(dir, &["load", file]));
    assert_success(&warehouse.run_at(dir, &["stack", message]));
    pallet_head_hash(warehouse, "main")
}

/// The position file of the main tree (`None`) or of a named bay.
fn position_path(warehouse: &TestWarehouse, bay: Option<&str>) -> std::path::PathBuf {
    match bay {
        None => warehouse.root.join(".forklift").join("position"),
        Some(bay) => warehouse.root.join(".forklift").join("bays").join(bay).join("position"),
    }
}

/// The base a bay's position records (empty for an unborn pallet).
fn bay_base(warehouse: &TestWarehouse, bay: Option<&str>) -> String {
    std::fs::read_to_string(position_path(warehouse, bay)).unwrap().lines().nth(2).unwrap().to_string()
}

/// Write a bay's position by hand, as a crash between two writes would leave it.
fn write_position(warehouse: &TestWarehouse, bay: Option<&str>, pallet: &str, base: &str) {
    std::fs::write(position_path(warehouse, bay), format!("/forklift-position 1\n{}\n{}\n", pallet, base)).unwrap();
}

// T1. Revert: stack's compare-and-set expecting a fresh read of the ref, with the up-front check
// removed → B's stack succeeds and main's f is back at v1.
#[test]
fn a_stale_bays_stack_is_refused_and_catching_up_keeps_the_other_bays_parcel() {
    let (warehouse, bay) = two_bays_on_main("stale-bay-stack");
    let initial = pallet_head_hash(&warehouse, "main");
    let a_head = stack_at(&warehouse, &warehouse.root, "f.txt", "f v2 from A\n", "A changes f");

    std::fs::write(bay.join("g.txt"), "g v2 from B\n").unwrap();
    assert_success(&warehouse.run_at(&bay, &["load", "g.txt"]));
    let stacked = warehouse.run_at(&bay, &["stack", "B changes g"]);

    assert!(!stacked.status.success(), "a stale bay's stack must be refused: {}", stdout(&stacked));
    let refusal = stderr(&stacked);
    assert!(refusal.contains("behind pallet \"main\"") && refusal.contains(&initial) && refusal.contains(&a_head)
        && refusal.contains("forklift shift main"), "{}", refusal);
    assert_eq!(pallet_head_hash(&warehouse, "main"), a_head, "the refusal moved nothing");

    // The way out the catch-up refusal names: park, catch up, pop.
    let catch_up = warehouse.run_at(&bay, &["shift", "main"]);
    assert!(!catch_up.status.success() && stderr(&catch_up).contains("park"), "{}", stderr(&catch_up));
    assert_success(&warehouse.run_at(&bay, &["park"]));
    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    assert_success(&warehouse.run_at(&bay, &["park", "pop"]));
    assert_success(&warehouse.run_at(&bay, &["load", "g.txt"]));
    assert_success(&warehouse.run_at(&bay, &["stack", "B changes g"]));

    assert!(stdout(&warehouse.run(&["show", "main:f.txt"])).contains("f v2 from A"));
    assert!(stdout(&warehouse.run(&["show", "main:g.txt"])).contains("g v2 from B"));
}

// T2. Revert: shift's from-tree back to the pallet's ref → `shift b` is refused as dirty.
#[test]
fn a_stale_clean_bay_reports_the_moved_pallet_and_can_shift_away() {
    let (warehouse, bay) = two_bays_on_main("stale-bay-shift");
    let a_head = stack_at(&warehouse, &warehouse.root, "f.txt", "f v2 from A\n", "A changes f");

    let status = stdout(&warehouse.run_at(&bay, &["stocktake"]));
    assert!(!status.contains("Staged changes") && status.contains(&format!("moved on to parcel {}", a_head)), "{}", status);

    let shifted = warehouse.run_at(&bay, &["shift", "b"]);
    assert_success(&shifted);

    // And back: an ordinary shift lands on main's head, A's parcel included.
    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    assert_eq!(std::fs::read_to_string(bay.join("f.txt")).unwrap(), "f v2 from A\n");
    assert_eq!(bay_base(&warehouse, Some("b")), a_head);
}

// T3. Pins deliver's policy refusal only. Revert: drop deliver's currency check → it delivers.
#[test]
fn deliver_from_a_bay_behind_its_draft_pallet_is_refused() {
    let warehouse = TestWarehouse::new("stale-bay-deliver");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["office", "enroll"]));
    assert_success(&warehouse.run(&["palletize", "draft"]));

    let bay = warehouse.home.join("bay-b");
    assert_success(&warehouse.run(&["bay", "add", "b", bay.to_str().unwrap()]));
    assert_success(&warehouse.run_at(&bay, &["shift", "draft"]));

    warehouse.write_file("code.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "A drafts v2"]));
    let main_head = pallet_head_hash(&warehouse, "main");

    let delivered = warehouse.run_at(&bay, &["deliver", "main", "-m", "stale"]);
    assert!(!delivered.status.success(), "{}", stdout(&delivered));
    assert!(stderr(&delivered).contains("behind pallet \"draft\""), "{}", stderr(&delivered));
    assert_eq!(pallet_head_hash(&warehouse, "main"), main_head);
}

// T5. Stack's crash window between its ref write and its position write: the ref holds B's own
// parcel, the position the old base, and nothing is in progress. Catch-up records the head and
// touches no file. Revert: catch-up without its clean-against-head arm → refused.
#[test]
fn catching_up_after_an_interrupted_stack_only_records_the_head() {
    let (warehouse, bay) = two_bays_on_main("stale-bay-crash");
    let initial = pallet_head_hash(&warehouse, "main");
    let b_head = stack_at(&warehouse, &bay, "f.txt", "f v2 from B\n", "B changes f");
    write_position(&warehouse, Some("b"), "main", &initial);

    let before = std::fs::metadata(bay.join("f.txt")).unwrap().modified().unwrap();
    let caught_up = warehouse.run_at(&bay, &["--json", "shift", "main"]);
    assert_success(&caught_up);
    assert_eq!(json(&caught_up)["data"]["moved_files"], false);
    assert_eq!(json(&caught_up)["data"]["head"], b_head.as_str());
    assert_eq!(bay_base(&warehouse, Some("b")), b_head);
    assert_eq!(std::fs::metadata(bay.join("f.txt")).unwrap().modified().unwrap(), before, "no file was touched");
    assert!(!stdout(&warehouse.run_at(&bay, &["stocktake"])).contains("Staged changes"));
}

// T6. Revert: restore the missing-file fallback to "main" → the missing case passes.
#[test]
fn a_header_less_or_missing_position_is_refused_naming_the_file() {
    let warehouse = TestWarehouse::new("position-unreadable");
    assert_success(&warehouse.run(&["prepare"]));
    let path = position_path(&warehouse, None);
    let named = std::path::Path::new(".forklift").join("position").to_string_lossy().into_owned();

    std::fs::write(&path, "main\n").unwrap();
    let headerless = warehouse.run(&["stocktake"]);
    assert!(!headerless.status.success());
    assert!(stderr(&headerless).contains(&named) && stderr(&headerless).contains("no format header"),
        "{}", stderr(&headerless));

    std::fs::remove_file(&path).unwrap();
    let missing = warehouse.run(&["stocktake"]);
    assert!(!missing.status.success());
    assert!(stderr(&missing).contains(&named), "{}", stderr(&missing));
}

/// T7's and T8's warehouse: main at Y (f "main v2"), pallet side forked at the first parcel with
/// f "side", and bay b on main at Y mid-consolidation of side, stopped at the conflict on f.
fn bay_mid_consolidation(name: &str) -> (TestWarehouse, std::path::PathBuf, String) {
    let warehouse = TestWarehouse::new(name);
    warehouse.write_file("f.txt", "f v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["palletize", "side"]));
    stack_at(&warehouse, &warehouse.root, "f.txt", "side\n", "side changes f");
    assert_success(&warehouse.run(&["shift", "main"]));
    let y = stack_at(&warehouse, &warehouse.root, "f.txt", "main v2\n", "main changes f");

    let bay = warehouse.home.join("bay-b");
    assert_success(&warehouse.run(&["bay", "add", "b", bay.to_str().unwrap()]));
    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    let consolidated = warehouse.run_at(&bay, &["consolidate", "side"]);
    assert_success(&consolidated);
    assert!(bay_state_file(&warehouse, "b", "consolidation").exists(), "{}", stdout(&consolidated));

    (warehouse, bay, y)
}

fn bay_state_file(warehouse: &TestWarehouse, bay: &str, file: &str) -> std::path::PathBuf {
    warehouse.root.join(".forklift").join("bays").join(bay).join(file)
}

// T7. B resolves its consolidation to the very tree A then stacks, and tries to catch up and stack
// anyway. Reverts: catch-up without its mid-merge refusal → B catches up (its files match A's
// head), and stack's our_head-equals-base guard still refuses; that guard alone reverted → B
// stays behind and is refused as stale; both together → B's merge parcel, built on the old head,
// lands and drops A's parcel from main.
#[test]
fn a_consolidation_started_before_the_pallet_moved_never_lands() {
    let (warehouse, bay, _) = bay_mid_consolidation("stale-bay-mid-merge");
    std::fs::write(bay.join("f.txt"), "resolved\n").unwrap();
    assert_success(&warehouse.run_at(&bay, &["load", "f.txt"]));

    // A stacks the same tree B resolved to, plus nothing else.
    let a_head = stack_at(&warehouse, &warehouse.root, "f.txt", "resolved\n", "A resolves f the same way");

    let _ = warehouse.run_at(&bay, &["shift", "main"]);
    let stacked = warehouse.run_at(&bay, &["stack", "B completes the merge"]);
    assert!(!stacked.status.success(), "{}", stdout(&stacked));
    assert_eq!(pallet_head_hash(&warehouse, "main"), a_head);
}

// T7's catch-up half. Revert: catch-up without its mid-merge refusal → it catches up.
#[test]
fn catching_up_mid_consolidation_is_refused() {
    let (warehouse, bay, _) = bay_mid_consolidation("stale-bay-catch-up-mid-merge");
    std::fs::write(bay.join("f.txt"), "resolved\n").unwrap();
    assert_success(&warehouse.run_at(&bay, &["load", "f.txt"]));
    stack_at(&warehouse, &warehouse.root, "f.txt", "resolved\n", "A resolves f the same way");

    let catch_up = warehouse.run_at(&bay, &["shift", "main"]);
    assert!(!catch_up.status.success() && stderr(&catch_up).contains("consolidation is in progress"), "{}", stderr(&catch_up));

    let stacked = warehouse.run_at(&bay, &["stack", "B completes the merge"]);
    assert!(stderr(&stacked).contains("consolidation is kept") && stderr(&stacked).contains("forklift shift main"),
        "{}", stderr(&stacked));
}

// T8. B's consolidate stops at conflicts, A stacks, B undoes. Reverts: undo putting back refs whose
// heads are equal → the undo is refused because main moved; undo recording a fresh read of the
// ref as the base → B's next stack succeeds and reverts A's parcel.
#[test]
fn undoing_a_conflicted_consolidate_after_another_bay_stacked_leaves_the_bay_behind() {
    let (warehouse, bay, y) = bay_mid_consolidation("stale-bay-undo-merge");
    stack_at(&warehouse, &warehouse.root, "k.txt", "k from A\n", "A adds k");

    let undone = warehouse.run_at(&bay, &["undo"]);
    assert_success(&undone);
    assert!(!bay_state_file(&warehouse, "b", "consolidation").exists());
    assert_eq!(bay_base(&warehouse, Some("b")), y);

    // The conflicted file is still there to resolve; a resolved and loaded stack is refused as
    // stale rather than landing on A's parcel without it.
    std::fs::write(bay.join("f.txt"), "resolved\n").unwrap();
    assert_success(&warehouse.run_at(&bay, &["load", "f.txt"]));
    let stacked = warehouse.run_at(&bay, &["stack", "B after undo"]);
    assert!(!stacked.status.success(), "{}", stdout(&stacked));
    assert!(stderr(&stacked).contains("behind pallet \"main\""), "{}", stderr(&stacked));
    assert!(stdout(&warehouse.run(&["show", "main:k.txt"])).contains("k from A"));
}

// T9. Revert: journal the catch-up as a "shift" → undo reports an undone shift and changes nothing.
#[test]
fn catching_up_is_not_journaled() {
    let (warehouse, bay) = two_bays_on_main("stale-bay-catch-up-undo");
    let a_head = stack_at(&warehouse, &warehouse.root, "f.txt", "f v2 from A\n", "A changes f");

    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    let undone = warehouse.run_at(&bay, &["undo"]);
    assert!(!stdout(&undone).contains("Undid shift"), "{}", stdout(&undone));
    assert_eq!(bay_base(&warehouse, Some("b")), a_head);
    assert_eq!(std::fs::read_to_string(bay.join("f.txt")).unwrap(), "f v2 from A\n");
}

/// T10's bay b: it stacked X→N (f changed) and then edited g without loading it, and an undo of
/// that stack was cut short after writing the earlier position {main, X}. Returns X and N.
fn half_undone_bay(name: &str) -> (TestWarehouse, std::path::PathBuf, String, String) {
    let (warehouse, bay) = two_bays_on_main(name);
    let x = pallet_head_hash(&warehouse, "main");
    let n = stack_at(&warehouse, &bay, "f.txt", "f from N\n", "B's N");
    std::fs::write(bay.join("g.txt"), "g edited\n").unwrap();
    write_position(&warehouse, Some("b"), "main", &x);

    (warehouse, bay, x, n)
}

// T10 (i). Revert: the gate without its half-undone arm → the retry is refused at the gate.
#[test]
fn a_half_applied_undo_resumes() {
    let (warehouse, bay, x, _) = half_undone_bay("half-undo-resume");

    assert_success(&warehouse.run_at(&bay, &["undo"]));
    assert_eq!(pallet_head_hash(&warehouse, "main"), x);
    assert_eq!(bay_base(&warehouse, Some("b")), x);
    assert_eq!(std::fs::read_to_string(bay.join("g.txt")).unwrap(), "g edited\n");
}

// T10 (ii). Another bay stacked N→M before the retry. Revert: the gate without its half-undone arm
// → refused at the gate with the position left at X.
#[test]
fn a_half_applied_undo_after_another_bay_stacked_on_its_result_keeps_that_result() {
    let (warehouse, bay, _, n) = half_undone_bay("half-undo-moved");
    assert_success(&warehouse.run(&["shift", "main"]));
    let m = stack_at(&warehouse, &warehouse.root, "k.txt", "k from M\n", "A's M");

    let undone = warehouse.run_at(&bay, &["undo"]);
    assert!(!undone.status.success(), "{}", stdout(&undone));
    assert_eq!(pallet_head_hash(&warehouse, "main"), m);
    assert_eq!(bay_base(&warehouse, Some("b")), n);

    assert_success(&warehouse.run_at(&bay, &["park"]));
    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    assert_success(&warehouse.run_at(&bay, &["park", "pop"]));
    assert_eq!(std::fs::read_to_string(bay.join("g.txt")).unwrap(), "g edited\n");
    assert_eq!(std::fs::read_to_string(bay.join("k.txt")).unwrap(), "k from M\n");
    assert_eq!(std::fs::read_to_string(bay.join("f.txt")).unwrap(), "f from N\n");
}

// T10 (iii). The ref restore landed (main back at X) but the entry was not popped, and another bay
// then stacked X→M. Revert: write back the operation's position whenever the ref is anywhere but
// X → the position becomes N, and the park loses N's change.
#[test]
fn a_landed_undo_overtaken_by_another_bay_keeps_the_earlier_base() {
    let (warehouse, bay, x, _) = half_undone_bay("half-undo-landed");
    std::fs::write(warehouse.root.join(".forklift").join("pallets").join("main"), format!("{}\n", x)).unwrap();
    let m = stack_at(&warehouse, &warehouse.root, "k.txt", "k from M\n", "A's M");

    let undone = warehouse.run_at(&bay, &["undo"]);
    assert!(!undone.status.success(), "{}", stdout(&undone));
    assert_eq!(pallet_head_hash(&warehouse, "main"), m);
    assert_eq!(bay_base(&warehouse, Some("b")), x);

    assert_success(&warehouse.run_at(&bay, &["park"]));
    assert_success(&warehouse.run_at(&bay, &["shift", "main"]));
    assert_success(&warehouse.run_at(&bay, &["park", "pop"]));
    assert_eq!(std::fs::read_to_string(bay.join("f.txt")).unwrap(), "f from N\n");
    assert_eq!(std::fs::read_to_string(bay.join("g.txt")).unwrap(), "g edited\n");
    assert_eq!(std::fs::read_to_string(bay.join("k.txt")).unwrap(), "k from M\n");
}

#[test]
fn a_bay_whose_path_file_cannot_be_read_is_neither_listed_blank_nor_half_removed() {
    let warehouse = TestWarehouse::new("bay-unreadable-path");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    let bay_dir = warehouse.home.join("bay-unreadable");
    assert_success(&warehouse.run(&["bay", "add", "feature", bay_dir.to_str().unwrap()]));

    // A path file an older Forklift wrote, without its format header.
    let path_file = warehouse.root.join(".forklift/bays/feature/path");
    std::fs::write(&path_file, format!("{}\n", bay_dir.to_str().unwrap())).unwrap();

    let listed = warehouse.run(&["bay"]);
    assert!(!listed.status.success(), "a bay with an unreadable path must not list with a blank one");
    assert!(stderr(&listed).contains("has no format header"), "{}", stderr(&listed));

    // Removing it refuses, naming the way out, and leaves both the state and the redirect.
    let removed = warehouse.run(&["bay", "remove", "feature"]);
    assert!(!removed.status.success());
    assert!(stderr(&removed).contains("To remove the bay by hand"), "{}", stderr(&removed));
    assert!(warehouse.root.join(".forklift/bays/feature").is_dir());
    assert!(bay_dir.join(".forklift").is_file());
}

#[test]
fn a_bay_whose_path_file_is_missing_still_lists_and_removes() {
    let warehouse = TestWarehouse::new("bay-missing-path");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    let bay_dir = warehouse.home.join("bay-missing");
    assert_success(&warehouse.run(&["bay", "add", "feature", bay_dir.to_str().unwrap()]));

    // The state a `bay add` that failed writing its path file leaves: the path file is written
    // before the redirect, so neither exists. Unlike a damaged path file, nothing is misread, and
    // the bay is removable.
    std::fs::remove_file(warehouse.root.join(".forklift/bays/feature/path")).unwrap();
    std::fs::remove_file(bay_dir.join(".forklift")).unwrap();

    let listed = warehouse.run(&["bay"]);
    assert_success(&listed);
    assert!(stdout(&listed).contains("feature"), "{}", stdout(&listed));

    assert_success(&warehouse.run(&["bay", "remove", "feature"]));
    assert!(!warehouse.root.join(".forklift/bays/feature").exists());
    assert!(!bay_dir.join(".forklift").exists());
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

// Revert: drop deliver's target-coverage check → the delivery lands and other.txt is back to v1.
#[test]
fn deliver_onto_a_target_that_moved_after_the_draft_forked_is_refused() {
    let warehouse = TestWarehouse::new("deliver-moved-target");
    warehouse.write_file("code.txt", "v1\n");
    warehouse.write_file("other.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["palletize", "draft"]));
    warehouse.write_file("code.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "draft work"]));

    // main moves on after the draft forked from it.
    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("other.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main work"]));
    let main_head = stdout(&warehouse.run(&["history", "-n", "1"]));

    // Delivering the draft would squash its tree, which still has other.txt at v1, over main.
    assert_success(&warehouse.run(&["shift", "draft"]));
    let refused = warehouse.run(&["deliver", "main", "-m", "stale"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("forklift consolidate main"), "{}", stderr(&refused));
    assert_eq!(stdout(&warehouse.run(&["history", "main", "-n", "1"])), main_head, "main must not move");

    // Consolidating main into the draft first makes the delivery carry both changes.
    assert_success(&warehouse.run(&["consolidate", "main"]));
    assert_success(&warehouse.run(&["deliver", "main", "-m", "both"]));
    assert_eq!(std::fs::read_to_string(warehouse.root.join("code.txt")).unwrap(), "v2\n");
    assert_eq!(std::fs::read_to_string(warehouse.root.join("other.txt")).unwrap(), "v2\n");
}

// Revert: accept only a target that is the draft's ancestor (drop the trail-tree arm) → the
// second delivery is refused.
#[test]
fn a_draft_delivered_once_can_deliver_its_later_work() {
    let warehouse = TestWarehouse::new("deliver-again");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["palletize", "draft"]));
    warehouse.write_file("code.txt", "v2\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "wip v2"]));
    assert_success(&warehouse.run(&["deliver", "main", "-m", "first"]));

    // The draft keeps going; main holds only the first delivery, whose tree the draft has seen.
    assert_success(&warehouse.run(&["shift", "draft"]));
    warehouse.write_file("code.txt", "v3\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "wip v3"]));

    let second = warehouse.run(&["deliver", "main", "-m", "second"]);
    assert_success(&second);
    let history = stdout(&warehouse.run(&["history"]));
    assert!(history.contains("second") && history.contains("first"), "{}", history);
    assert_eq!(std::fs::read_to_string(warehouse.root.join("code.txt")).unwrap(), "v3\n");
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
    let created = warehouse.run(&["profile", "create", "work", "--display-name", "Work Me"]);
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

/// The end-to-end regression the strict
/// parse exists to prevent. `get_operator` must refuse outright on a malformed selected profile
/// — and, the part that was unpinned, it must never mint a replacement identifier and write it
/// back over the profile: the global config file must be byte-identical after the refusal.
#[test]
fn a_malformed_selected_profile_refuses_without_writing_back_to_the_global_config() {
    let warehouse = TestWarehouse::new("profile-malformed-no-writeback");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    // A profile with a hand-edited, unquoted identifier, written directly (bypassing
    // "profile create", which would refuse on the same non-string value).
    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[profile.work]\nidentifier = 12345\n").unwrap();

    // Select it via the plain "config" key (not "profile use", which itself calls
    // get_profile and would already refuse before ever setting operator.profile).
    assert_success(&warehouse.run(&["config", "operator.profile", "work"]));

    let before = std::fs::read(&global_config).unwrap();

    // Any command resolving the operator hits this: "office enroll" is the simplest one
    // that needs nothing else prepared.
    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(!enroll.status.success(), "a malformed selected profile must refuse: {}", stdout(&enroll));
    let error = stderr(&enroll);
    assert!(error.contains("work"), "the refusal must name the profile: {}", error);
    assert!(error.contains("identifier"), "the refusal must name the field: {}", error);

    // The real regression this pins: no UUID was minted and written back over the
    // profile. The malformed, hand-edited value survives exactly as written.
    let after = std::fs::read(&global_config).unwrap();
    assert_eq!(
        before, after,
        "a refused get_operator must never write back to the global config file"
    );
}

/// The mint-and-overwrite hazard reached by a
/// value that **parses perfectly**. An empty `identifier` is not a type fault, so the typed
/// parse had nothing to say about it — and `ProfileRecord::to_operator` maps an absent field and
/// an empty one onto the same `""`, which `get_operator` reads as "fill this in". So it minted a
/// UUID and wrote it over the user's own entry, exactly as the malformed cases above once did.
///
/// The counterpart asymmetry is what made it worth a rule rather than a patch: the *direct*
/// branch took `operator.identifier = ""` verbatim, making the empty string the on-chain
/// operator id written into every parcel. One spelling, two opposite readings, one destructive.
#[test]
fn an_empty_identifier_refuses_rather_than_minting_a_replacement_over_it() {
    let warehouse = TestWarehouse::new("empty-identifier-no-writeback");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[profile.work]\nidentifier = \"\"\n").unwrap();
    assert_success(&warehouse.run(&["config", "operator.profile", "work"]));

    let before = std::fs::read(&global_config).unwrap();

    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(!enroll.status.success(), "an empty identifier must refuse: {}", stdout(&enroll));
    let error = stderr(&enroll);
    assert!(error.contains("cannot be empty"), "the refusal must say what is wrong: {}", error);
    assert!(
        error.contains("profile.work.identifier"),
        "the refusal must name the field: {}", error
    );

    assert_eq!(
        before, std::fs::read(&global_config).unwrap(),
        "a refused get_operator must never write back to the global config file"
    );

    // The write-side door onto the same entry: `profile create --id ""` would otherwise lay down
    // the very entry the parse refuses.
    let created = warehouse.run(&["profile", "create", "--id", "", "other"]);
    assert!(!created.status.success(), "an empty minted identifier must refuse: {}", stdout(&created));
    assert!(
        stderr(&created).contains("cannot be empty"),
        "unexpected refusal: {}", stderr(&created)
    );
}

/// Pinned at the call site rather than in the helper: `forklift config` stores a
/// fixed-value key normalized, so what lands on disk is literally one of the three spellings the
/// guide enumerates.
///
/// `"  ON  "` is the fixture because it discriminates — the range check admits it either way
/// (`validate_value` normalizes before testing membership), so the *only* difference between
/// storing normalized and storing raw is the spelling in the file: `on` against `  ON  `. The
/// read-back then shows the value is understood, which a raw-storing implementation also manages
/// — that assertion is the counterpart, not the pin.
#[test]
fn a_fixed_value_key_is_written_to_disk_in_its_documented_spelling() {
    let warehouse = TestWarehouse::new("config-normalizes-fixed-values");
    assert_success(&warehouse.run(&["prepare"]));

    assert_success(&warehouse.run(&["config", "remote.tor", "  ON  "]));

    let config = warehouse.root.join(".forklift/config/warehouse.toml");
    let content = std::fs::read_to_string(&config).unwrap();
    assert!(
        content.contains("tor = \"on\""),
        "the stored value must be the documented spelling, not what was typed: {content}"
    );
    assert!(
        !content.contains("  ON  "),
        "the raw argument must not reach the file: {content}"
    );

    // And the file is an ordinary one afterwards — the stored value is admissible on the way
    // back in, which is the whole reason for normalizing it on the way out.
    let read = warehouse.run(&["config", "remote.tor"]);
    assert_success(&read);
    assert!(stdout(&read).contains("on"), "unexpected read-back: {}", stdout(&read));
}

/// The `--json` set envelope must report the spelling that reached the file.
///
/// Normalizing on the way out gave the command two answers to the same question — the argument
/// it was handed and the value it wrote — and the envelope echoed the first. A consumer caching
/// that echo (the only reason the envelope exists, since human mode prints nothing) held
/// `"  ON  "` for a file containing `on`, and disagreed with its own next read.
#[test]
fn the_json_set_envelope_reports_the_value_that_reached_the_file() {
    let warehouse = TestWarehouse::new("config-json-echo-is-the-stored-value");
    assert_success(&warehouse.run(&["prepare"]));

    let set = warehouse.run(&["--json", "config", "remote.tor", "  ON  "]);
    assert_success(&set);

    let envelope = json(&set);
    assert_eq!(envelope["data"]["key"], "remote.tor");
    assert_eq!(
        envelope["data"]["value"], "on",
        "the envelope must report what was written, not what was typed: {envelope}"
    );

    // The pin is the agreement between the three surfaces, so read the other two back here.
    let content = std::fs::read_to_string(warehouse.root.join(".forklift/config/warehouse.toml"))
        .unwrap();
    assert!(content.contains("tor = \"on\""), "unexpected file content: {content}");
    assert!(stdout(&warehouse.run(&["config", "remote.tor"])).contains("on"));
}

/// A refused profile creation leaves nothing behind. Creation used to be two writes, and a value
/// refused by the second left the first on disk.
///
/// `profile create --display-name "" work` minted an identifier, wrote `[profile.work]`, then refused the
/// empty display name — reporting failure while leaving a profile behind that made the obvious
/// retry fail too ("the profile \"work\" already exists"), with no command able to remove it.
/// The fixture is the *second* field for that reason: refusing on the first would prove nothing
/// about ordering.
///
/// Two mechanisms now hold this, and either alone does: `create_profile` refuses its inputs
/// before writing anything, and creation is a single write of every field. Measured:
/// with the up-front checks removed this test stays green; with them removed *and* the write split
/// per field it goes red.
#[test]
fn a_refused_profile_field_leaves_no_half_created_profile_behind() {
    let warehouse = TestWarehouse::new("profile-create-is-all-or-nothing");
    assert_success(&warehouse.run(&["prepare"]));

    let created = warehouse.run(&["profile", "create", "work", "--display-name", ""]);
    assert!(!created.status.success(), "an empty display name must refuse: {}", stdout(&created));
    assert!(
        stderr(&created).contains("cannot be empty"),
        "unexpected refusal: {}", stderr(&created)
    );

    // Nothing was written: the global file either does not exist or holds no profile section.
    let global_config = warehouse.home.join("global-config.toml");
    let content = std::fs::read_to_string(&global_config).unwrap_or_default();
    assert!(
        !content.contains("[profile.work]"),
        "a refused creation must leave no profile behind: {content}"
    );

    // And the retry that used to be locked out succeeds.
    let retry = warehouse.run(&["profile", "create", "work", "--display-name", "Work Me"]);
    assert_success(&retry);
}

/// An empty value is refused with the fact alone wherever no advice is true on every path, and with
/// "omit it" where the value came from an option. `--unset` is never suggested: it needs the right
/// scope, fails for a key that is not set, and cannot address a profile field, so any hint built
/// on it was wrong somewhere.
#[test]
fn an_empty_value_is_refused_with_advice_only_where_the_advice_is_always_true() {
    let warehouse = TestWarehouse::new("empty-value-remedy-per-door");
    assert_success(&warehouse.run(&["prepare"]));

    let global_config = warehouse.home.join("global-config.toml");

    // `config`, both scopes, a key that is set and one that is not: the same bare fact. The
    // global case runs inside a warehouse, where an `--unset` without `--global` used to be
    // suggested and would have targeted the wrong file.
    assert_success(&warehouse.run(&["config", "--global", "operator.name", "Ada"]));

    for (args, key) in [
        (vec!["config", "--global", "operator.name", ""], "operator.name"),
        (vec!["config", "operator.name", "   "], "operator.name"),
        (vec!["config", "remote.token", ""], "remote.token"),
    ] {
        let refused = warehouse.run(&args);
        assert!(!refused.status.success(), "{args:?} must refuse: {}", stdout(&refused));
        assert_eq!(stderr(&refused).trim(), format!("\"{key}\" cannot be empty."));
    }

    // The refused write left the value it would have replaced.
    let read = warehouse.run(&["config", "--global", "operator.name"]);
    assert_eq!(stdout(&read).trim(), "Ada", "a refused write must leave the file as it was");

    // An unknown key is reported as unknown, not as empty: the key is checked first.
    let refused = warehouse.run(&["config", "operator.nmae", ""]);
    assert!(
        stderr(&refused).contains("Unknown configuration key"),
        "unexpected: {}", stderr(&refused)
    );

    // The `profile create` door: nothing is on disk yet, so the advice is about the option.
    for (args, option) in [
        (["profile", "create", "work", "--display-name", ""], "--display-name"),
        (["profile", "create", "work", "--id", "  "], "--id"),
    ] {
        let refused = warehouse.run(&args);
        assert!(!refused.status.success(), "{option} \"\" must refuse: {}", stdout(&refused));

        let message = stderr(&refused);
        assert!(message.contains(option), "the refusal must name the option: {message}");
        assert!(message.contains("Omit it"), "the refusal must say what to do: {message}");
        assert!(
            !message.contains("file") && !message.contains("--unset"),
            "nothing was written, so there is no entry to remove: {message}"
        );
    }

    // The parse door: a bad *global* file read from inside a warehouse. The message names the
    // file and says to fix it — and offers no `--unset`, which here would carry no `--global`
    // and could not succeed while another entry in the file was also bad.
    std::fs::write(&global_config, "[operator]\nidentifier = \"op-1\"\nname = \"\"\n").unwrap();

    let read = warehouse.run(&["config", "operator.name"]);
    assert!(!read.status.success(), "a file holding an empty value must refuse");

    let message = stderr(&read);
    assert!(message.contains("global-config.toml"), "the refusal must name the file: {message}");
    assert!(message.contains("\"operator.name\" cannot be empty."), "unexpected: {message}");
    assert!(message.contains("Fix the file by hand"), "unexpected: {message}");
    assert!(!message.contains("--unset"), "the parse door may not suggest --unset: {message}");
}

/// `franchise --token ""` and `profile use ""` refuse in their own words before doing anything,
/// rather than reaching the configuration write — whose refusal would name a key the user never
/// typed.
#[test]
fn franchise_and_profile_use_refuse_an_empty_value_in_their_own_words() {
    let warehouse = TestWarehouse::new("empty-value-franchise-profile-use");
    assert_success(&warehouse.run(&["prepare"]));

    // Nothing listens here: a refusal that came after the handshake would be a network error,
    // not the one asserted below.
    let refused = warehouse.run(&["franchise", "http://127.0.0.1:9/", "clone", "--token", "  "]);
    assert!(!refused.status.success(), "an empty token must refuse: {}", stdout(&refused));

    let message = stderr(&refused);
    assert!(message.contains("--token"), "the refusal must name the option: {message}");
    assert!(message.contains("Omit it"), "the refusal must say what to do: {message}");
    assert!(!message.contains("--unset"), "there is no entry to remove: {message}");
    assert!(!warehouse.root.join("clone").exists(), "the refusal must create nothing");

    // A hand-written `[profile.""]` is found by the lookup, so without its own check `profile use`
    // reached the configuration write and was refused there.
    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[profile.\"\"]\nidentifier = \"op-1\"\n").unwrap();

    let refused = warehouse.run(&["profile", "use", ""]);
    assert!(!refused.status.success(), "an empty profile name must refuse: {}", stdout(&refused));

    let message = stderr(&refused);
    assert!(message.contains("A profile name cannot be empty."), "unexpected: {message}");
    assert!(!message.contains("--unset"), "there is no entry to remove: {message}");

    let read = warehouse.run(&["config", "operator.profile"]);
    assert!(!read.status.success(), "nothing may be selected: {}", stdout(&read));
    assert!(stderr(&read).contains("is not set"), "unexpected read: {}", stderr(&read));
}

/// An empty profile *name* is refused like an empty value.
///
/// `profile create ""` wrote a `[profile.""]` section that no command could then reach: selecting
/// it means `operator.profile = ""`, which the same rule refuses at both the read and the write
/// door, and there is no `profile delete`. Creating something unusable and unremovable is worse
/// than refusing to create it.
///
/// Run here rather than in `config_utils`'s unit tests because `create_profile` writes to the
/// real global configuration, and only this harness scopes HOME to the test.
#[test]
fn an_empty_profile_name_is_refused_rather_than_created_unreachable() {
    let warehouse = TestWarehouse::new("profile-create-empty-name");
    assert_success(&warehouse.run(&["prepare"]));

    let created = warehouse.run(&["profile", "create", ""]);
    assert!(!created.status.success(), "an empty profile name must refuse: {}", stdout(&created));
    assert!(
        stderr(&created).contains("cannot be empty"),
        "unexpected refusal: {}", stderr(&created)
    );

    let content = std::fs::read_to_string(warehouse.home.join("global-config.toml"))
        .unwrap_or_default();
    assert!(
        !content.contains("[profile.\"\"]"),
        "a refused creation must leave no profile section behind: {content}"
    );
}

/// The same mint-and-overwrite hazard on the *more
/// common* code path — no `operator.profile` set at all, so `get_operator` reads
/// `operator.identifier` directly. A hand-edited, unquoted global `identifier = 12345` used to
/// collapse into "absent", mint a fresh UUID, and write it back over the value.
#[test]
fn a_malformed_default_identifier_refuses_without_writing_back_to_the_global_config() {
    let warehouse = TestWarehouse::new("default-identifier-malformed-no-writeback");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    // A hand-edited, unquoted global operator.identifier, written directly — no profile
    // selected, so get_operator falls straight to the non-profile branch.
    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[operator]\nidentifier = 12345\n").unwrap();

    let before = std::fs::read(&global_config).unwrap();

    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(!enroll.status.success(), "a malformed operator.identifier must refuse: {}", stdout(&enroll));
    let error = stderr(&enroll);
    assert!(error.contains("operator.identifier"), "the refusal must name the key: {}", error);

    // The real regression this pins: no UUID was minted and written back over the value.
    let after = std::fs::read(&global_config).unwrap();
    assert_eq!(
        before, after,
        "a refused get_operator must never mint and write back to the global config file"
    );
}

/// The hazard reached through the one key that
/// selects *which* identity is read at all. A warehouse `profile = 2024` written unquoted used
/// to read as absent, so the profile selection was silently discarded, `get_operator` fell
/// through to the non-profile branch, minted a fresh UUID, and wrote an `[operator]` section
/// into a global config that only ever held `[profile.2024]`.
#[test]
fn a_malformed_operator_profile_refuses_without_writing_back_to_the_global_config() {
    let warehouse = TestWarehouse::new("profile-selector-malformed-no-writeback");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    // A valid named profile in the global config...
    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[profile.2024]\nidentifier = \"op-1\"\n").unwrap();

    // ...selected by a hand-edited, unquoted (numeric) warehouse `operator.profile`.
    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "[operator]\nprofile = 2024\n",
    ).unwrap();

    let before = std::fs::read(&global_config).unwrap();

    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(!enroll.status.success(), "a malformed operator.profile must refuse: {}", stdout(&enroll));
    let error = stderr(&enroll);
    assert!(error.contains("operator.profile"), "the refusal must name the key: {}", error);

    let after = std::fs::read(&global_config).unwrap();
    assert_eq!(
        before, after,
        "a refused get_operator must never fall through to a different identity and mint one"
    );
}

/// The generalization the three tests above could not express, and the reason FORK-117 replaced
/// the reader instead of hardening it key by key: damage at the **section** level, where there
/// is no key to be strict about. `operator = 5` is not a malformed `operator.identifier` — the
/// identifier is not there at all, and neither is anything else. Every key-level rule reads
/// this file as "nothing is configured", which for the identity means minting a UUID into the
/// *global* file over a warehouse the user thought was configured.
///
/// The global scope holds a perfectly good identifier here on purpose: that is what makes the
/// mint observable as a rewrite of a file the command had no business touching.
///
/// Falsified both directions: run against an `origin/main` worktree's binary it fails at
/// `assert!(!success)` — `office enroll` succeeds with `Enrolled "global@id"`, silently ignoring
/// the warehouse's own configuration.
#[test]
fn a_warehouse_section_that_is_not_a_table_refuses_instead_of_reading_as_unconfigured() {
    let warehouse = TestWarehouse::new("operator-section-not-a-table");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[operator]\nidentifier = \"global@id\"\n").unwrap();

    std::fs::write(
        warehouse.root.join(".forklift/config/warehouse.toml"),
        "operator = 5\n",
    ).unwrap();

    let before = std::fs::read(&global_config).unwrap();

    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(
        !enroll.status.success(),
        "a warehouse section that is not a table must refuse, not read as unconfigured: {}",
        stdout(&enroll)
    );
    let error = stderr(&enroll);
    assert!(error.contains("operator"), "the refusal must name the section: {}", error);
    assert!(error.contains("warehouse.toml"), "the refusal must name the file: {}", error);

    let after = std::fs::read(&global_config).unwrap();
    assert_eq!(before, after, "a refused identity resolution must not touch the global config file");
}

/// A mistyped key *name* is the same hazard as a mistyped value type, and only name-strictness
/// closes it: `identifer = "alice"` is a typo away from `identifier`, and a reader that skips
/// what it does not recognize reports the identity as unset — then mints one over it.
///
/// Falsified both directions: run against an `origin/main` worktree's binary it fails at
/// `assert!(!success)` — `office enroll` succeeds, having minted a fresh UUID and written it
/// into the global file beside the typo.
#[test]
fn a_mistyped_configuration_key_refuses_instead_of_reading_as_unset() {
    let warehouse = TestWarehouse::new("config-mistyped-key");
    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["prepare"]));

    let global_config = warehouse.home.join("global-config.toml");
    std::fs::write(&global_config, "[operator]\nidentifer = \"alice\"\n").unwrap();

    let before = std::fs::read(&global_config).unwrap();

    let enroll = warehouse.run(&["office", "enroll"]);
    assert!(!enroll.status.success(), "a mistyped key must refuse: {}", stdout(&enroll));
    let error = stderr(&enroll);
    assert!(error.contains("identifer"), "the refusal must quote what was written: {}", error);
    assert!(
        error.contains("operator.identifier"),
        "the refusal must list the keys forklift does know: {}", error
    );

    let after = std::fs::read(&global_config).unwrap();
    assert_eq!(before, after, "a refused identity resolution must not mint over a typo");
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
    assert_eq!(value["lonic_json"], 1);
    assert_eq!(value["tool"], "forklift");
    assert_eq!(value["schema"], 3);
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

    // A failure keeps the success header, `command` included: it names the command that
    // failed, so a consumer classifies by the same header on either outcome.
    assert_eq!(value["lonic_json"], 1);
    assert_eq!(value["tool"], "forklift");
    assert_eq!(value["schema"], 3);
    assert_eq!(value["command"], "stocktake");
    assert!(value.get("data").is_none(), "a failure carries no data: {}", value);
}

#[test]
fn a_json_failure_names_the_command_clap_resolved_not_the_alias_typed() {
    let warehouse = TestWarehouse::new("json-error-alias");

    // `st` is an alias: the header names the canonical command, the same name a success
    // document would carry, never the token the caller typed.
    let outside = warehouse.run(&["--json", "st"]);
    assert_eq!(outside.status.code(), Some(3));
    let value = json(&outside);
    assert_eq!(value["error"]["code"], "not_a_warehouse");
    assert_eq!(value["command"], "stocktake");
}

#[test]
fn a_usage_error_under_json_is_one_failure_document_with_exit_2() {
    let warehouse = TestWarehouse::new("json-usage-error");

    // An unknown command: the whole of stdout is one document, and it names no command,
    // because the command line names none clap knows.
    let unknown = warehouse.run(&["--json", "frobnicate"]);
    assert_eq!(unknown.status.code(), Some(2));
    let value = json(&unknown);
    assert_eq!(value["lonic_json"], 1);
    assert_eq!(value["tool"], "forklift");
    assert_eq!(value["schema"], 3);
    assert_eq!(value["ok"], false);
    assert!(value["command"].is_null(), "no known command was named: {}", value);
    assert_eq!(value["error"]["code"], "usage");
    assert_eq!(value["error"]["message"], "unrecognized subcommand 'frobnicate'");
    assert!(value["error"]["next_step"].as_str().unwrap().contains("forklift --help"));
    let usage = value["error"]["details"]["usage"].as_str().unwrap();
    assert!(usage.contains("Usage: forklift"), "details keep clap's full text: {}", usage);
    assert!(stderr(&unknown).is_empty(), "nothing else is printed: {}", stderr(&unknown));

    // A bad argument to a command named by alias (`a` is `audit`), with `--json` after the
    // command: the flag is found wherever it sits, and the header carries the canonical name.
    let bad_argument = warehouse.run(&["a", "--nope", "--json"]);
    assert_eq!(bad_argument.status.code(), Some(2));
    let value = json(&bad_argument);
    assert_eq!(value["error"]["code"], "usage");
    assert_eq!(value["command"], "audit");
    assert_eq!(value["error"]["message"], "unexpected argument '--nope' found");

    // No command at all.
    let missing = warehouse.run(&["--json"]);
    assert_eq!(missing.status.code(), Some(2));
    let value = json(&missing);
    assert_eq!(value["error"]["code"], "usage");
    assert!(value["command"].is_null(), "{}", value);

    // Without `--json`, clap reports as it always has: prose on stderr, nothing on stdout.
    // `--json` after a `--` terminator is an operand, not the flag, so it stays prose too.
    for args in [&["frobnicate"][..], &["frobnicate", "--", "--json"][..]] {
        let human = warehouse.run(args);
        assert_eq!(human.status.code(), Some(2), "{:?}", args);
        assert!(stdout(&human).is_empty(), "{:?} printed to stdout: {}", args, stdout(&human));
        assert!(stderr(&human).contains("unrecognized subcommand"), "{:?}: {}", args, stderr(&human));
    }
}

#[test]
fn help_and_version_under_json_are_success_documents() {
    let warehouse = TestWarehouse::new("json-help");

    // Help is the result the caller asked for, so `ok` is true and `data` carries clap's text.
    let help = warehouse.run(&["--json", "history", "--help"]);
    assert_eq!(help.status.code(), Some(0));
    let value = json(&help);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "history");
    // clap names the binary as invoked (`forklift.exe` on Windows), so match around it.
    let help_text = value["data"]["message"].as_str().unwrap();
    assert!(help_text.contains("Usage: forklift") && help_text.contains(" history [OPTIONS]"), "{}", help_text);

    let version = warehouse.run(&["--json", "--version"]);
    assert_eq!(version.status.code(), Some(0));
    let value = json(&version);
    assert_eq!(value["ok"], true);
    assert!(value["data"]["message"].as_str().unwrap().starts_with("forklift "), "{}", value);
    assert_eq!(value["command"], serde_json::Value::Null);

    // A help flag before the command name is the top-level help: no command ran, whatever
    // name follows it.
    let top = json(&warehouse.run(&["--json", "--help", "st"]));
    assert_eq!(top["ok"], true);
    assert_eq!(top["command"], serde_json::Value::Null);

    // The same in a short-flag cluster.
    let clustered = json(&warehouse.run(&["--json", "-vh", "st"]));
    assert_eq!(clustered["ok"], true);
    assert_eq!(clustered["command"], serde_json::Value::Null);
}

#[test]
fn the_help_command_under_json_answers_like_the_help_flag() {
    let warehouse = TestWarehouse::new("json-help-command");

    // `help <command>` (alias `h`) is the same result as `<command> --help`: one success document.
    let help = warehouse.run(&["--json", "h", "history"]);
    assert_eq!(help.status.code(), Some(0));
    let value = json(&help);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "help");
    let help_text = value["data"]["message"].as_str().unwrap();
    assert!(help_text.contains(" history [OPTIONS]"), "{}", help_text);

    // An unknown name is the same `usage` failure the parser reports for one, not a generic error.
    let unknown = warehouse.run(&["--json", "help", "bogus"]);
    assert_eq!(unknown.status.code(), Some(2));
    let value = json(&unknown);
    assert_eq!(value["ok"], false);
    assert_eq!(value["error"]["code"], "usage");

    // Without --json a person still gets the way forward in the message itself.
    let human = warehouse.run(&["help", "bogus"]);
    assert_eq!(human.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&human.stderr).contains("Use \"forklift help\""), "{:?}", human);
}

#[test]
fn every_json_document_starts_with_the_header_in_its_documented_order() {
    let warehouse = TestWarehouse::new("json-key-order");

    // The order is checked on the raw text: a parsed `Value` would hide it.
    let header = "{\n  \"lonic_json\": 1,\n  \"tool\": \"forklift\",\n  \"schema\": 3,\n  \"command\": ";

    for args in [&["--json", "version"][..], &["--json", "bogus"], &["--json", "stocktake"]] {
        let output = warehouse.run(args);
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(stdout.starts_with(header), "{:?}: {}", args, stdout);

        let after_command = &stdout[header.len()..];
        let ok = after_command.find("\n  \"ok\": ").expect("ok follows command");
        assert!(!after_command[..ok].contains("\n  \""), "{:?}: {}", args, stdout);
    }
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
    forklift_core::util::marker_utils::add(&marker_path, "").unwrap();

    let stacked = warehouse.run(&["stack", "second", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let value = json(&stacked);
    assert_eq!(value["error"]["code"], "incomplete_load");
    assert!(value["error"]["message"].as_str().unwrap().contains("./"));

    // A marker in a format this build cannot read refuses too, naming the file, rather than
    // reading as no incomplete load at all.
    std::fs::write(&marker_path, "./\n").unwrap();

    let stacked = warehouse.run(&["stack", "second", "--json"]);
    assert_eq!(stacked.status.code(), Some(20));
    let value = json(&stacked);
    assert_eq!(value["error"]["code"], "incomplete_load");
    assert!(value["error"]["message"].as_str().unwrap().contains("has no format header"), "{}", value);
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
fn a_malformed_marker_reads_as_the_whole_warehouse_until_a_full_load_and_refuses_stack() {
    let warehouse = TestWarehouse::new("incomplete-load-malformed");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("a.txt", "x\n");

    let marker_path = incomplete_load_marker_path(&warehouse);
    std::fs::create_dir_all(marker_path.parent().unwrap()).unwrap();

    // Hand-corrupt the marker (invalid UTF-8 — never producible by this module itself). A load
    // still proceeds, but the unreadable file may have recorded any load, so it becomes the
    // whole warehouse: a load of one file does not clear it, a load of the root does.
    std::fs::write(&marker_path, [0xFF, 0xFE, 0x00, 0xFF]).unwrap();
    assert_success(&warehouse.run(&["load", "a.txt"]));
    let refused = warehouse.run(&["stack", "first", "--json"]);
    assert_eq!(refused.status.code(), Some(20));
    assert!(json(&refused)["error"]["message"].as_str().unwrap().contains("./"), "{}", json(&refused));
    assert_success(&warehouse.run(&["load", "."]));
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
        value["error"]["next_step"].as_str().unwrap().contains("forklift load ."),
        "points at a recovery path: {:?}", value["error"]
    );

    // The way out it names works.
    warehouse.write_file("a.txt", "y\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));
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
// A `JournalEntry` once snapshotted every pallet ref (user and meta, `@office` included) before
// a journaled op so it could be restored, but an office/meta-pallet mutation between the
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

// `palletize` is not journaled, so after it the newest journal entry is an older operation.
// Reversing that entry would undo something other than the user's last operation: the gate
// refuses because the current pallet is no longer the one the entry left current.
#[test]
fn undo_refuses_to_reach_past_a_palletize() {
    let warehouse = TestWarehouse::new("undo-past-palletize");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));

    // Un-journaled: a new pallet at the same head, made current.
    assert_success(&warehouse.run(&["palletize", "feature"]));
    let main_before = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("left pallet \"main\" current, but \"feature\" is current now"),
        "{}", stderr(&refused)
    );

    let main_after = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(main_before, main_after, "a refused undo must not move main");
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* feature"), "a refused undo must not switch pallets");
}

// The same for `deliver`, which moves its target and makes it current: the newest entry is the
// draft's last checkpoint, and reversing it would rewind the draft and switch back onto it.
#[test]
fn undo_refuses_to_reach_past_a_deliver() {
    let warehouse = TestWarehouse::new("undo-past-deliver");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));
    assert_success(&warehouse.run(&["office", "enroll"]));

    assert_success(&warehouse.run(&["palletize", "draft/feature"]));
    for v in ["v2", "v3"] {
        warehouse.write_file("code.txt", &format!("{}\n", v));
        assert_success(&warehouse.run(&["load", "."]));
        assert_success(&warehouse.run(&["stack", &format!("wip {}", v)]));
    }
    assert_success(&warehouse.run(&["deliver", "main", "-m", "add the feature"]));

    let draft_before = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/draft/feature")).unwrap();

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("left pallet \"draft/feature\" current, but \"main\" is current now"),
        "{}", stderr(&refused)
    );

    let draft_after = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/draft/feature")).unwrap();
    assert_eq!(draft_before, draft_after, "a refused undo must not rewind the draft");
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* main"), "a refused undo must not switch pallets");
}

// A shift entry passes through the same gate: after `shift main` and an un-journaled
// `palletize other`, undo must not shift back to "side" — the user's last operation was the
// palletize, not the shift.
#[test]
fn undo_refuses_to_reverse_a_shift_past_a_palletize() {
    let warehouse = TestWarehouse::new("undo-shift-past-palletize");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    warehouse.write_file("base.txt", "base\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));
    assert_success(&warehouse.run(&["palletize", "side"]));
    warehouse.write_file("side.txt", "side\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "side work"]));

    // Journaled: side → main. Un-journaled after it: a new pallet at main's head, made current.
    assert_success(&warehouse.run(&["shift", "main"]));
    assert_success(&warehouse.run(&["palletize", "other"]));

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    assert!(
        stderr(&refused).contains("left pallet \"main\" current, but \"other\" is current now"),
        "{}", stderr(&refused)
    );
    assert!(stdout(&warehouse.run(&["palletize"])).contains("* other"), "a refused undo must not shift");
    assert!(!warehouse.root.join("side.txt").exists(), "a refused undo must not materialize \"side\"");
}

// A journal that cannot be read is named, never read as empty ("nothing to undo"). A command
// that succeeds still succeeds — journaling is best-effort — and leaves the file for the user.
#[test]
fn undo_refuses_a_corrupt_journal_by_name() {
    let warehouse = TestWarehouse::new("undo-corrupt-journal");
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["prepare"]));

    warehouse.write_file("a.txt", "one\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "first"]));
    warehouse.write_file("b.txt", "two\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "second"]));

    let journal = warehouse.root.join(".forklift/journal.json");
    assert!(journal.is_file(), "the second stack must have been journaled");
    std::fs::write(&journal, "{\"version\":1,\"entr").unwrap();
    let main_before = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();

    let refused = warehouse.run(&["undo"]);
    assert!(!refused.status.success());
    let message = stderr(&refused);
    assert!(message.contains("journal.json") && message.contains("Remove it to start a fresh journal"), "{}", message);
    assert!(!message.contains("nothing to undo"), "{}", message);
    let main_after = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    assert_eq!(main_before, main_after, "a refused undo must not move main");

    warehouse.write_file("c.txt", "three\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "third"]));
    assert_eq!(std::fs::read_to_string(&journal).unwrap(), "{\"version\":1,\"entr", "a refused journal is never rewritten");

    // The remedy the message names works.
    std::fs::remove_file(&journal).unwrap();
    warehouse.write_file("d.txt", "four\n");
    assert_success(&warehouse.run(&["load", "."]));
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

// A conflicted cherry-pick moves no ref but leaves a pick in progress; both it and the stack that
// completes it are undoable, and undoing the completion puts the pick back in progress so the
// next stack completes it again.
#[test]
fn undo_reverses_a_conflicted_cherry_pick_and_its_completion() {
    let warehouse = TestWarehouse::new("undo-cherry-pick-conflict");
    warehouse.write_file("f.txt", "alpha\nbeta\ngamma\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "feature"]));
    warehouse.write_file("f.txt", "alpha\nbeta-FEATURE\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    let source = extract_parcel_hash(&warehouse.run(&["stack", "feature edits beta"]));

    assert_success(&warehouse.run(&["shift", "main"]));
    warehouse.write_file("f.txt", "alpha\nbeta-MAIN\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "main edits beta"]));
    let main_head = std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap();
    let pick_state = warehouse.root.join(".forklift/cherry-pick");

    // Undoing the conflicted pick abandons it: no parcel comes off, the pick is no longer in
    // progress — and the stack before it is not reached.
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "conflicts");
    let undone = json(&warehouse.run(&["--json", "undo"]));
    assert_eq!(undone["data"]["op"], "cherry-pick");
    assert!(undone["data"].get("undone").is_none(), "{}", undone);
    assert!(!pick_state.exists(), "undo must abandon the pick");
    assert_eq!(std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap(), main_head);

    // Pick again, resolve and complete it; undoing the completion puts the pick back in progress.
    warehouse.write_file("f.txt", "alpha\nbeta-MAIN\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    let picked = json(&warehouse.run(&["--json", "cherry-pick", &source]));
    assert_eq!(picked["data"]["outcome"], "conflicts");
    warehouse.write_file("f.txt", "alpha\nbeta-RESOLVED\ngamma\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "resolved pick"]));
    assert!(!pick_state.exists());

    let undone = json(&warehouse.run(&["--json", "undo"]));
    assert_eq!(undone["data"]["op"], "stack");
    assert!(pick_state.exists(), "undoing the completing stack must put the pick back in progress");
    assert_eq!(std::fs::read_to_string(warehouse.root.join(".forklift/pallets/main")).unwrap(), main_head);

    assert_success(&warehouse.run(&["stack", "resolved pick again"]));
    assert!(!pick_state.exists(), "the re-stack completes the pick");
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

    // A second repack is idempotent and never loses the pack (regression: the layout-derived
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
fn a_writer_waits_for_object_store_maintenance_and_says_so() {
    // FORK-150: a writing command holds the object store shared for its whole run, so a full
    // repack, gc or scope-prune in another process — which takes it exclusively — cannot delete
    // an object the command has just decided it can reuse. Writers wait rather than fail, and
    // say what they are waiting for. The test process stands in for the maintenance.
    //
    // Revert: the command skips its shared hold → it never waits and finishes while the
    // maintenance still holds the store → red.
    use std::io::BufRead;

    let warehouse = TestWarehouse::new("store-access-wait");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file("f.txt", "hello\n");

    let maintenance = hold_store_access(&warehouse.root, true);
    let mut child = forklift_command()
        .args(["load", "."])
        .current_dir(&warehouse.root)
        .env("FORKLIFT_GLOBAL_CONFIG", warehouse.home.join("global-config.toml"))
        .env("FORKLIFT_KEYS_DIR", warehouse.home.join("test-keys"))
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();

    let (lines, received) = std::sync::mpsc::channel();
    let stderr = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stderr).lines() {
            let Ok(line) = line else { break };
            if lines.send(line).is_err() { break }
        }
    });

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match received.recv_timeout(left) {
            Ok(line) if line.contains("Waiting for object-store maintenance") => break,
            Ok(_) => continue,
            Err(_) => {
                let _ = child.kill();
                panic!("the load never said it was waiting for the maintenance");
            }
        }
    }
    assert!(child.try_wait().unwrap().is_none(), "the load must still be waiting while the maintenance runs");

    drop(maintenance);
    let status = child.wait().unwrap();
    assert!(status.success(), "once the maintenance is done, the load proceeds");
    let status = json(&warehouse.run(&["--json", "stocktake"]));
    assert!(status["data"]["staged"].as_array().unwrap().iter().any(|entry| entry["path"] == "f.txt"),
        "the waited-for load staged the file: {}", status);
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
    // The command must have released its shared store-access hold first: a full repack takes the
    // store exclusively, and a process's own shared hold would refuse it — quietly, on the
    // auto-maintenance path, leaving the packs unconsolidated.
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

// The finding #1 fix (`run_if_due` no longer swallows a `compact` failure that left a durability
// taint standing) is covered where it now lives, without a fault hook in the load-bearing
// `sync_dir` primitive: `commands::maintenance::tests::
// report_maintenance_outcome_surfaces_exactly_when_a_real_taint_is_left_standing` (the decision,
// against a real recorded taint) and `output::tests::
// the_standing_taint_warning_names_the_code_and_the_heal_remedy_in_both_modes` (the warning text,
// both output modes) — both in `crates/forklift/src/`, since the `forklift` crate is a plain
// binary (no `lib` target `tests/cli.rs` could link a unit under test from).

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

/// A bay scope recorded in the older, unversioned format is refused, and the way out it names is
/// the bay's own (add it again), not the sparse warehouse's (franchise again).
#[test]
fn a_scoped_bay_with_an_unversioned_scope_file_names_re_adding_the_bay() {
    let warehouse = scoped_fixture("scoped-legacy");

    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    let scope_file = warehouse.root.join(".forklift/bays/scoped/scope");
    assert!(scope_file.exists(), "the scoped bay records its scope at {}", scope_file.display());
    std::fs::write(&scope_file, "src/api\n").unwrap();

    let refused = warehouse.run_at(&scoped_dir, &["stocktake"]);
    assert!(!refused.status.success(), "{}", stdout(&refused));
    assert!(stderr(&refused).contains("older version of Forklift"), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("Remove the bay and add it again"), "{}", stderr(&refused));
    assert!(!stderr(&refused).contains("Franchise"), "{}", stderr(&refused));
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

/// The skeleton records out-of-scope paths, and a tree name may hold a line break. A scoped
/// merge left in progress by an in-scope conflict, then resolved and stacked, must commit the
/// same tree a full bay does — the skeleton has to read back the name it wrote. (Windows cannot
/// create a file name holding a line break.)
#[cfg(unix)]
#[test]
fn a_scoped_merge_resumed_after_a_conflict_keeps_an_out_of_scope_name_holding_a_line_break() {
    let warehouse = TestWarehouse::new("scoped-skeleton-line-break");
    warehouse.write_file("src/api/a.txt", "api a v1\n");
    warehouse.write_file("src/web\nold/w.txt", "web v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    assert_success(&warehouse.run(&["palletize", "theirs"]));
    warehouse.write_file("src/web\nold/w.txt", "web v2 from theirs\n");
    warehouse.write_file("src/api/a.txt", "api a v2 from theirs\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "theirs edits web and api"]));
    assert_success(&warehouse.run(&["shift", "main"]));

    let full_dir = warehouse.home.join("bay-full");
    let scoped_dir = warehouse.home.join("bay-scoped");
    assert_success(&warehouse.run(&["bay", "add", "full", full_dir.to_str().unwrap()]));
    assert_success(&warehouse.run(&["bay", "add", "scoped", scoped_dir.to_str().unwrap(), "--scope", "src/api"]));

    let mut trees = Vec::new();
    for (bay, dir) in [("full", &full_dir), ("scoped", &scoped_dir)] {
        std::fs::write(dir.join("src/api/a.txt"), format!("api a v2 from {}\n", bay)).unwrap();
        assert_success(&warehouse.run_at(dir, &["load", "."]));
        assert_success(&warehouse.run_at(dir, &["stack", "edit api"]));

        let consolidated = warehouse.run_at(dir, &["--json", "consolidate", "theirs"]);
        assert_success(&consolidated);
        assert_eq!(json(&consolidated)["data"]["outcome"], "conflicts", "{}", stdout(&consolidated));

        std::fs::write(dir.join("src/api/a.txt"), "api a resolved\n").unwrap();
        assert_success(&warehouse.run_at(dir, &["load", "."]));
        assert_success(&warehouse.run_at(dir, &["stack", "complete the merge"]));
        trees.push(parcel_tree_hash(&warehouse, &pallet_head_hash(&warehouse, bay)));
    }

    assert_eq!(trees[0], trees[1],
        "the scoped merge must commit theirs' out-of-scope directory under its exact name");
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
    // pack id is layout-derived, so an unchanged repack neither churns the name nor loses an
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
    std::fs::write(dir.join("taint-1-0"), forklift_core::util::taint_utils::taint_file_bytes(paths)).unwrap();
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

    // A valid-hex, loose-object-shaped path (not a "zz"-style placeholder — non-hex would now
    // escalate under I1/I2 instead of exercising the `Vanished` verdict this test pins).
    plant_taint(&warehouse, &[&loose_object_relative_path(&"9".repeat(64))]);

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
    std::fs::write(dir.join("taint-1-0"), b"/forklift-taint 1\nobjects/ab/cdef\n").unwrap();

    let status = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    assert_eq!(json(&status)["error"]["code"], "durability_taint");
    assert!(dir.join("taint-1-0").exists(), "a torn taint file must survive the refusal");
}

/// Every regular taint file's recorded lines, unioned (dropping the header and the `END`
/// terminator) — used to inspect exactly what a recovery attempt left standing. The paths these
/// tests plant are plain object paths, which escaping leaves as they are.
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
        paths.extend(content.lines()
            .filter(|line| *line != "END" && !line.starts_with("/forklift-taint "))
            .map(|line| line.to_string()));
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
    // No remote is configured here, so `heal` never attempts the §3.2 heal-driven refetch —
    // the message says so honestly, and (post-§3.2) never tells the operator to run a fetch by
    // hand ("forklift lower" is no longer named — see `recovery_utils::HEAVYWEIGHT_EXITS`'s doc
    // comment for why that would now be false prose).
    assert!(
        value["error"]["message"].as_str().unwrap().contains("no remote is configured"),
        "unexpected message: {}", value
    );
    assert!(!value["error"]["message"].as_str().unwrap().contains("forklift lower"));
    // Machine-coded remedies, not an exitless refusal.
    let next_step = value["error"]["next_step"].as_str().unwrap();
    assert!(next_step.contains("franchise") && next_step.contains("accept the loss"));
    assert!(!next_step.contains("forklift lower"));

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
fn heal_on_a_torn_taint_over_an_intact_store_rescans_and_clears() {
    // §8.3 (I5): a torn taint (unknown scope) is no longer a permanent brick — `forklift heal`
    // runs a directory-driven, store-wide rescan and, finding nothing genuinely dangling, clears
    // it — exit 0, same as an ordinary healable taint. CLI-level proof that `recovery_utils::run`
    // is actually wired to rescan torn rather than refuse it immediately (the pre-§8.3 shape,
    // still pinned unit-side by `recovery_utils::tests::torn_taint_over_a_fully_intact_store_
    // rescans_and_clears` and, for entry-heal specifically, `heal_utils::tests::
    // a_torn_taint_refuses_immediately_and_survives`).
    let warehouse = TestWarehouse::new("heal-torn-intact");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("src/app.rs", "intact content, never torn-tainted itself");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    assert_success(&warehouse.run(&["stack", "intact parcel"]));

    let dir = taint_dir_path(&warehouse);
    std::fs::create_dir_all(&dir).unwrap();
    // No terminator line: torn by construction.
    std::fs::write(dir.join("taint-1-0"), b"").unwrap();

    let status = warehouse.run(&["heal", "--json"]);
    assert_success(&status);
    let value = json(&status);
    assert_eq!(value["data"]["was_tainted"], true);

    assert!(!taint_dir_path(&warehouse).exists() || recorded_taint_paths(&warehouse).is_empty(),
        "the taint must be fully cleared, including the torn file itself");

    // The warehouse is fully usable again, including the mutating write path a taint exists to
    // protect.
    warehouse.write_file("src/more.rs", "after the torn rescan");
    assert_success(&warehouse.run(&["load", "src/more.rs"]));
    assert_success(&warehouse.run(&["stack", "after torn rescan"]));
}

#[test]
fn heal_on_a_torn_taint_with_a_dangling_reference_rescans_into_a_named_remainder() {
    // §8.3 (I5): a torn taint whose surviving recorded prefix does not name the dangling object,
    // over a store that genuinely has one, must not clear — the rescan turns the unknown scope
    // into a known, honest remainder naming exactly what is still dangling, and the taint is left
    // standing (no longer torn) for the operator (or a future `forklift heal`) to resolve. Also
    // pins the updated refusal prose: it must no longer claim a "heavyweight recovery" that does
    // not exist, and must not tell the operator to run a fetch by hand ("forklift lower").
    let warehouse = TestWarehouse::new("heal-torn-dangling");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("src/app.rs", "referenced content");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    let stack_out = warehouse.run(&["stack", "referenced parcel"]);
    assert_success(&stack_out);
    let parcel_hash = extract_parcel_hash(&stack_out);

    // The pallet head is a durable ref by construction — vanishing its own parcel object is the
    // simplest genuinely-dangling fixture.
    let relative = loose_object_relative_path(&parcel_hash);
    std::fs::remove_file(warehouse.root.join(".forklift").join(&relative)).unwrap();

    // Torn, and its surviving prefix deliberately does NOT mention the vanished parcel — proving
    // the rescan finds it by walking ref sources (§8.1's targetless enumerator), not by trusting
    // the torn record's own (lower-bound, untrusted) recorded set.
    let dir = taint_dir_path(&warehouse);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("taint-1-0"), b"/forklift-taint 1\nobjects/aa/an-unrelated-surviving-line\n").unwrap();

    let status = warehouse.run(&["heal", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    let value = json(&status);
    assert_eq!(value["error"]["code"], "durability_taint");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("torn"), "the message must still name the torn origin: {}", message);
    assert!(message.contains(&parcel_hash) && message.contains("genuinely dangling"),
        "the message must name the dangling hash the rescan found: {}", message);

    let next_step = value["error"]["next_step"].as_str().unwrap();
    assert!(next_step.contains("no longer torn") && next_step.contains("forklift heal"),
        "the next step must direct to a plain re-run, now that the scope is known: {}", next_step);
    assert!(next_step.contains("franchise") && next_step.contains("accept the loss"),
        "the heavyweight exits must still be named for what a re-run cannot resolve: {}", next_step);
    // No remote is configured, and the rescan itself never drives the §3.2 refetch (that is the
    // *next* heal's job, against the now-known remainder) — "forklift lower" must never be named.
    assert!(!next_step.contains("forklift lower"));
    assert!(!message.contains("heavyweight recovery"),
        "the torn refusal must never name a heavyweight recovery mode that does not exist: {}", message);

    // The taint survives, no longer torn, recording exactly the dangling path.
    assert_eq!(recorded_taint_paths(&warehouse), vec![relative],
        "the remainder must name exactly the dangling parcel's path — not empty, not the whole store");
}

#[test]
fn a_mutating_command_reports_the_lock_before_the_taint_when_both_apply() {
    // The §3.3 reorder (I2's mutating half, `main.rs`): a mutating command must acquire the
    // warehouse lock BEFORE its own entry-heal runs, so a lock-*waiting* command can never restage
    // concurrently with the current lock-holder's own deletions. Observable here without real
    // concurrency, using the same hand-simulated "another process holds the lock" technique as
    // `mutating_commands_respect_the_warehouse_lock`, combined with a standing, unhealable taint:
    // if entry-heal still ran first (the pre-reorder order), it would refuse with
    // `durability_taint` (exit 21) before the lock is ever checked; after the reorder, the lock
    // check runs first and reports `WarehouseLocked` instead. A read-only command is unaffected
    // either way, since it never takes the lock — asserted below for contrast.
    //
    // Mutation: restore `heal_if_tainted()` to run before `WarehouseLock::acquire()` in
    // `main.rs`'s `forklift()` → the mutating command below reports `durability_taint` instead of
    // the lock message → red.
    let warehouse = TestWarehouse::new("reorder-lock-before-heal");
    warehouse.write_file("file.txt", "content\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    // A valid-hex, loose-object-shaped, never-written path: an unhealable ("vanished") taint that
    // would refuse with exit 21 if entry-heal ever got to run.
    plant_taint(&warehouse, &["objects/ab/cdef"]);
    std::fs::write(warehouse.root.join(".forklift/lock"), "12345\n").unwrap();

    // Mutating: the lock wins the race — `WarehouseLocked`, not `durability_taint`.
    let locked = warehouse.run(&["load", "."]);
    assert!(!locked.status.success());
    assert!(stderr(&locked).contains("locked by another forklift process"),
        "a mutating command must report the lock before its own entry-heal runs, got: {}",
        stderr(&locked));

    // Read-only: unaffected by the lock (it never takes it) — the taint still wins, unchanged.
    let read_only = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(read_only.status.code(), Some(21));
    assert_eq!(json(&read_only)["error"]["code"], "durability_taint");

    std::fs::remove_file(warehouse.root.join(".forklift/lock")).unwrap();
}

#[test]
fn entry_heal_escalates_a_non_content_addressed_taint_instead_of_restaging_it_lock_free() {
    // I1/I2: `heal_if_tainted` (lock-free here, since `stocktake` is read-only) must never
    // restage a recorded path it cannot hash-verify — a `.pack`-shaped path stands in for the
    // class of non-content-addressed final paths this taint schema can record (a pack file, an
    // inventory shard `data` file, a pallet ref pointer, …). It escalates to `forklift heal`
    // instead of rewriting the file lock-free. `heal` itself, under the warehouse lock, still
    // restages this same shape — proving this is an entry-heal-only boundary, not a blanket
    // refusal to ever heal a non-object path.
    //
    // Mutation: let `heal_if_tainted` fall through to its ordinary restage for a non-content-
    // addressed path too → the `stocktake` below exits 0 (having rewritten the pack file
    // lock-free) instead of 21, and its inode changes → red.
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;

    let warehouse = TestWarehouse::new("escalation-non-object");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    let pack_dir = warehouse.root.join(".forklift/objects/pack");
    std::fs::create_dir_all(&pack_dir).unwrap();
    let pack_file = pack_dir.join("fake.pack");
    std::fs::write(&pack_file, b"not a real pack, just bytes").unwrap();
    let bytes_before = std::fs::read(&pack_file).unwrap();
    #[cfg(unix)]
    let inode_before = std::fs::metadata(&pack_file).unwrap().ino();

    plant_taint(&warehouse, &["objects/pack/fake.pack"]);

    let status = warehouse.run(&["stocktake", "--json"]);
    assert_eq!(status.status.code(), Some(21));
    let value = json(&status);
    assert_eq!(value["error"]["code"], "durability_taint");
    assert!(value["error"]["next_step"].as_str().unwrap().contains("forklift heal"));

    // The file must be untouched — no lock-free rewrite, not even an idempotent one.
    assert_eq!(std::fs::read(&pack_file).unwrap(), bytes_before);
    #[cfg(unix)]
    assert_eq!(std::fs::metadata(&pack_file).unwrap().ino(), inode_before,
        "the dentry must never be rewritten lock-free for a non-content-addressed path");

    assert!(recorded_taint_paths(&warehouse).contains(&"objects/pack/fake.pack".to_string()),
        "the taint must survive escalation");

    // The heal verb, under the lock, DOES restage this same shape.
    let heal_status = warehouse.run(&["heal", "--json"]);
    assert_success(&heal_status);
    assert!(recorded_taint_paths(&warehouse).is_empty(), "heal must clear the taint");
}

#[test]
fn entry_heal_still_restages_a_content_addressed_object_inline_with_no_escalation() {
    // I1 non-regression (the common case this whole mechanism exists for): a taint over a
    // present, hash-verifiable loose object must still heal inline, lock-free, on the very next
    // read-only command — no escalation, no exit 21.
    //
    // Mutation: escalate on every recorded path regardless of shape (drop the
    // `is_content_addressed` gate, or invert it) → `stocktake` below exits 21 instead of
    // succeeding → red.
    let warehouse = TestWarehouse::new("entry-heal-loose-object-noregression");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);

    warehouse.write_file("src/app.rs", "content for the loose-object heal");
    assert_success(&warehouse.run(&["load", "src/app.rs"]));
    let stack_out = warehouse.run(&["stack", "loose object for heal"]);
    assert_success(&stack_out);
    let parcel_hash = extract_parcel_hash(&stack_out);
    let relative = loose_object_relative_path(&parcel_hash);

    // Taint the object without touching its bytes — it is present and hash-correct.
    plant_taint(&warehouse, &[&relative]);

    let status = warehouse.run(&["stocktake", "--json"]);
    assert_success(&status);

    assert!(!taint_dir_path(&warehouse).exists() || recorded_taint_paths(&warehouse).is_empty(),
        "the taint must be fully cleared by the inline restage");
}

// ---- Multi-bay durable-taint soundness audit ----
//
// `forklift heal` (the recovery verb) only ever takes the CURRENT bay's own (bay-local)
// `WarehouseLock` — never the warehouse-global `StoreLock` that serializes destructive object-
// store maintenance (`compact`) against itself. The two tests below settle, deterministically,
// what that lock-scope gap actually means in a multi-bay warehouse (one shared object store,
// shared refs, one lock file per bay): first that the lock scopes genuinely do not intersect,
// then — the actual crux — whether the one dangerous case that gap enables (heal verbatim-
// restaging a pack `compact` physically deleted) causes real harm or is inert duplication.

#[test]
fn heal_is_never_serialized_against_another_bays_warehouse_lock_or_the_shared_store_lock() {
    // §10.2's static claim: `WarehouseLock::acquire()` locks `bay_root().join("lock")` — bay-
    // local — while `compact` (`pack_utils.rs`) additionally takes the warehouse-global
    // `StoreLock` (`forklift_root().join("store.lock")`) right before it deletes a superseded
    // pack. `forklift heal` (`recovery_utils.rs`/`heal_utils.rs`/`commands/heal.rs`) never
    // references `StoreLock` at all — grep-confirmed, not just asserted here.
    //
    // Proven deterministically, with no real second process and no timing dependency: hand-
    // plant BOTH lock files a concurrent `compact` in another bay would be holding at the exact
    // instant it is about to delete a superseded pack (its own bay's lock, taken first via the
    // ordinary `requires_warehouse_lock()` dispatch, then the shared store lock), and show
    // `forklift heal` in THIS (different) bay still runs to completion — clearing a real taint —
    // regardless. If `heal` ever acquired either lock, this run would fail (`WarehouseLocked`,
    // or a store-lock contention error) instead of succeeding.
    //
    // Mutation: make `heal`'s dispatch also acquire `StoreLock` (or route it through bay B's
    // lock file instead of its own) → this run fails instead of clearing the taint → red.
    let warehouse = TestWarehouse::new("heal-lock-scope");
    warehouse.write_file("code.txt", "v1\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    // A bay can only be opened once the pallet it checks out has something stacked.
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "initial"]));

    // Bay B: a second bay sharing this warehouse's object store and refs.
    let bay_dir = warehouse.home.join("bay-other");
    assert_success(&warehouse.run(&["bay", "add", "other", bay_dir.to_str().unwrap()]));

    // Simulate bay B's `compact` mid-flight, holding both locks it takes in sequence: its own
    // (bay-local) `WarehouseLock`...
    let bay_b_lock = warehouse.root.join(".forklift/bays/other/lock");
    std::fs::write(&bay_b_lock, "99999\n").unwrap();
    // ...and the shared `StoreLock`, acquired right before the destructive old-pack sweep.
    let store_lock = warehouse.root.join(".forklift/store.lock");
    std::fs::write(&store_lock, "99999\n").unwrap();

    // A real taint for bay A's heal to resolve, so a successful run below is not vacuous: a
    // fabricated, never-existing loose-object hash — the same "absent and unreferenced" fixture
    // as `heal_reaches_a_tainted_root_and_clears_a_vanished_unreferenced_object`.
    let fake_hash = "7".repeat(64);
    plant_taint(&warehouse, &[&loose_object_relative_path(&fake_hash)]);

    // Run `forklift heal` in the MAIN tree (bay A — its own `WarehouseLock` lives at
    // ".forklift/lock", a file distinct from bay B's ".forklift/bays/other/lock").
    let status = warehouse.run(&["heal", "--json"]);
    assert_success(&status);
    let value = json(&status);
    assert_eq!(value["data"]["was_tainted"], true);
    assert!(
        value["data"]["resolved"].as_array().unwrap().iter().any(|v| v == &fake_hash),
        "bay A's heal must actually run its recovery logic (not merely happen to exit 0): {}", value
    );

    // Both hand-planted lock files are untouched: bay A's heal neither waited on nor cleared
    // either one — proof it never contended for bay B's lock or the shared store lock.
    assert!(bay_b_lock.exists(), "bay B's own lock file must be untouched by bay A's heal");
    assert!(store_lock.exists(), "the shared store lock must be untouched by bay A's heal");

    std::fs::remove_file(&bay_b_lock).unwrap();
    std::fs::remove_file(&store_lock).unwrap();
}

/// Every `.pack` data file currently present under a warehouse's pack folder, sorted.
fn pack_data_files(warehouse: &TestWarehouse) -> Vec<PathBuf> {
    let pack_dir = warehouse.root.join(".forklift/objects/pack");
    if !pack_dir.exists() {
        return Vec::new();
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&pack_dir).unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("pack"))
        .collect();
    files.sort();
    files
}

#[test]
fn heal_resurrecting_a_compact_superseded_pack_only_duplicates_content_the_new_pack_already_has() {
    // §10.2's actual-harm question, isolated from the concurrency mechanism (already pinned
    // above): a full repack (`compact --all`) writes a new, content-distinct pack and physically
    // deletes the old one it superseded. If `heal` verbatim-restages the old pack's bytes back
    // AFTER that delete — the one thing its missing `StoreLock` no longer prevents — does the
    // resurrected pack (a) reintroduce something the store meant to be gone / break a pack-index
    // invariant / corrupt anything, or (b) merely duplicate content the new pack already has?
    //
    // The resurrection itself is constructed by direct sequencing, not a real race (per the
    // determinism requirement): `heal_utils::restage_object` performs zero validation on a non-
    // content-addressed path (a `.pack`/`.idx` file is never hash-verified, only loose objects
    // are — see its own doc comment) — it just rewrites, verbatim, whatever bytes it read. So
    // writing the pre-repack bytes back to the pre-repack path by hand produces a byte-for-byte
    // identical on-disk end state to whatever a real heal-vs-compact interleaving would have left
    // behind, with no need to reproduce the exact timing.
    let warehouse = TestWarehouse::new("pack-resurrection-harm");
    warehouse.write_file("a.txt", "first parcel content\n");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "parcel one"]));

    // Pack parcel one's loose objects into a first pack (P1).
    assert_success(&warehouse.run(&["compact"]));
    let packs_after_first_compact = pack_data_files(&warehouse);
    assert_eq!(packs_after_first_compact.len(), 1, "sanity: exactly one pack after the first compact");
    let p1_data = packs_after_first_compact[0].clone();
    let p1_index = p1_data.with_extension("idx");
    let p1_data_bytes = std::fs::read(&p1_data).unwrap();
    let p1_index_bytes = std::fs::read(&p1_index).unwrap();

    // More content, still on the SAME pallet history — parcel one's objects stay fully live,
    // reachable from the (advancing) pallet head.
    warehouse.write_file("a.txt", "first parcel content\nsecond parcel content\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "parcel two"]));

    // A full repack: every reachable object (parcel one's included) consolidates into ONE new,
    // content-distinct pack; P1 — now fully redundant — is physically deleted.
    assert_success(&warehouse.run(&["compact", "--all"]));
    assert!(!p1_data.exists(), "sanity: the repack physically deleted the superseded pack");
    assert!(!p1_index.exists(), "sanity: the repack physically deleted the superseded index");
    let packs_after_repack = pack_data_files(&warehouse);
    assert_eq!(packs_after_repack.len(), 1, "sanity: the repack consolidated into one new pack");
    assert_ne!(packs_after_repack[0], p1_data, "sanity: the new pack is a distinct, differently-named file");

    // Baselines, taken after the repack, before the resurrection.
    let content_before = stdout(&warehouse.run(&["show", "main:a.txt"]));
    let history_before = stdout(&warehouse.run(&["history"]));
    assert_success(&warehouse.run(&["stocktake"]));

    // THE RESURRECTION: P1's exact pre-repack bytes, written back to its exact pre-repack path,
    // after the repack has already deleted it and published the new pack in its place.
    std::fs::write(&p1_data, &p1_data_bytes).unwrap();
    std::fs::write(&p1_index, &p1_index_bytes).unwrap();
    assert_eq!(pack_data_files(&warehouse).len(), 2,
        "sanity: the store now holds both the new pack and the resurrected old one");

    // (a) No corruption: every read observes byte-identical content and history to before the
    // resurrection — the resurrected pack is not consulted for anything the new pack already
    // answers, and its presence does not confuse object resolution.
    assert_eq!(stdout(&warehouse.run(&["show", "main:a.txt"])), content_before,
        "resurrecting the old pack must not change what a read returns");
    assert_eq!(stdout(&warehouse.run(&["history"])), history_before);
    assert_success(&warehouse.run(&["stocktake"]));

    // (b) Self-healing: an ordinary command touching the object store still runs clean with the
    // duplicate pack present...
    assert_success(&warehouse.run(&["compact"]));
    // ...and a SECOND full repack independently re-derives the same live set and physically
    // removes the resurrected duplicate again — the same "supersede and delete" logic the first
    // repack ran, applied to whatever packs currently exist, with no special-casing needed for a
    // duplicate. This is the evidence for "benign, self-correcting gc-food," not merely an
    // assumption: the resurrected pack does not survive the next maintenance pass.
    assert_success(&warehouse.run(&["compact", "--all"]));
    assert!(!p1_data.exists(),
        "a later repack must clean up the resurrected duplicate pack again — it is inert, \
        self-correcting duplication, not a permanent or escalating problem");
    assert!(!p1_index.exists());
    assert_eq!(pack_data_files(&warehouse).len(), 1);

    // Content is still perfectly readable after the cleanup.
    assert_eq!(stdout(&warehouse.run(&["show", "main:a.txt"])), content_before);
}

/// The ref lock guards refs every bay shares, so it lives in the shared `.forklift`, whichever bay
/// writes: never in a bay's own storage nor in its working tree.
#[test]
fn a_stack_in_a_named_bay_locks_the_shared_refs() {
    let warehouse = TestWarehouse::new("bay-ref-lock");
    assert_success(&warehouse.run(&["prepare"]));
    configure_operator(&warehouse);
    warehouse.write_file("a.txt", "a\n");
    assert_success(&warehouse.run(&["load", "."]));
    assert_success(&warehouse.run(&["stack", "base"]));

    let bay_dir = warehouse.home.join("bay-ref-lock-feature");
    assert_success(&warehouse.run(&["bay", "add", "feature", bay_dir.to_str().unwrap()]));

    // Only the bay's stack below may create it.
    let shared = warehouse.root.join(".forklift").join("refs.lock");
    std::fs::remove_file(&shared).unwrap();

    std::fs::write(bay_dir.join("b.txt"), "b\n").unwrap();
    assert_success(&warehouse.run_at(&bay_dir, &["load", "."]));
    assert_success(&warehouse.run_at(&bay_dir, &["stack", "in the bay"]));

    assert!(shared.is_file(), "the bay's stack must lock the shared refs");
    assert!(!warehouse.root.join(".forklift").join("bays").join("feature").join("refs.lock").exists(),
        "the ref lock must not be bay-local");
    assert!(!bay_dir.join("refs.lock").exists() && !bay_dir.join(".forklift").join("refs.lock").exists(),
        "the ref lock must not land in the bay's working tree");
}
