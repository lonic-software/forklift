use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;
use crate::enums::config_scope::ConfigScope;
use crate::globals::forklift_root;
use crate::model::operator::Operator;
use crate::util::file_utils;

/// The folder inside the forklift root that holds configuration files. The warehouse-level
/// configuration lives directly in it; per-pallet configuration files are planned to live
/// in a `pallets` subfolder (so pallet names can never collide with the warehouse file).
const FOLDER_NAME_CONFIG_ROOT: &str = "config";

/// The name of the warehouse-level configuration file.
const FILE_NAME_WAREHOUSE_CONFIG: &str = "warehouse.toml";

/// The name of the global (per-user) configuration file, located in the user's home
/// directory. It is deliberately *not* kept in a `~/.forklift/` folder: a `.forklift`
/// folder marks a warehouse root, so placing the global configuration there would turn
/// the home directory into a warehouse.
const FILE_NAME_GLOBAL_CONFIG: &str = ".forkliftconfig";

/// Overrides the path of the global configuration file when set. This keeps tests away
/// from the real home directory and lets users relocate their configuration.
const ENV_GLOBAL_CONFIG_PATH: &str = "FORKLIFT_GLOBAL_CONFIG";

/// The display name of the operator. Local only — never written on-chain.
pub const KEY_OPERATOR_NAME: &str = "operator.name";

/// The operator's on-chain id — an opaque string. Minted as a UUID when unset (so
/// chains are pseudonymous by default); a hosting provider supplies its own minted id;
/// a team may set any string, accepting that it is public in every clone, forever.
pub const KEY_OPERATOR_IDENTIFIER: &str = "operator.identifier";

/// The profile the warehouse acts under: a named identity bundle stored as a
/// `[profile.<name>]` section in the global configuration file. When set, the
/// profile's identifier and name take precedence over `operator.*`.
pub const KEY_OPERATOR_PROFILE: &str = "operator.profile";

/// The URL of the warehouse's remote (see `docs/format/REMOTE_PROTOCOL.md`).
pub const KEY_REMOTE_URL: &str = "remote.url";

/// The bearer token for remotes that require one.
pub const KEY_REMOTE_TOKEN: &str = "remote.token";

/// The remote a sparse (scoped) franchise fetched against — its origin. A sparse warehouse
/// only proved its out-of-scope closure present on this remote, so `lift` refuses to publish
/// to a different one (it would fail late at that remote's closure check). Unset for a full
/// franchise, which has the whole closure and can lift anywhere.
pub const KEY_REMOTE_ORIGIN: &str = "remote.origin";

/// Whether background object-store maintenance (auto-compaction) runs after mutating
/// commands. Anything falsey (`false`/`0`/`off`/`no`) turns it off; default is on.
pub const KEY_MAINTENANCE_AUTO: &str = "maintenance.auto";

/// The loose-object count above which background maintenance packs the store (default 6700).
pub const KEY_MAINTENANCE_LOOSE: &str = "maintenance.loose";

/// The pack count above which background maintenance consolidates the packs (default 20).
pub const KEY_MAINTENANCE_PACKS: &str = "maintenance.packs";

/// The configuration keys Forklift understands, in `section.key` form.
/// Setting a key outside this list is rejected (it would silently do nothing).
pub const KNOWN_KEYS: [&str; 9] = [
    KEY_OPERATOR_NAME, KEY_OPERATOR_IDENTIFIER, KEY_OPERATOR_PROFILE, KEY_REMOTE_URL, KEY_REMOTE_TOKEN,
    KEY_REMOTE_ORIGIN, KEY_MAINTENANCE_AUTO, KEY_MAINTENANCE_LOOSE, KEY_MAINTENANCE_PACKS,
];

/// The global-config section that holds the named profiles (`[profile.<name>]`).
const SECTION_PROFILE: &str = "profile";

/// The fields a profile may hold.
const PROFILE_FIELD_IDENTIFIER: &str = "identifier";
const PROFILE_FIELD_NAME: &str = "name";

