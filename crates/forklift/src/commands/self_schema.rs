//! `forklift self schema`: this binary's command surface, described in Pult's manifest
//! vocabulary, so `pult tool` can offer Forklift's commands as Pult commands.
//!
//! The document is generated from the clap definitions of the running binary, so it cannot
//! drift from the binary that will run it. The tree is walked *unbuilt*: `build()` adds a help
//! argument and copies the global flags into every command, and neither is part of a
//! command's meaning. Every argument is classified against a closed whitelist of clap shapes,
//! and anything outside it is refused with an error naming the command and argument. A
//! refusal is the whole result, never a document with one command left out, because a
//! consumer cannot tell a silently shortened surface from a complete one.
//!
//! Each `run` is the argument tail after the program name; `pult tool` prefixes the quoted
//! path of the binary it resolved. The document is therefore not a runnable manifest on its
//! own: some command words (`diff`, `audit`) name host programs.

use clap::{Arg, ArgAction, Command, CommandFactory};
use serde::ser::SerializeMap;
use serde::{Serialize, Serializer};
use crate::cli::Cli;
use crate::output::{self, CommandOutput};

/// The Pult manifest version the document is written in. Pult refuses a version above the
/// one it supports, so a future change of shape fails loudly there.
const MANIFEST_VERSION: u32 = 1;

/// Command ids Pult reserves for its own verbs (its resolver's `RESERVED_IDS`, pult 0.5.1).
/// Pult refuses a manifest command with one of them, so the emitter refuses it first.
const RESERVED_IDS: &[&str] = &[
    "includes", "registry", "module", "update", "self", "init", "trust", "cache", "ui",
    "events", "x", "tap", "registries", "serve", "doctor",
];

/// Param names Pult reserves (its resolver's `RESERVED_PARAMS`, pult 0.5.1).
const RESERVED_PARAMS: &[&str] = &["trust", "print", "params-json", "run-id", "help", "json", "var"];

/// What separates the members of a collapsed run of positionals in its param's description.
const RUN_SEPARATOR: &str = " | ";
/// The character that marks [`RUN_SEPARATOR`]; no key may hold it, nor the help of any member
/// of a run of two or more.
const RUN_MARK: char = '|';

/// Substrings that make an argument's name look like a credential. Every value-taking
/// argument whose id or long name contains one must be classified in [`Tables::secret`] or
/// [`Tables::not_secret`]. This catches a new credential by its name; one hiding behind an
/// innocent name is beyond any name check.
const CREDENTIAL_WORDS: &[&str] = &[
    "token", "secret", "password", "passphrase", "key", "credential", "bearer", "cookie", "private",
];

/// What the emitter needs to know that clap cannot hold. Every entry is checked against the
/// tree it describes: one that names nothing is an error, never dead weight.
pub(crate) struct Tables {
    /// Commands that are not emitted, by full command path (`self schema`), each with its
    /// reason. Everything below an excluded family is left out with it.
    excluded: &'static [(&'static str, &'static str)],

    /// Value-taking arguments whose value is a credential, as (command path, clap id).
    secret: &'static [(&'static str, &'static str)],

    /// Value-taking arguments whose name looks like a credential but whose value is not one,
    /// as (command path, clap id, reason).
    not_secret: &'static [(&'static str, &'static str, &'static str)],
}

/// Forklift's tables.
const FORKLIFT: Tables = Tables {
    excluded: &[
        ("help", "clap meta: it repeats the descriptions this document already carries"),
        ("mcp", "a long-running stdio JSON-RPC server, not a one-shot invocation"),
        ("self schema", "the describer itself"),
        ("haul review", "its verdict is exactly one of three switches, which no Pult param can require"),
    ],
    secret: &[
        ("clone", "token"),     // the bearer token, remembered as remote.token
        ("config set", "value"), // a configuration value may be that token
        ("remote move", "token"), // the new origin's bearer token, remembered as remote.token
    ],
    not_secret: &[
        ("config get", "key", "a configuration key name, not key material"),
        ("config set", "key", "a configuration key name, not key material"),
        ("config unset", "key", "a configuration key name, not key material"),
        ("office admit", "public_key", "the public half of a key pair"),
        ("office link", "public_key", "the public half of a key pair"),
        ("office authorize", "public_key", "the public half of a key pair"),
        ("office retire", "key_id", "an identifier, not key material"),
        ("manifest set", "key", "a custom-metadata key name, not key material"),
        ("manifest get", "key", "a custom-metadata key name, not key material"),
        ("manifest unset", "key", "a custom-metadata key name, not key material"),
        ("manifest log", "key", "a custom-metadata key name, not key material"),
    ],
};

/// Handle the self schema command: describe the command surface and emit the document.
///
/// # Returns
/// * `Ok(())`      - The document was emitted.
/// * `Err(String)` - Some part of the surface cannot be described in Pult's vocabulary
///                   (named in the message); nothing is emitted.
pub fn handle_command() -> Result<(), String> {
    // Ordered: Pult binds positional params in the order the document lists them.
    output::emit_ordered(&describe(&Cli::command(), &FORKLIFT)?);
    Ok(())
}

/// The command surface as a Pult manifest: exactly the top-level keys a root manifest may
/// carry, since Pult refuses unknown ones.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct SelfSchema {
    /// Pult's manifest version (1).
    version: u32,

    /// The tool's name (`forklift`).
    name: String,

    /// The tool's one-line description.
    description: String,

    /// One command per runnable command path, in clap's declaration order.
    commands: Vec<SchemaCommand>,
}

/// One runnable command path.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
struct SchemaCommand {
    /// The command path joined with `-` (`office-admit`).
    id: String,

    /// The command's one-line description.
    title: String,

    /// The top-level group the command belongs to; absent for a top-level command.
    #[serde(skip_serializing_if = "Option::is_none")]
    category: Option<String>,

    /// The command's long description, when it has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,

    /// The command's params by key, in order: options and switches first, then positionals.
    #[cfg_attr(feature = "docgen", schemars(with = "std::collections::BTreeMap<String, Param>"))]
    params: Params,

    /// The argument tail after the program name, with a `{key}` placeholder per param.
    /// Options and switches come before a literal `--`, positionals after it.
    run: String,

    /// Always false: no Forklift command requires a terminal by itself.
    interactive: bool,
}

/// A command's params, serialized as a map in insertion order. Pult binds positional params
/// in declaration order, so the order is part of the meaning.
#[derive(Default)]
struct Params(Vec<(String, Param)>);

impl Serialize for Params {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, param) in &self.0 {
            map.serialize_entry(key, param)?;
        }
        map.end()
    }
}

/// One param: its kind, and what its value is for.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[cfg_attr(feature = "docgen", schemars(deny_unknown_fields))]
#[derive(Serialize)]
struct Param {
    #[serde(flatten)]
    kind: Kind,

    /// What the value is for, from the argument's help; absent when it has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
}

/// A param's kind, in Pult's vocabulary.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
#[serde(rename_all = "lowercase")]
enum Kind {
    /// A required single positional; Pult prompts for it when it is absent.
    Input(Input),

    /// A switch.
    Flag(Flag),

    /// An option (with `arg`) or the trailing positionals (without).
    Repeat(Repeat),
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
struct Input {
    /// The value is a credential.
    #[serde(skip_serializing_if = "crate::output::is_false")]
    secret: bool,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
struct Flag {
    /// The word passed when the switch is on.
    arg: String,
}

#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
struct Repeat {
    /// The option each value follows; absent for positionals.
    #[serde(skip_serializing_if = "Option::is_none")]
    arg: Option<String>,

