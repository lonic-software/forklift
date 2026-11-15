//! The binary's one argument: a report folder it cannot read is refused before any window opens.

/// Revert: exit 0 on a refused folder → the refusal reads as success.
#[test]
fn a_report_folder_that_cannot_be_read_is_refused() {
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_proof-load-desktop"))
        .arg("/nonexistent/proof-load-run")
        .output()
        .expect("the binary runs");
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.starts_with("proof-load-desktop: cannot read /nonexistent/proof-load-run"), "{stderr}");
}