// The template ends with an (empty) section header on purpose: without any item in the
// document, toml_edit would treat the whole comment block as trailing decor and place
// newly set values *above* it.
const WAREHOUSE_CONFIG_TEMPLATE: &str = r#"# Forklift warehouse configuration.
# Values set here apply to this warehouse only and override the global
# configuration (the ".forkliftconfig" file in your home directory).
#
# Set values with "forklift config <key> <value>", e.g.:
#   forklift config operator.name "Your Name"
#
# operator.identifier is the on-chain operator id. Leave it unset to stay
# pseudonymous (a UUID is minted automatically on first use); a hosting provider
# sets its own. The name is local display data — it is never written into parcels.
# operator.profile selects a named identity from the global configuration instead.

[operator]
"#;

/// Get the path of the configuration folder of the current warehouse.
///
/// # Returns
/// * `PathBuf` - The path of the configuration folder (relative to the warehouse root).
pub fn get_warehouse_config_folder() -> PathBuf {
    forklift_root().join(FOLDER_NAME_CONFIG_ROOT)
}

/// Get the path of the configuration file for the given scope.
///
/// # Arguments
/// * `scope` - The configuration scope.
///
/// # Returns
/// * `Ok(PathBuf)`  - The path of the configuration file (which may not exist yet).
/// * `Err(String)`  - If the home directory could not be determined (global scope only).
pub fn get_config_path(scope: ConfigScope) -> Result<PathBuf, String> {
    match scope {
        ConfigScope::Warehouse => Ok(get_warehouse_config_folder().join(FILE_NAME_WAREHOUSE_CONFIG)),
        ConfigScope::Global => {
            if let Ok(path) = std::env::var(ENV_GLOBAL_CONFIG_PATH) {
                return Ok(PathBuf::from(path));
            }

            std::env::home_dir()
                .filter(|home| !home.as_os_str().is_empty())
                .map(|home| home.join(FILE_NAME_GLOBAL_CONFIG))
                .ok_or("Could not determine the home directory for the global configuration file.".to_string())
        }
    }
}

/// Create the warehouse configuration file (with a commented template) if it does not
/// exist yet. The configuration folder must already exist.
///
/// # Returns
/// * `Ok(true)`    - If the configuration file was created.
/// * `Ok(false)`   - If the configuration file already existed.
/// * `Err(String)` - If an error occurred while creating the file.
pub fn create_warehouse_config_if_not_exists() -> Result<bool, String> {
    let path = get_config_path(ConfigScope::Warehouse)?;

    if path.exists() {
        return Ok(false);
    }

    std::fs::write(&path, WAREHOUSE_CONFIG_TEMPLATE)
        .map_err(|e| format!("Error while creating configuration file \"{}\": {}", path.to_string_lossy(), e))?;

    Ok(true)
}

/// Get the value of a configuration key from the configuration file of the given scope.
///
/// # Arguments
/// * `key`   - The configuration key, in `section.key` form (must be a known key).
/// * `scope` - The configuration scope to read from.
///
/// # Returns
/// * `Ok(Some(String))` - The value of the key.
/// * `Ok(None)`         - If the key (or the configuration file) does not exist.
/// * `Err(String)`      - If the key is unknown, or the file could not be read or parsed.
pub fn get_scoped_value(key: &str, scope: ConfigScope) -> Result<Option<String>, String> {
    let (section, field) = split_key(key)?;
    let path = get_config_path(scope)?;

    let Some(document) = load_document(&path)? else {
        return Ok(None);
    };

    Ok(get_value_from_document(&document, section, field))
}

/// Get the effective value of a configuration key: the warehouse configuration is
/// consulted first, and the global configuration is the fallback.
///
/// # Arguments
/// * `key` - The configuration key, in `section.key` form (must be a known key).
///
/// # Returns
/// * `Ok(Some((String, ConfigScope)))` - The value and the scope it came from.
/// * `Ok(None)`                        - If the key is not set in either scope.
/// * `Err(String)`                     - If the key is unknown, or a file could not be
///                                       read or parsed.
pub fn get_effective_value(key: &str) -> Result<Option<(String, ConfigScope)>, String> {
    for scope in [ConfigScope::Warehouse, ConfigScope::Global] {
        if let Some(value) = get_scoped_value(key, scope)? {
            return Ok(Some((value, scope)));
        }
    }

    Ok(None)
}

