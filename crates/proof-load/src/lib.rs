//! Proof Load: a fault-injection harness that drives forklift, forklift-server and git only as
//! subprocesses, found by path, and grades what they read back against an oracle built from the
//! seed. It never links Forklift: an oracle that shares the code under test inherits its bugs.
//!
//! Built in slices. So far: the census reader, the copy-only rule and environment scrub, the oracle
//! and grades, damaged-file faults with their completeness check, client kill points (Linux), the
//! write-op completeness check over the version's own schema, client power loss over a
//! persistence model of the traced calls (Linux), dead-holder locks, disk full on a capped volume
//! and a read-only warehouse, a work tree on a case-folding or no-exec volume, and the report.

pub mod bindings;
pub mod census;
pub mod conditions;
pub mod corpus;
pub mod damage;
pub mod faulted;
pub mod host;
pub mod kill;
pub mod network;
pub mod oracle;
pub mod power;
pub mod proxy;
pub mod remedy;
pub mod report;
pub mod runner;
pub mod scenario;
pub mod scrub;
pub mod tool;
pub mod trace;
pub mod volume;
pub mod writeops;
