use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::{config_utils, office_utils, sign_utils, warehouse_utils};
use forklift_core::util::config_utils::OperatorSource;
use crate::commands::office;
use crate::output::{self, CommandOutput};

// The profile command — named identity bundles in the global configuration
// (`[profile.<name>]` sections holding an operator id), selected
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
            local_keys: sign_utils::keys_owned_by(identifier)?.len(),
        },
        None => ProfileEntry {
            name: "default".to_string(),
            identifier: None,
            local_keys: 0,
        },
    };

    let mut named = Vec::new();

    for (name, identity) in &profiles {
        named.push(ProfileEntry {
            name: name.clone(),
            identifier: Some(identity.identifier.clone()),
            local_keys: sign_utils::keys_owned_by(&identity.identifier)?.len(),
        });
    }

    output::emit(&ProfileList { default, profiles: named });

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
            println!(
                "{} — {} ({} local key(s))",
                entry.name,
                entry.identifier.as_deref().unwrap_or(""),
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
pub fn create(profile: &str, identifier: Option<String>) -> Result<(), String> {
    let identity = config_utils::create_profile(profile, identifier.as_deref())?;

    output::message(format!(
        "Created profile \"{}\" with operator id {}.\n\
        Select it for a warehouse with \"forklift profile use {}\".",
        profile, identity.identifier, profile
    ));

    Ok(())
}

/// Select a profile for the current warehouse (sets `operator.profile` in the
/// warehouse configuration).
pub fn use_profile(profile: &str) -> Result<(), String> {
    // Before the lookup: a hand-written `[profile.""]` would be found, and the empty selection
    // then refused by the configuration write with no word on what to do instead.
    if config_utils::is_blank(profile) {
        return Err(config_utils::EMPTY_PROFILE_NAME.to_string());
    }

    warehouse_utils::enter_warehouse()?;

    if config_utils::get_profile(profile)?.is_none() {
        return Err(format!(
            "The profile \"{}\" does not exist. Create it with \"forklift profile create {}\".",
            profile, profile
        ));
    }

    config_utils::set_value(config_utils::KEY_OPERATOR_PROFILE, profile, ConfigScope::Warehouse)?;

    let identity = config_utils::get_operator()?;

    output::message(format!(
        "This warehouse now acts as profile \"{}\" (operator id {}).",
        profile, identity.identifier
    ));

    Ok(())
}


/// Generate a keypair locally and print the public half plus its proof-of-possession
/// (the CSR of the enrollment flow, §8.6): the new key signs for the operator id
/// itself, so an admission cannot enroll a key the operator does not control — nor
/// attribute a consenting key to someone else.
///
/// Nothing is recorded in a warehouse: the key, its owner entry and a first-use operator id are
/// all per-user. Without a profile name, the identity is the one this directory acts as, so the
/// enclosing warehouse (if any) is entered for its `operator.profile` / `operator.identifier`;
/// the message says which identity that was, since it differs from directory to directory.
///
/// # Arguments
/// * `profile`    - The profile whose operator id the key belongs to, or `None` for the
///                  identity [`config_utils::resolve_operator`] resolves here.
/// * `passphrase` - Whether to protect the key with a passphrase.
pub fn keygen(profile: Option<&str>, passphrase: bool) -> Result<(), String> {
    let (operator, identity) = match profile {
        Some(profile) => {
            if config_utils::is_blank(profile) {
                return Err(config_utils::EMPTY_PROFILE_NAME.to_string());
            }

            (config_utils::get_profile_operator(profile)?, format!("profile \"{}\"", profile))
        }
        None => {
            warehouse_utils::enter_warehouse_if_inside()?;

            let (operator, source) = config_utils::resolve_operator()?;

            let identity = match source {
                OperatorSource::Profile { name, selected_in: ConfigScope::Warehouse } =>
                    format!("profile \"{}\", which this warehouse selects", name),
                OperatorSource::Profile { name, selected_in: ConfigScope::Global } =>
                    format!("profile \"{}\", selected in the global configuration", name),
                OperatorSource::Identifier { set_in: ConfigScope::Warehouse } =>
                    "this warehouse's operator.identifier".to_string(),
                OperatorSource::Identifier { set_in: ConfigScope::Global } =>
                    "the default identity".to_string(),
            };

            (operator, identity)
        }
    };

    let (key_id, public_key) = office::generate_operator_key(&operator.identifier, passphrase)?;
    let pop = office_utils::sign_key_pop(&key_id, &public_key, &operator.identifier)?;

    output::message(format!(
        "Generated a new keypair for {identity}; the private key stays on this machine.\n\
        Key id:      {key_id}\n\
        Operator id: {identifier}\n\n\
        To be admitted, hand an office admin this line:\n\
        \x20 office admit {identifier} {public_key} {pop}\n\n\
        To link this as another device of an identity already enrolled as {identifier}, run\n\
        on a machine that holds one of its keys:\n\
        \x20 office link {public_key} {pop}",
        identity = identity, key_id = key_id, identifier = operator.identifier,
        public_key = public_key, pop = pop
    ));

    Ok(())
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ProfileList>(),
    ]
}
