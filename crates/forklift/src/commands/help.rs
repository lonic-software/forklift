use clap::CommandFactory;
use crate::cli::Cli;
use crate::color;
use crate::output::{self, ErrorCode, ForkliftError};
use crate::prefix::{self, Resolved};

/// Handle the help command: print the command list, or the detailed help of one command
/// (following subcommands, so "help office admit" works too). This is the same help clap
/// renders for `--help`; the subcommand only exists to keep `forklift help <command>`
/// and the `h` alias working. Under `--json` the help is a success document, exactly as
/// `--help` is, and an unknown name is the same `usage` failure the parser reports for one.
///
/// # Arguments
/// * `path` - The command (and subcommand) names to explain; empty for the command list.
///
/// # Returns
/// * `Ok(())`      - If the help was printed.
/// * `Err(ForkliftError)` - A `usage` failure, if no command with the given name exists.
pub fn handle_command(path: &[String]) -> Result<(), ForkliftError> {
    // Each subcommand is cloned out of an unbuilt parent, which has not yet propagated its
    // settings down, so every level takes `--color` itself.
    let choice = color::chosen().for_clap();
    let mut command = Cli::command().color(choice);
    let mut bin_name = command.get_name().to_string();

    let mut reached: Vec<String> = Vec::new();
    for name in path {
        // A name is matched as on the command line: exactly, or by a unique prefix.
        command = match prefix::subcommand(&command, name) {
            Resolved::Command(subcommand) => subcommand.clone(),
            Resolved::Ambiguous(names) => return Err(ForkliftError::new(
                ErrorCode::Usage,
                prefix::ambiguity(name, &reached.iter().map(String::as_str).collect::<Vec<_>>(), &names),
                "Run \"forklift help\" for a list of available commands.",
            )),
            Resolved::Unknown => return Err(ForkliftError::new(
                ErrorCode::Usage,
                format!(
                    "Unknown command: {}. Use \"forklift help\" for a list of available commands.",
                    path.join(" ")
                ),
                "Run \"forklift help\" for a list of available commands.",
            )),
        };

        // A standalone subcommand does not know its parent chain; rebuilding the binary
        // name keeps the usage line correct (e.g. "forklift office admit ...").
        bin_name = format!("{} {}", bin_name, command.get_name());
        reached.push(command.get_name().to_string());
        command = command.bin_name(bin_name.clone()).color(choice);
    }

    if output::is_json() {
        output::message(command.render_long_help().to_string().trim_end());
        return Ok(());
    }

    command.print_long_help()
        .map_err(|e| format!("Error while printing the help: {}", e).into())
}