/// Set the value of a configuration key in the configuration file of the given scope.
/// The file is created when it does not exist yet; existing content (including comments)
/// is preserved.
///
/// # Arguments
/// * `key`   - The configuration key, in `section.key` form (must be a known key).
/// * `value` - The value to set.
/// * `scope` - The configuration scope to write to.
///
/// # Returns
/// * `Ok(())`      - If the value was set successfully.
/// * `Err(String)` - If the key is unknown, or the file could not be read, parsed
///                   or written.
pub fn set_value(key: &str, value: &str, scope: ConfigScope) -> Result<(), String> {
    let (section, field) = split_key(key)?;
    let path = get_config_path(scope)?;

    let mut document = load_document(&path)?.unwrap_or_default();
    set_value_in_document(&mut document, section, field, value)?;

    if let Some(parent) = path.parent() {
        // The configuration folder may not exist yet in warehouses prepared before this
        // feature was added.
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    std::fs::write(&path, document.to_string())
        .map_err(|e| format!("Error while writing configuration file \"{}\": {}", path.to_string_lossy(), e))
}

/// Remove a configuration key from the configuration file of the given scope (the
/// counterpart of [`set_value`], e.g. to clear a `remote.token`). Existing content —
/// comments, other entries, the now-empty section header — is preserved.
///
/// # Arguments
/// * `key`   - The configuration key, in `section.key` form (must be a known key).
/// * `scope` - The configuration scope to write to.
///
/// # Returns
/// * `Ok(())`      - If the key was removed.
/// * `Err(String)` - If the key is unknown, was not set in that scope, or the file
///                   could not be read, parsed or written.
pub fn unset_value(key: &str, scope: ConfigScope) -> Result<(), String> {
    let (section, field) = split_key(key)?;
    let path = get_config_path(scope)?;

    let Some(mut document) = load_document(&path)? else {
        return Err(format!("\"{}\" is not set.", key));
    };

    if !remove_value_from_document(&mut document, section, field)? {
        return Err(format!("\"{}\" is not set.", key));
    }

    std::fs::write(&path, document.to_string())
        .map_err(|e| format!("Error while writing configuration file \"{}\": {}", path.to_string_lossy(), e))
}

/// Get the operator identity for parcel authorship. Identity is zero-configuration:
/// when no identifier is set, a UUID is minted (chains are pseudonymous by default),
/// and the display name falls back to the identifier.
///
/// Resolution order:
/// 1. `operator.profile` (warehouse overrides global) → the named profile's
///    identifier/name from the global configuration, minting the profile's identifier
///    on first use.
/// 2. `operator.identifier` / `operator.name` (warehouse overrides global), minting a
///    global identifier on first use.
///
/// # Returns
/// * `Ok(Operator)` - The resolved operator.
/// * `Err(String)`  - If a configuration file could not be read or written, or the
///                    selected profile does not exist.
pub fn get_operator() -> Result<Operator, String> {
    if let Some((profile, _)) = get_effective_value(KEY_OPERATOR_PROFILE)? {
        let Some(mut identity) = get_profile(&profile)? else {
            return Err(format!(
                "The selected profile \"{}\" does not exist in the global configuration. \
                Create it with \"forklift profile create {}\".",
                profile, profile
            ));
        };

        if identity.identifier.is_empty() {
            identity.identifier = mint_uuid_v4();
            set_profile_field(&profile, PROFILE_FIELD_IDENTIFIER, &identity.identifier)?;
        }

        if identity.name.is_empty() {
            identity.name = identity.identifier.clone();
        }

        return Ok(identity);
    }

    let identifier = match get_effective_value(KEY_OPERATOR_IDENTIFIER)? {
        Some((identifier, _)) => identifier,
        None => {
            let minted = mint_uuid_v4();
            set_value(KEY_OPERATOR_IDENTIFIER, &minted, ConfigScope::Global)?;
            minted
        }
    };

    let name = get_effective_value(KEY_OPERATOR_NAME)?
        .map(|(name, _)| name)
        .unwrap_or_else(|| identifier.clone());

    Ok(Operator { name, identifier })
}

/// Read a named profile from the global configuration.
///
/// # Arguments
/// * `profile` - The profile name (the `<name>` of a `[profile.<name>]` section).
///
/// # Returns
/// * `Ok(Some(Operator))` - The profile's identity (fields may be empty strings when
///                          unset — `get_operator` fills them in).
/// * `Ok(None)`           - If no such profile exists.
/// * `Err(String)`        - If the global configuration could not be read.
pub fn get_profile(profile: &str) -> Result<Option<Operator>, String> {
    let path = get_config_path(ConfigScope::Global)?;

    let Some(document) = load_document(&path)? else {
        return Ok(None);
    };

    let Some(table) = document.get(SECTION_PROFILE)
        .and_then(|item| item.as_table_like())
        .and_then(|profiles| profiles.get(profile))
        .and_then(|item| item.as_table_like())
    else {
        return Ok(None);
    };

    let field = |name: &str| table.get(name)
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_string();

    Ok(Some(Operator {
        name: field(PROFILE_FIELD_NAME),
        identifier: field(PROFILE_FIELD_IDENTIFIER),
    }))
}

/// List the named profiles in the global configuration (in file order).
///
/// # Returns
/// * `Ok(Vec<(String, Operator)>)` - The profile names and their identities.
/// * `Err(String)`                 - If the global configuration could not be read.
pub fn list_profiles() -> Result<Vec<(String, Operator)>, String> {
    let path = get_config_path(ConfigScope::Global)?;

    let Some(document) = load_document(&path)? else {
        return Ok(Vec::new());
    };

    let Some(profiles) = document.get(SECTION_PROFILE).and_then(|item| item.as_table_like()) else {
        return Ok(Vec::new());
    };

    let mut result = Vec::new();

    for (name, _) in profiles.iter() {
        if let Some(identity) = get_profile(name)? {
            result.push((name.to_string(), identity));
        }
    }

    Ok(result)
}

/// Set one field of a named profile in the global configuration, creating the profile
/// section when it does not exist yet.
///
/// # Arguments
/// * `profile` - The profile name.
/// * `field`   - The field to set (`identifier` or `name`).
/// * `value`   - The value.
///
/// # Returns
/// * `Ok(())`      - If the field was written.
/// * `Err(String)` - If the file could not be read, parsed or written.
pub fn set_profile_field(profile: &str, field: &str, value: &str) -> Result<(), String> {
    let path = get_config_path(ConfigScope::Global)?;
    let mut document = load_document(&path)?.unwrap_or_default();

    let profiles = document.entry(SECTION_PROFILE)
        .or_insert(toml_edit::table())
        .as_table_like_mut()
        .ok_or(format!(
            "\"{}\" in the global configuration file is not a table; please fix the \
            file by hand.",
            SECTION_PROFILE
        ))?;

    if profiles.get(profile).is_none() {
        profiles.insert(profile, toml_edit::table());
    }

    let table = profiles.get_mut(profile)
        .and_then(|item| item.as_table_like_mut())
        .ok_or(format!(
            "\"{}.{}\" in the global configuration file is not a table; please fix \
            the file by hand.",
            SECTION_PROFILE, profile
        ))?;

    table.insert(field, toml_edit::value(value));

    std::fs::write(&path, document.to_string())
        .map_err(|e| format!("Error while writing configuration file \"{}\": {}", path.to_string_lossy(), e))
}

/// Create a named profile: record its display name and identifier, minting an
/// identifier when none is given.
///
/// # Arguments
/// * `profile`    - The profile name.
/// * `name`       - The display name (`None` leaves it to fall back to the identifier).
/// * `identifier` - The on-chain id (`None` mints a UUID).
///
/// # Returns
/// * `Ok(Operator)` - The created identity.
/// * `Err(String)`  - If the profile already exists or a write failed.
pub fn create_profile(profile: &str,
                      name: Option<&str>,
                      identifier: Option<&str>) -> Result<Operator, String> {
    if get_profile(profile)?.is_some() {
        return Err(format!("The profile \"{}\" already exists.", profile));
    }

    let identifier = identifier.map(|id| id.to_string()).unwrap_or_else(mint_uuid_v4);
    set_profile_field(profile, PROFILE_FIELD_IDENTIFIER, &identifier)?;

    if let Some(name) = name {
        set_profile_field(profile, PROFILE_FIELD_NAME, name)?;
    }

    Ok(Operator {
        name: name.unwrap_or(&identifier).to_string(),
        identifier,
    })
}

/// Mint a random version-4 UUID (lowercase hyphenated form) — the default operator id,
/// so chains are pseudonymous unless someone deliberately configures otherwise.
pub fn mint_uuid_v4() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut bytes);

    bytes[6] = (bytes[6] & 0x0f) | 0x40; // version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // RFC 4122 variant

    let hex: String = bytes.iter().map(|byte| format!("{:02x}", byte)).collect();

    format!("{}-{}-{}-{}-{}", &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32])
}

