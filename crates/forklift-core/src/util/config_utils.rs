use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;
use crate::enums::config_scope::ConfigScope;
use crate::globals::forklift_root;
use crate::model::operator::Operator;
use crate::util::file_utils;
use crate::util::remote_utils;

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
/// commands: one of [`MAINTENANCE_AUTO_ON`] or [`MAINTENANCE_AUTO_OFF`]; default is on.
pub const KEY_MAINTENANCE_AUTO: &str = "maintenance.auto";

/// The [`KEY_MAINTENANCE_AUTO`] spellings that mean on.
pub const MAINTENANCE_AUTO_ON: [&str; 4] = ["true", "1", "on", "yes"];

/// The [`KEY_MAINTENANCE_AUTO`] spellings that mean off.
///
/// Anything outside the two lists is refused rather than read as one of them. Reading only the
/// off-list and treating the rest as on made `auto = "flase"` a silent "on" — the opposite of
/// what was typed, with nothing to say so.
pub const MAINTENANCE_AUTO_OFF: [&str; 4] = ["false", "0", "off", "no"];

/// The loose-object count above which background maintenance packs the store (default 6700).
pub const KEY_MAINTENANCE_LOOSE: &str = "maintenance.loose";

/// The pack count above which background maintenance consolidates the packs (default 20).
pub const KEY_MAINTENANCE_PACKS: &str = "maintenance.packs";

/// What a [`KEY_MAINTENANCE_AUTO`] value means — the one reading the file check and the
/// maintenance decision share, so a value the file admits is never read as something else.
/// Case- and surrounding-whitespace-insensitive.
///
/// # Returns
/// * `Ok(bool)`    - Whether background maintenance is on.
/// * `Err(String)` - If the value is in neither [`MAINTENANCE_AUTO_ON`] nor [`MAINTENANCE_AUTO_OFF`].
pub fn parse_maintenance_auto(value: &str) -> Result<bool, String> {
    let normalized = value.trim().to_ascii_lowercase();

    if MAINTENANCE_AUTO_ON.contains(&normalized.as_str()) {
        return Ok(true);
    }
    if MAINTENANCE_AUTO_OFF.contains(&normalized.as_str()) {
        return Ok(false);
    }

    Err(format!(
        "\"{}\" is not a valid value for {}. Use one of: {}.",
        value, KEY_MAINTENANCE_AUTO,
        MAINTENANCE_AUTO_ON.iter().chain(MAINTENANCE_AUTO_OFF.iter()).copied().collect::<Vec<_>>().join(", ")
    ))
}

/// What a [`KEY_MAINTENANCE_LOOSE`] or [`KEY_MAINTENANCE_PACKS`] value means: a whole,
/// non-negative count. Surrounding whitespace is ignored.
///
/// A value that is not a count is refused, not replaced by the default — `loose = "10k"` was
/// otherwise read as 6700 with nothing to say the setting had been ignored.
///
/// # Returns
/// * `Ok(usize)`   - The threshold.
/// * `Err(String)` - If the value is not a non-negative whole number.
pub fn parse_maintenance_threshold(key: &str, value: &str) -> Result<usize, String> {
    let digits = value.trim();

    // `usize::from_str` also takes a leading `+`, which is not a bare number.
    digits.bytes().all(|byte| byte.is_ascii_digit())
        .then(|| digits.parse().ok()).flatten()
        .ok_or_else(|| format!(
            "\"{}\" is not a valid value for {}. Use a whole number, e.g. \"6700\".",
            value, key
        ))
}

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

