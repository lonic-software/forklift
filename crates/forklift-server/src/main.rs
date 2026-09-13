//! The self-hostable server head of the remote protocol
//! (`docs/format/REMOTE_PROTOCOL.md`). The warehouse root is entered as a storage-root
//! scope per storage closure (never by changing the working directory), and the server
//! reuses the exact same storage code (and the exact same audit code) the CLI uses
//! locally — a remote can never be pushed into a state a local audit would reject.
//!
//! This head and the AWS serverless head are equal ways to host a warehouse for real
//! use: teams self-host with this binary; the hosted service builds on the serverless
//! head. Both speak the same protocol, so clients cannot tell them apart — and because
//! this one is open source, the protocol stays independently verifiable.

use clap::{Parser, Subcommand};

mod server;

#[derive(Parser)]
#[command(
    name = "forklift-server",
    version,
    about = "Forklift — the self-hostable server head.",
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve one warehouse (--root) or a folder of warehouses (--warehouses)
    Serve {
        /// The warehouse root to serve at /v1 (prepared with "forklift-server prepare")
        #[arg(long, conflicts_with = "warehouses")]
        root: Option<String>,

        /// A base folder whose subdirectories are served at /warehouses/{id}/v1
        #[arg(long)]
        warehouses: Option<String>,

        /// The address to bind (port 0 picks a free port; default 127.0.0.1:9418)
        #[arg(long)]
        addr: Option<String>,

        /// Require this bearer token on every request (also gates warehouse creation)
        #[arg(long)]
        token: Option<String>,

        /// A TOML file of per-operator tokens: [operators] "<token>" = "<identifier>"
        #[arg(long)]
        tokens: Option<String>,

        /// Refuse request bodies over this size (MiB; default: unlimited — the hash
        /// check gates correctness, this gates disk-fill abuse)
        #[arg(long)]
        max_body_mb: Option<u64>,

        /// Rebuild the served bundle after this many accepted lifts (default: never)
        #[arg(long)]
        rebuild_after_lifts: Option<u32>,

        /// Serve with no authentication at all: the explicit opt-out. Without this (and
        /// without --token/--tokens/a configured authentication hook), the server refuses
        /// to start rather than infer "open" from an empty auth config.
        #[arg(long)]
        open: bool,

        /// A TOML config file with the same keys as these flags; flags override it
        #[arg(long)]
        config: Option<String>,
    },

    /// Prepare a bare warehouse to serve (creates the folder if needed)
    Prepare {
        /// The warehouse root to prepare
        #[arg(long)]
        root: String,
    },

    /// Build the bundle served at /v1/bundles/latest (see BUNDLE_FORMAT.md)
    Bundle {
        /// The warehouse root to bundle
        #[arg(long)]
        root: String,
    },

    /// Delete unreferenced objects (mark-and-sweep from the pallet heads)
    #[command(
        long_about = "Delete objects no pallet head reaches (a failed or abandoned lift \
                      leaves verified objects behind). Unreferenced objects younger than \
                      the grace period are kept: an in-flight lift may still be uploading \
                      the parcels that will reference them."
    )]
    Gc {
        /// The warehouse root to collect
        #[arg(long)]
        root: String,

        /// The grace period in hours
        #[arg(long, default_value_t = 24)]
        grace_hours: u64,
    },
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Serve { root, warehouses, addr, token, tokens, max_body_mb, rebuild_after_lifts, open, config } =>
            serve(root, warehouses, addr, token, tokens, max_body_mb, rebuild_after_lifts, open, config).await,
        Command::Prepare { root } => prepare(&root),
        Command::Bundle { root } => bundle(&root),
        Command::Gc { root, grace_hours } => gc(&root, grace_hours),
    };

    if let Err(e) = result {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

/// Merge the flags with the config file (flags win) and serve.
#[allow(clippy::too_many_arguments)]
async fn serve(root: Option<String>,
               warehouses: Option<String>,
               addr: Option<String>,
               token: Option<String>,
               tokens: Option<String>,
               max_body_mb: Option<u64>,
               rebuild_after_lifts: Option<u32>,
               open: bool,
               config: Option<String>) -> Result<(), String> {
    let file = match config {
        Some(path) => parse_config(&path)?,
        None => ConfigFile::default(),
    };

    let options = server::ServeOptions {
        root: root.or(file.root),
        warehouses: warehouses.or(file.warehouses),
        addr: addr.or(file.addr).unwrap_or("127.0.0.1:9418".to_string()),
        token: token.or(file.token),
        tokens: tokens.or(file.tokens),
        max_body_mb: max_body_mb.or(file.max_body_mb),
        rebuild_after_lifts: rebuild_after_lifts.or(file.rebuild_after_lifts),
        authentication_hook: file.authentication_hook,
        admission_hook: file.admission_hook,
        events_hook: file.events_hook,
        resolution_hook: file.resolution_hook,
        authentication_cache_secs: file.authentication_cache_secs,
        // The flag is the affirmative case: passing `--open` always opts out, regardless of
        // the config file. There is no flag-side way to *cancel* `open = true` in the file
        // (matching every other flag/file pair here, where "flags override" only ever means
        // "a flag can supply a value", never "a flag can unset one") — remove it from the
        // file to close that hole.
        open: open || file.open.unwrap_or(false),
    };

    server::serve(options).await
}

/// The serve keys of the TOML config file (same names as the flags). The hooks
/// (`docs/format/HOOK_PROTOCOL.md`) are config-file-only — they come in URL+secret
/// pairs, which flags handle poorly:
///
/// ```toml
/// open = true    # explicit opt-out of authentication — see ConfigFile::open
///
/// [hooks]
/// authentication_url = "https://provider/hooks/auth"
/// authentication_secret = "…"
/// admission_url = "…"
/// admission_secret = "…"
/// events_url = "…"
/// events_secret = "…"
/// resolution_url = "…"
/// resolution_secret = "…"
/// authentication_cache_secs = 60
/// ```
#[derive(Default, Debug)]
struct ConfigFile {
    root: Option<String>,
    warehouses: Option<String>,
    addr: Option<String>,
    token: Option<String>,
    tokens: Option<String>,
    max_body_mb: Option<u64>,
    rebuild_after_lifts: Option<u32>,

    /// The explicit opt-out of authentication (`server::ServeOptions::open`'s config-file
    /// counterpart). `Option<bool>`, like every other field here, rather than a bare `bool`
    /// defaulting to `false`: that would make an absent key and a present-but-wrong-typed
    /// `open = "yes"` parse identically, which is the exact class of bug this fix exists to
    /// close. `optional_bool` below keeps them distinguishable; `serve` collapses `None` to
    /// `false` only after parsing has already rejected a malformed value.
    open: Option<bool>,

    authentication_hook: Option<server::HookEndpoint>,
    admission_hook: Option<server::HookEndpoint>,
    events_hook: Option<server::HookEndpoint>,
    resolution_hook: Option<server::HookEndpoint>,
    authentication_cache_secs: Option<u64>,
}

/// Read an optional string field from a parsed TOML item, strictly: an absent key is `None`
/// (every key in this config file is optional, with a defined default the caller applies), but
/// a *present* key that is not a string is an error naming the file and the key — never
/// silently collapsed to `None` the way `.and_then(|item| item.as_str())` did. That collapse is
/// exactly how a hand-edited, unquoted `token = 12345` used to vanish into "no token
/// configured": every caller downstream of this reader treats an absent value as license to
/// serve `Principal::Open`, so a present-but-wrong-typed value must never be reported the same
/// way an absent one is.
fn optional_string(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<String>, String> {
    let Some(item) = item else { return Ok(None); };

    item.as_str()
        .map(|s| Some(s.to_string()))
        .ok_or_else(|| format!(
            "The config file \"{}\" has a \"{}\" entry that is present but is not a string.",
            path, key
        ))
}

/// [`optional_string`]'s integer counterpart, same absent-vs-malformed split.
fn optional_integer(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<i64>, String> {
    let Some(item) = item else { return Ok(None); };

    item.as_integer()
        .map(Some)
        .ok_or_else(|| format!(
            "The config file \"{}\" has a \"{}\" entry that is present but is not an integer.",
            path, key
        ))
}

/// [`optional_integer`]'s non-negative counterpart, for every integer field that is cast to an
/// unsigned type downstream (`authentication_cache_secs`, `max_body_mb`, `rebuild_after_lifts` —
/// all read via this function below). A negative `i64` surviving to `as u64`/`as u32` does not
/// error, it wraps: `-1i64 as u64` is `u64::MAX`. For `authentication_cache_secs` specifically
/// that wrap is an auth-path hole, not just a display glitch — `Duration::from_secs(u64::MAX)`
/// makes the authentication-hook cache TTL effectively infinite, so a credential the provider has
/// revoked is never re-checked again for the life of the process. Rejecting the negative value
/// here, before the cast, closes it the same way `optional_string`/`optional_bool` already close
/// the analogous "present but wrong shape" gap for their own types.
fn optional_non_negative_integer(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<i64>, String> {
    match optional_integer(item, path, key)? {
        Some(v) if v < 0 => Err(format!(
            "The config file \"{}\" has a \"{}\" entry that is negative ({}); it must be zero or \
            positive.",
            path, key, v
        )),
        other => Ok(other),
    }
}

/// [`optional_string`]'s boolean counterpart, for `open`.
fn optional_bool(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<bool>, String> {
    let Some(item) = item else { return Ok(None); };

    item.as_bool()
        .map(Some)
        .ok_or_else(|| format!(
            "The config file \"{}\" has a \"{}\" entry that is present but is not a boolean \
            (true or false).",
            path, key
        ))
}

/// The top-level keys `parse_config` understands (including `hooks`, whose own nested keys are
/// [`HOOKS_KEYS`]). Not documentation only: [`reject_unknown_keys`] checks every key actually
/// present in the file against this list, so `tokenn = "…"` (a typo `parse_config`'s strict
/// per-field readers cannot catch on their own, since the misspelled key is simply never looked
/// up) is a startup error rather than a silently no-op setting.
const CONFIG_TOP_LEVEL_KEYS: [&str; 9] = [
    "root", "warehouses", "addr", "token", "tokens", "max_body_mb", "rebuild_after_lifts",
    "open", "hooks",
];

/// The keys `parse_config` understands inside `[hooks]` — four independent `{name}_url` /
/// `{name}_secret` pairs (`docs/format/HOOK_PROTOCOL.md`) plus the one shared cache setting.
/// Same role as [`CONFIG_TOP_LEVEL_KEYS`]: a typo'd `autentication_url` is a startup error, not
/// a silently-never-configured hook (the second fail-open path this fix closes — with no other
/// auth configured, a hook that silently failed to parse used to leave `check_auth` reading
/// "nothing is configured" and serving every request as `Principal::Open`).
const HOOKS_KEYS: [&str; 9] = [
    "authentication_url", "authentication_secret",
    "admission_url", "admission_secret",
    "events_url", "events_secret",
    "resolution_url", "resolution_secret",
    "authentication_cache_secs",
];

/// Refuse a key that is not in `known` — the mechanism behind [`CONFIG_TOP_LEVEL_KEYS`] and
/// [`HOOKS_KEYS`]: every key actually present in the parsed table must be one this function
/// recognizes, or this errors naming the file, the offending key and where it was found.
///
/// Takes `&dyn TableLike` rather than `&toml_edit::Table` so it works on an inline table too
/// (`hooks = { authentication_url = "…", authentication_secret = "…" }`), not only the
/// `[hooks]` form — `Table` and `InlineTable` are two distinct types in this crate, and only
/// `TableLike` is implemented by both.
fn reject_unknown_keys(table: &dyn toml_edit::TableLike,
                       known: &[&str],
                       path: &str,
                       location: &str) -> Result<(), String> {
    for (key, _) in table.iter() {
        if !known.contains(&key) {
            return Err(format!(
                "The config file \"{}\" has an unrecognized key \"{}\" in {}. Recognized keys: \
                {}.",
                path, key, location, known.join(", ")
            ));
        }
    }

    Ok(())
}

/// Parse the serve config file, strictly: an absent key keeps its defined default, but a
/// *present* key of the wrong type — or a key this function does not recognize at all — is
/// always an error naming the file, the key and what was expected, never silently treated as
/// absent. Before this, every reader here was `doc.get(key).and_then(|item| item.as_str())`
/// (or `.as_integer()`), which conflates "key not set" with "key set to garbage": a hand-edited
/// `token = 12345` (a natural typo — dropping the quotes around a numeric-looking token) parsed
/// as `None`, and with no `tokens` file and no hook configured, `check_auth` reads that as "no
/// auth configured at all" and serves `Principal::Open` to every request, silently and without
/// warning. The same collapse dropped a mistyped hook key (`autentication_url`) to "no hook
/// configured" instead of erroring — [`reject_unknown_keys`] is what catches that one, since a
/// misspelled key is never looked up by the per-field readers at all. `hooks` itself gets the
/// wrong-type treatment too: a present-but-not-a-table value (`hooks = "oops"`) used to
/// silently read as "no hooks configured" via `.and_then(|item| item.as_table())`; it is now an
/// error. `hooks` accepts either TOML shape a table can take — `[hooks]` (`Item::Table`) or an
/// inline table (`hooks = { … }`, `Value::InlineTable`) — via `as_table_like()` rather than
/// `as_table()`, which matches only the former: an inline `hooks` is legal, present, correctly
/// typed TOML, and `as_table()` used to reject it with the same "is not a table" message a
/// genuinely wrong-typed value gets.
fn parse_config(path: &str) -> Result<ConfigFile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading the config file \"{}\": {}", path, e))?;

    let doc: toml_edit::DocumentMut = content.parse()
        .map_err(|e| format!("The config file \"{}\" is not valid TOML: {}", path, e))?;

    reject_unknown_keys(doc.as_table(), &CONFIG_TOP_LEVEL_KEYS, path, "the top-level config")?;

    let hooks = match doc.get("hooks") {
        None => None,
        Some(item) => Some(item.as_table_like().ok_or_else(|| format!(
            "The config file \"{}\" has a \"hooks\" entry that is present but is not a table.",
            path
        ))?),
    };

    if let Some(hooks) = hooks {
        reject_unknown_keys(hooks, &HOOKS_KEYS, path, "the [hooks] table")?;
    }

    let hook_of = |name: &str| -> Result<Option<server::HookEndpoint>, String> {
        let Some(hooks) = hooks else {
            return Ok(None);
        };

        let url_key = format!("hooks.{}_url", name);
        let secret_key = format!("hooks.{}_secret", name);

        let url = optional_string(hooks.get(&format!("{}_url", name)), path, &url_key)?;
        let secret = optional_string(hooks.get(&format!("{}_secret", name)), path, &secret_key)?;

        match (url, secret) {
            (None, None) => Ok(None),
            (Some(url), Some(secret)) => Ok(Some(server::HookEndpoint { url, secret })),
            _ => Err(format!(
                "The config file \"{}\" configures the {} hook with only one of \
                {}_url and {}_secret; hook requests are signed, both are required.",
                path, name, name, name
            )),
        }
    };

    let authentication_cache_secs = match hooks {
        None => None,
        Some(table) => optional_non_negative_integer(
            table.get("authentication_cache_secs"), path, "hooks.authentication_cache_secs"
        )?.map(|v| v as u64),
    };

    Ok(ConfigFile {
        root: optional_string(doc.get("root"), path, "root")?,
        warehouses: optional_string(doc.get("warehouses"), path, "warehouses")?,
        addr: optional_string(doc.get("addr"), path, "addr")?,
        token: optional_string(doc.get("token"), path, "token")?,
        tokens: optional_string(doc.get("tokens"), path, "tokens")?,
        max_body_mb: optional_non_negative_integer(doc.get("max_body_mb"), path, "max_body_mb")?
            .map(|v| v as u64),
        rebuild_after_lifts: optional_non_negative_integer(
            doc.get("rebuild_after_lifts"), path, "rebuild_after_lifts"
        )?.map(|v| v as u32),
        open: optional_bool(doc.get("open"), path, "open")?,
        authentication_hook: hook_of("authentication")?,
        admission_hook: hook_of("admission")?,
        events_hook: hook_of("events")?,
        resolution_hook: hook_of("resolution")?,
        authentication_cache_secs,
    })
}

/// Resolve a warehouse root to an absolute path (creating the folder when preparing a
/// new one), for entering it as a storage-root scope.
fn resolve_root(root: &str, create: bool) -> Result<std::path::PathBuf, String> {
    if create {
        std::fs::create_dir_all(root)
            .map_err(|e| format!("Error while creating \"{}\": {}", root, e))?;
    }

    std::fs::canonicalize(root)
        .map_err(|e| format!("Error while resolving \"{}\": {}", root, e))
}

/// Prepare a bare warehouse: the same layout `forklift prepare` creates — the server
/// simply never uses the working-directory parts (inventory, ignore file).
fn prepare(root: &str) -> Result<(), String> {
    let resolved = resolve_root(root, true)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&resolved);

    let created = forklift_core::util::warehouse_utils::prepare_warehouse()?;

    if created.is_empty() {
        println!("Nothing to do.");
    } else {
        println!("Prepared warehouse at \"{}\".", root);
    }

    Ok(())
}

/// Build the bundle served at `/v1/bundles/latest`.
fn bundle(root: &str) -> Result<(), String> {
    let resolved = resolve_root(root, false)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&resolved);

    // Unlike `gc`, `bundle` is safe to run against a live server and is deliberately *not*
    // serve-locked: it never deletes an object, it writes the bundle atomically (temp +
    // rename), and a bundle is "a clone-time optimization, never a source of truth" — a bundle
    // built mid-lift that misses the newest objects is self-healing (clients fetch the rest
    // loose). Refreshing a live server's bundle with this command is a supported workflow (the
    // server also auto-rebuilds in-process via --rebuild-after-lifts).
    let stats = forklift_core::util::bundle_utils::build_bundle()?;

    println!(
        "Bundled {} object(s), {} delta(s) and {} signature(s) into \"{}\".",
        stats.objects,
        stats.deltas,
        stats.signatures,
        stats.path.to_string_lossy()
    );

    Ok(())
}

