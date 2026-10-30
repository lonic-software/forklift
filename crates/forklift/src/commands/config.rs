use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::{config_utils, warehouse_utils};
use crate::cli::ConfigAction;
use crate::output::{self, CommandOutput};

/// Handle the config family.
/// * `config [list]`            - List the known configuration keys and their values.
/// * `config get <key>`         - Print the value of a key.
/// * `config set <key> <value>` - Set the value of a key.
/// * `config unset <key>`       - Remove a key.
///
/// The warehouse configuration is targeted by default; each subcommand's `--global` flag
/// targets the per-user configuration instead. When reading without `--global`, the warehouse
/// value wins over the global one.
///
/// # Arguments
/// * `action` - The subcommand (`None` lists the effective configuration).
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(action: Option<ConfigAction>) -> Result<(), String> {
    let global = match &action {
        None => false,
        Some(ConfigAction::List { global } | ConfigAction::Get { global, .. }
             | ConfigAction::Set { global, .. } | ConfigAction::Unset { global, .. }) => *global,
    };
    let scope = if global { ConfigScope::Global } else { ConfigScope::Warehouse };

    // Global-only operations must work outside a warehouse (e.g. configuring the operator
    // identity once, before preparing the first warehouse), so the warehouse is only
    // entered when the warehouse configuration is actually involved.
    if scope == ConfigScope::Warehouse {
        warehouse_utils::enter_warehouse()?;
    }

    match action {
        Some(ConfigAction::Set { key, value, .. }) => {
            let stored = config_utils::set_value(&key, &value, scope)?;

            // Setting a value has always been silent in human mode; keep it so, but
            // give `--json` a confirmation envelope.
            //
            // The envelope reports what was *written*, not what was asked for: a fixed-value key
            // is stored normalized, so echoing the argument would answer `"  ON  "` for a file
            // that holds `on` — a disagreement a consumer caching the echo would carry until its
            // next read. `set_value` returns the stored spelling so this cannot be got wrong.
            output::emit(&ConfigSet { key, value: stored });

            Ok(())
        }
        Some(ConfigAction::Unset { key, .. }) => {
            config_utils::unset_value(&key, scope)?;

            output::message(format!("Unset \"{}\".", key));

            Ok(())
        }
        Some(ConfigAction::Get { key, .. }) => print_value(&key, scope),
        Some(ConfigAction::List { .. }) | None => list_configuration(scope),
    }
}

/// A `config set` (human output stays silent).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConfigSet {
    key: String,

    /// The value as written to the file, which is not always the argument given: a key with a
    /// fixed set of values (`remote.tor`) is stored in its documented spelling, so `"  ON  "` is
    /// reported, and stored, as `"on"`. A later `config get` returns this same string.
    value: String,
}

impl CommandOutput for ConfigSet {
    fn render_human(&self) {}
}

/// Print the value of the given configuration key to stdout.
///
/// # Arguments
/// * `key`   - The configuration key.
/// * `scope` - The scope to read. For the warehouse scope the *effective* value is
///             printed (the warehouse value, falling back to the global one).
///
/// # Returns
/// * `Ok(())`      - If the value was printed.
/// * `Err(String)` - If the key is unknown or not set.
fn print_value(key: &str, scope: ConfigScope) -> Result<(), String> {
    let value = match scope {
        ConfigScope::Global => config_utils::get_scoped_value(key, ConfigScope::Global)?,
        ConfigScope::Warehouse => config_utils::get_effective_value(key)?.map(|(value, _)| value),
    };

    match value {
        Some(value) => {
            output::emit(&ConfigValue { key: key.to_string(), value });
            Ok(())
        }
        None => Err(format!("\"{}\" is not set.", key)),
    }
}

/// A `config get` read.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConfigValue {
    key: String,
    value: String,
}

impl CommandOutput for ConfigValue {
    fn render_human(&self) {
        // Human output is the bare value (scriptable as it always was).
        println!("{}", self.value);
    }
}

/// List every known configuration key with its value (and the scope the value comes from).
/// If the operator identity is not fully configured, the instructions for configuring it
/// are printed as well.
///
/// # Arguments
/// * `scope` - The scope to list. For the warehouse scope the effective values are listed.
///
/// # Returns
/// * `Ok(())`      - If the configuration was listed successfully.
/// * `Err(String)` - If a configuration file could not be read or parsed.
fn list_configuration(scope: ConfigScope) -> Result<(), String> {
    let mut entries = Vec::new();

    for key in config_utils::KNOWN_KEYS {
        let value = match scope {
            ConfigScope::Global => config_utils::get_scoped_value(key, ConfigScope::Global)?
                .map(|value| (value, ConfigScope::Global)),
            ConfigScope::Warehouse => config_utils::get_effective_value(key)?,
        };

        entries.push(match value {
            Some((value, source)) => ConfigEntry {
                key: key.to_string(),
                value: Some(value),
                scope: Some(source.to_string()),
            },
            None => ConfigEntry { key: key.to_string(), value: None, scope: None },
        });
    }

    output::emit(&ConfigList { entries });

    Ok(())
}

/// The full configuration listing.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConfigList {
    entries: Vec<ConfigEntry>,
}

/// One known configuration key and its effective value (if set).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConfigEntry {
    key: String,

    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    /// Which scope the value came from (`warehouse` or `global`), when set.
    #[serde(skip_serializing_if = "Option::is_none")]
    scope: Option<String>,
}

impl CommandOutput for ConfigList {
    fn render_human(&self) {
        for entry in &self.entries {
            match (&entry.value, &entry.scope) {
                (Some(value), Some(scope)) => println!("{} = {} ({})", entry.key, value, scope),
                _ => println!("{} (not set)", entry.key),
            }
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ConfigSet>(),
        crate::docgen::OutputSchema::of::<ConfigValue>(),
        crate::docgen::OutputSchema::of::<ConfigList>(),
    ]
}
