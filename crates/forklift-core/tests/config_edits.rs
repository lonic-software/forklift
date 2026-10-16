//! Configuration edits are atomic and serialized (FORK-119).
//!
//! Every scenario runs inside **one** test function, in order: the global configuration is found
//! through `FORKLIFT_GLOBAL_CONFIG`, a process-wide environment variable, and `cargo test` runs a
//! binary's tests as threads of one process — two tests here would point each other's writes at
//! the wrong file.
//!
//! The locked scenarios share one construction. The test takes the configuration file's edit lock
//! itself (the `<config>.lock` sidecar, computed here independently of the code under test),
//! starts the operation on a thread, waits and asserts the thread is still blocked, edits the file
//! directly, then releases. The wait matters: an implementation that reads before it locks has
//! already read by the time the test writes, so it acts on a stale file and the assertion catches
//! it.

use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::config_utils;
use forklift_core::util::file_utils::BulkStoreSession;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long a blocked operation is given to (wrongly) finish while the test holds the lock.
const SETTLE: Duration = Duration::from_millis(500);

#[test]
fn configuration_edits_are_atomic_and_serialized() {
    let root = std::env::temp_dir().join(format!("forklift-config-edits-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    an_edit_waits_for_the_lock_and_loads_only_after_taking_it(&root);
    a_first_use_mint_returns_the_identifier_the_file_holds(&root);
    a_profile_name_is_claimed_under_the_lock(&root);
    a_reader_holding_the_old_file_still_reads_it_whole(&root);
    a_config_write_is_never_deferred_into_a_bulk_store_session(&root);
    a_symlinked_config_is_written_through_and_shares_its_targets_lock(&root);
    a_dangling_symlinked_config_is_refused_not_replaced(&root);
    a_missing_config_folder_is_created_before_locking(&root);

    std::fs::remove_dir_all(&root).ok();
}

/// Point the global configuration at a fresh file in its own folder, optionally seeded.
fn fresh_global(root: &Path, name: &str, seed: Option<&str>) -> PathBuf {
    let folder = root.join(name);
    std::fs::create_dir_all(&folder).unwrap();
    let path = folder.join("global.toml");

    if let Some(seed) = seed {
        std::fs::write(&path, seed).unwrap();
    }

    std::env::set_var("FORKLIFT_GLOBAL_CONFIG", &path);
    path
}

/// Hold `config`'s edit lock while `operation` runs on a thread: assert it is still blocked after
/// [`SETTLE`], run `while_held` (which edits the file directly), release, and return the result.
fn while_locked<T: Send + 'static>(config: &Path,
                                   operation: impl FnOnce() -> T + Send + 'static,
                                   while_held: impl FnOnce()) -> T {
    let mut lock_name = config.file_name().unwrap().to_os_string();
    lock_name.push(".lock");

    let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(config.with_file_name(lock_name))
        .unwrap();
    lock.lock().unwrap();

    let thread = std::thread::spawn(operation);
    std::thread::sleep(SETTLE);
    assert!(!thread.is_finished(), "the edit must wait while another holder has the lock");

    while_held();
    drop(lock);

    thread.join().unwrap()
}

fn an_edit_waits_for_the_lock_and_loads_only_after_taking_it(root: &Path) {
    let config = fresh_global(root, "t1", Some("[operator]\nname = \"Ada\"\n"));

    let stored = while_locked(
        &config,
        || config_utils::set_value("remote.url", "https://example.com/w", ConfigScope::Global),
        || std::fs::write(&config, "[operator]\nname = \"Ada\"\nidentifier = \"written-meanwhile\"\n").unwrap(),
    );
    stored.unwrap();

    let content = std::fs::read_to_string(&config).unwrap();
    assert!(content.contains("written-meanwhile"), "an edit made while it waited was lost: {content}");
    assert!(content.contains("https://example.com/w"), "the waiting edit itself was lost: {content}");
}

fn a_first_use_mint_returns_the_identifier_the_file_holds(root: &Path) {
    let config = fresh_global(root, "t2", None);

    let operator = while_locked(
        &config,
        config_utils::get_operator,
        || std::fs::write(&config, "[operator]\nidentifier = \"minted-elsewhere\"\n").unwrap(),
    ).unwrap();

    assert_eq!(operator.identifier, "minted-elsewhere", "the caller must take the identifier already on disk");
    assert_eq!(
        config_utils::get_scoped_value("operator.identifier", ConfigScope::Global).unwrap().as_deref(),
        Some("minted-elsewhere"),
        "the identifier on disk must not be replaced by a second mint",
    );
}

fn a_profile_name_is_claimed_under_the_lock(root: &Path) {
    let config = fresh_global(root, "t5", None);

    let created = while_locked(
        &config,
        || config_utils::create_profile("work", None, None),
        || std::fs::write(&config, "[profile.work]\nidentifier = \"created-elsewhere\"\n").unwrap(),
    );

    let Err(refusal) = created else {
        panic!("a profile created while the call waited must make it refuse");
    };
    assert!(refusal.contains("already exists"), "unexpected refusal: {refusal}");
    assert_eq!(
        config_utils::get_profile("work").unwrap().unwrap().identifier,
        "created-elsewhere",
        "the existing profile must not be overwritten",
    );
}

/// A write replaces the file instead of truncating and rewriting it: a handle opened before the
/// write keeps reading the old file, whole. Unix only, because there an open handle survives a
/// rename over its name.
#[cfg(unix)]
fn a_reader_holding_the_old_file_still_reads_it_whole(root: &Path) {
    use std::io::Read;

    let old = "[operator]\nname = \"Old\"\n";
    let config = fresh_global(root, "t3", Some(old));
    let mut reader = std::fs::File::open(&config).unwrap();

    config_utils::set_value("operator.name", "New", ConfigScope::Global).unwrap();

    let mut content = String::new();
    reader.read_to_string(&mut content).unwrap();
    assert_eq!(content, old, "a reader that opened the file before the write must see it unchanged");
}

#[cfg(not(unix))]
fn a_reader_holding_the_old_file_still_reads_it_whole(_root: &Path) {}

fn a_config_write_is_never_deferred_into_a_bulk_store_session(root: &Path) {
    fresh_global(root, "t4", None);

    let session = BulkStoreSession::open().unwrap();
    config_utils::set_value("operator.name", "During", ConfigScope::Global).unwrap();
    let read = config_utils::get_scoped_value("operator.name", ConfigScope::Global);
    drop(session);

    assert_eq!(read.unwrap().as_deref(), Some("During"), "a config write must be visible at once, not at the session's finish");
}

/// The link and its target are one file under one lock: the lock is held on the *target's*
/// sidecar while the write goes through the *link*.
#[cfg(unix)]
fn a_symlinked_config_is_written_through_and_shares_its_targets_lock(root: &Path) {
    let folder = root.join("t6");
    std::fs::create_dir_all(&folder).unwrap();
    let target = folder.join("real.toml");
    let link = folder.join("link.toml");
    std::fs::write(&target, "[operator]\nname = \"Ada\"\n").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    std::env::set_var("FORKLIFT_GLOBAL_CONFIG", &link);

    while_locked(
        &target,
        || config_utils::set_value("operator.identifier", "through-the-link", ConfigScope::Global),
        || (),
    ).unwrap();

    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link was replaced by a plain file");
    assert!(
        std::fs::read_to_string(&target).unwrap().contains("through-the-link"),
        "the write must land in the link's target",
    );
}

#[cfg(not(unix))]
fn a_symlinked_config_is_written_through_and_shares_its_targets_lock(_root: &Path) {}

#[cfg(unix)]
fn a_dangling_symlinked_config_is_refused_not_replaced(root: &Path) {
    let folder = root.join("t7");
    std::fs::create_dir_all(&folder).unwrap();
    let link = folder.join("link.toml");
    std::os::unix::fs::symlink(folder.join("gone.toml"), &link).unwrap();
    std::env::set_var("FORKLIFT_GLOBAL_CONFIG", &link);

    let refusal = config_utils::set_value("operator.name", "Ada", ConfigScope::Global)
        .expect_err("a write through a dangling link must refuse");
    assert!(refusal.contains("symbolic link"), "unexpected refusal: {refusal}");
    assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the dangling link was replaced");
    assert!(!folder.join("gone.toml").exists(), "a refused write must not create the link's target");
}

#[cfg(not(unix))]
fn a_dangling_symlinked_config_is_refused_not_replaced(_root: &Path) {}

/// The lock lives beside the file, so the folder has to exist before the lock is taken — and the
/// first write to a not-yet-existing folder has always worked.
fn a_missing_config_folder_is_created_before_locking(root: &Path) {
    let config = root.join("t8").join("not").join("yet").join("global.toml");
    std::env::set_var("FORKLIFT_GLOBAL_CONFIG", &config);

    let operator = config_utils::get_operator().unwrap();

    assert_eq!(
        config_utils::get_scoped_value("operator.identifier", ConfigScope::Global).unwrap(),
        Some(operator.identifier),
    );
}
