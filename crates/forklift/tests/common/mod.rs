//! Harness shared by the integration test files that launch the `forklift` binary.

use std::process::Command;

/// `forklift` with fsync off. Durability is not what these tests check, and paying an fsync per
/// write made the suite much slower. `crash_consistency.rs` forces it on, and the in-process
/// tests leave it on.
pub fn forklift_command() -> Command {
    test_command(env!("CARGO_BIN_EXE_forklift"))
}

/// `program` in the environment every launch here shares, for a test that reaches `forklift`
/// another way (through an alias) and must not differ from [`forklift_command`] in anything else.
pub fn test_command(program: impl AsRef<std::ffi::OsStr>) -> Command {
    let mut command = Command::new(program);
    command.env("FORKLIFT_FSYNC", "0");
    command
}

/// Hold the store-access lock of the warehouse at `root` from this test process, the way another
/// forklift process would: shared like a writer in another bay, or exclusive like a running
/// repack, gc or scope-prune. The kernel lock is held for as long as the returned file is open.
#[allow(dead_code)] // Used by only some of the test files that include this module.
pub fn hold_store_access(root: &std::path::Path, exclusive: bool) -> std::fs::File {
    let path = root.join(".forklift").join("store-access.lock");
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
        .open(&path)
        .unwrap_or_else(|e| panic!("open {}: {}", path.display(), e));

    if exclusive {
        file.try_lock().unwrap_or_else(|e| panic!("lock {} exclusively: {}", path.display(), e));
    } else {
        file.try_lock_shared().unwrap_or_else(|e| panic!("lock {} shared: {}", path.display(), e));
    }

    file
}
