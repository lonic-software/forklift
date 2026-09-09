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

/// Whether `key` selects which identity the warehouse acts as (`operator.identifier`,
/// `operator.profile` — see the strict-read rule at `config_utils::get_operator`'s doc).
/// `config` and `profile list` are diagnostic commands, so for these two keys they must
/// report a present-but-malformed value honestly instead of conflating it with "not set" —
/// the conflation used to make both commands promise an outcome (an id minted on first
/// use, or a named profile in effect) that a command actually resolving the identity (e.g.
/// `office enroll`) would refuse to deliver (PR #122 round 6, F3). Every other key's
/// "malformed reads as absent" is the real, intended behavior, so reporting it as "not set"
/// there is simply true.
fn is_identity_selecting(key: &str) -> bool {
    key == config_utils::KEY_OPERATOR_IDENTIFIER || key == config_utils::KEY_OPERATOR_PROFILE
}

/// Print the value of the given configuration key to stdout.
///
/// # Arguments
/// * `key`   - The configuration key.
/// * `scope` - The scope to read. For the warehouse scope the *effective* value is
///             printed (the warehouse value, falling back to the global one).
///
/// # Returns
/// * `Ok(())`      - If the value was printed (including a present-but-malformed
///                   identity-selecting key — see [`is_identity_selecting`]; this command
///                   reports the malformed state rather than refusing).
/// * `Err(String)` - If the key is unknown or not set.
fn print_value(key: &str, scope: ConfigScope) -> Result<(), String> {
    if is_identity_selecting(key) {
        let read = match scope {
            ConfigScope::Global => config_utils::get_scoped_value_strict(key, ConfigScope::Global)
                .map(|value| value.map(|value| (value, ConfigScope::Global))),
            ConfigScope::Warehouse => config_utils::get_effective_value_strict(key),
        };

        return match read {
            Ok(Some((value, _))) => {
                output::emit("config", &ConfigValue { key: key.to_string(), value: Some(value), malformed: None });
                Ok(())
            }
            Ok(None) => Err(format!("\"{}\" is not set.", key)),
            Err(error) => {
                output::emit("config", &ConfigValue { key: key.to_string(), value: None, malformed: Some(error) });
                Ok(())
            }
        };
    }

    let value = match scope {
        ConfigScope::Global => config_utils::get_scoped_value(key, ConfigScope::Global)?,
        ConfigScope::Warehouse => config_utils::get_effective_value(key)?.map(|(value, _)| value),
    };

    match value {
        Some(value) => {
            output::emit("config", &ConfigValue { key: key.to_string(), value: Some(value), malformed: None });
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

    /// `None` when the key is present but not a string — see `malformed`. Never `None`
    /// alongside `malformed` also being `None`; an actually-unset key is an `Err`, not
    /// this envelope (see `print_value`).
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<String>,

    /// Set instead of `value` when an identity-selecting key (`operator.identifier`,
    /// `operator.profile`) is present but not a string. `config` is a diagnostic command,
    /// so it reports this rather than refusing — unlike a command that actually resolves
    /// the identity (e.g. `office enroll`), which refuses outright.
    #[serde(skip_serializing_if = "Option::is_none")]
    malformed: Option<String>,
}

impl CommandOutput for ConfigValue {
    fn render_human(&self) {
        match (&self.value, &self.malformed) {
            // Human output is the bare value (scriptable as it always was).
            (Some(value), _) => println!("{}", value),
            (None, Some(error)) => println!("{}", error),
            (None, None) => {}
        }
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
        if is_identity_selecting(key) {
            let read = match scope {
                ConfigScope::Global => config_utils::get_scoped_value_strict(key, ConfigScope::Global)
                    .map(|value| value.map(|value| (value, ConfigScope::Global))),
                ConfigScope::Warehouse => config_utils::get_effective_value_strict(key),
            };

            entries.push(match read {
                Ok(Some((value, source))) => ConfigEntry {
                    key: key.to_string(),
                    value: Some(value),
                    scope: Some(source.to_string()),
                    malformed: None,
                },
                Ok(None) => ConfigEntry { key: key.to_string(), value: None, scope: None, malformed: None },
                Err(error) => ConfigEntry { key: key.to_string(), value: None, scope: None, malformed: Some(error) },
            });
            continue;
        }

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
                malformed: None,
            },
            None => ConfigEntry { key: key.to_string(), value: None, scope: None, malformed: None },
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

    /// Set instead of `value`/`scope` when an identity-selecting key (`operator.identifier`,
    /// `operator.profile`) is present but not a string — see `ConfigValue::malformed`.
    #[serde(skip_serializing_if = "Option::is_none")]
    malformed: Option<String>,
}

impl CommandOutput for ConfigList {
    fn render_human(&self) {
        for entry in &self.entries {
            match (&entry.value, &entry.scope, &entry.malformed) {
                (Some(value), Some(scope), _) => println!("{} = {} ({})", entry.key, value, scope),
                (_, _, Some(error)) => println!("{} — malformed: {}", entry.key, error),
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
