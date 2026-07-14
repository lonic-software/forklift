//! Generates the reference fragments under `docs/generated/` (errors table, per-command
//! `--json` schemas). Only compiled in behind the `docgen` feature (see `bin/gen-docs`); a
//! release build never enables it, so this module — and the `schemars` dependency it needs —
//! costs the shipped binary nothing.
//!
//! Both renderers are deterministic (fixed iteration order), so `bin/gen-docs` run twice
//! produces byte-identical output and `bin/check` can verify freshness with a plain diff.

use crate::cli::Cli;
use crate::output::ErrorCode;
use clap::CommandFactory;

/// Render `docs/generated/errors.md`: the exhaustive error-code / exit-code reference.
pub fn render_errors() -> String {
    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# Error codes and exit codes\n\n");
    out.push_str(
        "Every `forklift` failure carries a stable `code` (in the `--json` error envelope) \
         and the process exits with the matching deterministic status, so a script or an agent \
         branches without parsing prose. `2` is reserved for clap's own argument/usage errors; \
         `0` is success. Both tables are generated from the single `ErrorCode` enum in \
         `crates/forklift/src/output.rs` — see `docs/guide/cli.md` for how a script is meant to \
         use them.\n\n\
         Exit codes 17 and 18 are reserved for future features and are not yet assigned to any \
         code.\n\n",
    );
    out.push_str("| `code` | exit | Meaning |\n");
    out.push_str("|---|---|---|\n");
    for code in ErrorCode::ALL {
        out.push_str(&format!(
            "| `{}` | {} | {} |\n",
            code.as_str(),
            code.exit_code(),
            code.description(),
        ));
    }
    out.push('\n');
    out
}

/// Commands whose `--json` data is either the generic message shape (`{ "message": string }`)
/// or no structured data (or no `--json` output) at all — deliberately outside the schema
/// registry, with the reason. Every *other* CLI command must be in [`command_schemas`]:
/// [`render_json_schemas`] checks the two lists are exhaustive and disjoint over the real CLI
/// surface, the same shape as `mcp.rs`'s `every_cli_command_is_an_mcp_tool_or_explicitly_human_only`
/// test — except this check runs *inside the generator itself* (`bin/gen-docs`, so `bin/check`),
/// because `cargo test --workspace` never compiles the `docgen` feature and so could never run a
/// `#[cfg(test)]` version of it. A command with a JSON output struct that never got wired into
/// the registry now fails the build instead of silently missing from json-schemas.md — the exact
/// class of drift this whole feature exists to kill (one level up from the stale exit-code table).
const GENERIC_OR_NO_DATA: &[(&str, &str)] = &[
    ("help", "prints clap-rendered help text directly; no --json output at all"),
    ("mcp", "runs the MCP JSON-RPC server on stdio, a different protocol, not a --json envelope"),
    ("restore", "reports only the generic message shape"),
    ("unload", "reports only the generic message shape (delegates to restore)"),
];

/// Render `docs/generated/json-schemas.md`: every CLI command's `--json` `data` payload
/// schema(s), derived from the `#[derive(schemars::JsonSchema)]` output structs in
/// `crates/forklift/src/commands/`.
///
/// # Returns
/// * `Ok(String)`  - The rendered markdown.
/// * `Err(String)` - A CLI command is neither in the schema registry nor on
///                   [`GENERIC_OR_NO_DATA`] (or a stale allow-list entry names no real command) —
///                   `bin/gen-docs` propagates this as a failing exit code.
pub fn render_json_schemas() -> Result<String, String> {
    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# `--json` output schemas\n\n");
    out.push_str(
        "Every command's `--json` result is `{ \"forklift_json\", \"command\", \"ok\": true, \
         \"data\": … }` on success (see `docs/MACHINE_INTERFACE.md` for the envelope and the \
         failure shape). This page is the exhaustive reference for each command's `data` — one \
         [JSON Schema](https://json-schema.org/) per shape a command can emit; a command with \
         more than one (e.g. one per subcommand) lists all of them. Descriptions come straight \
         from the Rust doc comments on the underlying struct, so they stay in sync with the \
         field they describe.\n\n\
         A command not listed here either reports only the generic human-message shape \
         `{ \"message\": string }`, or produces no `--json` data at all — see the command's \
         entry in `docs/guide/cli.md`.\n\n",
    );

    let cli = Cli::command();
    let mut cli_names = std::collections::HashSet::new();

    for command in cli.get_subcommands() {
        let name = command.get_name();
        if name.starts_with("__") {
            continue; // hidden dev-only diagnostics (this generator itself), not a CLI command
        }
        cli_names.insert(name);

        let generic_reason = GENERIC_OR_NO_DATA.iter().find(|(n, _)| *n == name).map(|(_, r)| *r);
        let schemas = command_schemas(name);

        match (schemas, generic_reason) {
            (Some(_), Some(_)) => return Err(format!(
                "docgen: CLI command `{name}` is both in the schema registry (command_schemas) \
                 and on GENERIC_OR_NO_DATA — remove it from one of them."
            )),
            (None, None) => return Err(format!(
                "docgen: CLI command `{name}` has no --json schema registered \
                 (crates/forklift/src/docgen.rs's command_schemas) and is not on \
                 GENERIC_OR_NO_DATA. Add a `__docgen_schemas` fn to its command module and wire \
                 it into command_schemas, or, if it genuinely has no typed --json data, add \
                 `{name}` to GENERIC_OR_NO_DATA with a reason."
            )),
            (None, Some(_)) => {} // generic/no-data, nothing to render
            (Some(schemas), None) => {
                // An empty registration is not "nothing to render" — it is a command that
                // claims a typed schema (it's in command_schemas, not GENERIC_OR_NO_DATA) but
                // supplied none, which would otherwise vanish from json-schemas.md silently.
                if schemas.is_empty() {
                    return Err(format!(
                        "docgen: CLI command `{name}` is registered in command_schemas but its \
                         __docgen_schemas() returned no schemas. A command must yield at least \
                         one schema, or be on GENERIC_OR_NO_DATA instead — never an empty \
                         registration."
                    ));
                }

                out.push_str(&format!("## `{}`\n\n", name));
                for (struct_name, schema) in schemas {
                    out.push_str(&format!("### `{}`\n\n", struct_name));
                    let pretty = serde_json::to_string_pretty(schema.as_value()).map_err(|e| {
                        format!(
                            "docgen: failed to render the `{name}`/`{struct_name}` JSON schema \
                             to pretty-printed text: {e}"
                        )
                    })?;
                    out.push_str("```json\n");
                    out.push_str(&pretty);
                    out.push_str("\n```\n\n");
                }
            }
        }
    }

    // No stale allow-list entries: each names a real (non-hidden) CLI command.
    for (name, _) in GENERIC_OR_NO_DATA {
        if !cli_names.contains(name) {
            return Err(format!(
                "docgen: GENERIC_OR_NO_DATA lists `{name}`, which is not a CLI command."
            ));
        }
    }

    Ok(out)
}