/// Split a configuration key into its section and field parts, rejecting unknown keys
/// (setting an unknown key would silently do nothing, which hides typos).
///
/// # Arguments
/// * `key` - The configuration key, in `section.key` form.
///
/// # Returns
/// * `Ok((&str, &str))` - The section and the field.
/// * `Err(String)`      - If the key is not a known configuration key.
fn split_key(key: &str) -> Result<(&str, &str), String> {
    if !KNOWN_KEYS.contains(&key) {
        return Err(format!(
            "Unknown configuration key \"{}\". Known keys: {}.",
            key,
            KNOWN_KEYS.join(", ")
        ));
    }

    key.split_once('.')
        .ok_or(format!("Configuration key \"{}\" is not in \"section.key\" form.", key))
}

/// Load and parse the configuration file at the given path, if it exists.
///
/// # Arguments
/// * `path` - The path of the configuration file.
///
/// # Returns
/// * `Ok(Some(DocumentMut))` - The parsed configuration file.
/// * `Ok(None)`              - If the file does not exist.
/// * `Err(String)`           - If the file could not be read or parsed.
fn load_document(path: &Path) -> Result<Option<DocumentMut>, String> {
    if !path.exists() {
        return Ok(None);
    }

    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading configuration file \"{}\": {}", path.to_string_lossy(), e))?;

    content.parse::<DocumentMut>()
        .map(Some)
        .map_err(|e| format!("Error while parsing configuration file \"{}\": {}", path.to_string_lossy(), e))
}

