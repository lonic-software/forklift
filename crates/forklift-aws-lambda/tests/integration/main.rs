//! The lambda head's in-process tests, one module per former test file, built as a single test
//! binary.
//!
//! Every file under `tests/` is a binary of its own that CI compiles and links on every run, so
//! the tests share one binary unless they need a process to themselves. Three files stay outside:
//! `protocol.rs` and `sdk_retry_scope.rs` set environment variables, which `cargo test`, running
//! a binary's tests on threads of one process, would let leak into the others, and
//! `aws_integration.rs` is run on its own, by name, against real AWS. A new test file goes here
//! as a module unless it changes process-wide state the same way.

mod aws_format;
mod entrypoint;
mod iam_conformance;
