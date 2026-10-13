//! Harness shared by the integration test files that launch the `forklift` binary.

use std::process::Command;

/// `forklift` with fsync off. Durability is not what these tests check, and paying an fsync per
/// write made the suite much slower.
/// `crash_consistency.rs` forces it on, and the in-process tests leave it on.
pub fn forklift_command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_forklift"));
    command.env("FORKLIFT_FSYNC", "0");
    command
}