/// Get a string value from a parsed configuration document.
/// Values of other types (numbers, tables, …) are treated as unset: every known
/// configuration key holds a string.
///
/// # Arguments
/// * `document` - The parsed configuration document.
/// * `section`  - The section (table) name.
/// * `field`    - The field name inside the section.
///
/// # Returns
/// * `Some(String)` - The value of the field.
/// * `None`         - If the section or field does not exist (or is not a string).
fn get_value_from_document(document: &DocumentMut, section: &str, field: &str) -> Option<String> {
    document.get(section)
        .and_then(|section_item| section_item.as_table_like())
        .and_then(|table| table.get(field))
        .and_then(|field_item| field_item.as_str())
        .map(|value| value.to_string())
}

/// Set a string value in a parsed configuration document, creating the section if needed.
///
/// # Arguments
/// * `document` - The parsed configuration document.
/// * `section`  - The section (table) name.
/// * `field`    - The field name inside the section.
/// * `value`    - The value to set.
///
/// # Returns
/// * `Ok(())`      - If the value was set.
/// * `Err(String)` - If the section exists but is not a table (e.g. `operator = 1`).
fn set_value_in_document(document: &mut DocumentMut,
                         section: &str,
                         field: &str,
                         value: &str) -> Result<(), String> {
    let section_item = document.entry(section).or_insert(toml_edit::table());

    let table = section_item.as_table_like_mut().ok_or(format!(
        "\"{}\" in the configuration file is not a table; please fix the file by hand.",
        section
    ))?;

    table.insert(field, toml_edit::value(value));

    Ok(())
}