    /// The fewest values accepted.
    #[serde(skip_serializing_if = "is_zero")]
    min: u32,

    /// The most values accepted; unbounded when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    max: Option<u32>,

    /// Every value is a credential.
    #[serde(skip_serializing_if = "crate::output::is_false")]
    secret: bool,
}

fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl CommandOutput for SelfSchema {
    /// The document is the product, so human mode prints it too, pretty-printed: a prose
    /// rendering would only be a second description that could disagree with the first.
    fn render_human(&self) {
        println!("{}", serde_json::to_string_pretty(self).expect("the document serializes"));
    }
}

/// Describe `root`'s command surface.
///
/// # Arguments
/// * `root`   - The unbuilt clap command to describe.
/// * `tables` - What clap cannot say about it: exclusions and secrets.
///
/// # Returns
/// * `Ok(SelfSchema)` - The whole surface.
/// * `Err(String)`    - The first part of the surface, or table entry, that cannot be
///                      described; nothing partial is returned.
fn describe(root: &Command, tables: &Tables) -> Result<SelfSchema, String> {
    let description = about(root, &[])?;
    command_settings(&[], root)?;

    let mut walk = Walk {
        tables,
        commands: Vec::new(),
        excluded_used: vec![false; tables.excluded.len()],
        secret_used: vec![false; tables.secret.len()],
        not_secret_used: vec![false; tables.not_secret.len()],
    };

    for command in root.get_subcommands().filter(|command| !command.is_hide_set()) {
        walk.command(command, vec![command.get_name()])?;
    }

    for (index, (path, _)) in tables.excluded.iter().enumerate() {
        if !walk.excluded_used[index] {
            return Err(format!(
                "self schema: the exclusion list names `{path}`, which is not a visible command \
                 under its own name"
            ));
        }
    }
    for (index, (path, id)) in tables.secret.iter().enumerate() {
        if !walk.secret_used[index] {
            return Err(format!(
                "self schema: the secret list names `{id}` of `{path}`, which is not a \
                 value-taking argument of a described command"
            ));
        }
    }
    for (index, (path, id, _)) in tables.not_secret.iter().enumerate() {
        if !walk.not_secret_used[index] {
            return Err(format!(
                "self schema: the not-secret list names `{id}` of `{path}`, which is not a \
                 value-taking argument of a described command"
            ));
        }
    }

    Ok(SelfSchema {
        version: MANIFEST_VERSION,
        name: root.get_name().to_string(),
        description,
        commands: walk.commands,
    })
}

/// The state of one walk over the tree.
struct Walk<'a> {
    tables: &'a Tables,
    commands: Vec<SchemaCommand>,

    /// Which table entries matched a command or an argument, for the staleness check.
    excluded_used: Vec<bool>,
    secret_used: Vec<bool>,
    not_secret_used: Vec<bool>,
}

/// How a positional argument is shaped.
struct Positional {
    id: String,
    key: String,
    required: bool,
    variadic: bool,
    secret: bool,
    help: Option<String>,
}

impl Walk<'_> {
    /// Describe `command` at `path`: a group contributes its bare form when it runs without a
    /// subcommand, then each visible subcommand; a leaf contributes itself.
    fn command(&mut self, command: &Command, path: Vec<&str>) -> Result<(), String> {
        let full = path.join(" ");
        if let Some(index) = self.tables.excluded.iter().position(|(excluded, _)| *excluded == full) {
            self.excluded_used[index] = true;
            return Ok(());
        }
        command_settings(&path, command)?;
        if command.has_subcommands() {
            // A group's own arguments would have no place in its children's commands.
            if let Some(arg) = command.get_arguments().next() {
                return Err(refusal(&path, arg, "it belongs to a group that has subcommands"));
            }
            if !command.is_subcommand_required_set() {
                self.push(command, &path, Params::default(), path.join(" "))?;
            }
            for sub in command.get_subcommands().filter(|sub| !sub.is_hide_set()) {
                let mut sub_path = path.clone();
                sub_path.push(sub.get_name());
                self.command(sub, sub_path)?;
            }
            return Ok(());
        }

        let mut named = Params::default();
        let mut positionals = Vec::new();

        for arg in command.get_arguments() {
            let secret = self.secret(&path, arg)?;
            if arg.is_positional() {
                positionals.push(positional(&path, arg, secret)?);
                continue;
            }

            let long = arg.get_long()
                .ok_or_else(|| refusal(&path, arg, "it has no long name for Pult to pass"))?;
            let kind = match arg.get_action() {
                ArgAction::SetTrue => {
                    switch_refusals(&path, arg)?;
                    if arg.get_num_args().is_some() {
                        return Err(refusal(&path, arg, "it is a switch with a value count"));
                    }
                    Kind::Flag(Flag { arg: format!("--{long}") })
                }
                ArgAction::Set | ArgAction::Append => {
                    single_valued(&path, arg)?;
                    let required = arg.is_required_set();
                    let append = matches!(arg.get_action(), ArgAction::Append);
                    Kind::Repeat(Repeat {
                        arg: Some(format!("--{long}")),
                        min: if required { 1 } else { 0 },
                        max: if append { None } else { Some(1) },
                        secret,
                    })
                }
                other => return Err(refusal(&path, arg, &format!("its action {other:?} has no Pult param"))),
            };
            named.0.push((long.to_string(), Param { kind, description: help(&path, arg)? }));
        }

        let mut params = named;
        let mut run = path.join(" ");
        for (key, _) in &params.0 {
            run.push_str(&format!(" {{{key}}}"));
        }

        let positional_params = layout(&path, positionals)?;
        if !positional_params.is_empty() {
            // Without `--`, a positional value that spells one of the command's own flags
            // would be read as that flag, silently. After it, every word is a positional.
            run.push_str(" --");
            for (key, _) in &positional_params {
                run.push_str(&format!(" {{{key}}}"));
            }
        }
        params.0.extend(positional_params);

        self.push(command, &path, params, run)
    }

    /// Validate and record one command.
    fn push(&mut self, command: &Command, path: &[&str], params: Params, run: String) -> Result<(), String> {
        let id = path.join("-");
        let shown = path.join(" ");
        if RESERVED_IDS.contains(&id.as_str()) {
            return Err(format!("self schema: `forklift {shown}` would have the id `{id}`, which Pult reserves"));
        }
        if self.commands.iter().any(|command| command.id == id) {
            return Err(format!("self schema: `forklift {shown}` would have the id `{id}`, which another command already has"));
        }

        let mut keys = std::collections::HashSet::new();
        for (key, _) in &params.0 {
            let valid = !key.is_empty()
                && key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
            if !valid {
                return Err(format!("self schema: `forklift {shown}` has a param `{key}` whose name Pult does not accept"));
            }
            if RESERVED_PARAMS.contains(&key.as_str()) {
                return Err(format!("self schema: `forklift {shown}` has a param `{key}`, a name Pult reserves"));
            }
            if !keys.insert(key.as_str()) {
                return Err(format!(
                    "self schema: `forklift {shown}` has two params named `{key}` (a positional and \
                     an option sharing a name); Pult would keep only one"
                ));
            }
        }

        let title = about(command, path)?;
        let long_about = command.get_long_about()
            .map(|text| text.to_string())
            .filter(|text| !text.trim().is_empty());
        // Pult substitutes `${...}` in a command's title and description as it does in a
        // param's, so a literal one would be silently rewritten.
        for (what, text) in [("title", Some(&title)), ("description", long_about.as_ref())] {
            if text.is_some_and(|text| text.contains("${")) {
                return Err(format!(
                    "self schema: `forklift {shown}` has a {what} containing `${{`, which Pult would \
                     read as a variable"
                ));
            }
        }

        self.commands.push(SchemaCommand {
            id,
            title,
            category: (path.len() > 1 || command.has_subcommands()).then(|| path[0].to_string()),
            description: long_about,
            params,
            run,
            interactive: false,
        });
        Ok(())
    }