/// The refusal for an empty profile name — one spelling, shared by every command that takes one.
pub const EMPTY_PROFILE_NAME: &str = "A profile name cannot be empty.";

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

            // Profile fields do not pass through `validate_value` — the keys here are
            // user-chosen, so the strictness is applied one level deeper, in this loop. The
            // emptiness rule has to be applied again with it: this is the branch where an empty
            // `identifier` is destructive rather than merely wrong.
            require_non_empty(&format!("{}.{}.{}", SECTION_PROFILE, name, field), value)
                .map_err(|detail| not_valid(path, detail))?;

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
///
/// On Windows `canonicalize` hands back a *verbatim* path (`\\?\C:\…`), which is the last thing
/// this message wants — its whole job is naming a file for a human to open in an editor — so the
/// prefix is stripped by the one helper that owns that rule.
fn not_valid(path: &Path, detail: String) -> String {
    let named = path.canonicalize()
        .map(|resolved| crate::globals::strip_verbatim_prefix(&resolved))
        .unwrap_or_else(|_| path.to_path_buf());

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
/// * `Ok(String)`  - The spelling actually written, which is not always `value`: a fixed-value
///                   key is stored normalized (see [`storable_value`]). It is returned rather
///                   than left for the caller to re-derive because a caller that echoes the
///                   value it passed in reports something the file does not contain — which is
///                   exactly what `forklift config --json remote.tor "  ON  "` did, answering
///                   `"  ON  "` while the file held `on`. A consumer caching that echo
///                   disagreed with the next read.
/// * `Err(String)` - If the key is unknown, or the file could not be read, parsed
///                   or written.
pub fn set_value(key: &str, value: &str, scope: ConfigScope) -> Result<String, String> {
    let (section, field) = split_key(key)?;

    validate_value(key, value)?;
    let path = get_config_path(scope)?;

    let stored = storable_value(key, value);

    let mut document = load_document(&path)?.unwrap_or_default();
    set_value_in_document(&mut document, section, field, &stored)
        .map_err(|detail| not_valid(&path, detail))?;

    if let Some(parent) = path.parent() {
        // The configuration folder may not exist yet in warehouses prepared before this
        // feature was added.
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }

    write_validated(&path, &document)?;

    Ok(stored)
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
            set_profile_fields(&profile, &[(PROFILE_FIELD_IDENTIFIER, &identity.identifier)])?;
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

/// Set fields of a named profile in the global configuration, creating the profile section
/// when it does not exist yet — **all of them in one document edit and one write**, so a profile
/// is never left holding some of the fields it was given.
///
/// # Arguments
/// * `profile` - The profile name.
/// * `fields`  - The `(field, value)` pairs to set (`identifier` and/or `name`).
///
/// # Returns
/// * `Ok(())`      - If every field was written.
/// * `Err(String)` - If a name or value is empty, a field is not `identifier`/`name` or is given
///                   twice, the file could not be read, parsed or written, or `profile` (or the
///                   `[profile]` section itself) is present but is not a table. Nothing is
///                   written in any of these cases.
pub fn set_profile_fields(profile: &str, fields: &[(&str, &str)]) -> Result<(), String> {
    // The write-side half of the parse's emptiness rule, so this function cannot lay down the
    // entry the parse refuses.
    //
    // The profile *name* is guarded for a different reason. Profile names are the user's to
    // choose and nothing constrains their content — but an empty one is not a name a command can
    // ever select again: selecting it means `operator.profile = ""`, which `validate_value`
    // refuses on both read and write (and `profile use` refuses up front), so `[profile.""]`
    // could be created and then neither used nor removed.
    //
    // A hand-written `[profile.""]` still parses, deliberately: it is unreachable but perfectly
    // understood, and refusing it on load would turn someone else's harmless litter into a file
    // that no command can read. The strict parse is for values that would be *misread*.
    //
    // Every check in this function — the name, the values, and the field checks below — is a
    // precondition on a public function, not a live defence: swept 2026-10-12, both callers
    // already exclude every case — `create_profile` validates its inputs with its own wording
    // before calling and passes only the two known fields once each, and `get_operator` passes an
    // `operator.profile` that `validate_value` refuses when empty, with a freshly minted
    // identifier. No caller can reach them, so a unit test calls this function directly — with a
    // blank value in every call, so that no mutation can make it write the real global file.
    // Re-check this list if a third caller appears.
    if is_blank(profile) {
        return Err(EMPTY_PROFILE_NAME.to_string());
    }

    // A caller error, not a file error, so not `not_valid`: an unknown field would be written
    // and then refused by the very parse `write_validated` runs, blaming the user's file for a
    // bug in the caller; a duplicate would be written twice with the last value silently winning.
    for (index, (field, _)) in fields.iter().enumerate() {
        if *field != PROFILE_FIELD_IDENTIFIER && *field != PROFILE_FIELD_NAME {
            return Err(format!(
                "Internal error: \"{}\" is not a profile field (expected \"{}\" or \"{}\").",
                field, PROFILE_FIELD_IDENTIFIER, PROFILE_FIELD_NAME
            ));
        }

        if fields[..index].iter().any(|(earlier, _)| earlier == field) {
            return Err(format!(
                "Internal error: the profile field \"{}\" was given more than once.", field
            ));
        }
    }

    for (field, value) in fields {
        require_non_empty(&format!("{}.{}.{}", SECTION_PROFILE, profile, field), value)?;
    }

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

    for (field, value) in fields {
        table.insert(field, toml_edit::value(*value));
    }

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
/// * `Err(String)`  - If an input is empty, the profile already exists, or the write failed.
///                    Nothing is written in any of these cases.
pub fn create_profile(profile: &str,
                      name: Option<&str>,
                      identifier: Option<&str>) -> Result<Operator, String> {
    // This door's own wording, checked before anything is read or written. Nothing exists on
    // disk at this point, so the generic "remove the entry" advice would point at an entry that
    // is not there — what the user can actually do is omit the option, and only this door knows
    // which option it was. Checked before the existence test too, so an empty name is reported
    // as itself and not as whatever the global file's state happens to be. The trade: input
    // errors are reported before any global-file error, so someone with both meets them one at
    // a time — fix the option, then meet the file.
    if is_blank(profile) {
        return Err(EMPTY_PROFILE_NAME.to_string());
    }

    if identifier.is_some_and(is_blank) {
        return Err(
            "The operator id (--id) cannot be empty. Omit it to have one minted.".to_string()
        );
    }

    if name.is_some_and(is_blank) {
        return Err(
            "The display name (--name) cannot be empty. Omit it to show the operator id instead."
                .to_string()
        );
    }

    if get_profile(profile)?.is_some() {
        return Err(format!("The profile \"{}\" already exists.", profile));
    }

    let identifier = identifier.map(|id| id.to_string()).unwrap_or_else(mint_uuid_v4);

    // One write for every field. Creation used to be one write per field, so a failure on the
    // second — a refusal then, any I/O fault after the refusals moved up front — left
    // `[profile.<name>] identifier = "<uuid>"` on disk, and the retry was refused because the
    // half-created profile "already exists".
    let mut fields = vec![(PROFILE_FIELD_IDENTIFIER, identifier.as_str())];

    if let Some(name) = name {
        fields.push((PROFILE_FIELD_NAME, name));
    }

    set_profile_fields(profile, &fields)?;

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

/// Reject an out-of-range value for a key whose value is constrained — a fixed set
/// (`remote.tor`, `maintenance.auto`) or a count (`maintenance.loose`, `maintenance.packs`) — so
/// a typo fails loudly instead of degrading silently at runtime. Keys with a free-form value
/// pass through.
///
/// Applied at **both** ends: [`set_value`] refuses to write one, and [`parse_config`] refuses to
/// read one. Set-time strictness alone only ever covered values this tool wrote; the file is
/// hand-editable, which is where the typo actually comes from.
fn validate_value(key: &str, value: &str) -> Result<(), String> {
    require_non_empty(key, value)?;

    match key {
        KEY_REMOTE_TOR
            if !REMOTE_TOR_VALUES.contains(&remote_utils::normalize_tor_value(value).as_str()) => {
            return Err(format!(
                "\"{}\" is not a valid value for {}. Use one of: {}.",
                value, KEY_REMOTE_TOR, REMOTE_TOR_VALUES.join(", ")
            ));
        }
        KEY_MAINTENANCE_AUTO => { parse_maintenance_auto(value)?; }
        KEY_MAINTENANCE_LOOSE | KEY_MAINTENANCE_PACKS => { parse_maintenance_threshold(key, value)?; }
        _ => {}
    }

    Ok(())
}

/// Whether `value` is empty for configuration purposes — the one definition every door uses.
///
/// Whitespace-only counts as empty: `identifier = "   "` is no more an identity than `""` is.
/// The value is only *tested* trimmed, never stored trimmed — a token or a display name is the
/// user's string.
pub fn is_blank(value: &str) -> bool {
    value.trim().is_empty()
}

/// Refuse an empty value for `label`, which no configuration key accepts.
///
/// **Set a key or remove it; there is no third state.** An empty value is the one spelling that
/// reads differently depending on who is looking, and the identity keys made that expensive:
///
/// * `operator.identifier = ""` was taken verbatim by [`get_operator`]'s direct branch, so the
///   **empty string became the on-chain operator id** written into every parcel authored.
/// * `[profile.<name>] identifier = ""` went the opposite way — [`ProfileRecord::to_operator`]
///   maps an absent field and an empty one onto the same `""`, which `get_operator` reads as
///   "fill this in", so it minted a UUID and **wrote it over the user's own entry**. That is the
///   mint-and-overwrite hazard this module's strict parse exists to close, reached by a value
///   that parses perfectly.
///
/// One file, one spelling, two opposite readings, one of them destructive. Refusing it at every
/// door removes the state rather than picking a winner, so neither reading can be reached and
/// `to_operator`'s collapse stays a question about *presence* only.
///
/// **This states the fact and nothing else.** It is reached from several commands, and no advice
/// is true at all of them (an `--unset` needs the scope, fails for a key that is not set, and
/// cannot be run for a profile field), so the refusal names the key and stops. The commands that
/// take a value as an *option* — `profile create --id`/`--name`, `franchise --token` — refuse it
/// themselves first and say to omit the option, which is true wherever they print it.
fn require_non_empty(label: &str, value: &str) -> Result<(), String> {
    if is_blank(value) {
        return Err(format!("\"{}\" cannot be empty.", label));
    }

    Ok(())
}

/// The spelling of `value` that [`set_value`] actually writes for `key`.
///
/// A constrained key is stored normalized, so what lands on disk is literally one of the
/// spellings the guide enumerates ([`REMOTE_TOR_VALUES`], [`MAINTENANCE_AUTO_ON`] /
/// [`MAINTENANCE_AUTO_OFF`]), or a bare number. `forklift config remote.tor
/// "  ON  "` otherwise left `tor = "  ON  "` in the file: admissible, correctly understood, and
/// not a member of the documented set — a value whose correctness rested on every reader
/// applying the same normalization forever. Normalizing once, here, means the stored value
/// needs no normalization to be understood at all.
///
/// Free-form keys pass through untouched: a token, a URL or a display name is the user's
/// string, and folding its case would corrupt it.
fn storable_value(key: &str, value: &str) -> String {
    match key {
        KEY_REMOTE_TOR => remote_utils::normalize_tor_value(value),
        KEY_MAINTENANCE_AUTO => value.trim().to_ascii_lowercase(),
        KEY_MAINTENANCE_LOOSE | KEY_MAINTENANCE_PACKS => parse_maintenance_threshold(key, value)
            .map_or_else(|_| value.to_string(), |count| count.to_string()),
        _ => value.to_string(),
    }
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
///
/// **Absent means nothing is at the path — not even a link, at any depth.** Asking
/// `path.exists()` answered `false` for a symlink whose target is gone and for a file in a
/// directory that cannot be searched — and an absent warehouse file falls through to the global
/// one, so a warehouse's own identity or remote was silently replaced by the user's global
/// settings. A dangling link reads as `NotFound` too, wherever it sits in the path, so the read
/// error alone cannot tell it from absence: see [`dangling_link_in`]. Every other failure to
/// read is an error.
///
/// A file that is not TOML at all is reported like any other invalid file, through
/// [`not_valid`], so it is named by its full path and the remedy is the same.
fn load_document(path: &Path) -> Result<Option<DocumentMut>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => match dangling_link_in(path) {
            None => return Ok(None),
            Some(link) => return Err(format!(
                "Configuration file \"{}\" cannot be read: \"{}\" is a symbolic link to a path \
                 that does not exist. Create the target, or remove the link.",
                path.to_string_lossy(), link.to_string_lossy()
            )),
        },
        Err(error) => return Err(format!(
            "Error while reading configuration file \"{}\": {}", path.to_string_lossy(), error
        )),
    };

    content.parse::<DocumentMut>()
        .map(Some)
        .map_err(|e| not_valid(path, e.to_string().trim_end().to_string()))
}

/// The dangling symbolic link that makes `path` unreachable, if one does — the path itself or
/// any directory above it.
///
/// Walks up from `path` to the nearest component that exists *as an entry* (without following
/// it). Everything above that entry resolved, or the lookup would have failed there; so the
/// entry is the only place a dangling link can hide. If it is a link and does not resolve, that
/// link is why `path` reads as missing; otherwise `path` is genuinely absent.
fn dangling_link_in(path: &Path) -> Option<PathBuf> {
    let entry = path.ancestors().find(|candidate| std::fs::symlink_metadata(candidate).is_ok())?;

    let is_link = std::fs::symlink_metadata(entry).is_ok_and(|metadata| metadata.file_type().is_symlink());

    (is_link && std::fs::metadata(entry).is_err()).then(|| entry.to_path_buf())
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

            // A constrained key has to round-trip a value it admits: `validate_value` runs inside
            // the parse, so an arbitrary marker string is refused on the way in. The two counts
            // get different numbers, so a field holding the other key's value cannot pass.
            let value = match key {
                KEY_REMOTE_TOR => REMOTE_TOR_VALUES[0].to_string(),
                KEY_MAINTENANCE_AUTO => MAINTENANCE_AUTO_OFF[0].to_string(),
                KEY_MAINTENANCE_LOOSE => "6701".to_string(),
                KEY_MAINTENANCE_PACKS => "21".to_string(),
                _ => format!("value-of-{}", key),
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
        // `""` is deliberately not in this list: it is refused by `require_non_empty`, which runs
        // first and is the more specific answer ("cannot be empty" rather than "not one of auto,
        // on, off"). `every_refusal_the_guide_tabulates_is_actually_refused` owns that case.
        for spelling in ["onn", "of", "yes", "true"] {
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

    /// An empty value is refused by the one shared validator, for every spelling of empty.
    ///
    /// The whitespace case is the one a length check would miss, so each path is probed with
    /// both spellings.
    ///
    /// **And the validator offers no remedy — that is pinned here, because it is the contract.**
    /// Every remedy it once printed was wrong somewhere: an `--unset` with no `--global` for the
    /// global file, an `--unset` the command rejects for a profile field, a "remove the entry"
    /// where nothing had been written. It is reached from several commands and cannot see which,
    /// so it may not give any. The commands' own wording is pinned in `cli.rs`, where HOME is
    /// scoped, so a write-path probe cannot reach the developer's real configuration.
    #[test]
    fn no_configuration_value_may_be_empty_and_the_shared_refusal_offers_no_remedy() {
        for blank in ["", "   ", "\t"] {
            for refused in [
                validate_value(KEY_OPERATOR_IDENTIFIER, blank)
                    .expect_err(&format!("{blank:?} must be refused for a section key")),
                require_non_empty("profile.work.identifier", blank)
                    .expect_err(&format!("{blank:?} must be refused for a profile field")),
            ] {
                assert!(refused.ends_with("cannot be empty."), "unexpected refusal: {refused}");

                for remedy in ["--unset", "--global", "editor", "remove", "Fix"] {
                    assert!(
                        !refused.contains(remedy),
                        "the shared refusal must not offer a remedy ({remedy:?}) — only the \
                         door knows the right one: {refused}"
                    );
                }
            }
        }

        assert_eq!(
            require_non_empty("profile.work.identifier", "").unwrap_err(),
            "\"profile.work.identifier\" cannot be empty."
        );

        // The separation that makes the above mean something: a value that merely *contains*
        // whitespace is untouched, and a free-form value is never trimmed on its way in.
        assert!(validate_value(KEY_OPERATOR_NAME, "  Ada  Lovelace  ").is_ok());
        assert!(require_non_empty("profile.work.name", " A ").is_ok());
    }

    /// An unknown or repeated profile field is the caller's bug, refused as
    /// such — not written (an unknown one would then be blamed on the user's file by the parse,
    /// and a repeated one would be written twice with the last silently winning).
    ///
    /// Every value here is blank **on purpose**: these checks run before the emptiness check, so
    /// each assertion distinguishes them by message — and if one is removed, the blank value is
    /// refused next, still before any path is resolved. This test can never reach the real
    /// global configuration, whichever way it fails.
    #[test]
    fn set_profile_fields_refuses_an_unknown_or_repeated_field_as_a_caller_error() {
        let unknown = set_profile_fields("work", &[("bogus", "")]).unwrap_err();
        assert!(unknown.contains("\"bogus\" is not a profile field"), "unexpected: {unknown}");

        let repeated = set_profile_fields(
            "work", &[(PROFILE_FIELD_NAME, "x"), (PROFILE_FIELD_NAME, "")]
        ).unwrap_err();
        assert!(repeated.contains("more than once"), "unexpected: {repeated}");

        // A field rides along so that, without the name check, the blank value is still refused
        // before any path is resolved — an empty list would go on to write the real file.
        assert_eq!(set_profile_fields(" ", &[(PROFILE_FIELD_NAME, "")]).unwrap_err(), EMPTY_PROFILE_NAME);
    }

    /// `remote.tor` is stored normalized, so what lands on disk is literally one of the three
    /// documented spellings rather than a value whose correctness depends on every reader
    /// applying the same normalization.
    ///
    /// The fixture discriminates: `"  ON  "` is accepted by the range check either way, so the
    /// only thing separating the normalizing implementation from the raw-storing one is the
    /// spelling that comes back — `"on"` against `"  ON  "`. Reverting `storable_value` makes
    /// the first assertion fail on exactly that difference, and no rival value in scope
    /// coincides with either.
    #[test]
    fn a_fixed_value_key_is_stored_normalized_and_a_free_form_one_is_not() {
        assert_eq!(storable_value(KEY_REMOTE_TOR, "  ON  "), "on");
        assert_eq!(storable_value(KEY_REMOTE_TOR, "Off"), "off");
        assert_eq!(storable_value(KEY_REMOTE_TOR, "on"), "on");

        // A token, a URL and a display name are the user's strings: case and surrounding
        // whitespace are content, and folding them would corrupt the value.
        assert_eq!(storable_value(KEY_REMOTE_TOKEN, "  AbC-123  "), "  AbC-123  ");
        assert_eq!(storable_value(KEY_OPERATOR_NAME, "Ada L."), "Ada L.");
        assert_eq!(storable_value(KEY_REMOTE_URL, "HTTP://Example.COM"), "HTTP://Example.COM");
    }

    /// Every spelling the file admits for `remote.tor` is read by `TorMode::parse` as the mode
    /// it names — checked against the reader itself, since the admissibility check and the read
    /// live in different modules and a spelling one accepts the other could misread.
    #[test]
    fn what_the_file_admits_for_remote_tor_is_what_the_reader_understands() {
        use remote_utils::TorMode;

        for (spelling, mode) in [
            ("auto", TorMode::Auto), ("on", TorMode::On), ("off", TorMode::Off),
            ("  OFF  ", TorMode::Off), ("ON", TorMode::On), ("\tAuto ", TorMode::Auto),
        ] {
            assert!(
                validate_value(KEY_REMOTE_TOR, spelling).is_ok(),
                "{spelling:?} should be admissible"
            );
            assert_eq!(TorMode::parse(spelling), mode, "{spelling:?} is admitted but read as another mode");
        }
    }

    /// Every `maintenance.*` value the file admits is read as the setting it names, and every
    /// other value is refused by both the file check and the reader — a typo is never read as
    /// the default or as the opposite of what was typed.
    #[test]
    fn maintenance_values_are_read_as_written_or_refused() {
        for (spelling, on) in [
            ("true", true), ("1", true), ("on", true), ("yes", true), (" ON ", true),
            ("false", false), ("0", false), ("off", false), ("no", false), ("\tNo", false),
        ] {
            assert!(validate_value(KEY_MAINTENANCE_AUTO, spelling).is_ok(), "{spelling:?} should be admissible");
            assert_eq!(parse_maintenance_auto(spelling), Ok(on), "{spelling:?} read as the wrong setting");
        }

        for typo in ["flase", "of", "enabled", "2"] {
            let refused = validate_value(KEY_MAINTENANCE_AUTO, typo)
                .expect_err(&format!("{typo:?} must be refused"));
            assert_eq!(
                refused,
                format!("\"{typo}\" is not a valid value for maintenance.auto. \
                         Use one of: true, 1, on, yes, false, 0, off, no.")
            );
            assert!(parse_maintenance_auto(typo).is_err(), "the reader must refuse {typo:?} too");
        }

        for key in [KEY_MAINTENANCE_LOOSE, KEY_MAINTENANCE_PACKS] {
            assert_eq!(parse_maintenance_threshold(key, " 6700 "), Ok(6700));
            assert_eq!(parse_maintenance_threshold(key, "0"), Ok(0));
            assert!(validate_value(key, "20").is_ok());

            for typo in ["10k", "-1", "1.5", "lots", "+6700"] {
                let refused = validate_value(key, typo)
                    .expect_err(&format!("{typo:?} must be refused for {key}"));
                assert_eq!(
                    refused,
                    format!("\"{typo}\" is not a valid value for {key}. Use a whole number, e.g. \"6700\".")
                );
                assert!(parse_maintenance_threshold(key, typo).is_err(), "the reader must refuse {typo:?} too");
            }
        }

        // The setter writes the documented spelling, so the file holds a listed value or a bare number.
        assert_eq!(storable_value(KEY_MAINTENANCE_AUTO, "  OFF "), "off");
        assert_eq!(storable_value(KEY_MAINTENANCE_LOOSE, " 6700 "), "6700");
        assert_eq!(storable_value(KEY_MAINTENANCE_PACKS, "007"), "7");
    }

    /// The data rows of `docs/guide/cli.md`'s "Refused, in either file" table, in file order,
    /// read out of the shipped guide — so neither the number of rows
    /// [`every_refusal_the_guide_tabulates_is_actually_refused`] checks itself against nor the
    /// pairing it assumes is one a hand edit can get wrong.
    ///
    /// Anchored on the table's header line, and the count of that line is asserted to be exactly
    /// one; a second table with the same columns would otherwise make this silently arbitrary.
    ///
    /// The rows are returned rather than counted because a count alone is a weak check: swapping
    /// one guide row for an unrelated refusal, or reordering two, keeps the count unchanged while
    /// every group below is paired with the wrong row. The caller matches an anchor phrase into
    /// each row, which is the part that makes the correspondence positional.
    fn guide_refusal_rows() -> Vec<String> {
        const HEADER: &str = "| Written | Why |";

        let guide = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/guide/cli.md");
        let text = std::fs::read_to_string(&guide)
            .unwrap_or_else(|e| panic!("the guide must be readable at {}: {e}", guide.display()));

        assert_eq!(
            text.lines().filter(|line| line.trim() == HEADER).count(), 1,
            "exactly one table in the guide may carry this test's anchor header ({HEADER})"
        );

        text.lines()
            .skip_while(|line| line.trim() != HEADER)
            .skip(2)                                        // the header itself, then `|---|---|`
            .take_while(|line| line.starts_with('|'))
            .map(|line| line.to_string())
            .collect()
    }

    /// `docs/guide/cli.md` §9 tabulates what a configuration file may not contain, and this is
    /// the other half of that table: one group of cases per row, every example refused, each for
    /// the reason the row actually gives.
    ///
    /// **The correspondence is checked, not promised.** The first version of this test asked the
    /// next editor, in a doc comment, to keep the two in step — an obligation in prose, which is
    /// a guard that is not there: a row added to the guide with no case here failed nothing, and
    /// a row deleted from it left an orphan case passing forever. It reads the guide now and
    /// requires exactly one group per row, **paired with that row**: each group carries an
    /// anchor phrase that must appear in the guide row it claims to cover, so reordering the
    /// table or swapping one refusal for another fails here rather than passing on a count that
    /// happens to still match. The *grouping* is what makes the count mean anything at all:
    /// several rows carry two examples, so a flat list of cases could never be compared against
    /// a row count.
    #[test]
    fn every_refusal_the_guide_tabulates_is_actually_refused() {
        // One group per guide-table row, in the guide's order. Each group leads with a phrase
        // from its guide row, then the cases. Each case carries the phrase the refusal must
        // contain, not merely "it is refused" — a case that started failing for an *unrelated*
        // reason would otherwise keep this test green while the guide's explanation of it
        // quietly became wrong.
        let rows: [(&str, &[(&str, &str)]); 10] = [
            ("any other unknown section",
             &[("[opperator]\nidentifier = \"ada\"\n",   "\"opperator\" is not a known configuration section")]),
            ("an unknown key inside a known section",
             &[("[operator]\nidentifer = \"ada\"\n",     "\"operator.identifer\" is not a known configuration key")]),
            ("an array-of-tables where a table is expected",
             &[("[[operator]]\nname = \"ada\"\n",        "\"operator\" must be a table (written \"[operator]\"), not array of tables"),
               ("[[profile.work]]\nname = \"ada\"\n",    "\"profile.work\" must be a table (written \"[profile.work]\"), not array of tables")]),
            ("a section that is not a table at all",
             &[("operator = 5\n",                        "\"operator\" must be a table (written \"[operator]\"), not integer")]),
            ("every value is a **string on disk**",
             &[("[maintenance]\nloose = 6700\n",         "\"maintenance.loose\" must be a string, not integer"),
               ("[remote]\ntor = true\n",                "\"remote.tor\" must be a string, not boolean")]),
            ("keys are case-sensitive",
             &[("[remote]\ntorproxy = \"socks5h://x\"\n", "\"remote.torproxy\" is not a known configuration key")]),
            ("accepts only `auto`, `on` or `off`",
             &[("[remote]\ntor = \"onn\"\n",             "\"onn\" is not a valid value for remote.tor")]),
            ("`maintenance.auto` accepts only",
             &[("[maintenance]\nauto = \"flase\"\n",     "\"flase\" is not a valid value for maintenance.auto"),
               ("[maintenance]\nloose = \"10k\"\n",      "\"10k\" is not a valid value for maintenance.loose")]),
            ("profiles hold `identifier` and `name`, nothing else",
             &[("[profile.work]\nnickname = \"w\"\n",    "\"profile.work.nickname\" is not a known profile field")]),
            ("there is no third state",
             &[("[operator]\nidentifier = \"\"\n",       "\"operator.identifier\" cannot be empty"),
               ("[profile.work]\nidentifier = \"   \"\n", "\"profile.work.identifier\" cannot be empty")]),
        ];

        let guide_rows = guide_refusal_rows();

        assert_eq!(
            rows.len(), guide_rows.len(),
            "the guide's refusal table and this test's groups must correspond one to one — \
             a row added there needs a group here, and a row removed there leaves an orphan"
        );

        for ((anchor, _), row) in rows.iter().zip(guide_rows.iter()) {
            assert!(
                row.contains(anchor),
                "this test's groups are paired with the guide's rows in order, and group \
                 {anchor:?} does not match the row it landed on.\n  row: {row}"
            );
        }

        for (content, expected) in rows.iter().flat_map(|(_, group)| group.iter()) {
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
