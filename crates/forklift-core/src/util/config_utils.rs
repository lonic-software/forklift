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

/// The values [`KEY_REMOTE_TOR`] accepts — at set time *and* on every read, which is the whole
/// point: this is the one key where a typo is a privacy failure. `onn` is a perfectly good
/// string, so type-strictness alone would let it through, and `remote_utils::TorMode::parse`
/// answers anything it does not recognize with `auto` — leaving non-onion remotes un-proxied
/// while the user believes everything is routed through Tor. [`parse_config`] therefore refuses
/// an out-of-range value in the file, not merely a wrongly-typed one.
///
/// This is deliberately narrower than `TorMode::parse`'s own vocabulary (which also takes
/// `true`/`yes`/`1` and `false`/`no`/`0`): one spelling per meaning, and anything else named as
/// an error rather than guessed at.
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

/// One configuration file, fully parsed.
///
/// The type exists to make a partial read unrepresentable. Every field here was accounted
/// for by [`parse_config`], so holding a `ConfigFile` means the file on disk parsed
/// *completely* — there is no "the rest of it was unreadable, but this key was fine" state
/// for a caller to be handed and quietly act on. A file that does not parse never becomes a
/// `ConfigFile` at all; it becomes an `Err` naming the file and the offending key.
///
/// Values are strings because every on-disk configuration value is a string today (see
/// [`KNOWN_KEYS`]); the numeric and boolean keys are parsed by their consumers. `None` means
/// the key is absent from a file that parsed, never "present but unreadable".
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConfigFile {
    /// `[operator]` — identity.
    pub operator: OperatorSection,
    /// `[remote]` — the warehouse's remote and how to reach it.
    pub remote: RemoteSection,
    /// `[maintenance]` — background object-store maintenance.
    pub maintenance: MaintenanceSection,
    /// `[profile.<name>]` sections, in file order (global configuration only in practice —
    /// nothing writes them to a warehouse file, but parsing them there is not an error).
    pub profiles: Vec<(String, ProfileRecord)>,
}

/// The `[operator]` section of a [`ConfigFile`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct OperatorSection {
    /// [`KEY_OPERATOR_NAME`].
    pub name: Option<String>,
    /// [`KEY_OPERATOR_IDENTIFIER`].
    pub identifier: Option<String>,
    /// [`KEY_OPERATOR_PROFILE`].
    pub profile: Option<String>,
}

/// The `[remote]` section of a [`ConfigFile`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RemoteSection {
    /// [`KEY_REMOTE_URL`].
    pub url: Option<String>,
    /// [`KEY_REMOTE_TOKEN`].
    pub token: Option<String>,
    /// [`KEY_REMOTE_ORIGIN`].
    pub origin: Option<String>,
    /// [`KEY_REMOTE_TOR`].
    pub tor: Option<String>,
    /// [`KEY_REMOTE_TOR_PROXY`].
    pub tor_proxy: Option<String>,
}

/// The `[maintenance]` section of a [`ConfigFile`].
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MaintenanceSection {
    /// [`KEY_MAINTENANCE_AUTO`].
    pub auto: Option<String>,
    /// [`KEY_MAINTENANCE_LOOSE`].
    pub loose: Option<String>,
    /// [`KEY_MAINTENANCE_PACKS`].
    pub packs: Option<String>,
}

/// One `[profile.<name>]` section. Both fields are optional in the same sense as every other
/// key: absent from a file that parsed. An absent identifier is minted on first use
/// (see [`get_operator`]); an absent display name falls back to the identifier.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ProfileRecord {
    /// The profile's on-chain operator id.
    pub identifier: Option<String>,
    /// The profile's display name (local only).
    pub name: Option<String>,
}

impl ProfileRecord {
    /// The identity this profile names, in the shape the rest of the tool consumes.
    ///
    /// An absent field becomes an empty string, which is the "fill this in" signal
    /// [`get_operator`] acts on (mint an identifier, fall the name back to it). That is a
    /// *presence* question, not a parse question: a profile that reached this point came out
    /// of a file that parsed completely.
    fn to_operator(&self) -> Operator {
        Operator {
            name: self.name.clone().unwrap_or_default(),
            identifier: self.identifier.clone().unwrap_or_default(),
        }
    }
}