    /// Whether `arg`'s value is a credential, by the tables; refuses a value-taking argument
    /// with a credential-like name that neither table classifies.
    fn secret(&mut self, path: &[&str], arg: &Arg) -> Result<bool, String> {
        if !matches!(arg.get_action(), ArgAction::Set | ArgAction::Append) {
            return Ok(false);
        }

        let shown = path.join(" ");
        let id = arg.get_id().as_str();
        let secret = self.tables.secret.iter().position(|(p, i)| *p == shown && *i == id);
        let not_secret = self.tables.not_secret.iter().position(|(p, i, _)| *p == shown && *i == id);

        match (secret, not_secret) {
            (Some(_), Some(_)) => Err(refusal(path, arg, "it is on both the secret and the not-secret list")),
            (Some(index), None) => {
                self.secret_used[index] = true;
                Ok(true)
            }
            (None, Some(index)) => {
                self.not_secret_used[index] = true;
                Ok(false)
            }
            (None, None) => {
                let names = [Some(id), arg.get_long()];
                let looks_secret = names.iter().flatten().any(|name| {
                    let name = name.to_ascii_lowercase();
                    CREDENTIAL_WORDS.iter().any(|word| name.contains(word))
                });
                if looks_secret {
                    return Err(refusal(
                        path, arg,
                        "its name looks like a credential; classify it on the secret or the \
                         not-secret list",
                    ));
                }
                Ok(false)
            }
        }
    }
}

/// Refuse the command-level parse settings that change how a composed line binds. Clap keeps
/// `allow_hyphen_values`, `ignore_errors` and `infer_long_args` private on a command, so they
/// cannot be refused here; the round-trip test's hostile values are what catch the first two
/// (a hostile option value would parse), and the third cannot misbind a full long name.
fn command_settings(path: &[&str], command: &Command) -> Result<(), String> {
    let why = if command.is_allow_negative_numbers_set() {
        "it accepts values beginning with `-`"
    } else if command.is_trailing_var_arg_set() {
        "its last positional swallows options"
    } else if command.is_allow_missing_positional_set() {
        "it lets a positional be skipped"
    } else if command.is_allow_external_subcommands_set() {
        "it accepts commands it does not declare"
    } else if command.is_args_override_self() {
        "its options override themselves"
    } else if command.is_subcommand_precedence_over_arg_set() || command.is_subcommand_negates_reqs_set() {
        "its subcommands change how its arguments bind"
    } else if command.is_multicall_set() {
        "it is a multicall command"
    } else if command.get_groups().any(|group| group.is_required_set()) {
        // Pult's params are independent of one another: "exactly one of these" has no form, and
        // listing the members as plain switches would describe a bare run that clap refuses.
        "it requires one of a group of arguments"
    } else {
        return Ok(());
    };
    Err(format!("self schema cannot describe `forklift {}`: {why}", path.join(" ")))
}

/// A command's one-line description, which Pult requires as its title.
fn about(command: &Command, path: &[&str]) -> Result<String, String> {
    command.get_about()
        .map(|text| text.to_string())
        .filter(|text| !text.trim().is_empty())
        .ok_or_else(|| format!("self schema: `forklift {}` has no one-line description", path.join(" ")))
}

/// The refusal for an argument outside the whitelist.
fn refusal(path: &[&str], arg: &Arg, why: &str) -> String {
    refusal_of(path, arg.get_id().as_str(), why)
}

/// [`refusal`], for an argument known by its clap id.
fn refusal_of(path: &[&str], id: &str, why: &str) -> String {
    let shown = if path.is_empty() { String::new() } else { format!(" {}", path.join(" ")) };
    format!("self schema cannot describe argument `{id}` of `forklift{shown}`: {why}")
}

/// Refuse the argument settings no Pult param can carry, then classify a positional.
fn positional(path: &[&str], arg: &Arg, secret: bool) -> Result<Positional, String> {
    common_refusals(path, arg)?;
    if arg.get_index().is_some() {
        return Err(refusal(path, arg, "it sets an explicit position"));
    }

    let variadic = match (arg.get_action(), arg.get_num_args()) {
        (ArgAction::Set, None) => false,
        (ArgAction::Set, Some(range)) if range.min_values() == 1 && range.max_values() == 1 => false,
        (ArgAction::Append, Some(range)) if range.min_values() == 1 && range.max_values() == usize::MAX => true,
        _ => return Err(refusal(path, arg, "its value count has no Pult param")),
    };

    Ok(Positional {
        id: arg.get_id().to_string(),
        key: arg.get_id().as_str().replace('_', "-"),
        required: arg.is_required_set(),
        variadic,
        secret,
        help: help(path, arg)?,
    })
}

/// An argument's help, as its param's description: the short help, else the long one; none
/// when it has neither, since Pult treats an empty description as absent anyway. Refuses a
/// help with `${`, which Pult reads as a variable reference in a description.
fn help(path: &[&str], arg: &Arg) -> Result<Option<String>, String> {
    let help = arg.get_help().or_else(|| arg.get_long_help())
        .map(|text| text.to_string())
        .filter(|text| !text.trim().is_empty());
    if help.as_deref().is_some_and(|text| text.contains("${")) {
        return Err(refusal(path, arg, "its help contains `${`, which Pult would read as a variable"));
    }
    Ok(help)
}

/// Refuse an option whose value count is anything but one value per occurrence. On the
/// unbuilt tree clap_derive leaves the count unset for a plain `Option<T>` or `Vec<T>`
/// option, so unset means single-valued; `Option<Option<T>>` sets it to `0..=1`.
fn single_valued(path: &[&str], arg: &Arg) -> Result<(), String> {
    common_refusals(path, arg)?;
    match arg.get_num_args() {
        None => Ok(()),
        Some(range) if range.min_values() == 1 && range.max_values() == 1 => Ok(()),
        Some(_) => Err(refusal(path, arg, "it takes other than exactly one value per occurrence")),
    }
}

/// The refusals that apply to every argument, switches included.
fn switch_refusals(path: &[&str], arg: &Arg) -> Result<(), String> {
    let why = if arg.is_global_set() {
        "it is global"
    } else if arg.is_hide_set() {
        "it is hidden"
    } else if arg.is_last_set() {
        "it is marked `last`"
    } else {
        return Ok(());
    };
    Err(refusal(path, arg, why))
}

