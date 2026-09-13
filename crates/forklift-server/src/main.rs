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

        /// Refuse request bodies over this size (MiB; default: 64 MiB, the largest
        /// legitimate object after chunking — the hash check gates correctness, this
        /// gates disk-fill abuse)
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

/// The static token as it enters the program: two independent sources (the `--token` flag, the
/// config file's `token` key), each normalized individually — an empty *or whitespace-only*
/// string is not a credential (`server::check_auth`'s doc, `server::ServeOptions::token`'s doc)
/// — *before* precedence between them is decided, rather than after the two are already merged
/// into one `Option<String>`.
///
/// That ordering is the actual fix, not a stylistic preference: `Option::or` only sees "was a
/// value supplied at all", so `Some("")` (a blank `--token`) counts as supplied and wins over a
/// real `Some("admin-secret")` from the config file — filtering *after* that merge, as the
/// single call site this replaces used to, can no longer recover the real value the merge
/// already discarded. Filtering each source first means a blank flag falls through to a real
/// config-file token exactly the way an *absent* flag already does.
///
/// Whitespace-only, not just empty (PR #124 round 3, F1): round 2 checked `is_empty()` here
/// while the identity rule added in the same round (`parse_operator_tokens`,
/// `authenticate_via_hook`) checked `trim().is_empty()`, on the argument that a token is only
/// ever compared byte-exact so a whitespace-only one presents no *comparison* hole the way an
/// untrimmed identity does. That argument answers a different question than the one this
/// function asks: not "should a real token's bytes be trimmed before comparing" (no — a token's
/// bytes are never trimmed, before or after this fix), but "does a whitespace-only value count as
/// a configured credential at all". It does not — a `--token "   "` either can never be
/// presented by a client at all on a transport that trims trailing whitespace off header values
/// (a silent lockout dressed up as a configured server), or, on a transport this crate makes no
/// assumption about *not* trimming, is presentable as a guessable, effectively-public credential.
/// Either way the server must refuse to start believing it has a real token, exactly as it
/// already refuses to for an empty one.
///
/// Returns the merged, already-normalized token, plus whether either raw source was
/// present-but-blank — carried only so `serve`'s startup refusal can name that case correctly
/// (F2 of PR #124 round 2: "no --token/token" is a misdiagnosis when a blank one was in fact
/// passed). It plays no role in authentication itself: by the time this returns, a blank
/// value from either source has already been discarded from the merged token.
fn merge_static_token(flag: Option<String>, file: Option<String>) -> (Option<String>, bool) {
    let is_blank = |value: &Option<String>| value.as_deref().is_some_and(|v| v.trim().is_empty());
    let flag_was_blank = is_blank(&flag);
    let file_was_blank = is_blank(&file);

    let non_blank = |value: Option<String>| value.filter(|v| !v.trim().is_empty());

    (non_blank(flag).or(non_blank(file)), flag_was_blank || file_was_blank)
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

    let (token, blank_token_supplied) = merge_static_token(token, file.token);

    let options = server::ServeOptions {
        root: root.or(file.root),
        warehouses: warehouses.or(file.warehouses),
        addr: addr.or(file.addr).unwrap_or("127.0.0.1:9418".to_string()),
        token,
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
        blank_token_supplied,
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
#[derive(Default)]
struct ConfigFile {
    root: Option<String>,
    warehouses: Option<String>,
    addr: Option<String>,

    /// The static bearer token, verbatim from the config file — the primary credential this
    /// head has (worth more than a hook's MAC key: it grants full access, including warehouse
    /// creation). `Debug` is hand-written below specifically so this field redacts: see
    /// `server::HookEndpoint`'s own hand-written `Debug` for the identical rationale (a future
    /// `tracing::debug!(?file)`, a panic message) — it applies verbatim here, and doubly so
    /// since every `parse_config(...).unwrap_err()` in this module's own tests would otherwise
    /// dump a real token straight into CI output the moment a test fixture used one.
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

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("root", &self.root)
            .field("warehouses", &self.warehouses)
            .field("addr", &self.addr)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("tokens", &self.tokens)
            .field("max_body_mb", &self.max_body_mb)
            .field("rebuild_after_lifts", &self.rebuild_after_lifts)
            .field("open", &self.open)
            .field("authentication_hook", &self.authentication_hook)
            .field("admission_hook", &self.admission_hook)
            .field("events_hook", &self.events_hook)
            .field("resolution_hook", &self.resolution_hook)
            .field("authentication_cache_secs", &self.authentication_cache_secs)
            .finish()
    }
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
/// all read via this function below, two of them through the field-specific ceilings just below
/// it). A negative `i64` surviving to `as u64`/`as u32` does not error, it wraps: `-1i64 as u64`
/// is `u64::MAX`. For `authentication_cache_secs` specifically that wrap is an auth-path hole,
/// not just a display glitch — `Duration::from_secs(u64::MAX)` makes the authentication-hook
/// cache TTL effectively infinite, so a credential the provider has revoked is never re-checked
/// again for the life of the process. Rejecting the negative value here, before the cast, closes
/// it the same way `optional_string`/`optional_bool` already close the analogous "present but
/// wrong shape" gap for their own types.
///
/// The sign check alone is not the whole story for either `authentication_cache_secs` or
/// `rebuild_after_lifts` (PR #124 round 3, F3/F4): a value can be non-negative and still be
/// nonsensical (`i64::MAX` seconds is the identical "never re-checked again" hole with the sign
/// removed) or silently truncating (`u32::MAX + 1` "wraps" on the eventual `as u32`, just via a
/// cast overflow rather than a sign flip). [`bounded_authentication_cache_secs`] and
/// [`optional_u32`] each layer their own magnitude check on top of this one for exactly those two
/// fields; `max_body_mb`'s `as u64` needs no equivalent ceiling here — see its own call site in
/// [`parse_config`] for why.
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

/// The ceiling [`bounded_authentication_cache_secs`] enforces on top of
/// [`optional_non_negative_integer`]'s sign check (PR #124 round 3, F3). This field bounds how
/// long a revoked credential can keep authenticating after the hook stops vouching for it
/// (`docs/format/HOOK_PROTOCOL.md`: "a revoked credential outlives its revocation by at most the
/// TTL") — it is a revocation-latency budget, not a general-purpose cache knob, and its
/// documented default is 60 seconds. One day is three orders of magnitude past that default —
/// room for any legitimate "reduce hook chatter" setting — while matching the one other
/// day-scale staleness window this same binary already accepts (`Gc`'s `--grace-hours`, default
/// 24) rather than inventing an unrelated number; it is nowhere near `i64::MAX` seconds (about
/// 292 billion years), which is what a sign check alone still lets through.
const MAX_AUTHENTICATION_CACHE_SECS: i64 = 24 * 60 * 60;

/// [`optional_non_negative_integer`]'s ceilinged counterpart for `authentication_cache_secs`
/// specifically: see [`MAX_AUTHENTICATION_CACHE_SECS`] for why that field, alone among the three
/// this module casts to an unsigned type, needs a magnitude check in addition to the sign check.
fn bounded_authentication_cache_secs(
    item: Option<&toml_edit::Item>, path: &str, key: &str
) -> Result<Option<u64>, String> {
    match optional_non_negative_integer(item, path, key)? {
        Some(v) if v > MAX_AUTHENTICATION_CACHE_SECS => Err(format!(
            "The config file \"{}\" has a \"{}\" entry of {} seconds, which is over the \
            {}-second (24h) ceiling: a revoked credential must not be able to outlive its \
            revocation by more than about a day.",
            path, key, v, MAX_AUTHENTICATION_CACHE_SECS
        )),
        other => Ok(other.map(|v| v as u64)),
    }
}

/// [`optional_non_negative_integer`]'s `u32`-bounded counterpart, for every field cast `as u32`
/// downstream (`rebuild_after_lifts`, the only one today). The sign check alone (round 1) does
/// not close this class: `optional_non_negative_integer` returns `i64`, and `v as u32` truncates
/// silently — for any `v` above `u32::MAX` — rather than erroring; `4294967297i64 as u32` is `1`,
/// not a value a caller could recognize as a wraparound. Unlike `max_body_mb`'s `as u64` (see
/// that field's call site in [`parse_config`] for why it is lossless and needs no counterpart
/// here), this cast genuinely can lose information, and silently: `rebuild_after_lifts =
/// 4294967297` would rebuild the bundle after every single lift instead of never (as configured),
/// with no warning that anything happened. CLAUDE.md: "silent breakage is still a bug."
fn optional_u32(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<u32>, String> {
    match optional_non_negative_integer(item, path, key)? {
        Some(v) => u32::try_from(v).map(Some).map_err(|_| format!(
            "The config file \"{}\" has a \"{}\" entry of {}, which is over u32::MAX ({}) and \
            would silently truncate.",
            path, key, v, u32::MAX
        )),
        None => Ok(None),
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
        Some(table) => bounded_authentication_cache_secs(
            table.get("authentication_cache_secs"), path, "hooks.authentication_cache_secs"
        )?,
    };

    Ok(ConfigFile {
        root: optional_string(doc.get("root"), path, "root")?,
        warehouses: optional_string(doc.get("warehouses"), path, "warehouses")?,
        addr: optional_string(doc.get("addr"), path, "addr")?,
        token: optional_string(doc.get("token"), path, "token")?,
        tokens: optional_string(doc.get("tokens"), path, "tokens")?,
        // No ceiling wrapper needed here (unlike `rebuild_after_lifts` just below): the cast is
        // `as u64`, and `optional_non_negative_integer` has already rejected every negative
        // value, so `v` here ranges over `0..=i64::MAX` — which is a strict subset of
        // `0..=u64::MAX` (`i64::MAX` is about half of `u64::MAX`). Every value that reaches this
        // cast round-trips losslessly; there is no magnitude this cast can silently misread the
        // way `rebuild_after_lifts`'s `as u32` can (PR #124 round 3, F4).
        max_body_mb: optional_non_negative_integer(doc.get("max_body_mb"), path, "max_body_mb")?
            .map(|v| v as u64),
        rebuild_after_lifts: optional_u32(
            doc.get("rebuild_after_lifts"), path, "rebuild_after_lifts"
        )?,
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

    /// The magnitude half of the same hole (PR #124 round 3, F3): the sign check alone lets
    /// `i64::MAX` seconds (about 292 billion years) straight through — non-negative, so
    /// `optional_non_negative_integer` accepts it — which is the identical
    /// never-re-checked-again behaviour `a_negative_authentication_cache_secs_is_an_error` closes
    /// for the negative side, achieved without ever going negative.
    /// `bounded_authentication_cache_secs` must refuse it instead.
    #[test]
    fn an_authentication_cache_secs_over_the_ceiling_is_an_error() {
        let path = scratch_config(
            "cache-secs-over-ceiling",
            &format!(
                "[hooks]\nauthentication_url = \"https://x\"\nauthentication_secret = \"s\"\n\
                authentication_cache_secs = {}\n",
                MAX_AUTHENTICATION_CACHE_SECS + 1
            )
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("ceiling"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The boundary itself must still be accepted: exactly the ceiling (24h) is a legal, if
    /// generous, cache TTL — not an off-by-one over the line the previous test pins.
    #[test]
    fn an_authentication_cache_secs_at_exactly_the_ceiling_is_accepted() {
        let path = scratch_config(
            "cache-secs-at-ceiling",
            &format!(
                "[hooks]\nauthentication_url = \"https://x\"\nauthentication_secret = \"s\"\n\
                authentication_cache_secs = {}\n",
                MAX_AUTHENTICATION_CACHE_SECS
            )
        );
        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(file.authentication_cache_secs, Some(MAX_AUTHENTICATION_CACHE_SECS as u64));

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
    /// case. The upper-bound case this used to defer — a positive value above `u32::MAX` still
    /// silently truncating on the `as u32` cast — is closed separately, by `optional_u32`; see
    /// `a_rebuild_after_lifts_over_u32_max_is_an_error` below (PR #124 round 3, F4).
    #[test]
    fn a_negative_rebuild_after_lifts_is_an_error() {
        let path = scratch_config("rebuild-after-lifts-negative", "rebuild_after_lifts = -20\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"rebuild_after_lifts\""), "{}", error);
        assert!(error.contains("negative"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The magnitude half of the class the sign check alone does not close (PR #124 round 3,
    /// F4): `rebuild_after_lifts = 4294967297` (`u32::MAX + 2`) is non-negative, so
    /// `optional_non_negative_integer` accepts it, and the old `.map(|v| v as u32)` truncated it
    /// to `1` with no warning — silently rebuilding the bundle after every single lift instead of
    /// never (as configured). `optional_u32` must refuse it instead.
    #[test]
    fn a_rebuild_after_lifts_over_u32_max_is_an_error() {
        let path = scratch_config(
            "rebuild-after-lifts-over-u32-max",
            &format!("rebuild_after_lifts = {}\n", u32::MAX as i64 + 2)
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"rebuild_after_lifts\""), "{}", error);
        assert!(error.contains("u32::MAX"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The boundary itself must still be accepted: `u32::MAX` is the largest legal value, not an
    /// off-by-one over the line `a_rebuild_after_lifts_over_u32_max_is_an_error` pins.
    #[test]
    fn a_rebuild_after_lifts_of_exactly_u32_max_is_accepted() {
        let path = scratch_config(
            "rebuild-after-lifts-at-u32-max",
            &format!("rebuild_after_lifts = {}\n", u32::MAX)
        );
        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(file.rebuild_after_lifts, Some(u32::MAX));

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

    // ---------------------------------------------------------------------------------
    // merge_static_token (F1 of PR #124 round 2): normalize each source before merging
    // ---------------------------------------------------------------------------------

    /// The exact repro from the finding: a blank `--token` must not beat a real config-file
    /// token. Before this fix, the merge was `token.or(file.token)` with the empty-string
    /// filter applied only afterward — `Option::or` sees `Some("")` as "a value was supplied"
    /// and picks it over `file.token`, so the real token was discarded and never recovered.
    #[test]
    fn a_blank_flag_token_falls_back_to_a_real_file_token() {
        let (token, blank) = merge_static_token(Some(String::new()), Some("admin-secret".to_string()));
        assert_eq!(token.as_deref(), Some("admin-secret"));
        assert!(blank, "the flag source was blank, even though a real token resulted");
    }

    /// The symmetric case: a blank config-file token must not beat a real `--token` flag either
    /// — normalization applies to both sources, not just the flag.
    #[test]
    fn a_blank_file_token_falls_back_to_a_real_flag_token() {
        let (token, blank) = merge_static_token(Some("flag-secret".to_string()), Some(String::new()));
        assert_eq!(token.as_deref(), Some("flag-secret"));
        assert!(blank, "the file source was blank, even though a real token resulted");
    }

    #[test]
    fn two_blank_sources_merge_to_no_token_and_report_blank() {
        let (token, blank) = merge_static_token(Some(String::new()), Some(String::new()));
        assert_eq!(token, None);
        assert!(blank);
    }

    #[test]
    fn two_absent_sources_merge_to_no_token_and_do_not_report_blank() {
        let (token, blank) = merge_static_token(None, None);
        assert_eq!(token, None);
        assert!(!blank, "nothing was ever supplied, blank or otherwise");
    }

    #[test]
    fn a_real_flag_token_wins_over_a_real_file_token() {
        let (token, blank) = merge_static_token(
            Some("flag-secret".to_string()), Some("file-secret".to_string())
        );
        assert_eq!(token.as_deref(), Some("flag-secret"), "the flag must still win between two real tokens");
        assert!(!blank);
    }

    // ---------------------------------------------------------------------------------
    // merge_static_token (F1 of PR #124 round 3): whitespace-only is blank too, not just empty
    // ---------------------------------------------------------------------------------

    /// The round-3 finding itself: a whitespace-only `--token` must not beat a real config-file
    /// token, exactly like a literally-empty one already does not. Before this fix, only
    /// `is_empty()` was checked here, so `Some("   ")` counted as "a value was supplied" and
    /// still won over `file.token` — the identical bug `a_blank_flag_token_falls_back_to_a_real_file_token`
    /// closes for the empty case, left open for whitespace.
    #[test]
    fn a_whitespace_only_flag_token_falls_back_to_a_real_file_token() {
        let (token, blank) = merge_static_token(
            Some("   ".to_string()), Some("admin-secret".to_string())
        );
        assert_eq!(token.as_deref(), Some("admin-secret"));
        assert!(blank, "the flag source was whitespace-only, even though a real token resulted");
    }

    /// The symmetric case, for the config-file source.
    #[test]
    fn a_whitespace_only_file_token_falls_back_to_a_real_flag_token() {
        let (token, blank) = merge_static_token(
            Some("flag-secret".to_string()), Some("   ".to_string())
        );
        assert_eq!(token.as_deref(), Some("flag-secret"));
        assert!(blank, "the file source was whitespace-only, even though a real token resulted");
    }

    #[test]
    fn two_whitespace_only_sources_merge_to_no_token_and_report_blank() {
        let (token, blank) = merge_static_token(Some("  ".to_string()), Some("\t".to_string()));
        assert_eq!(token, None);
        assert!(blank);
    }

    /// A real token is never trimmed, even though a whitespace-*only* one is treated as blank —
    /// the fix is a presence check, not a normalization: incidental internal or edge whitespace
    /// around real content must survive verbatim into the merged token (and, downstream, into
    /// `tokens_match`'s byte-exact comparison).
    #[test]
    fn a_flag_token_with_incidental_whitespace_survives_untrimmed() {
        let (token, blank) = merge_static_token(Some(" has spaces ".to_string()), None);
        assert_eq!(token.as_deref(), Some(" has spaces "));
        assert!(!blank, "this is a real, non-blank token");
    }

    // ---------------------------------------------------------------------------------
    // serve: the F1/F2 precedence fix, observed end to end through the merged options
    // ---------------------------------------------------------------------------------

    /// The full-stack version of `a_blank_flag_token_falls_back_to_a_real_file_token`: with the
    /// precedence bug, the real config-file token is discarded, no other auth source is
    /// configured, and `server::serve`'s startup gate refuses with "No authentication is
    /// configured". Fixed, the real token survives, the auth gate passes, and the function
    /// fails downstream instead — at root resolution, since the configured root does not exist.
    /// Distinguishing by which error comes back is the same technique `server.rs`'s own
    /// `serve_with_open_true_passes_the_auth_gate` uses; fully starting the server is out of
    /// scope for a fast unit test.
    #[tokio::test]
    async fn serve_falls_back_to_a_real_file_token_when_the_flag_token_is_blank() {
        let path = scratch_config("blank-flag-real-file-token", "token = \"admin-secret\"\n");

        let error = serve(
            Some("/does/not/exist/and/does/not/matter".to_string()),
            None,
            None,
            Some(String::new()),
            None,
            None,
            None,
            false,
            Some(path.to_str().unwrap().to_string()),
        ).await.unwrap_err();

        assert!(!error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("Error while resolving"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// The whitespace-only sibling of the test just above (PR #124 round 3, F1): a
    /// `--token "   "` must fall back to a real config-file token exactly like `--token ""`
    /// does — the whole point of round 3 being that round 2's fix only normalized emptiness.
    #[tokio::test]
    async fn serve_falls_back_to_a_real_file_token_when_the_flag_token_is_whitespace_only() {
        let path = scratch_config("whitespace-flag-real-file-token", "token = \"admin-secret\"\n");

        let error = serve(
            Some("/does/not/exist/and/does/not/matter".to_string()),
            None,
            None,
            Some("   ".to_string()),
            None,
            None,
            None,
            false,
            Some(path.to_str().unwrap().to_string()),
        ).await.unwrap_err();

        assert!(!error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("Error while resolving"), "{}", error);

        let _ = std::fs::remove_file(&path);
    }

    /// F2: when a blank `--token` is the *only* auth-flavored thing supplied (no real token
    /// anywhere, nothing else configured), the startup refusal must name that case correctly —
    /// "an empty --token/token", not "no --token/token", which sends an operator who passed the
    /// flag looking for a flag they never passed at all.
    #[tokio::test]
    async fn serve_names_a_blank_flag_token_distinctly_from_no_token_at_all() {
        let error = serve(
            Some("/does/not/exist/and/does/not/matter".to_string()),
            None,
            None,
            Some(String::new()),
            None,
            None,
            None,
            false,
            None,
        ).await.unwrap_err();

        assert!(error.contains("No authentication is configured"), "{}", error);
        assert!(error.contains("an empty --token/token"), "{}", error);
        assert!(!error.contains("no --token/token"), "{}", error);
    }

    // ---------------------------------------------------------------------------------
    // ConfigFile::fmt (F3 of PR #124 round 2): the static token must never print
    // ---------------------------------------------------------------------------------

    /// The same class `HookEndpoint::fmt` closes (`server.rs`'s
    /// `hook_endpoint_debug_never_prints_the_secret`), applied to the higher-value secret: the
    /// static bearer token, which grants full access including warehouse creation, versus a
    /// hook's MAC key which only signs requests. Checks both the value's absence and the field
    /// name's presence, so a future edit cannot "fix" the struct literal without the test
    /// noticing the field itself vanished too.
    #[test]
    fn config_file_debug_never_prints_the_token() {
        let path = scratch_config("debug-redaction", "token = \"s3cr3t-value\"\n");
        let file = parse_config(path.to_str().unwrap()).unwrap();
        let formatted = format!("{:?}", file);

        assert!(!formatted.contains("s3cr3t-value"), "{}", formatted);
        assert!(formatted.contains("token"), "{}", formatted);
        assert!(formatted.contains("redacted"), "{}", formatted);

        let _ = std::fs::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // F6: the max_body_mb default is documented accurately (it is NOT unlimited)
    // ---------------------------------------------------------------------------------

    /// `serve` falls back to `DefaultBodyLimit::max(object_utils::MAX_OBJECT_BYTES)` when
    /// `max_body_mb` is unset — the disk-fill cap defaults ON, not off. Both the clap `--help`
    /// text and `docs/SERVER.md`'s worked example used to say "unlimited", the opposite of the
    /// truth. Mechanized against the real constant (rather than a hand-typed "64" in this test
    /// too) so the two prose surfaces cannot drift from it silently again.
    #[test]
    fn the_max_body_mb_default_is_documented_correctly_everywhere() {
        let expected_mib = forklift_core::util::object_utils::MAX_OBJECT_BYTES / (1024 * 1024);
        let expected_phrase = format!("default: {} MiB", expected_mib);

        let command = <Cli as clap::CommandFactory>::command();
        let serve_command = command.find_subcommand("serve").expect("a \"serve\" subcommand");
        let arg = serve_command.get_arguments()
            .find(|a| a.get_id().as_str() == "max_body_mb")
            .expect("a \"max_body_mb\" argument");
        let help = arg.get_help().map(|s| s.to_string()).unwrap_or_default();

        assert!(help.contains(&expected_phrase), "{}", help);
        assert!(!help.to_lowercase().contains("unlimited"), "{}", help);

        let server_md = include_str!("../../../docs/SERVER.md");
        let line = server_md.lines()
            .find(|line| line.trim_start().starts_with("max_body_mb"))
            .expect("docs/SERVER.md documents max_body_mb");

        assert!(line.contains(&expected_phrase), "{}", line);
        assert!(!line.to_lowercase().contains("unlimited"), "{}", line);
    }
}
