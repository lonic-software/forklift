/// Handle the unload command.
/// Unstage a file or directory: reset its inventory entries to the pallet head, leaving the
/// working directory untouched — the inverse of `load`. The behavior is `restore --staged`
/// under its natural name; only the command label of the output envelope differs. Staging a
/// removal is `remove` — deliberately a different verb, so undoing a `load` can never turn
/// into a staged deletion.
///
/// # Arguments
/// * `subject` - The path of the file or directory to unstage.
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(subject: &str) -> Result<(), String> {
    crate::commands::restore::handle_unstage(subject, "unload")
}
