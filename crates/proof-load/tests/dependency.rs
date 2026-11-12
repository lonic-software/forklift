//! Proof Load never links the code it tests: an oracle that parsed a tree with forklift-core would
//! agree with a corrupt tree core misreads.
//!
//! Revert: add `forklift-core = { workspace = true }` to proof-load's dependencies → this fails
//! naming it.

use std::collections::BTreeSet;
use std::process::Command;

#[test]
fn proof_load_depends_on_no_forklift_crate() {
    let manifest = concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml");
    let output = Command::new(env!("CARGO")).args(["metadata", "--format-version", "1", "--manifest-path", manifest])
        .output().expect("cargo metadata runs");
    assert!(output.status.success(), "cargo metadata: {}", String::from_utf8_lossy(&output.stderr));

    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages = metadata["packages"].as_array().unwrap();
    let id_of = |name: &str| packages.iter().find(|package| package["name"] == name).map(|package| package["id"].as_str().unwrap().to_string());
    let root = id_of("proof-load").expect("proof-load is in the workspace");

    let nodes = metadata["resolve"]["nodes"].as_array().unwrap();
    let mut seen = BTreeSet::new();
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let node = nodes.iter().find(|node| node["id"] == id.as_str()).expect("every reached id has a node");
        for dependency in node["deps"].as_array().unwrap() {
            stack.push(dependency["pkg"].as_str().unwrap().to_string());
        }
    }

    // The forbidden crates must exist in the workspace, or this check would pass by naming nothing.
    for forbidden in ["forklift-core", "forklift", "forklift-server"] {
        let id = id_of(forbidden).unwrap_or_else(|| panic!("{} is in the workspace", forbidden));
        assert!(!seen.contains(&id), "proof-load's dependency graph reaches {}", forbidden);
    }
    assert!(seen.len() > 1, "the walk reached proof-load's dependencies");
}
