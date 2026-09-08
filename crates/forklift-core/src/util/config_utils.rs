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

/// Whether to reach the remote through a Tor SOCKS proxy — the peer-to-peer transport
/// (DESIGN.html §4.7). `auto` (the default when unset) routes through Tor only when the
/// remote is an onion service (its host ends in `.onion`), so a plain remote is untouched
/// and a `.onion` one Just Works; `on` always routes through Tor (e.g. to reach a clearnet
/// remote anonymously); `off` never does, even for an onion host. See
/// `crate::util::remote_utils::TorMode`.
pub const KEY_REMOTE_TOR: &str = "remote.tor";

/// The values [`KEY_REMOTE_TOR`] accepts at set time. The runtime parse
/// (`remote_utils::TorMode::parse`) is deliberately tolerant — an unrecognized value degrades
/// to `auto` — as defense in depth for hand-edited files; the `config` command is strict so a
/// privacy-relevant typo (`onn` silently meaning `auto`, leaving non-onion remotes un-proxied
/// when the user believes everything is routed through Tor) can't pass silently.
pub const REMOTE_TOR_VALUES: [&str; 3] = ["auto", "on", "off"];

/// The Tor SOCKS proxy the client dials when [`KEY_REMOTE_TOR`] applies (default
/// `socks5h://127.0.0.1:9050`, the address a stock local `tor` listens on). The `socks5h`
/// scheme resolves the hostname *at the proxy*, which is what lets an opaque `.onion` name —
/// which has no DNS record — resolve inside the Tor network rather than failing locally.
pub const KEY_REMOTE_TOR_PROXY: &str = "remote.torProxy";

/// Whether background object-store maintenance (auto-compaction) runs after mutating
/// commands. Anything falsey (`false`/`0`/`off`/`no`) turns it off; default is on.
pub const KEY_MAINTENANCE_AUTO: &str = "maintenance.auto";

/// The loose-object count above which background maintenance packs the store (default 6700).
pub const KEY_MAINTENANCE_LOOSE: &str = "maintenance.loose";

/// The pack count above which background maintenance consolidates the packs (default 20).
pub const KEY_MAINTENANCE_PACKS: &str = "maintenance.packs";

