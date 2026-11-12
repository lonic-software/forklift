//! Forklift's base logic: objects, inventory, refs, diff/merge, and the trust stack.
//!
//! This crate is shared by every head (the client CLI, the server, the serverless
//! adapters). Rule (docs/DESIGN.html §3.4): core logic never prints, never exits, and
//! never assumes a terminal — it returns data and errors; heads own all presentation.

pub mod builder;
pub mod enums;
pub mod error;
pub mod globals;
pub mod model;
pub mod parser;
pub mod traits;
pub mod types;
pub mod util;

/// Core's unit tests count each thread's allocations, so the fuzz tests can bound a decoder's
/// peak memory (see `util::peak_alloc`).
#[cfg(test)]
#[global_allocator]
static PEAK_ALLOC: util::peak_alloc::PeakAlloc = util::peak_alloc::PeakAlloc;
