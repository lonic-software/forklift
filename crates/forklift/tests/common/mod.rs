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
