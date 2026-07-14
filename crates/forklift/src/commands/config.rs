use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::{config_utils, warehouse_utils};
use crate::output::{self, CommandOutput};

/// Handle the config command.
/// * `config`                 - List the known configuration keys and their values.
/// * `config <key>`           - Print the value of a key.
/// * `config <key> <value>`   - Set the value of a key.
///
/// The warehouse configuration is targeted by default; the `--global` flag targets the
/// per-user configuration instead. When reading without `--global`, the warehouse value
/// wins over the global one.
///
/// # Arguments
/// * `global` - Whether to target the global (per-user) configuration.
/// * `key`    - The key to read or write (`None` lists the configuration).
/// * `value`  - The value to set the key to (`None` prints the current value).
///
/// # Returns
/// * `Ok(())`      - If the command was handled successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(global: bool,
                      unset: bool,
                      key: Option<String>,
                      value: Option<String>) -> Result<(), String> {
    let scope = if global { ConfigScope::Global } else { ConfigScope::Warehouse };

    // Global-only operations must work outside a warehouse (e.g. configuring the operator
    // identity once, before preparing the first warehouse), so the warehouse is only
    // entered when the warehouse configuration is actually involved.
    if scope == ConfigScope::Warehouse {
        warehouse_utils::enter_warehouse()?;
    }

    if unset {
        let Some(key) = &key else {
            return Err(
                "Specify the key to remove, e.g. \"config --unset remote.token\".".to_string()
            );
        };

        config_utils::unset_value(key, scope)?;

        output::message("config", format!("Unset \"{}\".", key));

        return Ok(());
    }

    match (&key, &value) {
        (Some(key), Some(value)) => {
            config_utils::set_value(key, value, scope)?;

            // Setting a value has always been silent in human mode; keep it so, but
            // give `--json` a confirmation envelope.
            output::emit("config", &ConfigSet { key: key.clone(), value: value.clone() });

            Ok(())
        }
        (Some(key), None) => print_value(key, scope),
        _                 => list_configuration(scope),
    }
}

/// A `config <key> <value>` set (human output stays silent).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ConfigSet {
    key: String,
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
            output::emit("config", &ConfigValue { key: key.to_string(), value });
            Ok(())
        }
        None => Err(format!("\"{}\" is not set.", key)),
    }
}

/// A `config <key>` read.
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

    output::emit("config", &ConfigList { entries });

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
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ConfigSet", schemars::schema_for!(ConfigSet)),
        ("ConfigValue", schemars::schema_for!(ConfigValue)),
        ("ConfigList", schemars::schema_for!(ConfigList)),
    ]
}