impl ConfigFile {
    /// The value of a known configuration key in this file.
    ///
    /// # Arguments
    /// * `key` - The configuration key, in `section.key` form (must be a known key).
    ///
    /// # Returns
    /// * `Some(&str)` - The value.
    /// * `None`       - The key is absent from this file.
    pub fn get(&self, key: &str) -> Option<&str> {
        let value = match key {
            KEY_OPERATOR_NAME       => &self.operator.name,
            KEY_OPERATOR_IDENTIFIER => &self.operator.identifier,
            KEY_OPERATOR_PROFILE    => &self.operator.profile,
            KEY_REMOTE_URL          => &self.remote.url,
            KEY_REMOTE_TOKEN        => &self.remote.token,
            KEY_REMOTE_ORIGIN       => &self.remote.origin,
            KEY_REMOTE_TOR          => &self.remote.tor,
            KEY_REMOTE_TOR_PROXY    => &self.remote.tor_proxy,
            KEY_MAINTENANCE_AUTO    => &self.maintenance.auto,
            KEY_MAINTENANCE_LOOSE   => &self.maintenance.loose,
            KEY_MAINTENANCE_PACKS   => &self.maintenance.packs,
            _                       => return None,
        };

        value.as_deref()
    }

    /// A named profile from this file.
    pub fn profile(&self, profile: &str) -> Option<&ProfileRecord> {
        self.profiles.iter()
            .find(|(name, _)| name == profile)
            .map(|(_, record)| record)
    }

    /// Record a parsed value under a known key. The mirror of [`ConfigFile::get`]; the two are
    /// pinned against [`KNOWN_KEYS`] by `every_known_key_round_trips_through_the_typed_file`,
    /// so a key added to that list without a field here fails the suite rather than reading as
    /// permanently unset.
    ///
    /// # Returns
    /// * `Ok(())`      - The value was recorded.
    /// * `Err(String)` - `key` is in [`KNOWN_KEYS`] but has no field here (a bug in this module).
    fn store(&mut self, key: &str, value: &str) -> Result<(), String> {
        let field = match key {
            KEY_OPERATOR_NAME       => &mut self.operator.name,
            KEY_OPERATOR_IDENTIFIER => &mut self.operator.identifier,
            KEY_OPERATOR_PROFILE    => &mut self.operator.profile,
            KEY_REMOTE_URL          => &mut self.remote.url,
            KEY_REMOTE_TOKEN        => &mut self.remote.token,
            KEY_REMOTE_ORIGIN       => &mut self.remote.origin,
            KEY_REMOTE_TOR          => &mut self.remote.tor,
            KEY_REMOTE_TOR_PROXY    => &mut self.remote.tor_proxy,
            KEY_MAINTENANCE_AUTO    => &mut self.maintenance.auto,
            KEY_MAINTENANCE_LOOSE   => &mut self.maintenance.loose,
            KEY_MAINTENANCE_PACKS   => &mut self.maintenance.packs,
            _ => return Err(format!(
                "Internal error: \"{}\" is a known configuration key with no field to hold it.",
                key
            )),
        };

        *field = Some(value.to_string());

        Ok(())
    }
}

/// Load and parse the configuration file of one scope.
///
/// # Arguments
/// * `scope` - The configuration scope to read.
///
/// # Returns
/// * `Ok(ConfigFile)` - The parsed file; a file that does not exist parses as empty.
/// * `Err(String)`    - The file could not be read, or does not parse completely.
pub fn load_scope(scope: ConfigScope) -> Result<ConfigFile, String> {
    let path = get_config_path(scope)?;

    let Some(document) = load_document(&path)? else {
        return Ok(ConfigFile::default());
    };

    parse_config(&document, &path)
}

/// Parse a whole configuration document into a [`ConfigFile`], refusing anything it cannot
/// account for: an unknown top-level section, a section that is not a table, an unknown key
/// inside a known section, a value that is not a string, a profile that is not a table, an
/// unknown profile field.
///
/// **Strict on key names, not only on value types.** `identifer = "alice"` is a typo away from
/// `identifier`, and a reader that silently skips what it does not recognize turns that typo
/// into "no identifier is set" — which, for the operator identity, means minting a fresh one
/// and writing it back over a file the user thought was configured. Refusing the file is the
/// only reading of it that cannot silently discard what the user wrote.
///
/// # Arguments
/// * `document` - The parsed TOML document.
/// * `path`     - The file it came from, named in every error so the user knows what to fix.
///
/// # Returns
/// * `Ok(ConfigFile)` - Every entry in the document was known and well-typed.
/// * `Err(String)`    - The first entry that was not, named.
fn parse_config(document: &DocumentMut, path: &Path) -> Result<ConfigFile, String> {
    let mut config = ConfigFile::default();

    for (section, item) in document.iter() {
        if section == SECTION_PROFILE {
            config.profiles = parse_profiles(item, path)?;
            continue;
        }

        if !is_known_section(section) {
            return Err(not_valid(path, format!(
                "\"{}\" is not a known configuration section. Known sections: {}.",
                section,
                known_sections().join(", ")
            )));
        }

        let table = item.as_table_like().ok_or_else(|| not_valid(path, format!(
            "\"{}\" must be a table (written \"[{}]\"), not {}.",
            section, section, item.type_name()
        )))?;

        for (field, value) in table.iter() {
            let key = format!("{}.{}", section, field);

            if !KNOWN_KEYS.contains(&key.as_str()) {
                return Err(not_valid(path, format!(
                    "\"{}\" is not a known configuration key. Known keys: {}.",
                    key,
                    KNOWN_KEYS.join(", ")
                )));
            }

            let value = value.as_str().ok_or_else(|| not_valid(path, format!(
                "\"{}\" must be a string, not {}. Every configuration value is a string — \
                 write it quoted.",
                key, value.type_name()
            )))?;

            // A fixed-value key is as damaged by an out-of-range *string* as by a wrong type, and
            // for `remote.tor` that damage is a silent clearnet dial rather than a wrong default.
            validate_value(&key, value).map_err(|detail| not_valid(path, detail))?;

            config.store(&key, value)?;
        }
    }

    Ok(config)
}