/// Remove a field from a document's section, leaving the (possibly now-empty) section
/// and every other entry in place.
///
/// # Arguments
/// * `document` - The parsed configuration document.
/// * `section`  - The section (table) name.
/// * `field`    - The field name inside the section.
///
/// # Returns
/// * `Ok(true)`    - If the field was present and removed.
/// * `Ok(false)`   - If the section or field was not present.
/// * `Err(String)` - If the section exists but is not a table.
fn remove_value_from_document(document: &mut DocumentMut,
                              section: &str,
                              field: &str) -> Result<bool, String> {
    let Some(section_item) = document.get_mut(section) else {
        return Ok(false);
    };

    let table = section_item.as_table_like_mut().ok_or(format!(
        "\"{}\" in the configuration file is not a table; please fix the file by hand.",
        section
    ))?;

    Ok(table.remove(field).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_can_be_set_and_read_back() {
        let mut document = DocumentMut::default();

        set_value_in_document(&mut document, "operator", "name", "Máté").unwrap();

        assert_eq!(get_value_from_document(&document, "operator", "name"), Some("Máté".to_string()));
        assert_eq!(get_value_from_document(&document, "operator", "identifier"), None);
        assert_eq!(get_value_from_document(&document, "missing", "name"), None);
    }

    #[test]
    fn setting_a_value_preserves_comments_and_other_entries() {
        // Only the overwritten value's own inline comment may be lost; everything else
        // (leading comments, comments on untouched lines) must survive the rewrite.
        let original = "# A comment that must survive.\n[operator]\nname = \"Old Name\"\nidentifier = \"old@id\" # untouched comment\n";
        let mut document: DocumentMut = original.parse().unwrap();

        set_value_in_document(&mut document, "operator", "name", "New Name").unwrap();

        let written = document.to_string();
        assert!(written.contains("# A comment that must survive."));
        assert!(written.contains("# untouched comment"));
        assert_eq!(get_value_from_document(&document, "operator", "name"), Some("New Name".to_string()));
        assert_eq!(get_value_from_document(&document, "operator", "identifier"), Some("old@id".to_string()));
    }

    #[test]
    fn removing_a_value_leaves_comments_and_other_entries() {
        let original = "# Keep me.\n[operator]\nname = \"Name\"\nidentifier = \"id\"\n[remote]\ntoken = \"secret\"\n";
        let mut document: DocumentMut = original.parse().unwrap();

        assert!(remove_value_from_document(&mut document, "remote", "token").unwrap());

        // The token is gone; everything else survives.
        assert_eq!(get_value_from_document(&document, "remote", "token"), None);
        let written = document.to_string();
        assert!(written.contains("# Keep me."));
        assert!(!written.contains("secret"));
        assert_eq!(get_value_from_document(&document, "operator", "name"), Some("Name".to_string()));

        // Removing an absent field (or an absent section) reports "not present".
        assert!(!remove_value_from_document(&mut document, "remote", "token").unwrap());
        assert!(!remove_value_from_document(&mut document, "missing", "field").unwrap());
    }

    #[test]
    fn dotted_keys_written_by_hand_are_readable() {
        // Users may write `operator.name = "..."` at the top level instead of using
        // an `[operator]` section; both spellings must be readable.
        let document: DocumentMut = "operator.name = \"Dotted\"\n".parse().unwrap();

        assert_eq!(get_value_from_document(&document, "operator", "name"), Some("Dotted".to_string()));
    }

    #[test]
    fn a_section_that_is_not_a_table_is_reported() {
        let mut document: DocumentMut = "operator = 1\n".parse().unwrap();

        let result = set_value_in_document(&mut document, "operator", "name", "x");
        assert!(result.is_err());
    }

    #[test]
    fn minted_ids_are_canonical_version_4_uuids() {
        let minted = mint_uuid_v4();

        let groups: Vec<&str> = minted.split('-').collect();
        assert_eq!(groups.iter().map(|group| group.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(minted.bytes().all(|b| b == b'-' || b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(minted.as_bytes()[14], b'4');
        assert_ne!(minted, mint_uuid_v4());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let error = split_key("operator.unknown").unwrap_err();
        assert!(error.contains("Unknown configuration key"));
        assert!(error.contains(KEY_OPERATOR_NAME));

        assert_eq!(split_key(KEY_OPERATOR_NAME).unwrap(), ("operator", "name"));
    }
}
