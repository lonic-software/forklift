//! The CLI's integration tests, one module per former test file, built as a single test binary.
//!
//! Every file under `tests/` is a binary of its own that CI compiles and links on every run, so
//! the tests share one binary unless they need a process to themselves. Three files stay outside:
//! `case_alias_backstop.rs` changes the process's working directory, and `cli.rs` and `remote.rs`
//! are each about as large as everything here together, so they build in parallel with this
//! binary instead of lengthening it. A new test file goes here as a module unless it changes
//! process-wide state (the working directory, environment variables, resource limits) that
//! `cargo test`, which runs a binary's tests on threads of one process, would let leak into the
//! others.

#[path = "../common/mod.rs"]
mod common;

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[path = "../channel/mod.rs"]
mod channel;

mod alias_pairs;
mod audit_bounds;
mod boundary_gc;
mod case_aliases;
mod crash_consistency;
mod determinism;
mod doc_links;
mod events;
mod guide_pages;
mod mirror;
mod nested_working_trees;
mod query;
mod recovery_page;
mod rollup_hash;
mod rollup_hash_skip;
mod shard_store;
mod tag_subject_gc;
mod take_writers;
mod tree_names;
mod warehouse_format;
mod warehouse_marker;