/// Parse the `[profile.*]` sections. Split out of [`parse_config`] because profiles are the
/// one section whose *keys* are user-chosen, so the strictness applies one level deeper: the
/// profile names are free, the fields inside each profile are not.
fn parse_profiles(item: &toml_edit::Item, path: &Path) -> Result<Vec<(String, ProfileRecord)>, String> {
    let profiles = item.as_table_like().ok_or_else(|| not_valid(path, format!(
        "\"{}\" must be a table (written \"[{}.<name>]\"), not {}.",
        SECTION_PROFILE, SECTION_PROFILE, item.type_name()
    )))?;

    let mut parsed = Vec::new();

    for (name, item) in profiles.iter() {
        let table = item.as_table_like().ok_or_else(|| not_valid(path, format!(
            "\"{}.{}\" must be a table (written \"[{}.{}]\"), not {}.",
            SECTION_PROFILE, name, SECTION_PROFILE, name, item.type_name()
        )))?;

        let mut record = ProfileRecord::default();

        for (field, value) in table.iter() {
            if field != PROFILE_FIELD_IDENTIFIER && field != PROFILE_FIELD_NAME {
                return Err(not_valid(path, format!(
                    "\"{}.{}.{}\" is not a known profile field. Known fields: {}, {}.",
                    SECTION_PROFILE, name, field, PROFILE_FIELD_IDENTIFIER, PROFILE_FIELD_NAME
                )));
            }

            let value = value.as_str().ok_or_else(|| not_valid(path, format!(
                "\"{}.{}.{}\" must be a string, not {}. Every configuration value is a string — \
                 write it quoted.",
                SECTION_PROFILE, name, field, value.type_name()
            )))?;

            match field {
                PROFILE_FIELD_IDENTIFIER => record.identifier = Some(value.to_string()),
                _                        => record.name = Some(value.to_string()),
            }
        }

        parsed.push((name.to_string(), record));
    }

    Ok(parsed)
}

/// Whether `section` is the section half of at least one [`KNOWN_KEYS`] entry — so the set of
/// known sections is derived from the key list rather than repeated beside it.
fn is_known_section(section: &str) -> bool {
    KNOWN_KEYS.iter().any(|key| key.split_once('.').is_some_and(|(known, _)| known == section))
}

/// The known top-level sections, for an error message: the sections of [`KNOWN_KEYS`] in
/// first-appearance order, plus the profile section.
fn known_sections() -> Vec<&'static str> {
    let mut sections = Vec::new();

    for key in KNOWN_KEYS {
        if let Some((section, _)) = key.split_once('.') {
            if !sections.contains(&section) {
                sections.push(section);
            }
        }
    }

    sections.push(SECTION_PROFILE);

    sections
}

/// The one shape every "this file does not parse" refusal takes: the file, what is wrong, and
/// that a hand edit is the fix.
///
/// The path is resolved to an absolute one where it can be. The warehouse-scope path is built
/// from the warehouse root and is normally relative (`.forklift/config/warehouse.toml`), which
/// names nothing in particular to anyone who keeps more than one warehouse — and this refusal
/// can reach a user who is not standing in the one it is about. Canonicalization is best-effort:
/// a file that has since been removed keeps the relative spelling rather than losing the name.
fn not_valid(path: &Path, detail: String) -> String {
    let named = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

    format!(
        "Configuration file \"{}\" is not valid: {} Fix the file by hand.",
        named.to_string_lossy(), detail
    )
}

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
/// `Ok(None)` means the key is absent from a file that parsed completely — never "the file
/// held something here that could not be read". Anything of the second kind is an `Err`, so a
/// caller with a side effect on `None` (minting an operator id, falling back to a default) can
/// only reach it in the case that side effect exists for.
///
/// # Arguments
/// * `key`   - The configuration key, in `section.key` form (must be a known key).
/// * `scope` - The configuration scope to read from.
///
/// # Returns
/// * `Ok(Some(String))` - The value of the key.
/// * `Ok(None)`         - If the key is not set (or the configuration file does not exist).
/// * `Err(String)`      - If the key is unknown, or the file could not be read or does not
///                        parse completely.
pub fn get_scoped_value(key: &str, scope: ConfigScope) -> Result<Option<String>, String> {
    require_known_key(key)?;

    Ok(load_scope(scope)?.get(key).map(str::to_string))
}

