//! Where the compiled `forklift` CLI is, for the test crates that drive it.
//!
//! Cargo exposes `CARGO_BIN_EXE_*` only to a package's own tests, so these tests find the CLI in
//! the profile directory (`target/debug`) the workspace build put it in. Where the test executable
//! sits under that directory is the toolchain's choice: `target/debug/deps/` through Rust 1.99,
//! `target/debug/build/<package>/<hash>/out/` from 1.100. So the profile directory is the parent
//! of the executable's nearest `deps` or `build` ancestor, and the CLI is looked for there only: a
//! `forklift` anywhere further up is not this build's.

use std::path::{Path, PathBuf};

/// The compiled `forklift` CLI in the profile directory this test executable was built under.
pub fn forklift_binary() -> PathBuf {
    let exe = std::env::current_exe().expect("the test binary has a path");
    cli_beside(&exe).unwrap_or_else(|| {
        panic!("forklift is not built beside {}; run the suite via a workspace `cargo test`.", exe.display())
    })
}

/// The `forklift` CLI in the profile directory above `exe`, if one is built there as a file.
pub fn cli_beside(exe: &Path) -> Option<PathBuf> {
    let name = format!("forklift{}", std::env::consts::EXE_SUFFIX);
    let below = exe.ancestors().skip(1)
        .find(|dir| matches!(dir.file_name().and_then(|n| n.to_str()), Some("deps" | "build")))?;
    let binary = below.parent()?.join(name);
    binary.is_file().then_some(binary)
}