const GENERATED_BANNER: &str = "<!--\n\
    GENERATED FILE — do not edit by hand.\n\
    Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). `bin/check` regenerates and\n\
    diffs this file to catch drift; run `bin/gen-docs` and commit the result after a change\n\
    that affects it (a new error code, or a `--json` output struct).\n\
-->\n\n";

/// Look up the schema-registry function for a CLI command name. `None` means the command has
/// no entry here — [`render_json_schemas`] only accepts that when the name is also on
/// [`GENERIC_OR_NO_DATA`]; otherwise it is undocumented drift and the generator fails.
fn command_schemas(name: &str) -> Option<Vec<(&'static str, schemars::Schema)>> {
    use crate::commands;

    Some(match name {
        "alias" => commands::alias::__docgen_schemas(),
        "audit" => commands::audit::__docgen_schemas(),
        "bay" => commands::bay::__docgen_schemas(),
        "blame" => commands::blame::__docgen_schemas(),
        "cherry-pick" => commands::cherry_pick::__docgen_schemas(),
        "compact" => commands::compact::__docgen_schemas(),
        "config" => commands::config::__docgen_schemas(),
        "conflicts" => commands::conflicts::__docgen_schemas(),
        "consolidate" => commands::consolidate::__docgen_schemas(),
        "deliver" => commands::deliver::__docgen_schemas(),
        "diff" => commands::diff::__docgen_schemas(),
        "expand" => commands::expand::__docgen_schemas(),
        "export-git" => commands::export_git::__docgen_schemas(),
        "franchise" => commands::franchise::__docgen_schemas(),
        "haul" => commands::haul::__docgen_schemas(),
        "history" => commands::history::__docgen_schemas(),
        "query" => commands::query::__docgen_schemas(),
        "import-git" => commands::import_git::__docgen_schemas(),
        "lift" => commands::lift::__docgen_schemas(),
        "load" => commands::load::__docgen_schemas(),
        "lower" => commands::lower::__docgen_schemas(),
        "manifest" => commands::manifest::__docgen_schemas(),
        "narrow" => commands::narrow::__docgen_schemas(),
        "office" => commands::office::__docgen_schemas(),
        "palletize" => commands::palletize::__docgen_schemas(),
        "park" => commands::park::__docgen_schemas(),
        "peek" => commands::peek::__docgen_schemas(),
        "prepare" => commands::prepare::__docgen_schemas(),
        "profile" => commands::profile::__docgen_schemas(),
        "remove" => commands::remove::__docgen_schemas(),
        "scope" => commands::scope::__docgen_schemas(),
        "scope-prune" => commands::scope_prune::__docgen_schemas(),
        "self-update" => commands::self_update::__docgen_schemas(),
        "shift" => commands::shift::__docgen_schemas(),
        "show" => commands::show::__docgen_schemas(),
        "stack" => commands::stack::__docgen_schemas(),
        "stocktake" => commands::stocktake::__docgen_schemas(),
        "store" => commands::store::__docgen_schemas(),
        "tag" => commands::tag::__docgen_schemas(),
        "undo" => commands::undo::__docgen_schemas(),
        "version" => commands::version::__docgen_schemas(),
        _ => return None,
    })
}