/// Collect unreferenced objects.
fn gc(root: &str, grace_hours: u64) -> Result<(), String> {
    let resolved = resolve_root(root, false)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&resolved);

    // Refuse while a server is serving this root: gc would sweep the server's in-flight
    // staged objects, and a lift slower than the grace period then fails its ref update. Held for
    // the whole command so a server cannot start mid-sweep.
    let _serve_lock = forklift_core::util::lock_utils::ServeLock::acquire()
        .map_err(|e| format!("Refusing to gc: {}", e))?;

    let stats = forklift_core::util::gc_utils::collect_garbage(grace_hours * 3600)?;

    println!(
        "Scanned {} object(s): deleted {} unreferenced, kept {} within the {}h grace \
        period.",
        stats.scanned, stats.deleted, stats.kept_recent, grace_hours
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unique scratch file for one test's config (never shared across tests, so parallel
    /// tests never collide on disk) — same shape as `server::tests::scratch_dir`, duplicated
    /// here rather than shared because that helper is private to `server`'s own test module.
    fn scratch_config(name: &str, contents: &str) -> std::path::PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

        let path = std::env::temp_dir().join(format!(
            "forklift-server-main-test-{}-{}-{}.toml", name, std::process::id(), id
        ));
        std::fs::write(&path, contents).unwrap();
        path
    }

    // ---------------------------------------------------------------------------------
    // parse_config: absence keeps defaults
    // ---------------------------------------------------------------------------------

    #[test]
    fn an_empty_config_parses_to_every_default() {
        let path = scratch_config("empty", "");
        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(file.root, None);
        assert_eq!(file.warehouses, None);
        assert_eq!(file.addr, None);
        assert_eq!(file.token, None);
        assert_eq!(file.tokens, None);
        assert_eq!(file.max_body_mb, None);
        assert_eq!(file.rebuild_after_lifts, None);
        assert_eq!(file.open, None);
        assert!(file.authentication_hook.is_none());
        assert!(file.admission_hook.is_none());
        assert!(file.events_hook.is_none());
        assert!(file.resolution_hook.is_none());
        assert_eq!(file.authentication_cache_secs, None);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_fully_populated_config_round_trips() {
        let path = scratch_config("full", r#"
            root = "/srv/forklift/wh"
            addr = "127.0.0.1:9418"
            token = "secret"
            tokens = "/etc/forklift/tokens.toml"
            max_body_mb = 4096
            rebuild_after_lifts = 20
            open = true

            [hooks]
            authentication_url = "https://provider.example/hooks/auth"
            authentication_secret = "s1"
            admission_url = "https://provider.example/hooks/admission"
            admission_secret = "s2"
            events_url = "https://provider.example/hooks/events"
            events_secret = "s3"
            resolution_url = "https://provider.example/hooks/resolve"
            resolution_secret = "s4"
            authentication_cache_secs = 60
        "#);

        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(file.root.as_deref(), Some("/srv/forklift/wh"));
        assert_eq!(file.token.as_deref(), Some("secret"));
        assert_eq!(file.max_body_mb, Some(4096));
        assert_eq!(file.rebuild_after_lifts, Some(20));
        assert_eq!(file.open, Some(true));
        assert_eq!(file.authentication_hook.as_ref().map(|h| h.url.as_str()),
                   Some("https://provider.example/hooks/auth"));
        assert_eq!(file.authentication_cache_secs, Some(60));

        let _ = std::fs::remove_file(&path);
    }

    /// The inline-table form of `hooks` (`hooks = { … }`, `Value::InlineTable`) is legal TOML
    /// and must configure the hook identically to the `[hooks]` table form — `as_table()` used
    /// to accept only the latter (`Item::Table`) and reject this with "is not a table", the same
    /// message a genuinely wrong-typed `hooks` value gets. Round-trips the same fields as
    /// `a_fully_populated_config_round_trips`, as an inline table, to confirm parity.
    #[test]
    fn an_inline_table_hooks_is_accepted_and_configures_the_hook() {
        let path = scratch_config("hooks-inline", r#"
            hooks = { authentication_url = "https://provider.example/hooks/auth", authentication_secret = "s1", authentication_cache_secs = 60 }
        "#);

        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(
            file.authentication_hook.as_ref().map(|h| h.url.as_str()),
            Some("https://provider.example/hooks/auth")
        );
        assert_eq!(
            file.authentication_hook.as_ref().map(|h| h.secret.as_str()),
            Some("s1")
        );
        assert_eq!(file.authentication_cache_secs, Some(60));

        let _ = std::fs::remove_file(&path);
    }

    /// The inline-table form must still reject an unrecognized key inside it, the same as the
    /// `[hooks]` form does (`reject_unknown_keys` runs against `TableLike`, not `Table`
    /// specifically) — otherwise an inline `hooks` would silently accept a typo the table form
    /// catches.
    #[test]
    fn an_inline_table_hooks_still_rejects_an_unrecognized_key() {
        let path = scratch_config(
            "hooks-inline-misspelled",
            "hooks = { autentication_url = \"https://x\", autentication_secret = \"s\" }\n"
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"autentication_url\""), "{}", error);
        assert!(error.contains("[hooks]"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: present-but-wrong-typed values error (Part 1 of the fix)
    // ---------------------------------------------------------------------------------

    /// The exact reproduction from the fail-open finding: a hand-edited, unquoted
    /// `token = 12345` must be a startup error naming the file and the key, never silently
    /// parsed as "no token configured".
    #[test]
    fn a_present_but_numeric_token_is_an_error_naming_the_key() {
        let path = scratch_config("token-numeric", "token = 12345\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"token\""), "{}", error);
        assert!(error.contains("not a string"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_present_but_stringy_max_body_mb_is_an_error() {
        let path = scratch_config("max-body-mb-stringy", "max_body_mb = \"4096\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"max_body_mb\""), "{}", error);
        assert!(error.contains("not an integer"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_present_but_stringy_rebuild_after_lifts_is_an_error() {
        let path = scratch_config("rebuild-after-lifts-stringy", "rebuild_after_lifts = \"20\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"rebuild_after_lifts\""), "{}", error);
        assert!(error.contains("not an integer"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// Part 2's own opt-in field gets the same strict treatment as every other scalar: a
    /// present-but-wrong-typed `open` must error rather than silently collapse to `false` (or,
    /// worse, to something truthy by accident).
    #[test]
    fn a_present_but_stringy_open_is_an_error() {
        let path = scratch_config("open-stringy", "open = \"yes\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"open\""), "{}", error);
        assert!(error.contains("not a boolean"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// A present-but-not-a-table `hooks` entry must error rather than silently read as "no
    /// hooks configured" — the same fail-open shape as `token = 12345`, one level up: with no
    /// other auth configured, a hooks table that silently vanished used to leave `check_auth`
    /// reading "nothing is configured" and serving `Principal::Open`.
    #[test]
    fn a_present_but_non_table_hooks_is_an_error() {
        let path = scratch_config("hooks-non-table", "hooks = \"oops\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"hooks\""), "{}", error);
        assert!(error.contains("not a table"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_present_but_numeric_hook_url_is_an_error() {
        let path = scratch_config("hook-url-numeric", "[hooks]\nauthentication_url = 5\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_url"), "{}", error);
        assert!(error.contains("not a string"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn a_present_but_stringy_authentication_cache_secs_is_an_error() {
        let path = scratch_config(
            "cache-secs-stringy",
            "[hooks]\nauthentication_url = \"https://x\"\nauthentication_secret = \"s\"\n\
            authentication_cache_secs = \"60\"\n"
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("not an integer"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The auth-path instance of the negative-integer-wraps-to-huge-unsigned hole:
    /// `optional_integer` returns `i64`, and `.map(|v| v as u64)` turns a negative value into a
    /// value near `u64::MAX` instead of erroring. For `authentication_cache_secs` specifically
    /// this is a revocation-cache hole, not just a display glitch:
    /// `Duration::from_secs(u64::MAX)` (`server::serve`) makes the authentication-hook cache TTL
    /// effectively infinite, so a credential the provider has revoked is never re-checked again
    /// for the life of the process.
    #[test]
    fn a_negative_authentication_cache_secs_is_an_error() {
        let path = scratch_config(
            "cache-secs-negative",
            "[hooks]\nauthentication_url = \"https://x\"\nauthentication_secret = \"s\"\n\
            authentication_cache_secs = -1\n"
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("negative"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The same negative-wraps-to-huge-unsigned hole, applied to `max_body_mb` (`as u64`) while
    /// `optional_non_negative_integer` already exists to close it for `authentication_cache_secs`
    /// — a one-liner once the helper is there. `-1i64 as u64` would otherwise raise the body-size
    /// cap to `u64::MAX` MiB, silently defeating the disk-fill protection (§9.4b R9/D7) the cap
    /// exists for.
    #[test]
    fn a_negative_max_body_mb_is_an_error() {
        let path = scratch_config("max-body-mb-negative", "max_body_mb = -1\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"max_body_mb\""), "{}", error);
        assert!(error.contains("negative"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// Same again for `rebuild_after_lifts` (`as u32`): closes the negative-wraps-to-huge-unsigned
    /// case. Not addressed here: a positive value above `u32::MAX` still silently truncates on
    /// the `as u32` cast — that upper-bound case needs a field-specific ceiling, not the same
    /// zero-or-positive check, so it is left for a future round.
    #[test]
    fn a_negative_rebuild_after_lifts_is_an_error() {
        let path = scratch_config("rebuild-after-lifts-negative", "rebuild_after_lifts = -20\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"rebuild_after_lifts\""), "{}", error);
        assert!(error.contains("negative"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// Pre-existing behaviour (not new in this fix): a hook configured with only one of its
    /// URL/secret pair is refused — hook requests are signed, so a partial pair can never work.
    #[test]
    fn a_hook_with_only_a_url_is_an_error() {
        let path = scratch_config("hook-url-only", "[hooks]\nauthentication_url = \"https://x\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication"), "{}", error);
        assert!(error.contains("only one of"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: unrecognized keys error (Part 1(b) of the fix)
    // ---------------------------------------------------------------------------------

    /// The second reproduction from the fail-open finding: a mistyped top-level key must be a
    /// startup error, never a silently-never-applied setting.
    #[test]
    fn a_misspelled_top_level_key_is_an_error() {
        let path = scratch_config("misspelled-top-level", "tokenn = \"secret\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"tokenn\""), "{}", error);
        assert!(error.contains("top-level"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The mistyped-hook-key reproduction: `autentication_url` (missing an "h") must never be
    /// treated as "no authentication hook configured".
    #[test]
    fn a_misspelled_hooks_key_is_an_error() {
        let path = scratch_config(
            "misspelled-hooks",
            "[hooks]\nautentication_url = \"https://x\"\nautentication_secret = \"s\"\n"
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"autentication_url\""), "{}", error);
        assert!(error.contains("[hooks]"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: pre-existing failure modes untouched by this fix
    // ---------------------------------------------------------------------------------

    #[test]
    fn a_missing_config_file_is_an_error() {
        let path = std::env::temp_dir().join("forklift-server-main-test-does-not-exist.toml");
        let _ = std::fs::remove_file(&path);
        assert!(parse_config(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let path = scratch_config("invalid-toml", "not [ valid toml");
        assert!(parse_config(path.to_str().unwrap()).is_err());
        let _ = std::fs::remove_file(&path);
    }
}