/// The configuration keys Forklift understands, in `section.key` form.
/// Setting a key outside this list is rejected (it would silently do nothing).
pub const KNOWN_KEYS: [&str; 11] = [
    KEY_OPERATOR_NAME, KEY_OPERATOR_IDENTIFIER, KEY_OPERATOR_PROFILE, KEY_REMOTE_URL, KEY_REMOTE_TOKEN,
    KEY_REMOTE_ORIGIN, KEY_REMOTE_TOR, KEY_REMOTE_TOR_PROXY, KEY_MAINTENANCE_AUTO, KEY_MAINTENANCE_LOOSE,
    KEY_MAINTENANCE_PACKS,
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
    validate_value(key, value)?;
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
/// The identifier read (both branches) is strict on a present-but-malformed value — see
/// [`read_profile_field`] and [`get_effective_operator_identifier`] — because leniency there
/// is destructive: an absent identifier reads as "mint one", and minting *writes the fresh
/// UUID back over the file*, so treating a hand-edited, unquoted value as absent would
/// silently sever the operator from the identity their office enrolment knows them by. The
/// name read stays lenient (`get_effective_value`): nothing is ever written back for it, a
/// malformed name only ever falls back to the identifier, exactly as an absent one does (PR
/// #122 round 5 — round 4 had made every known key strict this way, which broke two
/// legitimate uses of "malformed reads as absent" elsewhere: `TorSettings` falling through a
/// malformed warehouse `remote.tor` to a valid global one, and a malformed `maintenance.*`
/// threshold falling back to its default instead of disabling maintenance forever).
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

    let identifier = match get_effective_operator_identifier()? {
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

/// Read one field of a named profile, strictly on a **present** key.
///
/// An absent field is a defined shape the callers rely on — [`get_operator`] mints an
/// identifier when it is empty and falls back to the identifier for an empty name — but a
/// present field that is not a string must not collapse into that same empty case. It used
/// to: an `identifier = 12345` written unquoted by hand read back as `""`, and
/// [`get_operator`] then minted a fresh UUID and *wrote it back over the profile*, silently
/// severing the operator from the identity their office enrolment knows them by. Damaged
/// config now says so rather than being quietly repaired into a different person. Same rule
/// the record parsers follow (`office_utils::read_optional_string`), for the same reason.
fn read_profile_field(
    table: &dyn toml_edit::TableLike,
    profile: &str,
    name: &str,
    path: &Path,
) -> Result<String, String> {
    match table.get(name) {
        None => Ok(String::new()),
        Some(item) => item.as_str()
            .map(|value| value.to_string())
            .ok_or_else(|| {
                // `identifier` empties into a mint; `name` empties into the identifier
                // fallback (`get_operator`) — nothing is generated for it, so the hint
                // must not claim it is.
                let hint = if name == PROFILE_FIELD_IDENTIFIER {
                    "Quote it, or remove it to have one generated."
                } else {
                    "Quote it, or remove it to fall back to the identifier."
                };

                format!(
                    "The profile \"{}\" has a \"{}\" that is not a string, in {}. {}",
                    profile, name, path.display(), hint
                )
            }),
    }
}

/// Build a profile's identity from its already-located `[profile.<name>]` item — shared by
/// [`get_profile`] (which still has to locate the table itself) and [`list_profiles`] (which
/// already has the whole `profiles` table parsed and must not re-open and re-parse the global
/// configuration file once per entry, see FORK-81 follow-up PR #122 round 5 F7).
///
/// # Returns
/// * `Ok(Some(Operator))` - The profile's identity (fields may be empty strings when
///                          unset — `get_operator` fills them in).
/// * `Ok(None)`           - If `item` is absent, or present but not a table.
/// * `Err(String)`        - If a present field is not a string.
fn profile_from_item(item: Option<&toml_edit::Item>,
                     profile: &str,
                     path: &Path) -> Result<Option<Operator>, String> {
    let Some(table) = item.and_then(|item| item.as_table_like()) else {
        return Ok(None);
    };

    Ok(Some(Operator {
        name: read_profile_field(table, profile, PROFILE_FIELD_NAME, path)?,
        identifier: read_profile_field(table, profile, PROFILE_FIELD_IDENTIFIER, path)?,
    }))
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

    let Some(profiles) = document.get(SECTION_PROFILE).and_then(|item| item.as_table_like()) else {
        return Ok(None);
    };

    profile_from_item(profiles.get(profile), profile, &path)
}

/// List the named profiles in the global configuration (in file order).
///
/// This is the command that exists to tell a user which profile is broken, so it must
/// not itself refuse over one bad entry: a malformed profile is reported per-entry
/// (`Err`) alongside the good ones (`Ok`), rather than aborting the whole listing the
/// way propagating an error with `?` would. `get_operator` and `create_profile` keep
/// refusing outright — only this diagnostic path is tolerant.
///
/// A profile section that is not a table at all (e.g. `profile.old = 5`, as opposed to
/// `[profile.old]` with a malformed field inside it) is diagnosable here too:
/// [`profile_from_item`] reads that shape as `Ok(None)` — correct for `get_profile`'s other
/// caller, `create_profile`, which must treat it as "free to use" — but iterating this table
/// already proves the name is present, so `None` here can only mean "not a table", never
/// "does not exist" (unlike `get_profile`, which re-opens the file on every call, this loop
/// shares one already-parsed snapshot with every entry, so there is no window for a concurrent
/// rewrite to change that). Reporting it used to mean silently dropping the entry, which let
/// `profile use old` claim it "does not exist" — the opposite of what this command exists
/// for. `profile create old` was never at risk the same way: `set_profile_field` already
/// refuses to write through a non-table `profile.old`, naming it "is not a table; please fix
/// the file by hand", rather than creating a second, shadowing `[profile.old]` table past it.
///
/// # Returns
/// * `Ok(Vec<(String, Result<Operator, String>)>)` - The profile names in file order,
///   each paired with its identity or, for a profile with a present-but-malformed
///   field (or a section that is not a table at all), the error naming the profile.
/// * `Err(String)`                                  - If the global configuration
///   itself could not be read.
pub fn list_profiles() -> Result<Vec<(String, Result<Operator, String>)>, String> {
    let path = get_config_path(ConfigScope::Global)?;

    let Some(document) = load_document(&path)? else {
        return Ok(Vec::new());
    };

    let Some(profiles) = document.get(SECTION_PROFILE).and_then(|item| item.as_table_like()) else {
        return Ok(Vec::new());
    };

    let mut result = Vec::new();

    for (name, item) in profiles.iter() {
        match profile_from_item(Some(item), name, &path) {
            Ok(Some(identity)) => result.push((name.to_string(), Ok(identity))),
            // `name` is a key of `profiles`, so this can only mean "not a table".
            Ok(None) => result.push((name.to_string(), Err(format!(
                "The profile \"{}\" is not a table (e.g. \"{}.{} = ...\" instead of \"[{}.{}]\"), \
                in {}. Fix it by hand, or remove it.",
                name, SECTION_PROFILE, name, SECTION_PROFILE, name, path.display()
            )))),
            Err(error) => result.push((name.to_string(), Err(error))),
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

/// Reject an out-of-range value for a key whose value is a fixed set, so a typo fails loudly at
/// set time instead of degrading silently at runtime. Keys with a free-form value pass through.
fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if key == KEY_REMOTE_TOR && !REMOTE_TOR_VALUES.contains(&value.trim().to_ascii_lowercase().as_str()) {
        return Err(format!(
            "\"{}\" is not a valid value for {}. Use one of: {}. \
             A typo here would silently fall back to \"auto\", leaving non-onion remotes un-proxied.",
            value, KEY_REMOTE_TOR, REMOTE_TOR_VALUES.join(", ")
        ));
    }

    Ok(())
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

/// Get a string value from a parsed configuration document. Values of other types (numbers,
/// tables, …) are treated as unset, exactly like an absent field — deliberately lenient (PR
/// #122 round 5): this is the general reader every known key goes through
/// (`get_scoped_value` → `get_effective_value`), and for most of them "malformed reads as
/// absent" is the behavior callers actually need, not an oversight:
///   - `remote.tor`/`remote.torProxy`: `TorSettings::from_config` consults the warehouse
///     scope, then falls back to the global one. A malformed warehouse-scope value must read
///     as absent so that fallback still reaches a perfectly valid global value — erroring
///     there instead (as a stricter general reader once did) stops the scope walk on the
///     first bad entry and *masks* the good one, degrading to `TorMode::Auto` and un-proxying
///     a remote the user configured to always route through Tor.
///   - `maintenance.loose`/`maintenance.packs`/`maintenance.auto`: a malformed value must
///     fall back to the built-in default and let maintenance keep running, not propagate an
///     `Err` that `maintenance.rs` swallows into "maintenance is off", silently and
///     permanently, with no message on any command.
/// The one read where this leniency is actively harmful — the operator identifier, where
/// "reads as absent" is also "mint a fresh one and write it back over the hand-edited value"
/// — does not go through this reader; see [`get_value_from_document_strict`] and
/// [`get_effective_operator_identifier`].
///
/// # Arguments
/// * `document` - The parsed configuration document.
/// * `section`  - The section (table) name.
/// * `field`    - The field name inside the section.
///
/// # Returns
/// * `Some(String)` - The value of the field.
/// * `None`         - If the section or field does not exist, the section is not a table, or
///                    the field is present but not a string.
fn get_value_from_document(document: &DocumentMut, section: &str, field: &str) -> Option<String> {
    document.get(section)
        .and_then(|section_item| section_item.as_table_like())
        .and_then(|table| table.get(field))
        .and_then(|field_item| field_item.as_str())
        .map(|value| value.to_string())
}

/// [`get_value_from_document`]'s strict counterpart: a present-but-non-string field is an
/// error rather than being read as unset. Used only to resolve the operator identifier (see
/// [`get_effective_operator_identifier`], [`get_operator`]) — the one read where degrading a
/// malformed value into "unset" is destructive, not merely surprising. Every other known key
/// goes through the lenient reader instead; see its doc comment for why leniency there is
/// load-bearing.
///
/// # Arguments
/// * `document` - The parsed configuration document.
/// * `section`  - The section (table) name.
/// * `field`    - The field name inside the section.
/// * `path`     - The file the document was read from (named in the error).
///
/// # Returns
/// * `Ok(Some(String))` - The value of the field.
/// * `Ok(None)`         - If the section or field does not exist (or the section is not a
///                        table).
/// * `Err(String)`      - If the field is present but not a string.
fn get_value_from_document_strict(document: &DocumentMut,
                                  section: &str,
                                  field: &str,
                                  path: &Path) -> Result<Option<String>, String> {
    let Some(table) = document.get(section).and_then(|section_item| section_item.as_table_like()) else {
        return Ok(None);
    };

    match table.get(field) {
        None => Ok(None),
        Some(field_item) => field_item.as_str()
            .map(|value| Some(value.to_string()))
            .ok_or_else(|| format!(
                "\"{}.{}\" is present but is not a string, in {}. Quote it, or remove it to leave it unset.",
                section, field, path.display()
            )),
    }
}

/// Get the effective operator identifier, strictly: like [`get_effective_value`] the
/// warehouse scope is consulted first and the global scope is the fallback, but a
/// present-but-non-string value in *either* scope refuses immediately instead of falling
/// through to the next scope. Falling through would still be wrong here, unlike the
/// legitimate Tor/maintenance fallback [`get_value_from_document`]'s doc comment describes:
/// it would silently resolve to a *different* value than the one actually configured — the
/// other scope's, or (once the caller sees `None`) a freshly minted one — instead of naming
/// the damage.
///
/// # Returns
/// * `Ok(Some((String, ConfigScope)))` - The identifier and the scope it came from.
/// * `Ok(None)`                        - If `operator.identifier` is not set in either scope.
/// * `Err(String)`                     - If a configuration file could not be read or
///                                       parsed, or `operator.identifier` is present in some
///                                       scope but not a string.
fn get_effective_operator_identifier() -> Result<Option<(String, ConfigScope)>, String> {
    let (section, field) = split_key(KEY_OPERATOR_IDENTIFIER)?;

    for scope in [ConfigScope::Warehouse, ConfigScope::Global] {
        let path = get_config_path(scope)?;

        let Some(document) = load_document(&path)? else {
            continue;
        };

        if let Some(value) = get_value_from_document_strict(&document, section, field, &path)? {
            return Ok(Some((value, scope)));
        }
    }

    Ok(None)
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
    fn a_present_but_non_string_profile_field_errors_instead_of_reading_as_empty() {
        // The silent path this replaced: `identifier = 12345` (unquoted by hand) read back as
        // "", and `get_operator` treats an empty identifier as unset — minting a fresh UUID and
        // writing it back over the profile, so the operator quietly becomes a different person
        // than their office enrolment knows. An absent field must still be the empty default,
        // because that is the case the minting exists to serve.
        let document: DocumentMut =
            "[profile.work]\nidentifier = 12345\n".parse().unwrap();
        let table = document.get("profile").unwrap()
            .as_table_like().unwrap()
            .get("work").unwrap()
            .as_table_like().unwrap();

        let error = match read_profile_field(table, "work", "identifier", Path::new("/cfg")) {
            Err(error) => error,
            Ok(value) => panic!(
                "a present, non-string identifier must error rather than read as {:?}", value
            ),
        };

        assert!(error.contains("identifier"), "the error must name the field: {}", error);
        assert!(error.contains("work"), "the error must name the profile: {}", error);

        // The absent case is unchanged — this is what keeps minting working.
        assert_eq!(
            read_profile_field(table, "work", "name", Path::new("/cfg")).unwrap(),
            "",
            "an absent field must still read as empty, or a profile without a name would refuse"
        );
    }

    #[test]
    fn a_present_string_profile_field_reads_back_verbatim() {
        let document: DocumentMut =
            "[profile.work]\nidentifier = \"someone@example.com\"\n".parse().unwrap();
        let table = document.get("profile").unwrap()
            .as_table_like().unwrap()
            .get("work").unwrap()
            .as_table_like().unwrap();

        assert_eq!(
            read_profile_field(table, "work", "identifier", Path::new("/cfg")).unwrap(),
            "someone@example.com"
        );
    }

    #[test]
    fn a_present_but_non_string_value_reads_as_unset_on_the_general_reader() {
        // PR #122 round 5: round 4 made this reader strict generally (matching the assertion
        // this test used to make), but that broke two legitimate callers of "malformed reads
        // as absent" — `TorSettings::from_config` falling through a malformed warehouse
        // `remote.tor` to a valid global one, and a malformed `maintenance.*` threshold
        // falling back to its default instead of disabling maintenance forever. Reverted to
        // lenient; strictness is now scoped to the operator identifier alone, which is the
        // one read where "absent" also means "mint a fresh one and write it back over the
        // hand-edited value" — see `a_present_but_non_string_value_errors_on_the_strict_reader`.
        let document: DocumentMut = "[operator]\nidentifier = 12345\n".parse().unwrap();

        assert_eq!(get_value_from_document(&document, "operator", "identifier"), None);
        // The absent case reads the same way.
        assert_eq!(get_value_from_document(&document, "operator", "name"), None);
    }

    #[test]
    fn a_present_but_non_string_value_errors_on_the_strict_reader() {
        // The strict counterpart used only for the operator identifier: this is exactly the
        // hazard `read_profile_field`'s fix closes for a named profile's own fields, on the
        // non-profile branch — `operator.identifier = 12345` must not read as `None`, because
        // `get_operator` treats an absent identifier as "mint one", silently overwriting the
        // hand-written value.
        let document: DocumentMut = "[operator]\nidentifier = 12345\n".parse().unwrap();

        let error = match get_value_from_document_strict(&document, "operator", "identifier", Path::new("/cfg")) {
            Err(error) => error,
            Ok(value) => panic!(
                "a present, non-string value must error rather than read as {:?}", value
            ),
        };

        assert!(error.contains("operator.identifier"), "the error must name the key: {}", error);
        assert!(error.contains("/cfg"), "the error must name the file: {}", error);

        // The absent case is unchanged.
        assert_eq!(
            get_value_from_document_strict(&document, "operator", "name", Path::new("/cfg")).unwrap(),
            None
        );
    }

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

    #[test]
    fn remote_tor_rejects_a_typo_but_accepts_the_documented_values() {
        // The privacy-relevant case: a typo must fail loudly, not degrade to `auto`.
        let error = validate_value(KEY_REMOTE_TOR, "onn").unwrap_err();
        assert!(error.contains("not a valid value"));
        assert!(error.contains("auto, on, off"));

        for value in ["auto", "on", "off", "OFF", "  On  "] {
            assert!(validate_value(KEY_REMOTE_TOR, value).is_ok(), "\"{value}\" should be accepted");
        }

        // A free-form key is unconstrained.
        assert!(validate_value(KEY_REMOTE_TOR_PROXY, "socks5h://127.0.0.1:9150").is_ok());
    }
}
