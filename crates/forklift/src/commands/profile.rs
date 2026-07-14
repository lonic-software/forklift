use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::{config_utils, sign_utils, warehouse_utils};
use crate::output::{self, CommandOutput};

// The profile command — named identity bundles in the global configuration
// (`[profile.<name>]` sections holding an operator id and a display name), selected
// per warehouse via `operator.profile`. Profiles keep one machine's identities apart:
// which operator id a warehouse acts under, and which local keys belong to whom (the
// key-owner manifest in the key directory).

/// List the profiles: the default identity (the `operator.*` keys) and every named
/// profile, with the local keys each one holds.
pub fn list() -> Result<(), String> {
    let profiles = config_utils::list_profiles()?;

    let default_identifier = config_utils::get_scoped_value(
        config_utils::KEY_OPERATOR_IDENTIFIER,
        ConfigScope::Global
    )?;

    let default = match &default_identifier {
        Some(identifier) => ProfileEntry {
            name: "default".to_string(),
            identifier: Some(identifier.clone()),
            display_name: None,
            local_keys: sign_utils::keys_owned_by(identifier)?.len(),
        },
        None => ProfileEntry {
            name: "default".to_string(),
            identifier: None,
            display_name: None,
            local_keys: 0,
        },
    };

    let mut named = Vec::new();

    for (name, identity) in &profiles {
        named.push(ProfileEntry {
            name: name.clone(),
            identifier: Some(identity.identifier.clone()),
            display_name: (!identity.name.is_empty()).then(|| identity.name.clone()),
            local_keys: sign_utils::keys_owned_by(&identity.identifier)?.len(),
        });
    }

    output::emit("profile", &ProfileList { default, profiles: named });

    Ok(())
}

/// The profile listing: the default identity and every named profile.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ProfileList {
    default: ProfileEntry,
    profiles: Vec<ProfileEntry>,
}

/// One profile and how many local keys it holds.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ProfileEntry {
    name: String,

    /// The operator id (`null` for the default before any id is minted).
    #[serde(skip_serializing_if = "Option::is_none")]
    identifier: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    display_name: Option<String>,

    local_keys: usize,
}

impl CommandOutput for ProfileList {
    fn render_human(&self) {
        match &self.default.identifier {
            Some(identifier) => println!(
                "default — {} ({} local key(s))", identifier, self.default.local_keys
            ),
            None => println!("default — no identity yet (an id is minted on first use)"),
        }

        for entry in &self.profiles {
            let display = match &entry.display_name {
                Some(name) => format!(" \"{}\"", name),
                None => String::new(),
            };

            println!(
                "{} — {}{} ({} local key(s))",
                entry.name,
                entry.identifier.as_deref().unwrap_or(""),
                display,
                entry.local_keys
            );
        }

        if self.profiles.is_empty() {
            println!();
            println!("No named profiles. Create one with \"forklift profile create <name>\".");
        }
    }
}

/// Create a named profile, minting an operator id unless one is supplied (a hosting
/// provider hands out its own ids).
pub fn create(profile: &str,
              display_name: Option<String>,
              identifier: Option<String>) -> Result<(), String> {
    let identity = config_utils::create_profile(
        profile,
        display_name.as_deref(),
        identifier.as_deref()
    )?;

    output::message("profile", format!(
        "Created profile \"{}\" with operator id {}.\n\
        Select it for a warehouse with \"forklift profile use {}\".",
        profile, identity.identifier, profile
    ));

    Ok(())
}

/// Select a profile for the current warehouse (sets `operator.profile` in the
/// warehouse configuration).
pub fn use_profile(profile: &str) -> Result<(), String> {
    warehouse_utils::enter_warehouse()?;

    if config_utils::get_profile(profile)?.is_none() {
        return Err(format!(
            "The profile \"{}\" does not exist. Create it with \"forklift profile create {}\".",
            profile, profile
        ));
    }

    config_utils::set_value(config_utils::KEY_OPERATOR_PROFILE, profile, ConfigScope::Warehouse)?;

    let identity = config_utils::get_operator()?;

    output::message("profile", format!(
        "This warehouse now acts as profile \"{}\" (operator id {}).",
        profile, identity.identifier
    ));

    Ok(())
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ProfileList", schemars::schema_for!(ProfileList)),
    ]
}
