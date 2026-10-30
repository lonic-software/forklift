//! Generates the reference fragments under `docs/generated/` (errors table, command and alias
//! list, per-command `--json` schemas). Only compiled in behind the `docgen` feature (see `bin/gen-docs`); a
//! release build never enables it, so this module — and the `schemars` dependency it needs —
//! costs the shipped binary nothing.
//!
//! Every renderer is deterministic (fixed iteration order), so `bin/gen-docs` run twice
//! produces byte-identical output and `bin/gen-docs --check` (run by CI and `bin/check`) can
//! verify freshness with a plain diff.

use crate::cli::Cli;
use crate::internal_refs::check_no_internal_refs;
use crate::output::ErrorCode;
use clap::CommandFactory;

/// Render `docs/generated/errors.md`: the exhaustive error-code / exit-code reference.
///
/// # Returns
/// * `Ok(String)`  - The rendered markdown.
/// * `Err(String)` - A code's description cites a reference only the project's own trackers
///                   can resolve (see [`check_no_internal_refs`], which runs here rather than
///                   in a test for the reason [`GENERIC_OR_NO_DATA`] gives).
pub fn render_errors() -> Result<String, String> {
    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# Error codes and exit codes\n\n");
    out.push_str(
        "Every `forklift` failure carries a stable `code` (in the `--json` error envelope) \
         and the process exits with the matching deterministic status, so a script or an agent \
         branches without parsing prose. `0` (success), `1`, `2` (`usage`), `130` (cancelled \
         by the user) and `128`+n (killed by signal n) mean the same for every Lonic tool; every \
         other number is Forklift's own. Both tables are generated from the single `ErrorCode` enum in \
         `crates/forklift/src/output.rs` — see `docs/guide/cli.md` for how a script is meant to \
         use them.\n\n\
         Exit code 17 is reserved for a future feature and is not yet assigned to any code.\n\n",
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
    check_no_internal_refs("errors.md", &out)?;
    Ok(out)
}

/// Render `docs/generated/commands.md`: every command and subcommand, with its aliases and its
/// one-line description, read from the clap definition in `cli.rs` the binary parses with.
///
/// # Returns
/// * `Ok(String)`  - The rendered markdown.
/// * `Err(String)` - A listed command's one-line description is empty or spans several lines
///                   (either would break its row), or a description cites a reference only the project's own trackers can
///                   resolve (see [`check_no_internal_refs`]).
pub fn render_commands() -> Result<String, String> {
    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# Commands and aliases\n\n");
    out.push_str(
        "Every `forklift` command and subcommand, generated from the same command definition the \
         binary parses its arguments with; CI fails when this page falls behind it. Commands are \
         in alphabetical order, each followed by its subcommands. \
         An **alias** is a short name shown in `forklift help`. A **hidden alias** is accepted \
         but not shown; most are git's name for the same command, so `forklift commit` runs \
         `forklift stack`. `forklift help <command>` explains any row in full, and `docs/guide/cli.md` \
         walks through them by task.\n\n",
    );
    out.push_str("| Command | Alias | Hidden alias | Does |\n");
    out.push_str("|---|---|---|---|\n");
    let cli = Cli::command();
    let mut commands: Vec<&clap::Command> = cli.get_subcommands().collect();
    commands.sort_by_key(|command| command.get_name());
    for command in commands {
        render_command_rows(&mut out, "", command)?;
    }
    out.push('\n');
    check_no_internal_refs("commands.md", &out)?;
    Ok(out)
}

/// Append one table row for `command` (prefixed by its parents' names), then one per visible
/// subcommand, depth first, in declaration order (the order `forklift help <command>` lists them). Hidden commands (the `__docgen` diagnostic
/// among them) are skipped along with everything under them.
fn render_command_rows(out: &mut String, parent: &str, command: &clap::Command) -> Result<(), String> {
    if command.is_hide_set() {
        return Ok(());
    }
    let name = if parent.is_empty() {
        command.get_name().to_string()
    } else {
        format!("{parent} {}", command.get_name())
    };
    let about = command.get_about().map(|about| about.to_string()).unwrap_or_default();
    let about = about.trim();
    if about.contains('\n') {
        return Err(format!(
            "docgen: command `{name}`'s one-line description spans several lines, which would \
             break its row in commands.md. Shorten it to one line in cli.rs."
        ));
    }
    if about.is_empty() {
        return Err(format!(
            "docgen: command `{name}` has no one-line description (its doc comment's first \
             line), so commands.md would show an empty cell. Give it one in cli.rs."
        ));
    }
    let visible: Vec<&str> = command.get_visible_aliases().collect();
    let hidden: Vec<&str> = command.get_all_aliases().filter(|alias| !visible.contains(alias)).collect();
    out.push_str(&format!(
        "| `{}` | {} | {} | {} |\n",
        name,
        code_list(&visible),
        code_list(&hidden),
        about.replace('|', "\\|"),
    ));
    for sub in command.get_subcommands() {
        render_command_rows(out, &name, sub)?;
    }
    Ok(())
}

/// Render `docs/generated/mcp-tools.md`: every tool `forklift mcp` serves, with the command it
/// runs and its arguments, then the commands no agent drives. Both come from the tool list the
/// server itself generates from the command tree.
///
/// # Returns
/// * `Ok(String)`  - The rendered markdown.
/// * `Err(String)` - The tree has no tool shape (the server would refuse `tools/list` too), a
///                   tool's command has no one-line description, or a description cites a
///                   reference only the project's own trackers can resolve (see
///                   [`check_no_internal_refs`]).
pub fn render_mcp_tools() -> Result<String, String> {
    use crate::commands::mcp;

    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# MCP tools\n\n");
    out.push_str(
        "Every tool `forklift mcp` serves, generated from the same command definition the binary \
         parses its arguments with, as the server's own tool list is; CI fails when this page \
         falls behind it. A tool is a command path joined by `_` (`forklift tag create` is \
         `tag_create`, `forklift manifest set` is `manifest_set`), \
         and its description is the command's one-line description followed by its long one; \
         each argument's own help is that argument's description in the tool's input schema. \
         An argument is an option's long name (with `_` for `-`) or a positional's name. \
         `docs/MACHINE_INTERFACE.md` describes the server.\n\n",
    );
    out.push_str("| Tool | Runs | Does | Arguments |\n");
    out.push_str("|---|---|---|---|\n");
    let cli = Cli::command();
    for tool in mcp::tools()? {
        let mut command = &cli;
        for name in &tool.path {
            command = command.find_subcommand(name)
                .ok_or_else(|| format!("docgen: no command `{}`", tool.path.join(" ")))?;
        }
        let about = command.get_about().map(|about| about.to_string()).unwrap_or_default();
        if about.trim().is_empty() || about.contains('\n') {
            return Err(format!(
                "docgen: command `{}` needs a one-line description for its row in mcp-tools.md",
                tool.path.join(" ")
            ));
        }
        let arguments: Vec<String> = tool.params.iter().map(|param| {
            let kind = if param.injected.is_some() { "set by the server".to_string() } else { param.describe() };
            let required = if param.required && param.injected.is_none() { ", required" } else { "" };
            format!("`{}` ({kind}{required})", param.key)
        }).collect();
        out.push_str(&format!(
            "| `{}` | `forklift {}` | {} | {} |\n",
            tool.name,
            tool.path.join(" "),
            about.trim().replace('|', "\\|"),
            arguments.join("<br>").replace('|', "\\|"),
        ));
    }

    out.push_str("\n## Not served\n\n");
    out.push_str(
        "Each of these commands, and every command below it, is a tool for nobody: a person \
         runs it.\n\n",
    );
    for (path, why) in mcp::HUMAN_ONLY {
        out.push_str(&format!("- `forklift {path}`: {why}.\n"));
    }
    out.push_str("\nThese arguments are left out of their command's tool, which is served without them:\n\n");
    for (path, id, why) in mcp::HUMAN_ONLY_ARGS {
        out.push_str(&format!("- `{id}` of `forklift {path}`: it {why}.\n"));
    }
    out.push_str(
        "\nThese families' bare form runs the same operation as one of their subcommands, so only \
         the subcommand is a tool:\n\n",
    );
    for (family, sub) in mcp::BARE_SAME_AS {
        out.push_str(&format!("- `forklift {family}` is `{family}_{sub}`.\n"));
    }
    out.push('\n');
    check_no_internal_refs("mcp-tools.md", &out)?;
    Ok(out)
}

/// `a`, `b` as inline code, or nothing for an empty list.
fn code_list(names: &[&str]) -> String {
    names.iter().map(|name| format!("`{name}`")).collect::<Vec<_>>().join(", ")
}

/// Commands whose `--json` data is either the generic message shape (`{ "message": string }`)
/// or no structured data (or no `--json` output) at all — deliberately outside the schema
/// registry, with the reason. Every *other* CLI command must be in [`command_schemas`]:
/// [`render_json_schemas`] checks the two lists are exhaustive and disjoint over the real CLI
/// surface. It runs *inside the generator itself* (`bin/gen-docs`, so CI and `bin/check`),
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
///                   [`GENERIC_OR_NO_DATA`] (or a stale allow-list entry names no real command),
///                   or [`OutputSchema`] no longer describes the serialized shape —
///                   `bin/gen-docs` propagates this as a failing exit code.
pub fn render_json_schemas() -> Result<String, String> {
    check_output_contract()?;

    let mut out = String::new();
    out.push_str(GENERATED_BANNER);
    out.push_str("# `--json` output schemas\n\n");
    out.push_str(
        "Every command's `--json` result is `{ \"lonic_json\", \"tool\", \"schema\", \"command\", \
         \"ok\": true, \"data\": … }` on success (see `docs/MACHINE_INTERFACE.md` for the envelope and the \
         failure shape). This page is the exhaustive reference for each command's `data` — one \
         [JSON Schema](https://json-schema.org/) per shape a command can emit; a command with \
         more than one (e.g. one per subcommand) lists all of them. Descriptions come straight \
         from the Rust doc comments on the underlying struct, so they stay in sync with the \
         field they describe. Each schema describes what Forklift writes: a field under \
         `required` is always present (possibly as `null`), and any other field is left out \
         when it has nothing to say.\n\n\
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
                for OutputSchema { name: struct_name, schema } in schemas {
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

    check_no_internal_refs("json-schemas.md", &out)?;
    Ok(out)
}

const GENERATED_BANNER: &str = "<!--\n\
    GENERATED FILE — do not edit by hand.\n\
    Produced by `bin/gen-docs` (crates/forklift/src/docgen.rs). CI and `bin/check` run\n\
    `bin/gen-docs --check`, which fails when this file is stale; run `bin/gen-docs` and commit\n\
    the result after a change that affects it (an error code, a command, alias or help line,\n\
    or a `--json` output struct).\n\
-->\n\n";

/// One `--json` `data` shape for json-schemas.md, generated under schemars' *serialize*
/// contract: the schema describes what Forklift writes, not what a deserializer would accept.
/// The two disagree on exactly the fields a reader of our output cares about: a field with
/// `skip_serializing_if` is absent whenever its predicate holds, so it must not be `required`,
/// and an `Option` field without one is always written (as `null`), so it must be. The default
/// (deserialize) contract gets both backwards.
///
/// The fields are private, so [`OutputSchema::of`] is the only way a command module can build
/// one, and a registration cannot fall back to `schemars::schema_for!`.
pub(crate) struct OutputSchema {
    name: String,
    schema: schemars::Schema,
}

impl OutputSchema {
    /// The schema of `T` as serialized, headed by `T`'s schema name.
    pub(crate) fn of<T: schemars::JsonSchema>() -> Self {
        let generator = schemars::generate::SchemaSettings::default().for_serialize().into_generator();
        Self { name: T::schema_name().into_owned(), schema: generator.into_root_schema_for::<T>() }
    }
}

/// Fails the generator unless [`OutputSchema::of`] marks `required` the serialized way. It runs
/// inside the generator for the reason [`GENERIC_OR_NO_DATA`] gives.
fn check_output_contract() -> Result<(), String> {
    #[derive(schemars::JsonSchema, serde::Serialize)]
    #[allow(dead_code)]
    struct Probe {
        #[serde(skip_serializing_if = "Vec::is_empty")]
        skipped_when_empty: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        skipped_with_a_default: Vec<String>,
        always_written: Option<String>,
        #[serde(default)]
        written_with_a_default: Vec<String>,
    }

    let schema = OutputSchema::of::<Probe>().schema;
    let required: std::collections::BTreeSet<&str> = schema
        .get("required")
        .and_then(|value| value.as_array())
        .map(|names| names.iter().filter_map(|name| name.as_str()).collect())
        .unwrap_or_default();
    let expected = std::collections::BTreeSet::from(["always_written", "written_with_a_default"]);
    if required != expected {
        return Err(format!(
            "docgen: output schemas must mark `required` as serialized: a field with \
             `skip_serializing_if` is optional, with or without `#[serde(default)]`, and every \
             other field (an `Option`, or one with only `#[serde(default)]`) is required. The \
             probe expected {expected:?} and got {required:?}."
        ));
    }
    Ok(())
}

/// Look up the schema-registry function for a CLI command name. `None` means the command has
/// no entry here — [`render_json_schemas`] only accepts that when the name is also on
/// [`GENERIC_OR_NO_DATA`]; otherwise it is undocumented drift and the generator fails.
fn command_schemas(name: &str) -> Option<Vec<OutputSchema>> {
    use crate::commands;

    Some(match name {
        "audit" => commands::audit::__docgen_schemas(),
        "bay" => commands::bay::__docgen_schemas(),
        "blame" => commands::blame::__docgen_schemas(),
        "clone" => commands::clone::__docgen_schemas(),
        "config" => commands::config::__docgen_schemas(),
        "conflicts" => commands::conflicts::__docgen_schemas(),
        "consolidate" => commands::consolidate::__docgen_schemas(),
        "deliver" => commands::deliver::__docgen_schemas(),
        "diff" => commands::diff::__docgen_schemas(),
        "expand" => commands::expand::__docgen_schemas(),
        "git" => [
            commands::import_git::__docgen_schemas(),
            commands::export_git::__docgen_schemas(),
            commands::mirror::__docgen_schemas(),
        ].into_iter().flatten().collect(),
        "haul" => commands::haul::__docgen_schemas(),
        "heal" => commands::heal::__docgen_schemas(),
        "history" => commands::history::__docgen_schemas(),
        "query" => commands::query::__docgen_schemas(),
        "lift" => commands::lift::__docgen_schemas(),
        "load" => commands::load::__docgen_schemas(),
        "lower" => commands::lower::__docgen_schemas(),
        "manifest" => [
            commands::manifest::__docgen_schemas(),
            commands::kv::__docgen_schemas(),
        ].into_iter().flatten().collect(),
        "narrow" => commands::narrow::__docgen_schemas(),
        "office" => commands::office::__docgen_schemas(),
        "pallet" => commands::pallet::__docgen_schemas(),
        "park" => commands::park::__docgen_schemas(),
        "pick" => commands::pick::__docgen_schemas(),
        "prepare" => commands::prepare::__docgen_schemas(),
        "profile" => commands::profile::__docgen_schemas(),
        "remove" => commands::remove::__docgen_schemas(),
        "scope" => commands::scope::__docgen_schemas(),
        "scope-prune" => commands::scope_prune::__docgen_schemas(),
        "remote" => commands::remote::__docgen_schemas(),
        "self" => [
            commands::self_update::__docgen_schemas(),
            commands::alias::__docgen_schemas(),
            commands::self_schema::__docgen_schemas(),
        ].into_iter().flatten().collect(),
        "shift" => commands::shift::__docgen_schemas(),
        "show" => commands::show::__docgen_schemas(),
        "stack" => commands::stack::__docgen_schemas(),
        "stocktake" => commands::stocktake::__docgen_schemas(),
        "store" => [
            commands::store::__docgen_schemas(),
            commands::compact::__docgen_schemas(),
            commands::peek::__docgen_schemas(),
            commands::evict::__docgen_schemas(),
        ].into_iter().flatten().collect(),
        "tag" => commands::tag::__docgen_schemas(),
        "undo" => commands::undo::__docgen_schemas(),
        "version" => commands::version::__docgen_schemas(),
        _ => return None,
    })
}