/// Get the effective value of a configuration key: the warehouse configuration is
/// consulted first, and the global configuration is the fallback.
///
/// **The warehouse scope is always loaded, and its failure is always the answer.** That
/// ordering — not a merge of both files — is what stops a malformed warehouse file from falling
/// through to a valid global value and going unnoticed: the fallback is only ever reached from a
/// warehouse file that parsed completely and simply did not set this key.
///
/// The global scope is consulted only when the warehouse scope did not answer, which is the
/// case where its content can still change the result. An earlier version of this loaded both
/// unconditionally, on the theory that reporting a broken file the user is not currently reading
/// from is a service; it is not worth paying for a scope that cannot change the answer.
///
/// **Be precise about what that buys, because it is less than it first appears.** It rescues a
/// *lookup*, not a command. Commands read several keys, and a warehouse file rarely sets
/// `operator.profile` or `maintenance.*` — so a broken `~/.forkliftconfig` still fails most
/// identity- or maintenance-touching commands even in a fully configured warehouse, because
/// some key they read falls through to it. What is gone is the case where a scope nothing asked
/// about failed the whole lookup anyway.
///
/// # Arguments
/// * `key` - The configuration key, in `section.key` form (must be a known key).
///
/// # Returns
/// * `Ok(Some((String, ConfigScope)))` - The value and the scope it came from.
/// * `Ok(None)`                        - If the key is not set in either scope.
/// * `Err(String)`                     - If the key is unknown, or a file that had to be
///                                       consulted could not be read or does not parse.
pub fn get_effective_value(key: &str) -> Result<Option<(String, ConfigScope)>, String> {
    require_known_key(key)?;

    for scope in [ConfigScope::Warehouse, ConfigScope::Global] {
        if let Some(value) = load_scope(scope)?.get(key) {
            return Ok(Some((value.to_string(), scope)));
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
    set_value_in_document(&mut document, section, field, value)
        .map_err(|detail| not_valid(&path, detail))?;

    if let Some(parent) = path.parent() {
        // The configuration folder may not exist yet in warehouses prepared before this
        // feature was added.
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    write_validated(&path, &document)
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

    let removed = remove_value_from_document(&mut document, section, field)
        .map_err(|detail| not_valid(&path, detail))?;

    if !removed {
        return Err(format!("\"{}\" is not set.", key));
    }

    write_validated(&path, &document)
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
    Ok(load_scope(ConfigScope::Global)?
        .profile(profile)
        .map(ProfileRecord::to_operator))
}

/// List the named profiles in the global configuration (in file order).
///
/// # Returns
/// * `Ok(Vec<(String, Operator)>)` - The profile names and their identities.
/// * `Err(String)`                 - If the global configuration could not be read.
pub fn list_profiles() -> Result<Vec<(String, Operator)>, String> {
    Ok(load_scope(ConfigScope::Global)?
        .profiles
        .iter()
        .map(|(name, record)| (name.clone(), record.to_operator()))
        .collect())
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
/// * `Err(String)` - If the file could not be read, parsed or written, or if `profile` (or the
///                   `[profile]` section itself) is present but is not a table.
pub fn set_profile_field(profile: &str, field: &str, value: &str) -> Result<(), String> {
    let path = get_config_path(ConfigScope::Global)?;
    let mut document = load_document(&path)?.unwrap_or_default();

    let section_item = document.entry(SECTION_PROFILE).or_insert(toml_edit::table());
    let actual = section_item.type_name();

    let profiles = section_item.as_table_like_mut()
        .ok_or_else(|| not_valid(&path, format!(
            "\"{}\" must be a table (written \"[{}.<name>]\"), not {}.",
            SECTION_PROFILE, SECTION_PROFILE, actual
        )))?;

    if profiles.get(profile).is_none() {
        profiles.insert(profile, toml_edit::table());
    }

    let profile_item = profiles.get_mut(profile).ok_or_else(|| not_valid(&path, format!(
        "\"{}.{}\" vanished between being created and being written.", SECTION_PROFILE, profile
    )))?;
    let actual = profile_item.type_name();

    let table = profile_item.as_table_like_mut().ok_or_else(|| not_valid(&path, format!(
        "\"{}.{}\" must be a table (written \"[{}.{}]\"), not {}.",
        SECTION_PROFILE, profile, SECTION_PROFILE, profile, actual
    )))?;

    table.insert(field, toml_edit::value(value));

    write_validated(&path, &document)
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
    require_known_key(key)?;

    key.split_once('.')
        .ok_or(format!("Configuration key \"{}\" is not in \"section.key\" form.", key))
}

/// Reject a key that is not one Forklift understands. Separate from [`split_key`] because a
/// read never needs the section/field split — [`ConfigFile::get`] takes the whole key — but
/// still has to refuse a key nothing could ever answer for.
///
/// # Returns
/// * `Ok(())`      - `key` is a known configuration key.
/// * `Err(String)` - It is not, with the known keys listed.
fn require_known_key(key: &str) -> Result<(), String> {
    if !KNOWN_KEYS.contains(&key) {
        return Err(format!(
            "Unknown configuration key \"{}\". Known keys: {}.",
            key,
            KNOWN_KEYS.join(", ")
        ));
    }

    Ok(())
}

/// Reject an out-of-range value for a key whose value is a fixed set, so a typo fails loudly
/// instead of degrading silently at runtime. Keys with a free-form value pass through.
///
/// Applied at **both** ends: [`set_value`] refuses to write one, and [`parse_config`] refuses to
/// read one. Set-time strictness alone only ever covered values this tool wrote; the file is
/// hand-editable, which is where the typo actually comes from.
fn validate_value(key: &str, value: &str) -> Result<(), String> {
    if key == KEY_REMOTE_TOR && !REMOTE_TOR_VALUES.contains(&value.trim().to_ascii_lowercase().as_str()) {
        return Err(format!(
            "\"{}\" is not a valid value for {}. Use one of: {}.",
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

/// Write a document back to `path`, but only after re-parsing **the document as edited** —
/// never the one that was read.
///
/// Validating after the edit rather than before it is what lets `forklift config` repair the
/// very entry that is refusing. A pre-edit check turned `remote.tor = "onn"` into a trap: the
/// file refused every write, so `config remote.tor on` — the command that fixes it, at the one
/// moment it is most needed — was refused by the value it was correcting, and a text editor was
/// the only way out. A post-edit check accepts that write, because the file it is about to
/// leave on disk is valid.
///
/// Nothing is relaxed by the move. A write that would leave any *other* entry unaccounted for is
/// still refused, naming that entry, because the contract has always been about what lands on
/// disk — this is simply the first version that checks exactly that.
///
/// # Arguments
/// * `path`     - The configuration file to write.
/// * `document` - The edited document.
///
/// # Returns
/// * `Ok(())`      - The edited document parses completely and was written.
/// * `Err(String)` - It does not parse (naming the offending key), or the write failed.
fn write_validated(path: &Path, document: &DocumentMut) -> Result<(), String> {
    parse_config(document, path)?;

    std::fs::write(path, document.to_string())
        .map_err(|e| format!("Error while writing configuration file \"{}\": {}", path.to_string_lossy(), e))
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
/// * `Err(String)` - If the section exists but is not a table (e.g. `operator = 1`) — a detail
///                   for [`not_valid`] to wrap, since this function does not know the file.
///                   This *is* the text a user sees for that fixture: the edit runs before the
///                   parse now (see [`write_validated`]), so the mutation is what refuses.
fn set_value_in_document(document: &mut DocumentMut,
                         section: &str,
                         field: &str,
                         value: &str) -> Result<(), String> {
    let section_item = document.entry(section).or_insert(toml_edit::table());
    let actual = section_item.type_name();

    let table = section_item.as_table_like_mut().ok_or(format!(
        "\"{}\" must be a table (written \"[{}]\"), not {}.", section, section, actual
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
/// * `Err(String)` - If the section exists but is not a table — a detail for [`not_valid`] to
///                   wrap, for the reason given on [`set_value_in_document`].
fn remove_value_from_document(document: &mut DocumentMut,
                              section: &str,
                              field: &str) -> Result<bool, String> {
    let Some(section_item) = document.get_mut(section) else {
        return Ok(false);
    };

    let actual = section_item.type_name();

    let table = section_item.as_table_like_mut().ok_or(format!(
        "\"{}\" must be a table (written \"[{}]\"), not {}.", section, section, actual
    ))?;

    Ok(table.remove(field).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Parse a document written inline, as if it had been read from `config.toml`.
    fn parse(content: &str) -> Result<ConfigFile, String> {
        parse_config(&content.parse::<DocumentMut>().unwrap(), Path::new("config.toml"))
    }

    #[test]
    fn values_can_be_set_and_read_back() {
        let mut document = DocumentMut::default();

        set_value_in_document(&mut document, "operator", "name", "Máté").unwrap();

        let config = parse_config(&document, Path::new("config.toml")).unwrap();
        assert_eq!(config.get(KEY_OPERATOR_NAME), Some("Máté"));
        assert_eq!(config.get(KEY_OPERATOR_IDENTIFIER), None);
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

        let config = parse_config(&document, Path::new("config.toml")).unwrap();
        assert_eq!(config.get(KEY_OPERATOR_NAME), Some("New Name"));
        assert_eq!(config.get(KEY_OPERATOR_IDENTIFIER), Some("old@id"));
    }

    #[test]
    fn removing_a_value_leaves_comments_and_other_entries() {
        let original = "# Keep me.\n[operator]\nname = \"Name\"\nidentifier = \"id\"\n[remote]\ntoken = \"secret\"\n";
        let mut document: DocumentMut = original.parse().unwrap();

        assert!(remove_value_from_document(&mut document, "remote", "token").unwrap());

        // The token is gone; everything else survives.
        let config = parse_config(&document, Path::new("config.toml")).unwrap();
        assert_eq!(config.get(KEY_REMOTE_TOKEN), None);
        let written = document.to_string();
        assert!(written.contains("# Keep me."));
        assert!(!written.contains("secret"));
        assert_eq!(config.get(KEY_OPERATOR_NAME), Some("Name"));

        // Removing an absent field (or an absent section) reports "not present".
        assert!(!remove_value_from_document(&mut document, "remote", "token").unwrap());
        assert!(!remove_value_from_document(&mut document, "missing", "field").unwrap());
    }

    #[test]
    fn dotted_keys_written_by_hand_are_readable() {
        // Users may write `operator.name = "..."` at the top level instead of using
        // an `[operator]` section; both spellings must be readable.
        assert_eq!(parse("operator.name = \"Dotted\"\n").unwrap().get(KEY_OPERATOR_NAME), Some("Dotted"));
    }

    /// Reachable again, and therefore user-visible: since `write_validated` moved the parse to
    /// *after* the edit, this mutation is what refuses `operator = 1`, not `parse_config`. Its
    /// wording has to match the parse's, or the same fault reads as two different problems
    /// depending on whether you were reading the file or writing it.
    #[test]
    fn setting_a_value_in_a_section_that_is_not_a_table_is_reported() {
        let mut document: DocumentMut = "operator = 1\n".parse().unwrap();

        let error = set_value_in_document(&mut document, "operator", "name", "x").unwrap_err();

        assert!(error.contains("\"operator\" must be a table"), "unexpected error: {error}");
        assert!(error.contains("not integer"), "the error must name what is there: {error}");

        // The read path's wording for the same fault, minus the file-naming wrapper the caller
        // adds. Pinned together so the two cannot drift.
        let read = parse("operator = 1\n").unwrap_err();
        assert!(read.contains(&error), "read and write must describe this fault identically.\n  \
            write: {error}\n  read:  {read}");
    }

    /// The trap `write_validated` exists to remove, at the unit level: a document holding an
    /// out-of-range value accepts the edit that corrects it, and still refuses an edit that
    /// leaves the bad value in place.
    #[test]
    fn an_edit_that_repairs_the_offending_entry_is_accepted_and_one_that_leaves_it_is_not() {
        let mut repaired: DocumentMut = "[remote]\ntor = \"onn\"\n".parse().unwrap();
        set_value_in_document(&mut repaired, "remote", "tor", "on").unwrap();
        assert!(
            parse_config(&repaired, Path::new("config.toml")).is_ok(),
            "the corrected document must parse"
        );

        let mut untouched: DocumentMut = "[remote]\ntor = \"onn\"\n".parse().unwrap();
        set_value_in_document(&mut untouched, "remote", "url", "http://example").unwrap();
        let error = parse_config(&untouched, Path::new("config.toml"))
            .expect_err("a write that leaves the bad value must still refuse");
        assert!(error.contains("not a valid value for remote.tor"), "unexpected refusal: {error}");
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

    /// The typed file and the key list are two spellings of the same thing, and nothing in the
    /// compiler ties them together: a key added to `KNOWN_KEYS` with no field behind it would
    /// parse (the key is "known"), fail to store, and — before `store` returned an error —
    /// would have read as permanently unset. Every key must survive the round trip.
    #[test]
    fn every_known_key_round_trips_through_the_typed_file() {
        for key in KNOWN_KEYS {
            let (section, field) = key.split_once('.').unwrap();

            // A key with a fixed value set has to round-trip one of *its* values: `validate_value`
            // runs inside the parse now, so an arbitrary marker string is refused on the way in.
            let value = if key == KEY_REMOTE_TOR {
                REMOTE_TOR_VALUES[0].to_string()
            } else {
                format!("value-of-{}", key)
            };

            let content = format!("[{}]\n{} = \"{}\"\n", section, field, value);

            let config = parse(&content)
                .unwrap_or_else(|error| panic!("\"{}\" did not parse: {}", key, error));

            assert_eq!(
                config.get(key), Some(value.as_str()),
                "\"{}\" is in KNOWN_KEYS but the typed file has no field holding it", key
            );
        }
    }

    /// The mint-and-overwrite hazard in its plainest form: a value the user wrote, in a shape
    /// the reader does not take, must not read as "nothing is set here".
    #[test]
    fn a_present_but_non_string_value_refuses_and_names_the_key() {
        let error = parse("[operator]\nidentifier = 12345\n").unwrap_err();

        assert!(error.contains("operator.identifier"), "the refusal must name the key: {}", error);
        assert!(error.contains("must be a string"), "unexpected refusal: {}", error);
        assert!(error.contains("config.toml"), "the refusal must name the file: {}", error);
    }

    /// The same hazard reached through the key *name* rather than the value type: `identifer`
    /// is a typo away from `identifier`, and a reader that skipped what it did not recognize
    /// would report the identity as unset and mint a replacement over it.
    #[test]
    fn an_unknown_key_in_a_known_section_refuses() {
        let error = parse("[operator]\nidentifer = \"alice\"\n").unwrap_err();

        assert!(error.contains("operator.identifer"), "the refusal must name the key: {}", error);
        assert!(error.contains("not a known configuration key"), "unexpected refusal: {}", error);
        assert!(error.contains(KEY_OPERATOR_IDENTIFIER), "the refusal must list the known keys: {}", error);
    }

    /// A mistyped *section* header (`[opperator]`) hides every key inside it, which is the same
    /// hazard one level up.
    #[test]
    fn an_unknown_section_refuses() {
        let error = parse("[opperator]\nidentifier = \"alice\"\n").unwrap_err();

        assert!(error.contains("opperator"), "the refusal must name the section: {}", error);
        assert!(error.contains("not a known configuration section"), "unexpected refusal: {}", error);
        assert!(error.contains(SECTION_PROFILE), "the refusal must list the known sections: {}", error);
    }

    #[test]
    fn a_section_that_is_not_a_table_refuses() {
        let error = parse("operator = 1\n").unwrap_err();

        assert!(error.contains("\"operator\" must be a table"), "unexpected refusal: {}", error);
    }

    /// `profile.old = 5` — a profile that is a scalar, not a section. It used to read as "no
    /// such profile", so `profile use old` said it did not exist and `profile create old` would
    /// have written a second entry beside it.
    #[test]
    fn a_profile_that_is_not_a_table_refuses() {
        let error = parse("[profile]\nold = 5\n").unwrap_err();

        assert!(error.contains("profile.old"), "the refusal must name the profile: {}", error);
        assert!(error.contains("must be a table"), "unexpected refusal: {}", error);
    }

    #[test]
    fn a_malformed_profile_field_refuses_and_names_the_profile_and_the_field() {
        let error = parse("[profile.work]\nidentifier = 12345\n").unwrap_err();

        assert!(error.contains("profile.work.identifier"), "unexpected refusal: {}", error);
        assert!(error.contains("must be a string"), "unexpected refusal: {}", error);

        let error = parse("[profile.work]\nidentifer = \"op-1\"\n").unwrap_err();

        assert!(error.contains("profile.work.identifer"), "unexpected refusal: {}", error);
        assert!(error.contains("not a known profile field"), "unexpected refusal: {}", error);
    }

    /// Profile *names* are the user's to choose; only the fields inside them are constrained.
    #[test]
    fn profiles_parse_in_file_order_with_their_optional_fields() {
        let config = parse(
            "[profile.work]\nidentifier = \"op-1\"\nname = \"Work\"\n\n[profile.home]\nname = \"Home\"\n"
        ).unwrap();

        assert_eq!(
            config.profiles.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["work", "home"]
        );
        assert_eq!(config.profile("work").unwrap().identifier.as_deref(), Some("op-1"));
        assert_eq!(config.profile("home").unwrap().identifier, None);
        assert_eq!(config.profile("missing"), None);

        // An absent field becomes the empty string the identity resolver treats as "fill in".
        assert_eq!(config.profile("home").unwrap().to_operator().identifier, "");
        assert_eq!(config.profile("work").unwrap().to_operator().name, "Work");
    }

    /// The privacy case type-strictness alone does not reach: `onn` is a perfectly good string,
    /// so nothing about its *type* is wrong — but `TorMode::parse` answers it with `Auto`, which
    /// silently un-proxies every non-onion remote. The file parse has to know the value set.
    #[test]
    fn an_out_of_range_remote_tor_value_refuses_even_though_it_is_a_string() {
        for spelling in ["onn", "of", "yes", "true", ""] {
            let error = parse(&format!("[remote]\ntor = \"{}\"\n", spelling))
                .expect_err(&format!("\"{spelling}\" must be refused"));

            assert!(error.contains("not a valid value"), "unexpected refusal for \"{spelling}\": {error}");
            assert!(error.contains("auto, on, off"), "the refusal must list the values: {error}");
        }

        for spelling in ["auto", "on", "off", "  OFF  "] {
            assert_eq!(
                parse(&format!("[remote]\ntor = \"{}\"\n", spelling)).unwrap().get(KEY_REMOTE_TOR),
                Some(spelling),
                "\"{spelling}\" must be accepted, and stored exactly as written"
            );
        }
    }

    /// `docs/guide/cli.md` §9 tabulates what a configuration file may not contain. A table of
    /// examples in prose is exactly the shape that rots — so every row of it is a case here, and
    /// a change that quietly starts accepting one of them fails this test rather than leaving the
    /// guide lying. Keep the two in step: a row added there gets a case here.
    #[test]
    fn every_refusal_the_guide_tabulates_is_actually_refused() {
        // Each row carries the phrase the refusal must contain, not merely "it is refused" — a
        // row that started failing for an *unrelated* reason would otherwise keep this test green
        // while the guide's explanation of it quietly became wrong.
        let rows = [
            ("[opperator]\nidentifier = \"ada\"\n",   "\"opperator\" is not a known configuration section"),
            ("[operator]\nidentifer = \"ada\"\n",     "\"operator.identifer\" is not a known configuration key"),
            ("[[operator]]\nname = \"ada\"\n",        "\"operator\" must be a table (written \"[operator]\"), not array of tables"),
            ("[[profile.work]]\nname = \"ada\"\n",    "\"profile.work\" must be a table (written \"[profile.work]\"), not array of tables"),
            ("operator = 5\n",                         "\"operator\" must be a table (written \"[operator]\"), not integer"),
            ("[maintenance]\nloose = 6700\n",          "\"maintenance.loose\" must be a string, not integer"),
            ("[remote]\ntor = true\n",                 "\"remote.tor\" must be a string, not boolean"),
            ("[remote]\ntorproxy = \"socks5h://x\"\n", "\"remote.torproxy\" is not a known configuration key"),
            ("[remote]\ntor = \"onn\"\n",             "\"onn\" is not a valid value for remote.tor"),
            ("[profile.work]\nnickname = \"w\"\n",    "\"profile.work.nickname\" is not a known profile field"),
        ];

        for (content, expected) in rows {
            let error = parse(content)
                .expect_err(&format!("the guide says this is refused: {content:?}"));

            assert!(
                error.contains(expected),
                "refused, but not for the reason the guide gives.\n  expected: {expected}\n  actual:   {error}"
            );
            assert!(error.contains("config.toml"), "every refusal must name the file: {error}");
        }

        // The counterpart the guide promises: the same values, written the documented way, parse.
        let good = parse(
            "[operator]\nidentifier = \"ada\"\n\n[maintenance]\nloose = \"6700\"\n\n\
             [remote]\ntor = \"on\"\ntorProxy = \"socks5h://127.0.0.1:9050\"\n\n\
             [profile.work]\nname = \"Work\"\n"
        ).unwrap();

        assert_eq!(good.get(KEY_MAINTENANCE_LOOSE), Some("6700"));
        assert_eq!(good.get(KEY_REMOTE_TOR), Some("on"));
        assert_eq!(good.get(KEY_REMOTE_TOR_PROXY), Some("socks5h://127.0.0.1:9050"));
    }

    /// The section list an error offers is derived from `KNOWN_KEYS`, not maintained beside it.
    #[test]
    fn known_sections_are_derived_from_the_key_list() {
        assert_eq!(known_sections(), vec!["operator", "remote", "maintenance", SECTION_PROFILE]);

        for key in KNOWN_KEYS {
            assert!(is_known_section(key.split_once('.').unwrap().0), "unhandled section in \"{}\"", key);
        }

        assert!(!is_known_section("opperator"));
    }

    /// An empty document is a valid configuration file that simply sets nothing — the state a
    /// freshly prepared warehouse is in, and the one `Ok(None)` is allowed to mean.
    #[test]
    fn an_empty_document_parses_as_a_file_that_sets_nothing() {
        let config = parse("# only a comment\n").unwrap();

        assert_eq!(config, ConfigFile::default());
        for key in KNOWN_KEYS {
            assert_eq!(config.get(key), None, "\"{}\" should be unset", key);
        }
    }
}