/// The refusals shared by every value-taking argument: [`switch_refusals`], and the settings
/// that shape a value. (A switch has the possible values `true` and `false`, so the last
/// check would refuse every switch.)
fn common_refusals(path: &[&str], arg: &Arg) -> Result<(), String> {
    switch_refusals(path, arg)?;
    let why = if arg.is_trailing_var_arg_set() || arg.is_allow_hyphen_values_set() || arg.is_allow_negative_numbers_set() {
        "it accepts values beginning with `-`"
    } else if arg.is_require_equals_set() {
        "it requires `--name=value`, which Pult does not render"
    } else if arg.get_value_delimiter().is_some() {
        "it splits values on a delimiter"
    } else if !arg.get_possible_values().is_empty() {
        "it has a fixed list of possible values"
    } else {
        return Ok(());
    };
    Err(refusal(path, arg, why))
}

/// Lay out a leaf's positionals as Pult params: each required single positional is an
/// input; a required variadic is an arg-less repeat with min 1; the trailing run of optional
/// positionals collapses into ONE arg-less repeat, because Pult allows only one and clap
/// fills the run in order, so the composed words are exactly what a person would type.
fn layout(path: &[&str], positionals: Vec<Positional>) -> Result<Vec<(String, Param)>, String> {
    let shown = path.join(" ");
    let mut params = Vec::new();
    let mut optional: Vec<Positional> = Vec::new();

    for (index, positional) in positionals.iter().enumerate() {
        if positional.variadic && index + 1 != positionals.len() {
            return Err(format!("self schema: `forklift {shown}` has a variadic positional `{}` before another", positional.key));
        }
    }

    for positional in positionals {
        if !positional.required {
            optional.push(positional);
            continue;
        }
        if let Some(earlier) = optional.first() {
            return Err(format!(
                "self schema: `forklift {shown}` has an optional positional `{}` before the \
                 required `{}`",
                earlier.key, positional.key
            ));
        }
        let kind = if positional.variadic {
            Kind::Repeat(Repeat { arg: None, min: 1, max: None, secret: positional.secret })
        } else {
            Kind::Input(Input { secret: positional.secret })
        };
        params.push((positional.key, Param { kind, description: positional.help }));
    }

    if !optional.is_empty() {
        let max = if optional.iter().any(|positional| positional.variadic) {
            None
        } else {
            Some(u32::try_from(optional.len()).expect("a handful of positionals"))
        };
        let key = optional.iter().map(|positional| positional.key.as_str()).collect::<Vec<_>>().join("-");
        let secret = optional.iter().any(|positional| positional.secret);
        // One param stands for the whole run, so every member is named by its key, in the
        // order the values fill them, with its help when it has one. Neither a key nor a
        // member's help may hold the separator's mark, so every mark in the description is a
        // separator and the reader can always tell where a member starts.
        let description = match optional.as_slice() {
            [only] => only.help.clone(),
            run if run.iter().all(|positional| positional.help.is_none()) => None,
            run => {
                let mut parts = Vec::new();
                for positional in run {
                    match &positional.help {
                        Some(help) if help.contains(RUN_MARK) => return Err(refusal_of(
                            path, &positional.id,
                            &format!("its help contains `{RUN_MARK}`, which separates the members of its run"),
                        )),
                        Some(help) => parts.push(format!("{}: {help}", positional.key)),
                        None => parts.push(positional.key.clone()),
                    }
                }
                Some(parts.join(RUN_SEPARATOR))
            }
        };
        let kind = Kind::Repeat(Repeat { arg: None, min: 0, max, secret });
        params.push((key, Param { kind, description }));
    }

    Ok(params)
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<SelfSchema>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::error::{ContextKind, ContextValue, ErrorKind};
    use clap::parser::ValueSource;
    use clap::{FromArgMatches, Parser, Subcommand};
    use std::any::TypeId;
    use std::collections::BTreeMap;

    const NO_TABLES: Tables = Tables { excluded: &[], secret: &[], not_secret: &[] };

    /// The real document; every test on the real tree starts here.
    fn forklift() -> SelfSchema {
        describe(&Cli::command(), &FORKLIFT).unwrap_or_else(|error| panic!("{error}"))
    }

    /// The runnable command paths of `command` at `path`, counted by a walk of its own: a leaf,
    /// or a group that runs without a subcommand, plus every visible subcommand below a group;
    /// nothing at or below a path in `left_out`.
    fn runnable(command: &Command, path: &str, left_out: &[&str]) -> usize {
        if left_out.contains(&path) {
            return 0;
        }
        if !command.has_subcommands() {
            return 1;
        }
        let bare = usize::from(!command.is_subcommand_required_set());
        bare + command.get_subcommands()
            .filter(|sub| !sub.is_hide_set())
            .map(|sub| runnable(sub, &format!("{path} {}", sub.get_name()), left_out))
            .sum::<usize>()
    }

    // (a) The whole tree is described, and the document passes Pult's id and key rules.

    #[test]
    fn the_whole_tree_is_described() {
        let root = Cli::command();
        let doc = forklift();

        // Written out rather than read from the table, so a wrong table entry shows up here.
        let left_out = ["help", "mcp", "self schema", "haul review"];
        let expected: usize = root.get_subcommands()
            .filter(|command| !command.is_hide_set())
            .map(|command| runnable(command, command.get_name(), &left_out))
            .sum();
        assert_eq!(doc.commands.len(), expected);

        let mut ids = std::collections::HashSet::new();
        for command in &doc.commands {
            assert!(!command.id.is_empty());
            assert!(ids.insert(command.id.as_str()), "duplicate id {}", command.id);
            assert!(!RESERVED_IDS.contains(&command.id.as_str()), "reserved id {}", command.id);
            assert!(!command.title.trim().is_empty());

            let mut keys = std::collections::HashSet::new();
            for (key, param) in &command.params.0 {
                // Absence is legal (an argument with no help), but none is absent today; a new
                // undocumented argument would ship an unexplained prompt in Pult.
                assert!(param.description.is_some(), "{}: param {key} has no description", command.id);
                assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.')));
                assert!(!RESERVED_PARAMS.contains(&key.as_str()), "{}: reserved param {key}", command.id);
                assert!(keys.insert(key.as_str()), "{}: duplicate param {key}", command.id);
            }
        }

        // A family member is left out by its path alone: its siblings are still described.
        for anchor in ["office", "office-admit", "config", "diff", "self-update", "self-alias", "self-alias-install"] {
            assert!(ids.contains(anchor), "missing {anchor}");
        }
        for excluded in ["help", "mcp", "self-schema", "self"] {
            assert!(!ids.contains(excluded), "{excluded} must not be described");
        }
        assert_eq!(doc.version, 1);
        assert_eq!(doc.name, "forklift");
    }

    #[test]
    fn the_shapes_of_known_commands() {
        let doc = forklift();
        let json = |id: &str| {
            let command = doc.commands.iter().find(|command| command.id == id).expect(id);
            serde_json::to_string(command).unwrap()
        };

        let diff = json("diff");
        assert!(diff.contains(r#""run":"diff {staged} -- {targets}""#), "{diff}");
        assert!(diff.contains(concat!(
            r#""staged":{"flag":{"arg":"--staged"},"#,
            r#""description":"Compare the inventory against the pallet head (what \"forklift stack\" would record)"}"#,
        )), "{diff}");
        // A run of one optional positional is described by that positional's help, unlabelled.
        assert!(diff.contains(
            r#""targets":{"repeat":{},"description":"[path], or <revision-a> <revision-b> [path] — \":empty\" is the empty tree"}"#
        ), "{diff}");

        // A configuration value may be a token; its key is a name, never key material.
        let config = json("config-set");
        assert!(config.contains(r#""value":{"input":{"secret":true},"description":"The value to set the key to"}"#), "{config}");
        assert!(config.contains(r#""key":{"input":{},"#), "{config}");
        assert!(config.contains(r#""run":"config set {global} -- {key} {value}""#), "{config}");

        let franchise = json("clone");
        assert!(franchise.contains(concat!(
            r#""token":{"repeat":{"arg":"--token","max":1,"secret":true},"#,
            r#""description":"The bearer token, when the remote requires one (remembered as \"remote.token\")"}"#,
        )), "{franchise}");
        assert!(franchise.contains(r#""run":"clone {pallet} {token} {only} {cache} {expect} -- {url} {directory}""#), "{franchise}");

        let office = json("office");
        assert!(office.contains(r#""category":"office""#) && office.contains(r#""params":{}"#), "{office}");
        assert!(office.contains(r#""run":"office""#), "{office}");
        assert!(json("office-admit").contains(r#""category":"office""#));
        assert!(!json("diff").contains("category"));
    }

    // (b) Every shape outside the whitelist is refused, naming the argument.

    #[derive(Parser)]
    #[command(name = "probe", about = "Probe")]
    struct ProbeCli {
        #[command(subcommand)]
        command: Probe,
    }

    #[derive(Clone, clap::ValueEnum)]
    enum Shade { Light, Dark }

    #[derive(Subcommand)]
    enum Probe {
        /// Count
        Count { #[arg(long, action = ArgAction::Count)] loud: u8 },
        /// SetFalse
        SetFalse { #[arg(long, action = ArgAction::SetFalse)] lit: bool },
        /// Possible values
        Enum { #[arg(long, value_enum)] shade: Option<Shade> },
        /// Hidden
        Hidden { #[arg(long, hide = true)] quiet: Option<String> },
        /// Last
        Last { #[arg(last = true)] rest: Vec<String> },
        /// Two values
        Pair { #[arg(long, num_args = 2)] pair: Vec<String> },
        /// Optional value
        Maybe { #[arg(long)] maybe: Option<Option<String>> },
        /// Short only
        Short { #[arg(short = 's')] small: Option<String> },
        /// Optional before required
        Order { first: Option<String>, second: String },
        /// Positional beside an option of its name
        Clash { #[arg(long = "x")] x_option: Option<String>, x: String },
        /// Plain
        Plain { #[arg(long)] label: Option<String> },
        /// Credential
        Creds { #[arg(long)] api_token: Option<String> },
        /// Credential-named switch
        Switch { #[arg(long)] no_token: bool },
        /// Hidden switch
        HiddenSwitch { #[arg(long, hide = true)] dump: bool },
        /// Global switch on a leaf
        GlobalSwitch { #[arg(long, global = true)] loudly: bool },
        /// A trailing positional that swallows options
        #[command(trailing_var_arg = true)]
        Swallow { rest: Vec<String> },
        /// Described
        Described {
            /// Short help
            ///
            /// Only in the long help
            #[arg(long)]
            both: Option<String>,
            #[arg(long, long_help = "Long help only")]
            long_only: bool,
            #[arg(long)]
            bare: bool,
            /// The first
            first: Option<String>,
            second: Option<String>,
        },
        /// Variable-like help
        Dollar {
            /// Defaults to ${HOME}
            #[arg(long)]
            home: Option<String>,
        },
        /// Uses ${HOME}
        DollarTitle,
        /// Plain title
        ///
        /// Writes to ${module.dir}
        DollarLong,
        /// Help holding the run separator's mark
        Piped {
            /// One |the other
            either: Option<String>,
            /// The next
            next: Option<String>,
        },
        /// One of a required group
        #[command(group = clap::ArgGroup::new("pick").required(true).args(["up", "down"]))]
        Either {
            #[arg(long)]
            up: bool,
            #[arg(long)]
            down: bool,
        },
    }

    /// The probe tree with only `name` under its root, so each probe is judged alone.
    fn probe(name: &str) -> Command {
        let sub = ProbeCli::command().find_subcommand(name).expect(name).clone();
        Command::new("probe").about("Probe").subcommand(sub)
    }

    #[test]
    fn shapes_outside_the_whitelist_are_refused() {
        for (name, needle) in [
            ("count", "`loud`"),
            ("set-false", "`lit`"),
            ("enum", "`shade`"),
            ("hidden", "`quiet`"),
            ("last", "`rest`"),
            ("pair", "`pair`"),
            ("maybe", "`maybe`"),
            ("short", "`small`"),
            ("order", "`first`"),
            ("clash", "`x`"),
            ("hidden-switch", "`dump`"),
            ("global-switch", "`loudly`"),
            ("swallow", "swallows options"),
            ("either", "requires one of a group"),
            ("dollar", "`home` of `forklift dollar`: its help contains `${`"),
            ("dollar-title", "has a title containing `${`"),
            ("dollar-long", "has a description containing `${`"),
            ("piped", "`either` of `forklift piped`: its help contains `|`"),
        ] {
            let error = describe(&probe(name), &NO_TABLES).err()
                .unwrap_or_else(|| panic!("probe {name} was accepted"));
            assert!(error.contains(needle) && error.contains(&format!("`forklift {name}`")), "{name}: {error}");
        }
    }

    #[test]
    fn a_plain_option_is_accepted_as_a_single_valued_repeat() {
        let doc = describe(&probe("plain"), &NO_TABLES).unwrap();
        assert_eq!(
            serde_json::to_string(&doc.commands).unwrap(),
            r#"[{"id":"plain","title":"Plain","params":{"label":{"repeat":{"arg":"--label","max":1}}},"run":"plain {label}","interactive":false}]"#
        );
    }

    /// A param's description is its argument's short help, else its long help, else absent;
    /// a collapsed run of positionals labels each member's help with that member's key.
    #[test]
    fn a_param_is_described_by_its_help() {
        // The refusal guards the mark, so the separator must be built around it.
        assert_eq!(RUN_SEPARATOR, format!(" {RUN_MARK} "));
        let doc = describe(&probe("described"), &NO_TABLES).unwrap();
        assert_eq!(
            serde_json::to_string(&doc.commands[0].params).unwrap(),
            concat!(
                r#"{"both":{"repeat":{"arg":"--both","max":1},"description":"Short help"},"#,
                r#""long-only":{"flag":{"arg":"--long-only"},"description":"Long help only"},"#,
                r#""bare":{"flag":{"arg":"--bare"}},"#,
                r#""first-second":{"repeat":{"max":2},"description":"first: The first | second"}}"#,
            )
        );
    }

    // (c) Every command's run, rendered the way Pult renders it, makes clap bind each value to
    // exactly the argument it was meant for.

    #[derive(Clone)]
    enum Value {
        Text(String),
        Switch(bool),
        List(Vec<String>),
    }

    /// How an option's items are written.
    #[derive(Clone, Copy)]
    enum Form {
        /// As `pult tool` writes them (Pult's `exec::argv`): an item starting with `-` under a
        /// `--` option rides inside it as one word, `--grep=-x`; any other item follows it.
        /// Every `arg` here starts with `--` (`assert_keys`), so only the item decides.
        Tool,
        /// Always as two words, `--grep -x`: how Pult renders a hand-written manifest, and how
        /// a person types it.
        Split,
    }

    /// The argv for `command` with `values`, option items written in `form`: Pult's render
    /// rules for each kind, substituted into the run, after the program name.
    fn render(command: &SchemaCommand, values: &BTreeMap<String, Value>, form: Form) -> Vec<String> {
        let mut argv = vec!["forklift".to_string()];
        for word in command.run.split_whitespace() {
            let Some(key) = word.strip_prefix('{').and_then(|word| word.strip_suffix('}')) else {
                argv.push(word.to_string());
                continue;
            };
            let param = &command.params.0.iter().find(|(name, _)| name == key).expect(key).1;
            match (&param.kind, values.get(key)) {
                (_, None) => {}
                (Kind::Input(_), Some(Value::Text(text))) => argv.push(text.clone()),
                (Kind::Flag(flag), Some(Value::Switch(on))) => {
                    if *on {
                        argv.push(flag.arg.clone());
                    }
                }
                (Kind::Repeat(repeat), Some(Value::List(items))) => {
                    for item in items {
                        match (&repeat.arg, form) {
                            (Some(arg), Form::Tool) if arg.starts_with("--") && item.starts_with('-') => {
                                argv.push(format!("{arg}={item}"))
                            }
                            (Some(arg), _) => argv.extend([arg.clone(), item.clone()]),
                            (None, _) => argv.push(item.clone()),
                        }
                    }
                }
                _ => panic!("{}: value of the wrong kind for {key}", command.id),
            }
        }
        argv
    }

    /// What clap recorded from the command line, by argument id.
    #[derive(Debug, PartialEq)]
    enum Bound {
        On,
        Values(Vec<String>),
    }

    /// The command path a run starts with.
    fn path(command: &SchemaCommand) -> Vec<&str> {
        command.run.split_whitespace().take_while(|word| !word.starts_with('{') && *word != "--").collect()
    }

    /// The built leaf for `command`: built, so its positionals carry their indices, which
    /// gives an expectation that does not depend on the walker's own layout.
    fn leaf<'a>(built: &'a Command, command: &SchemaCommand) -> &'a Command {
        path(command).iter().fold(built, |at, name| at.find_subcommand(name).expect(name))
    }

    /// The arguments a command line can set on `leaf`: not the globals and clap's own.
    fn own_args(leaf: &Command) -> impl Iterator<Item = &Arg> {
        leaf.get_arguments()
            .filter(|arg| !arg.is_global_set())
            .filter(|arg| !matches!(arg.get_id().as_str(), "help" | "version"))
    }

    fn positionals(leaf: &Command) -> Vec<&Arg> {
        let mut positionals: Vec<&Arg> = leaf.get_positionals().collect();
        positionals.sort_by_key(|arg| arg.get_index());
        positionals
    }

    fn variadic(arg: &Arg) -> bool {
        arg.get_num_args().is_some_and(|range| range.max_values() > 1)
    }

    /// The `n`th distinct value clap's parser for `arg` accepts: distinct, so a value that
    /// lands in a neighbour's place is seen. A type with no entry here is a decision this test
    /// forces: say what a valid and a hostile value are for it.
    fn valid_value(command: &SchemaCommand, arg: &Arg, n: usize) -> String {
        let parser = arg.get_value_parser().type_id();
        if parser == TypeId::of::<String>() {
            format!("word{n}")
        } else if parser == TypeId::of::<usize>() {
            (7 + n).to_string()
        } else {
            panic!("{}: argument `{}` has a value type this test does not know", command.id, arg.get_id())
        }
    }

    fn is_string(arg: &Arg) -> bool {
        arg.get_value_parser().type_id() == TypeId::of::<String>()
    }

    /// The positional words `values` render, in order: each input's text and each arg-less
    /// repeat's items, as the document lists them.
    fn positional_words(command: &SchemaCommand, values: &BTreeMap<String, Value>) -> Vec<String> {
        let mut words = Vec::new();
        for (key, param) in &command.params.0 {
            match (&param.kind, values.get(key)) {
                (Kind::Input(_), Some(Value::Text(text))) => words.push(text.clone()),
                (Kind::Repeat(Repeat { arg: None, .. }), Some(Value::List(items))) => words.extend(items.iter().cloned()),
                _ => {}
            }
        }
        words
    }

    /// What clap must record for `values`, derived from the built leaf alone: positional words
    /// fill the positionals by index, a variadic taking the rest; option values and switches
    /// bind to the argument with that long name.
    fn expectation(leaf: &Command, command: &SchemaCommand, values: &BTreeMap<String, Value>) -> BTreeMap<String, Bound> {
        let mut expected = BTreeMap::new();

        let mut words = positional_words(command, values).into_iter();
        for arg in positionals(leaf) {
            let taken: Vec<String> = if variadic(arg) { words.by_ref().collect() } else { words.next().into_iter().collect() };
            if !taken.is_empty() {
                expected.insert(arg.get_id().to_string(), Bound::Values(taken));
            }
        }
        assert!(words.next().is_none(), "{}: more positional words than positionals", command.id);

        for (key, param) in &command.params.0 {
            let long = match &param.kind {
                Kind::Flag(flag) => &flag.arg,
                Kind::Repeat(Repeat { arg: Some(arg), .. }) => arg,
                _ => continue,
            };
            let arg = own_args(leaf).find(|arg| arg.get_long() == long.strip_prefix("--")).expect(long);
            match values.get(key) {
                Some(Value::Switch(true)) => {
                    expected.insert(arg.get_id().to_string(), Bound::On);
                }
                Some(Value::List(items)) if !items.is_empty() => {
                    expected.insert(arg.get_id().to_string(), Bound::Values(items.clone()));
                }
                _ => {}
            }
        }
        expected
    }

    /// Parse `values` rendered for `command` the way `main` does. `Ok` means clap accepted the
    /// line and every value landed exactly where expected (anything else panics); `Err` is
    /// clap's refusal.
    fn parse(built: &Command, command: &SchemaCommand, values: &BTreeMap<String, Value>) -> Result<(), clap::Error> {
        parse_as(built, command, values, Form::Tool)
    }

    /// `parse`, with the option items written in `form`.
    fn parse_as(built: &Command, command: &SchemaCommand, values: &BTreeMap<String, Value>, form: Form) -> Result<(), clap::Error> {
        let argv = render(command, values, form);
        let mut matches = built.clone().try_get_matches_from(&argv)?;
        Cli::from_arg_matches_mut(&mut matches.clone())?;

        let at = &mut matches;
        for name in path(command) {
            let (got, sub) = at.remove_subcommand().unwrap_or_else(|| panic!("{argv:?}: no subcommand {name}"));
            assert_eq!(got, name, "{argv:?}");
            *at = sub;
        }
        assert!(at.subcommand_name().is_none(), "{argv:?}: ran a deeper command");

        let leaf = leaf(built, command);
        let mut bound = BTreeMap::new();
        for arg in own_args(leaf) {
            let id = arg.get_id().as_str();
            if at.value_source(id) != Some(ValueSource::CommandLine) {
                continue;
            }
            let value = if matches!(arg.get_action(), ArgAction::SetTrue) {
                Bound::On
            } else {
                let raw = at.get_raw(id).expect(id);
                Bound::Values(raw.map(|value| value.to_string_lossy().into_owned()).collect())
            };
            bound.insert(id.to_string(), value);
        }

        // No run carries a global, so a value that spells one (`--json`) must not set it.
        for global in built.get_arguments().filter(|arg| arg.is_global_set()) {
            let id = global.get_id().as_str();
            let long = global.get_long().map(|long| format!("--{long}"));
            assert!(
                !command.run.split_whitespace().any(|word| Some(word) == long.as_deref()),
                "{}: the run carries the global {id}", command.id
            );
            assert_ne!(at.value_source(id), Some(ValueSource::CommandLine), "{argv:?}: set the global {id}");
        }
        assert_eq!(bound, expectation(leaf, command, values), "{argv:?}");
        Ok(())
    }

    /// The values every scenario starts from: required params filled, and every positional
    /// slot filled with a plain value, since an adversarial value must have a place to land.
    fn base(leaf: &Command, command: &SchemaCommand) -> BTreeMap<String, Value> {
        let slots = positionals(leaf);
        let mut slot = 0;
        let mut values = BTreeMap::new();
        for (key, param) in &command.params.0 {
            match &param.kind {
                Kind::Input(_) => {
                    values.insert(key.clone(), Value::Text(valid_value(command, slots[slot], slot)));
                    slot += 1;
                }
                Kind::Repeat(Repeat { arg: None, max, .. }) => {
                    let count = max.unwrap_or(2) as usize;
                    let items = (0..count).map(|offset| {
                        let arg = slots[(slot + offset).min(slots.len() - 1)];
                        valid_value(command, arg, slot + offset)
                    }).collect();
                    values.insert(key.clone(), Value::List(items));
                    slot += count;
                }
                Kind::Repeat(Repeat { arg: Some(long), min, .. }) if *min > 0 => {
                    let arg = own_args(leaf).find(|arg| arg.get_long() == long.strip_prefix("--")).expect(long);
                    values.insert(key.clone(), Value::List(vec![valid_value(command, arg, 0)]));
                }
                _ => {}
            }
        }
        values
    }

    /// The base `key = value` lands on: `base` itself, or, when clap refuses the pair as a
    /// conflict (`peek --inventory` with a hash), `base` with
    /// optional positional words dropped from the end until it parses. Whatever base is
    /// returned, `value` was parsed on it and landed; any other refusal fails the test.
    fn fit(built: &Command, command: &SchemaCommand, base: &BTreeMap<String, Value>, key: &str, value: Value) -> BTreeMap<String, Value> {
        let mut base = base.clone();
        loop {
            let mut values = base.clone();
            values.insert(key.to_string(), value.clone());
            let error = match parse(built, command, &values) {
                Ok(()) => return base,
                Err(error) => error,
            };
            let optional = command.params.0.iter().rev().find_map(|(name, param)| match &param.kind {
                Kind::Repeat(Repeat { arg: None, min: 0, .. }) => Some(name),
                _ => None,
            });
            let dropped = match optional.and_then(|name| base.get_mut(name)) {
                Some(Value::List(items)) => items.pop().is_some(),
                _ => false,
            };
            if error.kind() != clap::error::ErrorKind::ArgumentConflict || !dropped {
                panic!("{}: {key}: {error}", command.id);
            }
        }
    }

    /// Each param's key names the clap argument(s) its values land in, judged from the built
    /// leaf: an option or switch by its long name; positional params, in order, by the ids of
    /// the positionals they cover (`_` as `-`, a collapsed run joined with `-`).
    fn assert_keys(leaf: &Command, command: &SchemaCommand) {
        let positionals = positionals(leaf);
        let mut slot = 0;
        for (key, param) in &command.params.0 {
            let covered = match &param.kind {
                Kind::Flag(Flag { arg }) | Kind::Repeat(Repeat { arg: Some(arg), .. }) => {
                    assert_eq!(arg.strip_prefix("--"), Some(key.as_str()), "{}", command.id);
                    continue;
                }
                Kind::Input(_) => 1,
                Kind::Repeat(Repeat { arg: None, max: Some(max), .. }) => *max as usize,
                Kind::Repeat(Repeat { arg: None, max: None, .. }) => positionals.len() - slot,
            };
            let ids: Vec<String> = positionals[slot..slot + covered].iter()
                .map(|arg| arg.get_id().as_str().replace('_', "-"))
                .collect();
            assert_eq!(*key, ids.join("-"), "{}: a positional param named for the wrong argument", command.id);
            slot += covered;
        }
        assert_eq!(slot, positionals.len(), "{}: positionals without a param", command.id);
    }

    /// The positional arg each word of `base`'s positional values lands in.
    fn slot_args<'a>(leaf: &'a Command, count: usize) -> Vec<&'a Arg> {
        let positionals = positionals(leaf);
        (0..count).map(|index| positionals[index.min(positionals.len() - 1)]).collect()
    }

    /// Replace the `index`th positional word of `values` with `word`.
    fn with_positional(command: &SchemaCommand, values: &BTreeMap<String, Value>, index: usize, word: &str) -> BTreeMap<String, Value> {
        let mut values = values.clone();
        let mut at = 0;
        for (key, param) in &command.params.0 {
            match (&param.kind, values.get_mut(key)) {
                (Kind::Input(_), Some(Value::Text(text))) => {
                    if at == index {
                        *text = word.to_string();
                    }
                    at += 1;
                }
                (Kind::Repeat(Repeat { arg: None, .. }), Some(Value::List(items))) => {
                    for item in items {
                        if at == index {
                            *item = word.to_string();
                        }
                        at += 1;
                    }
                }
                _ => {}
            }
        }
        values
    }

    #[test]
    fn every_run_binds_each_value_where_it_was_meant() {
        let mut built = Cli::command();
        built.build();
        let doc = forklift();
        // Dash-led option values checked, so no claim below passes by having nothing to check.
        let (mut bound_alone, mut bound_beside_plain, mut refused_by_type) = (0, 0, 0);

        for command in &doc.commands {
            let leaf = leaf(&built, command);
            assert_keys(leaf, command);
            let base = base(leaf, command);
            parse(&built, command, &base).unwrap_or_else(|error| panic!("{}: base: {error}", command.id));

            // The words a value could be mistaken for: this command's own options and switches,
            // the globals, an unknown short flag, the empty word and the terminator itself.
            let mut hostile: Vec<String> = own_args(leaf).filter_map(|arg| arg.get_long()).map(|long| format!("--{long}")).collect();
            hostile.extend(["--json", "--verbose", "--no-pager", "-x", "--"].map(String::from));

            // A positional value after `--` lands verbatim, whatever it spells.
            let words = positional_words(command, &base).len();
            for (index, arg) in slot_args(leaf, words).into_iter().enumerate() {
                if !is_string(arg) {
                    continue;
                }
                for word in hostile.iter().map(String::as_str).chain([""]) {
                    let values = with_positional(command, &base, index, word);
                    parse(&built, command, &values)
                        .unwrap_or_else(|error| panic!("{}: positional {word:?}: {error}", command.id));
                }
            }

            for (key, param) in &command.params.0 {
                match &param.kind {
                    Kind::Flag(_) => {
                        fit(&built, command, &base, key, Value::Switch(true));
                    }
                    Kind::Repeat(Repeat { arg: Some(long), max, .. }) => {
                        let arg = own_args(leaf).find(|arg| arg.get_long() == long.strip_prefix("--")).expect(long);
                        let valid = valid_value(command, arg, 0);

                        // Checked on a base it does not conflict with, so a conflict cannot
                        // stand in for the refusal of a hostile value.
                        let base = fit(&built, command, &base, key, Value::List(vec![valid.clone()]));
                        let mut good = vec![vec![valid.clone()]];
                        if is_string(arg) {
                            good.push(vec![String::new()]);
                            good.push(vec!["-".to_string()]);
                        }
                        if max.is_none() {
                            good.push(vec![valid.clone(), valid_value(command, arg, 1)]);
                        }
                        // In either form: `--scope -` and `--scope=-` both bind the bare `-`.
                        for items in good {
                            let mut values = base.clone();
                            values.insert(key.clone(), Value::List(items.clone()));
                            for form in [Form::Tool, Form::Split] {
                                parse_as(&built, command, &values, form)
                                    .unwrap_or_else(|error| panic!("{}: --{key} {items:?}: {error}", command.id));
                            }
                        }

                        for word in &hostile {
                            let mut lists = vec![vec![word.clone()]];
                            if max.is_none() {
                                lists.push(vec![valid.clone(), word.clone()]);
                            }
                            for items in lists {
                                let mut values = base.clone();
                                values.insert(key.clone(), Value::List(items.clone()));

                                // Written as two words, a value that spells an option is refused
                                // loudly, never bound elsewhere.
                                // The refusal is the word's: `good` parsed this same base, split,
                                // with a plain value where the word stands. Which argument clap
                                // blames varies (`--` ends the options, so a neighbour is).
                                assert!(
                                    parse_as(&built, command, &values, Form::Split).is_err(),
                                    "{}: --{key} {items:?} split in two was accepted", command.id
                                );

                                // Riding inside its option, it binds there verbatim; where the
                                // type cannot hold it, clap refuses it as this option's value,
                                // never reads it as a flag.
                                let result = parse(&built, command, &values);
                                if !is_string(arg) {
                                    let error = result.err()
                                        .unwrap_or_else(|| panic!("{}: --{key} {items:?} was accepted", command.id));
                                    let context = |kind| match error.get(kind) {
                                        Some(ContextValue::String(text)) => text.clone(),
                                        _ => String::new(),
                                    };
                                    assert_eq!(error.kind(), ErrorKind::ValueValidation, "{}: --{key} {items:?}: {error}", command.id);
                                    assert!(context(ContextKind::InvalidArg).starts_with(&format!("--{key} ")), "{}: {error}", command.id);
                                    assert_eq!(context(ContextKind::InvalidValue), *word, "{}: {error}", command.id);
                                    refused_by_type += 1;
                                    continue;
                                }
                                result.unwrap_or_else(|error| panic!("{}: --{key} {items:?}: {error}", command.id));
                                if items.len() == 1 {
                                    bound_alone += 1;
                                } else {
                                    bound_beside_plain += 1;
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        assert!(bound_alone > 0, "no dash-led value was bound alone");
        assert!(bound_beside_plain > 0, "no dash-led value was bound beside a plain one");
        assert!(refused_by_type > 0, "no dash-led value was refused by its option's type");
    }

    // (d) Credentials: classified by the tables, and the tables are checked against the tree.

    #[test]
    fn credentials_are_marked_and_unclassified_ones_refused() {
        let error = describe(&probe("creds"), &NO_TABLES).err().expect("an unclassified credential is refused");
        assert!(error.contains("`api_token`") && error.contains("credential"), "{error}");

        const CLASSIFIED: Tables = Tables { excluded: &[], secret: &[("creds", "api_token")], not_secret: &[] };
        let doc = describe(&probe("creds"), &CLASSIFIED).unwrap();
        assert!(serde_json::to_string(&doc).unwrap().contains(r#""api-token":{"repeat":{"arg":"--api-token","max":1,"secret":true}}"#));

        // A switch carries no value, so its name is not a credential.
        describe(&probe("switch"), &NO_TABLES).unwrap();

        const BOTH: Tables = Tables {
            excluded: &[],
            secret: &[("creds", "api_token")],
            not_secret: &[("creds", "api_token", "both")],
        };
        assert!(describe(&probe("creds"), &BOTH).err().unwrap().contains("both"));
    }

    // An exclusion names a full command path: it leaves out that command alone, never a
    // sibling, nor another command of the same name elsewhere in the tree.

    #[test]
    fn an_exclusion_is_a_command_path() {
        let root = Command::new("probe").about("Probe")
            .subcommand(Command::new("fam").about("Fam").subcommand_required(true)
                .subcommand(Command::new("keep").about("Keep"))
                .subcommand(Command::new("drop").about("Drop").alias("dr")))
            .subcommand(Command::new("drop").about("Drop at the top"))
            .subcommand(Command::new("secret").about("Hidden").hide(true));
        let ids = |tables: &Tables| -> Vec<String> {
            describe(&root, tables).unwrap().commands.into_iter().map(|command| command.id).collect()
        };

        assert_eq!(ids(&NO_TABLES), ["fam-keep", "fam-drop", "drop"]);
        const NESTED: Tables = Tables { excluded: &[("fam drop", "")], secret: &[], not_secret: &[] };
        assert_eq!(ids(&NESTED), ["fam-keep", "drop"]);
        const TOP: Tables = Tables { excluded: &[("drop", "")], secret: &[], not_secret: &[] };
        assert_eq!(ids(&TOP), ["fam-keep", "fam-drop"]);
        const FAMILY: Tables = Tables { excluded: &[("fam", "")], secret: &[], not_secret: &[] };
        assert_eq!(ids(&FAMILY), ["drop"]);

        const STALE: Tables = Tables { excluded: &[("fam gone", "")], secret: &[], not_secret: &[] };
        assert!(describe(&root, &STALE).err().unwrap().contains("`fam gone`"));
        // The walk matches visible commands by name, so neither an alias nor a hidden command
        // would ever be left out: naming one is as stale as naming nothing.
        const ALIAS: Tables = Tables { excluded: &[("fam dr", "")], secret: &[], not_secret: &[] };
        assert!(describe(&root, &ALIAS).err().unwrap().contains("`fam dr`"));
        const HIDDEN: Tables = Tables { excluded: &[("secret", "")], secret: &[], not_secret: &[] };
        assert!(describe(&root, &HIDDEN).err().unwrap().contains("`secret`"));
    }

    // (e) A table entry that names nothing is an error.

    #[test]
    fn stale_table_entries_are_refused() {
        const EXCLUDED: Tables = Tables { excluded: &[("gone", "")], secret: &[], not_secret: &[] };
        assert!(describe(&probe("plain"), &EXCLUDED).err().unwrap().contains("`gone`"));

        const SECRET: Tables = Tables { excluded: &[], secret: &[("plain", "gone")], not_secret: &[] };
        assert!(describe(&probe("plain"), &SECRET).err().unwrap().contains("`gone`"));

        const NOT_SECRET: Tables = Tables { excluded: &[], secret: &[], not_secret: &[("plain", "gone", "")] };
        assert!(describe(&probe("plain"), &NOT_SECRET).err().unwrap().contains("`gone`"));

        // A secret entry must name a value-taking argument, not a switch.
        const SWITCH: Tables = Tables { excluded: &[], secret: &[("switch", "no_token")], not_secret: &[] };
        assert!(describe(&probe("switch"), &SWITCH).err().unwrap().contains("`no_token`"));
    }
}
