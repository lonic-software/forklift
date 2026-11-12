//! Proof Load: a fault-injection harness that drives forklift, forklift-server and git only as
//! subprocesses, found by path, and grades what they read back against an oracle built from the
//! seed. It never links Forklift: an oracle that shares the code under test inherits its bugs.
//!
//! Built in slices. This one holds the census reader, the copy-only rule and environment scrub,
//! the oracle and grades, damaged-file faults with their completeness check, and the report.

pub mod bindings;
pub mod census;
pub mod corpus;
pub mod damage;
pub mod faulted;
pub mod kill;
pub mod oracle;
pub mod remedy;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod scrub;
pub mod tool;
pub mod trace;
