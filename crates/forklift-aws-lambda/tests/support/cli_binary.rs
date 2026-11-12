//! Where the compiled `forklift` CLI is, for the test crates that drive it.
//!
//! Cargo exposes `CARGO_BIN_EXE_*` only to a package's own tests, so these tests find the CLI in
//! the profile directory (`target/debug`) the workspace build put it in. The test executable is
//! somewhere under that directory, and where is the toolchain's choice: `target/debug/deps/`
//! through Rust 1.99, `target/debug/build/<package>/<hash>/out/` from 1.100. So the walk takes the
//! nearest ancestor of the executable that holds the CLI, rather than a fixed number of levels.

use std::path::{Path, PathBuf};

/// The compiled `forklift` CLI: the first ancestor directory of this test executable that holds
/// it.
pub fn forklift_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    cli_beside(&exe).unwrap_or_else(|| panic!(
        "forklift is not built in any directory above {}; run the suite via a workspace `cargo test`.",
        exe.display()))
}

/// The `forklift` CLI in the nearest ancestor directory of `exe` that holds one as a file.
pub fn cli_beside(exe: &Path) -> Option<PathBuf> {
    let name = format!("forklift{}", std::env::consts::EXE_SUFFIX);
    exe.ancestors().skip(1).map(|dir| dir.join(&name)).find(|binary| binary.is_file())
}
