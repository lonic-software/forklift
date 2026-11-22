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

use forklift_core::util::token_utils::TokenDigest;

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

        /// Require a bearer token whose SHA-256 digest is this, 64 lowercase hex characters
        /// (also gates warehouse creation; the server never holds the token itself)
        #[arg(long = "token-sha256", value_name = "HEX")]
        token_sha256: Option<String>,

        /// A TOML file of per-operator token digests:
        /// [operators_sha256] "<SHA-256 of the token>" = "<identifier>"
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

        /// Run at most this many costly requests at once (ref updates, pallet deletions, trust
        /// installs, batch fetches); past it a request waits briefly, then is refused with a 503
        /// and Retry-After (default: the host's available parallelism)
        #[arg(long)]
        max_heavy_requests: Option<u32>,

        /// Let each caller send at most this many cheap requests a minute (every authenticated
        /// route but the costly ones); past it a request is refused with a 429 and Retry-After
        /// (default: no limit; at least 4)
        #[arg(long)]
        light_requests_per_minute: Option<u32>,

        /// Let each caller send at most this many costly requests a minute; past it a request is
        /// refused with a 429 and Retry-After (default: no limit; at least 4)
        #[arg(long)]
        heavy_requests_per_minute: Option<u32>,

        /// Serve with no authentication at all: the explicit opt-out. Without this (and
        /// without --token-sha256/--tokens/a configured authentication hook), the server refuses
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

    /// Upgrade a warehouse to the format this version reads (refused while it is served)
    #[command(
        long_about = "Upgrade the warehouse at --root to the format this version of forklift-server \
                      reads, one format at a time, or finish an upgrade that was interrupted. The \
                      server refuses to serve a warehouse in another format, or one whose upgrade has \
                      not finished. The upgrade needs the warehouse to itself: stop every server \
                      serving it, and let any bundle or gc finish, before running it. Upgrades are \
                      one-way."
    )]
    Upgrade {
        /// The warehouse root to upgrade
        #[arg(long)]
        root: String,
    },

    /// Build the bundle served at /v1/bundles/latest (see BUNDLE_FORMAT.md)
    Bundle {
        /// The warehouse root to bundle
        #[arg(long)]
        root: String,
    },

    /// Delete unreferenced objects (mark-and-sweep from every live root)
    #[command(
        long_about = "Delete objects no pallet head, tombstone, trust pin, or parked, consolidating or \
                      staged work reaches (a failed or abandoned lift \
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

    /// List, drop or repair the tombstones that pallets deleted in a served warehouse leave
    #[command(
        long_about = "A pallet deleted in a warehouse that has been served leaves a tombstone: \
                      the head its ref held, which gc keeps, so the clones that freed that \
                      history against the server can fetch it again. The name stays reserved \
                      for that history until a re-creation that acknowledges it clears it, or \
                      until it is dropped here. A repaired list can also keep heads with no \
                      name: history the lost list may have held, kept until dropped by head."
    )]
    Tombstone {
        #[command(subcommand)]
        action: TombstoneAction,
    },

    /// Withhold a file's content on its host's authority, or list what is withheld
    #[command(
        long_about = "Withholding is a takedown: the host stops serving a file's content and \
                      refuses to take it, at once, whatever names it, and a read or upload of it is \
                      answered with the host's statement. Pallets keep naming the file; the \
                      warehouse's history is unchanged. Clients keep the copies they hold. It \
                      takes effect while the warehouse is being served, and the server deletes its \
                      own copy within seconds, unless metadata history, the history the trust \
                      anchor adopted, or a checkout's unfinished work still names it."
    )]
    Withhold {
        #[command(subcommand)]
        action: WithholdAction,
    },
}

/// What `forklift-server withhold` does.
#[derive(Subcommand)]
enum WithholdAction {
    /// Withhold a file's content, by its object hash, with a statement saying why. Withholding a
    /// hash again replaces its statement
    Add {
        /// The warehouse root to change
        #[arg(long)]
        root: String,

        /// The content's object hash
        hash: String,

        /// Why it is withheld, as a refused request is told: one plain line of at most 1000
        /// characters, with no control character, separator or direction mark
        #[arg(long)]
        statement: String,
    },

    /// List each withheld hash and its statement, and what a repair of the record left unaccounted
    /// that still is: each such file's hash after `unaccounted`, and each folder or part of
    /// history it could not walk beneath after `undescended`. The warehouse is not served while
    /// either is listed
    List {
        /// The warehouse root to read
        #[arg(long)]
        root: String,
    },

    /// Rebuild a lost or damaged record of withheld content while the warehouse is not served.
    /// A copy of the damaged record is kept aside and every entry that still reads is kept; a hash
    /// whose entry lost its statement stays withheld under a placeholder to restate, and every
    /// absent file the history names that no record holds is listed, to withhold again. Each such
    /// file, and each absent folder or part of history the walk could not see beneath, is recorded
    /// as unaccounted, and the warehouse is not served until each is accounted for; run again over
    /// a record that reads whole, it walks again for what an earlier repair left unaccounted
    Repair {
        /// The warehouse root to repair
        #[arg(long)]
        root: String,
    },
}

/// What `forklift-server tombstone` does.
#[derive(Subcommand)]
enum TombstoneAction {
    /// List each deleted pallet's tombstone and the head it holds, and every kept head
    List {
        /// The warehouse root to read
        #[arg(long)]
        root: String,
    },

    /// Drop a deleted pallet's tombstone, or a kept head: the name is free for unrelated history,
    /// and the history it held is collected by the next gc unless something else reaches it.
    /// Refused while the warehouse is being served
    Drop {
        /// The warehouse root to change
        #[arg(long)]
        root: String,

        /// The deleted pallet's name
        #[arg(required_unless_present = "head")]
        name: Option<String>,

        /// Drop this kept head instead of a named tombstone
        #[arg(long, conflicts_with = "name")]
        head: Option<String>,
    },

    /// Rebuild a lost or damaged tombstone list, or a lost served mark, while the warehouse is
    /// not served. The damaged list is moved aside and every entry that still reads is kept; every
    /// other history that no pallet, office pin or tombstone holds is kept too, under the name its
    /// deletion recorded in a bay's undo journal when the journals agree on it, else with no name
    Repair {
        /// The warehouse root to repair
        #[arg(long)]
        root: String,
    },
}

/// Every thread that reads objects needs core's
/// [`WORKER_STACK_SIZE`](forklift_core::util::fanout_utils::WORKER_STACK_SIZE), because a delta
/// chain's reconstruction recurses. The serve handlers read on the runtime's `spawn_blocking`
/// threads, so those get it instead of tokio's 2 MiB default. `bundle`, `gc` and `prepare` run
/// inline on the thread polling [`async_main`], so that is a worker thread with the same stack
/// rather than the process's main thread (1 MiB on Windows).
fn main() {
    forklift_core::util::fanout_utils::block_on_worker_stack("forklift-server-main", async_main);
}

/// Parse the command line and run it.
async fn async_main() {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Serve {
            root, warehouses, addr, token_sha256, tokens, max_body_mb, rebuild_after_lifts, max_heavy_requests,
            light_requests_per_minute, heavy_requests_per_minute, open, config,
        } => serve(
            root, warehouses, addr, token_sha256, tokens, max_body_mb, rebuild_after_lifts, max_heavy_requests,
            RequestLimitFlags { light_requests_per_minute, heavy_requests_per_minute }, open, config,
        ).await,
        Command::Prepare { root } => prepare(&root),
        Command::Upgrade { root } => upgrade(&root),
        Command::Bundle { root } => bundle(&root),
        Command::Gc { root, grace_hours } => gc(&root, grace_hours),
        Command::Tombstone { action: TombstoneAction::List { root } } => list_tombstones(&root),
        Command::Tombstone { action: TombstoneAction::Drop { root, name, head } } => drop_tombstone(&root, name.as_deref(), head.as_deref()),
        Command::Tombstone { action: TombstoneAction::Repair { root } } => repair_tombstones(&root),
        Command::Withhold { action: WithholdAction::Add { root, hash, statement } } => withhold(&root, &hash, &statement),
        Command::Withhold { action: WithholdAction::List { root } } => list_withheld(&root),
        Command::Withhold { action: WithholdAction::Repair { root } } => repair_withheld(&root),
    };

    if let Err(e) = result {
        // A core refusal arrives framed: print its message and next step, never the frame.
        eprintln!("{}", forklift_core::error::CoreError::from(e).human_text());
        std::process::exit(1);
    }
}

/// Merge the flags with the config file (flags win) and serve.
#[allow(clippy::too_many_arguments)]
async fn serve(root: Option<String>,
               warehouses: Option<String>,
               addr: Option<String>,
               token_sha256: Option<String>,
               tokens: Option<String>,
               max_body_mb: Option<u64>,
               rebuild_after_lifts: Option<u32>,
               max_heavy_requests: Option<u32>,
               limits: RequestLimitFlags,
               open: bool,
               config: Option<String>) -> Result<(), String> {
    let options = serve_options(
        root, warehouses, addr, token_sha256, tokens, max_body_mb, rebuild_after_lifts, max_heavy_requests, limits,
        open, config,
    )?;

    server::serve(options).await
}

/// The per-caller request limit flags of `serve` (`--light-requests-per-minute`,
/// `--heavy-requests-per-minute`).
#[derive(Default)]
struct RequestLimitFlags {
    light_requests_per_minute: Option<u32>,
    heavy_requests_per_minute: Option<u32>,
}

/// The flags merged with the config file `config` names, if any: a flag that is given wins.
#[allow(clippy::too_many_arguments)]
fn serve_options(root: Option<String>,
               warehouses: Option<String>,
               addr: Option<String>,
               token_sha256: Option<String>,
               tokens: Option<String>,
               max_body_mb: Option<u64>,
               rebuild_after_lifts: Option<u32>,
               max_heavy_requests: Option<u32>,
               limits: RequestLimitFlags,
               open: bool,
               config: Option<String>) -> Result<server::ServeOptions, String> {
    let file = match config {
        Some(path) => parse_config(&path)?,
        None => ConfigFile::default(),
    };

    // The flag wins, but a malformed digest is refused from either source: neither is ever
    // passed over unread.
    let flag_digest = token_sha256
        .map(|hex| TokenDigest::parse_hex(&hex))
        .transpose()
        .map_err(|e| format!("The --token-sha256 value is invalid: {}.", e))?;

    let options = server::ServeOptions {
        root: root.or(file.root),
        warehouses: warehouses.or(file.warehouses),
        addr: addr.or(file.addr).unwrap_or("127.0.0.1:9418".to_string()),
        token_sha256: flag_digest.or(file.token_sha256),
        tokens: tokens.or(file.tokens),
        max_body_mb: max_body_mb.or(file.max_body_mb),
        rebuild_after_lifts: rebuild_after_lifts.or(file.rebuild_after_lifts),
        authentication_hook: file.authentication_hook,
        admission_hook: file.admission_hook,
        events_hook: file.events_hook,
        resolution_hook: file.resolution_hook,
        authentication_cache_secs: file.authentication_cache_secs,
        admission_cache_secs: file.admission_cache_secs,
        max_heavy_requests: max_heavy_requests.or(file.max_heavy_requests),
        light_requests_per_minute: limits.light_requests_per_minute.or(file.light_requests_per_minute),
        heavy_requests_per_minute: limits.heavy_requests_per_minute.or(file.heavy_requests_per_minute),
        // The flag is the affirmative case: passing `--open` always opts out, regardless of
        // the config file. There is no flag-side way to *cancel* `open = true` in the file
        // (matching every other flag/file pair here, where "flags override" only ever means
        // "a flag can supply a value", never "a flag can unset one") — remove it from the
        // file to close that hole.
        open: open || file.open.unwrap_or(false),
    };

    Ok(options)
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
/// admission_cache_secs = 60
/// ```
#[derive(Default)]
struct ConfigFile {
    root: Option<String>,
    warehouses: Option<String>,
    addr: Option<String>,

    /// The static bearer token's digest (`token_sha256`). Its `Debug` prints no digest.
    token_sha256: Option<TokenDigest>,

    tokens: Option<String>,
    max_body_mb: Option<u64>,
    rebuild_after_lifts: Option<u32>,
    max_heavy_requests: Option<u32>,
    light_requests_per_minute: Option<u32>,
    heavy_requests_per_minute: Option<u32>,

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
    admission_cache_secs: Option<u64>,
}

impl std::fmt::Debug for ConfigFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigFile")
            .field("root", &self.root)
            .field("warehouses", &self.warehouses)
            .field("addr", &self.addr)
            .field("token_sha256", &self.token_sha256)
            .field("tokens", &self.tokens)
            .field("max_body_mb", &self.max_body_mb)
            .field("rebuild_after_lifts", &self.rebuild_after_lifts)
            .field("max_heavy_requests", &self.max_heavy_requests)
            .field("light_requests_per_minute", &self.light_requests_per_minute)
            .field("heavy_requests_per_minute", &self.heavy_requests_per_minute)
            .field("open", &self.open)
            .field("authentication_hook", &self.authentication_hook)
            .field("admission_hook", &self.admission_hook)
            .field("events_hook", &self.events_hook)
            .field("resolution_hook", &self.resolution_hook)
            .field("authentication_cache_secs", &self.authentication_cache_secs)
            .field("admission_cache_secs", &self.admission_cache_secs)
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
            "The config file \"{}\" has a \"{}\" entry that is negative; it must be zero or \
            positive.",
            path, key
        )),
        other => Ok(other),
    }
}

/// [`optional_non_negative_integer`]'s ceilinged counterpart for `authentication_cache_secs`
/// specifically: this field, alone among the three this module casts to an unsigned type, needs
/// a magnitude check in addition to the sign check (a value can be non-negative and still be
/// nonsensical — `i64::MAX` seconds is the "never re-checked again" hole with the sign flip
/// removed). The ceiling itself is [`server::MAX_HOOK_CACHE_SECS`], not a local copy
/// (PR #124 round 5, F4): `server::serve` re-enforces the identical bound at the `ServeOptions`
/// boundary, and a config-file-specific error message here is the reason this function still
/// exists rather than every caller going straight through `server`'s check — see that
/// constant's own doc for the rationale behind the number.
fn bounded_authentication_cache_secs(
    item: Option<&toml_edit::Item>, path: &str, key: &str
) -> Result<Option<u64>, String> {
    match optional_non_negative_integer(item, path, key)? {
        Some(v) if v > server::MAX_HOOK_CACHE_SECS as i64 => Err(format!(
            "The config file \"{}\" has a \"{}\" entry over the {}-second (24h) ceiling: a \
            revoked credential must not be able to outlive its revocation by more than about a \
            day.",
            path, key, server::MAX_HOOK_CACHE_SECS
        )),
        other => Ok(other.map(|v| v as u64)),
    }
}

/// [`bounded_authentication_cache_secs`]'s sibling for `admission_cache_secs`, under the same
/// ceiling: it bounds how long a suspended caller keeps being admitted new reads.
fn bounded_admission_cache_secs(
    item: Option<&toml_edit::Item>, path: &str, key: &str
) -> Result<Option<u64>, String> {
    match optional_non_negative_integer(item, path, key)? {
        Some(v) if v > server::MAX_HOOK_CACHE_SECS as i64 => Err(format!(
            "The config file \"{}\" has a \"{}\" entry over the {}-second (24h) ceiling: a \
            suspended caller must not keep being admitted for more than about a day.",
            path, key, server::MAX_HOOK_CACHE_SECS
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
            "The config file \"{}\" has a \"{}\" entry over u32::MAX ({}), which would silently \
            truncate.",
            path, key, u32::MAX
        )),
        None => Ok(None),
    }
}

/// [`optional_u32`]'s counterpart for a count that must be positive, `max_heavy_requests`: zero
/// would refuse every costly request, and a negative value is no count. `server::heavy_capacity`
/// refuses zero again where the flag and this file converge, because `server::ServeOptions` is
/// `pub`.
fn positive_u32(item: Option<&toml_edit::Item>, path: &str, key: &str) -> Result<Option<u32>, String> {
    match optional_integer(item, path, key)? {
        Some(v) if v < 1 => Err(format!(
            "The config file \"{}\" has a \"{}\" entry below one; it must be positive (zero \
            would refuse every costly request), or left unset.",
            path, key
        )),
        Some(v) => u32::try_from(v).map(Some).map_err(|_| format!(
            "The config file \"{}\" has a \"{}\" entry over u32::MAX ({}).",
            path, key, u32::MAX
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
const CONFIG_TOP_LEVEL_KEYS: [&str; 12] = [
    "root", "warehouses", "addr", "token_sha256", "tokens", "max_body_mb", "rebuild_after_lifts",
    "max_heavy_requests", "light_requests_per_minute", "heavy_requests_per_minute", "open", "hooks",
];

/// The keys `parse_config` understands inside `[hooks]` — four independent `{name}_url` /
/// `{name}_secret` pairs (`docs/format/HOOK_PROTOCOL.md`) plus the two cache settings.
/// Same role as [`CONFIG_TOP_LEVEL_KEYS`]: a typo'd `autentication_url` is a startup error, not
/// a silently-never-configured hook (the second fail-open path this fix closes — with no other
/// auth configured, a hook that silently failed to parse used to leave `check_auth` reading
/// "nothing is configured" and serving every request as `Principal::Open`).
const HOOKS_KEYS: [&str; 10] = [
    "authentication_url", "authentication_secret",
    "admission_url", "admission_secret",
    "events_url", "events_secret",
    "resolution_url", "resolution_secret",
    "authentication_cache_secs", "admission_cache_secs",
];

/// Refuse a key that is not in `known` — the mechanism behind [`CONFIG_TOP_LEVEL_KEYS`] and
/// [`HOOKS_KEYS`]: every key actually present in the parsed table must be one this function
/// recognizes, or this errors naming the file, where the key was found and its line. The key
/// itself is never quoted: an unrecognized key is whatever was typed, and a token pasted into
/// the config file is one.
///
/// Takes `&dyn TableLike` rather than `&toml_edit::Table` so it works on an inline table too
/// (`hooks = { authentication_url = "…", authentication_secret = "…" }`), not only the
/// `[hooks]` form — `Table` and `InlineTable` are two distinct types in this crate, and only
/// `TableLike` is implemented by both.
fn reject_unknown_keys(table: &dyn toml_edit::TableLike,
                       known: &[&str],
                       content: &str,
                       path: &str,
                       location: &str) -> Result<(), String> {
    for (key, _) in table.iter() {
        if !known.contains(&key) {
            let line = table.key(key).and_then(|key| key.span())
                .map(|span| format!(" (line {})", server::line_of(content, span.start)))
                .unwrap_or_default();
            return Err(format!(
                "The config file \"{}\" has an unrecognized key in {}{}. Recognized keys: {}.",
                path, location, line, known.join(", ")
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

    // Parsed as a `Document`, not a `DocumentMut`, which keeps each key's place in the file for
    // the unrecognized-key refusal.
    let doc = toml_edit::Document::parse(content.as_str())
        .map_err(|e| format!("The config file \"{}\" is not valid TOML{}.", path, server::toml_error_line(&content, &e)))?;

    // A plaintext token is refused by name, so the message can say what replaced it. Its value
    // is never echoed.
    if doc.contains_key("token") {
        return Err(format!(
            "The config file \"{}\" sets \"token\", but forklift-server holds no plaintext \
            credential: set token_sha256 to the token's SHA-256 digest (64 lowercase hex \
            characters) and remove token.",
            path
        ));
    }

    reject_unknown_keys(doc.as_table(), &CONFIG_TOP_LEVEL_KEYS, &content, path, "the top-level config")?;

    let hooks = match doc.get("hooks") {
        None => None,
        Some(item) => Some(item.as_table_like().ok_or_else(|| format!(
            "The config file \"{}\" has a \"hooks\" entry that is present but is not a table.",
            path
        ))?),
    };

    if let Some(hooks) = hooks {
        reject_unknown_keys(hooks, &HOOKS_KEYS, &content, path, "the [hooks] table")?;
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

    let admission_cache_secs = match hooks {
        None => None,
        Some(table) => bounded_admission_cache_secs(
            table.get("admission_cache_secs"), path, "hooks.admission_cache_secs"
        )?,
    };

    Ok(ConfigFile {
        root: optional_string(doc.get("root"), path, "root")?,
        warehouses: optional_string(doc.get("warehouses"), path, "warehouses")?,
        addr: optional_string(doc.get("addr"), path, "addr")?,
        token_sha256: optional_string(doc.get("token_sha256"), path, "token_sha256")?
            .map(|hex| TokenDigest::parse_hex(&hex))
            .transpose()
            .map_err(|e| format!("The config file \"{}\" has an invalid \"token_sha256\": {}.", path, e))?,
        tokens: optional_string(doc.get("tokens"), path, "tokens")?,
        // No ceiling wrapper needed here (unlike `rebuild_after_lifts` just below): the cast is
        // `as u64`, and `optional_non_negative_integer` has already rejected every negative
        // value, so `v` here ranges over `0..=i64::MAX` — which is a strict subset of
        // `0..=u64::MAX` (`i64::MAX` is about half of `u64::MAX`). Every value that reaches this
        // cast round-trips losslessly; there is no magnitude this cast can silently misread the
        // way `rebuild_after_lifts`'s `as u32` can (PR #124 round 3, F4). That covers only the
        // overflow half of "non-negative but nonsensical" — `0` is non-negative, round-trips
        // losslessly, and is still nonsensical (it would reject every request body); that half is
        // refused downstream, in `server::body_limit_bytes`, the one place both this config-file
        // path and the `--max-body-mb` flag path converge (PR #124 round 4, F6).
        max_body_mb: optional_non_negative_integer(doc.get("max_body_mb"), path, "max_body_mb")?
            .map(|v| v as u64),
        rebuild_after_lifts: optional_u32(
            doc.get("rebuild_after_lifts"), path, "rebuild_after_lifts"
        )?,
        max_heavy_requests: positive_u32(doc.get("max_heavy_requests"), path, "max_heavy_requests")?,
        // The lowest limit is refused in `server::RequestLimits::new`, where this file and the
        // flags converge.
        light_requests_per_minute: optional_u32(
            doc.get("light_requests_per_minute"), path, "light_requests_per_minute"
        )?,
        heavy_requests_per_minute: optional_u32(
            doc.get("heavy_requests_per_minute"), path, "heavy_requests_per_minute"
        )?,
        open: optional_bool(doc.get("open"), path, "open")?,
        authentication_hook: hook_of("authentication")?,
        admission_hook: hook_of("admission")?,
        events_hook: hook_of("events")?,
        resolution_hook: hook_of("resolution")?,
        authentication_cache_secs,
        admission_cache_secs,
    })
}

/// Prepare a bare warehouse: the same layout `forklift prepare` creates — the server
/// simply never uses the working-directory parts (the inventory).
fn prepare(root: &str) -> Result<(), String> {
    std::fs::create_dir_all(root).map_err(|e| format!("Error while creating \"{}\": {}", root, e))?;
    let resolved = server::canonical_root(root)?;
    // An unfinished clone, a bay's folder, or one with something in it but no warehouse marker,
    // is not prepare's to take.
    forklift_core::util::warehouse_utils::check_preparable(&resolved, forklift_core::util::warehouse_utils::Head::Server)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&resolved);

    let created = forklift_core::util::warehouse_utils::prepare_warehouse(forklift_core::util::warehouse_utils::PrepareMode::Full)?;

    if created.is_empty() {
        println!("Nothing to do.");
    } else {
        println!("Prepared warehouse at \"{}\".", root);
    }

    Ok(())
}

/// Upgrade the warehouse at `root` to the format this build reads. No serve lock is taken: the
/// format lease the upgrade holds exclusively already refuses while a server, a bundle or a gc
/// has the warehouse open.
fn upgrade(root: &str) -> Result<(), String> {
    use forklift_core::util::{upgrade_utils, warehouse_utils};

    let resolved = server::canonical_root(root)?;
    let report = upgrade_utils::upgrade_warehouse(&resolved, warehouse_utils::Head::Server, &upgrade_utils::CHAIN)?;

    for line in report.summary() {
        println!("{}", line);
    }

    Ok(())
}

/// Build the bundle served at `/v1/bundles/latest`.
fn bundle(root: &str) -> Result<(), String> {
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

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
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    // Refuse while a server is serving this root: gc would sweep the server's in-flight
    // staged objects, and a lift slower than the grace period then fails its ref update. Held for
    // the whole command so a server cannot start mid-sweep.
    let _serve_lock = forklift_core::util::lock_utils::ServeLock::acquire()
        .map_err(|e| forklift_core::util::lock_utils::refusing_to("gc", e))?;

    let stats = forklift_core::util::gc_utils::collect_garbage(grace_hours * 3600)?;

    println!(
        "Scanned {} object(s): deleted {} unreferenced, kept {} within the {}h grace \
        period and {} that packed content is stored against.",
        stats.scanned, stats.deleted, stats.kept_recent, grace_hours, stats.kept_for_deltas
    );

    Ok(())
}

/// List the tombstones of the warehouse at `root`.
fn list_tombstones(root: &str) -> Result<(), String> {
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    let tombstones = forklift_core::util::tombstone_utils::read_tombstones()?;

    if tombstones.is_empty() {
        println!("No deleted pallet has a tombstone in this warehouse.");
    }

    for (pallet, head) in &tombstones.named {
        println!("{} {}", head, pallet);
    }

    for head in &tombstones.kept {
        println!("{} (kept, no name)", head);
    }

    Ok(())
}

/// Drop deleted pallet `name`'s tombstone, or kept head `head`, in the warehouse at `root`.
fn drop_tombstone(root: &str, name: Option<&str>, head: Option<&str>) -> Result<(), String> {
    use forklift_core::util::tombstone_utils;

    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    match (name, head) {
        (_, Some(head)) => {
            tombstone_utils::drop_kept_head(head)?;
            println!("Dropped kept head {}. The next gc collects its history unless something else reaches it.", head);
        }
        (Some(name), None) => {
            let head = tombstone_utils::drop_tombstone(name)?;
            println!(
                "Dropped the tombstone of pallet \"{}\" (it held {}). The name is free, and the next gc \
                collects that history unless something else reaches it.",
                name, head
            );
        }
        (None, None) => return Err("Name the deleted pallet, or a kept head with --head.".to_string()),
    }

    Ok(())
}

/// Repair the tombstone list and served mark of the warehouse at `root`.
fn repair_tombstones(root: &str) -> Result<(), String> {
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    let repaired = forklift_core::util::tombstone_repair::repair_tombstones()?;

    for line in repaired.report() {
        println!("{}", line);
    }

    Ok(())
}

/// Withhold `hash` in the warehouse at `root`, with `statement`.
fn withhold(root: &str, hash: &str, statement: &str) -> Result<(), String> {
    use forklift_core::util::withholding::{self, Withheld};

    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    match withholding::withhold(hash, statement)? {
        Withheld::Added => println!(
            "Withheld {}: it is no longer served or taken, and a read or upload of it is given \
            the statement.", hash
        ),
        Withheld::Restated => println!("{} was withheld already; its statement is replaced.", hash),
        Withheld::Unchanged => println!("{} is withheld already, with this statement.", hash),
    }

    Ok(())
}

/// Repair the record of withheld content of the warehouse at `root`.
fn repair_withheld(root: &str) -> Result<(), String> {
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    let repaired = forklift_core::util::withheld_repair::repair_withheld()?;

    for line in repaired.report() {
        println!("{}", line);
    }

    Ok(())
}

/// List what the warehouse at `root` withholds.
fn list_withheld(root: &str) -> Result<(), String> {
    let opened = server::open_root(root)?;
    let _scope = forklift_core::globals::StorageRootScope::enter(&opened.root);

    let withheld = forklift_core::util::withholding::read_withheld()?;
    let applied = forklift_core::util::applied_redactions::read_applied()?;
    let outstanding = forklift_core::util::withholding::outstanding_unaccounted(&withheld, &applied)?;

    if withheld.is_empty() && outstanding.is_empty() && withheld.undescended().next().is_none() {
        println!("This warehouse withholds nothing.");
    }

    for (hash, statement) in withheld.entries() {
        println!("{} {}", hash, statement);
    }
    for hash in outstanding {
        println!("unaccounted {}", hash);
    }
    for hash in withheld.undescended() {
        println!("undescended {}", hash);
    }

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
        assert_eq!(file.token_sha256, None);
        assert_eq!(file.tokens, None);
        assert_eq!(file.max_body_mb, None);
        assert_eq!(file.rebuild_after_lifts, None);
        assert_eq!(file.max_heavy_requests, None);
        assert_eq!(file.light_requests_per_minute, None);
        assert_eq!(file.heavy_requests_per_minute, None);
        assert_eq!(file.open, None);
        assert!(file.authentication_hook.is_none());
        assert!(file.admission_hook.is_none());
        assert!(file.events_hook.is_none());
        assert!(file.resolution_hook.is_none());
        assert_eq!(file.authentication_cache_secs, None);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    #[test]
    fn a_fully_populated_config_round_trips() {
        let path = scratch_config("full", r#"
            root = "/srv/forklift/wh"
            addr = "127.0.0.1:9418"
            token_sha256 = "2bb80d537b1da3e38bd30361aa855686bde0eacd7162fef6a25fe97bf527a25b"
            tokens = "/etc/forklift/tokens.toml"
            max_body_mb = 4096
            rebuild_after_lifts = 20
            max_heavy_requests = 6
            light_requests_per_minute = 6000
            heavy_requests_per_minute = 120
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
        assert_eq!(file.token_sha256, Some(TokenDigest::of_token("secret").unwrap()));
        assert_eq!(file.max_body_mb, Some(4096));
        assert_eq!(file.rebuild_after_lifts, Some(20));
        assert_eq!(file.max_heavy_requests, Some(6));
        assert_eq!(file.light_requests_per_minute, Some(6000));
        assert_eq!(file.heavy_requests_per_minute, Some(120));
        assert_eq!(file.open, Some(true));
        assert_eq!(file.authentication_hook.as_ref().map(|h| h.url.as_str()),
                   Some("https://provider.example/hooks/auth"));
        assert_eq!(file.authentication_cache_secs, Some(60));

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        assert!(error.contains("an unrecognized key in the [hooks] table (line 1)"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: present-but-wrong-typed values error (Part 1 of the fix)
    // ---------------------------------------------------------------------------------

    /// A hand-edited, unquoted `token_sha256 = 12345` must be a startup error naming the file
    /// and the key, never silently parsed as "no token configured".
    #[test]
    fn a_present_but_numeric_token_digest_is_an_error_naming_the_key() {
        let path = scratch_config("token-numeric", "token_sha256 = 12345\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"token_sha256\""), "{}", error);
        assert!(error.contains("not a string"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// Zero and negative counts are refused naming the key; one is the smallest accepted.
    ///
    /// Reverts: read the key with `optional_u32` → zero parses → red; drop the key from
    /// `CONFIG_TOP_LEVEL_KEYS` → every value is an unrecognized key → red.
    #[test]
    fn max_heavy_requests_must_be_a_positive_count() {
        for (value, expected) in [
            ("0", "has a \"max_heavy_requests\" entry below one; it must be positive"),
            ("-2", "has a \"max_heavy_requests\" entry below one; it must be positive"),
            ("4294967296", "has a \"max_heavy_requests\" entry over u32::MAX"),
            ("\"4\"", "has a \"max_heavy_requests\" entry that is present but is not an integer"),
        ] {
            let path = scratch_config("max-heavy-requests-refused", &format!("max_heavy_requests = {value}\n"));
            let error = parse_config(path.to_str().unwrap()).unwrap_err();

            assert!(error.contains(expected), "{value}: {error}");
            let _ = forklift_core::util::file_utils::remove_file(&path);
        }

        let path = scratch_config("max-heavy-requests-one", "max_heavy_requests = 1\n");
        assert_eq!(parse_config(path.to_str().unwrap()).unwrap().max_heavy_requests, Some(1));
        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// The `--max-heavy-requests` flag wins over the file's key, and the key is used when no flag
    /// is given. The file's 3, the flag's 5 and every default (unset; the host's parallelism is
    /// not read here) are all distinct.
    ///
    /// Reverts: merge `file.max_heavy_requests.or(max_heavy_requests)` → `Some(3)` with the flag
    /// → red; take the flag alone → `None` with no flag → red.
    #[test]
    fn the_max_heavy_requests_flag_overrides_the_file() {
        let path = scratch_config("max-heavy-requests-flag", "max_heavy_requests = 3\n");
        let options_with = |flag: Option<u32>| serve_options(
            None, None, None, None, None, None, None, flag, RequestLimitFlags::default(), false,
            Some(path.to_str().unwrap().to_string()),
        ).unwrap().max_heavy_requests;

        assert_eq!(options_with(Some(5)), Some(5));
        assert_eq!(options_with(None), Some(3));

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// Each request limit flag wins over its own file key, and each key is used when its flag is
    /// not given; neither class reads the other's value. The file's 100 and 10, the flags' 700 and
    /// 70 and the unset default are all distinct.
    ///
    /// Reverts: merge a file key over its flag → red; read the light key for the heavy limit →
    /// red.
    #[test]
    fn the_request_limit_flags_override_the_file() {
        let path = scratch_config(
            "request-limit-flags", "light_requests_per_minute = 100\nheavy_requests_per_minute = 10\n"
        );
        let options_with = |light: Option<u32>, heavy: Option<u32>| {
            let options = serve_options(
                None, None, None, None, None, None, None, None,
                RequestLimitFlags { light_requests_per_minute: light, heavy_requests_per_minute: heavy }, false,
                Some(path.to_str().unwrap().to_string()),
            ).unwrap();

            (options.light_requests_per_minute, options.heavy_requests_per_minute)
        };

        assert_eq!(options_with(Some(700), Some(70)), (Some(700), Some(70)));
        assert_eq!(options_with(None, None), (Some(100), Some(10)));
        assert_eq!(options_with(Some(700), None), (Some(700), Some(10)));

        let _ = forklift_core::util::file_utils::remove_file(&path);
        let unset = serve_options(
            None, None, None, None, None, None, None, None, RequestLimitFlags::default(), false, None,
        ).unwrap();
        assert_eq!((unset.light_requests_per_minute, unset.heavy_requests_per_minute), (None, None));
    }

    #[test]
    fn a_present_but_stringy_max_body_mb_is_an_error() {
        let path = scratch_config("max-body-mb-stringy", "max_body_mb = \"4096\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"max_body_mb\""), "{}", error);
        assert!(error.contains("not an integer"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    #[test]
    fn a_present_but_stringy_rebuild_after_lifts_is_an_error() {
        let path = scratch_config("rebuild-after-lifts-stringy", "rebuild_after_lifts = \"20\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("\"rebuild_after_lifts\""), "{}", error);
        assert!(error.contains("not an integer"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    #[test]
    fn a_present_but_numeric_hook_url_is_an_error() {
        let path = scratch_config("hook-url-numeric", "[hooks]\nauthentication_url = 5\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_url"), "{}", error);
        assert!(error.contains("not a string"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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
                server::MAX_HOOK_CACHE_SECS + 1
            )
        );
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication_cache_secs"), "{}", error);
        assert!(error.contains("ceiling"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
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
                server::MAX_HOOK_CACHE_SECS
            )
        );
        let file = parse_config(path.to_str().unwrap()).unwrap();

        assert_eq!(file.authentication_cache_secs, Some(server::MAX_HOOK_CACHE_SECS));

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// I9. `admission_cache_secs` is read like `authentication_cache_secs`: a string, a negative
    /// value or one over the 24-hour ceiling is a startup error naming the key; zero (the cache
    /// off) and the ceiling itself are accepted.
    #[test]
    fn admission_cache_secs_is_bounded_like_authentication_cache_secs() {
        for (value, refusal) in [("\"60\"", "not an integer"), ("-1", "negative"), ("86401", "ceiling")] {
            let path = scratch_config(
                "admission-cache-secs-refused",
                &format!("[hooks]\nadmission_url = \"https://x\"\nadmission_secret = \"s\"\nadmission_cache_secs = {value}\n")
            );
            let error = parse_config(path.to_str().unwrap()).unwrap_err();

            assert!(error.contains("hooks.admission_cache_secs"), "{value}: {error}");
            assert!(error.contains(refusal), "{value}: {error}");
            let _ = forklift_core::util::file_utils::remove_file(&path);
        }

        for (value, read) in [("0", 0), ("86400", 86_400)] {
            let path = scratch_config(
                "admission-cache-secs-accepted",
                &format!("[hooks]\nadmission_url = \"https://x\"\nadmission_secret = \"s\"\nadmission_cache_secs = {value}\n")
            );
            let file = parse_config(path.to_str().unwrap()).unwrap();

            assert_eq!(file.admission_cache_secs, Some(read), "{value}");
            assert_eq!(file.authentication_cache_secs, None, "{value}");
            let _ = forklift_core::util::file_utils::remove_file(&path);
        }
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// Pre-existing behaviour (not new in this fix): a hook configured with only one of its
    /// URL/secret pair is refused — hook requests are signed, so a partial pair can never work.
    #[test]
    fn a_hook_with_only_a_url_is_an_error() {
        let path = scratch_config("hook-url-only", "[hooks]\nauthentication_url = \"https://x\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("authentication"), "{}", error);
        assert!(error.contains("only one of"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: unrecognized keys error (Part 1(b) of the fix)
    // ---------------------------------------------------------------------------------

    /// The second reproduction from the fail-open finding: a mistyped top-level key must be a
    /// startup error, never a silently-never-applied setting.
    #[test]
    fn a_misspelled_top_level_key_is_an_error() {
        let path = scratch_config("misspelled-top-level", "addr = \"127.0.0.1:1\"\ntokenn = \"secret\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("an unrecognized key in the top-level config (line 2)"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

        assert!(error.contains("an unrecognized key in the [hooks] table (line 2)"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    // ---------------------------------------------------------------------------------
    // parse_config: pre-existing failure modes untouched by this fix
    // ---------------------------------------------------------------------------------

    #[test]
    fn a_missing_config_file_is_an_error() {
        let path = std::env::temp_dir().join("forklift-server-main-test-does-not-exist.toml");
        let _ = forklift_core::util::file_utils::remove_file(&path);
        assert!(parse_config(path.to_str().unwrap()).is_err());
    }

    #[test]
    fn invalid_toml_is_an_error() {
        let path = scratch_config("invalid-toml", "not [ valid toml");
        assert!(parse_config(path.to_str().unwrap()).is_err());
        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// A token's digest, spelled as a deployer computes it: independently of [`TokenDigest`].
    fn hex_digest(token: &str) -> String {
        forklift_core::util::sign_utils::to_hex(&<sha2::Sha256 as sha2::Digest>::digest(token.as_bytes()))
    }

    /// The static token's digest, from the flag or the config file: the flag wins, a malformed
    /// digest from either source is refused even when the other is sound, and no refusal echoes
    /// the value it refused, which may be a plaintext token put where its digest belongs.
    #[test]
    fn the_static_token_digest_comes_from_the_flag_or_the_file_and_a_malformed_one_is_refused() {
        let flag = hex_digest("flag-secret");
        let file = hex_digest("file-secret");
        let options = |flag: Option<&str>, file: Option<&str>| {
            let config = file.map(|hex| scratch_config("token-digest", &format!("token_sha256 = \"{}\"\n", hex)));
            let options = serve_options(
                None, None, None, flag.map(str::to_string), None, None, None, None, RequestLimitFlags::default(), false,
                config.as_ref().map(|path| path.to_str().unwrap().to_string()),
            );
            if let Some(path) = config {
                let _ = forklift_core::util::file_utils::remove_file(&path);
            }
            options.map(|options| options.token_sha256)
        };
        let digest = |token: &str| Some(TokenDigest::of_token(token).unwrap());

        assert_eq!(options(Some(&flag), Some(&file)).unwrap(), digest("flag-secret"), "the flag wins");
        assert_eq!(options(None, Some(&file)).unwrap(), digest("file-secret"));
        assert_eq!(options(Some(&flag), None).unwrap(), digest("flag-secret"));
        assert_eq!(options(None, None).unwrap(), None);

        for (flag_value, file_value, names) in [
            (Some("plain-secret"), None, "--token-sha256"),
            (Some(""), None, "--token-sha256"),
            (Some("   "), Some(file.as_str()), "--token-sha256"),
            (Some(flag.as_str()), Some("plain-secret"), "\"token_sha256\""),
            (None, Some(hex_digest("").as_str()), "\"token_sha256\""),
        ] {
            let error = options(flag_value, file_value).unwrap_err();
            assert!(error.contains(names), "{:?}/{:?}: {}", flag_value, file_value, error);
            assert!(!error.contains("plain-secret"), "the refusal must not echo the value: {}", error);
        }
    }

    /// No refusal of a malformed config file quotes a secret it holds: a marker written as a
    /// hook secret or a token digest, in a line the parser cannot read or a value of the wrong
    /// kind, never appears in the message.
    #[test]
    fn a_malformed_config_file_never_echoes_a_secret() {
        const MARK: &str = "CANARY-7f3a";
        for (contents, expected) in [
            (format!("[hooks]\nadmission_secret = {MARK}\n"), "is not valid TOML (line 2)."),
            (format!("[hooks]\nadmission_secret = \"{MARK}\n"), "is not valid TOML (line 2)."),
            (format!("token_sha256 = \"{MARK}\"\ntoken_sha256 = \"{MARK}\"\n"), "is not valid TOML (line 2)."),
            (format!("[hooks]\nadmission_secret = [\"{MARK}\"]\n"), "has a \"hooks.admission_secret\" entry that is present but is not a string"),
            (format!("token_sha256 = \"{MARK}\"\n"), "has an invalid \"token_sha256\""),
        ] {
            let path = scratch_config("canary", &contents);
            assert!(!path.to_str().unwrap().contains(MARK));
            let error = parse_config(path.to_str().unwrap()).unwrap_err();
            let _ = forklift_core::util::file_utils::remove_file(&path);

            assert!(error.contains(expected), "{:?}: {}", contents, error);
            assert!(!error.contains(MARK), "{:?}: the refusal quotes the file: {}", contents, error);
        }
    }

    /// No refusal `parse_config` returns quotes what the file holds. The rows are generated from
    /// [`CONFIG_TOP_LEVEL_KEYS`] and [`HOOKS_KEYS`], so a key added to the vocabulary is in the
    /// grid without an edit here: each key is given a marker as a string, inside an array and an
    /// inline table, and as an integer of each sign, and a marker key is written at every place
    /// a key can stand. A refusal of an unrecognized key names its line. A value that is a place
    /// (`root`, `tokens`, a hook's URL) is accepted here and checked later, at startup.
    #[test]
    fn no_config_refusal_quotes_the_file() {
        const MARK: &str = "CANARY-7f3a";
        const NUMBER: &str = "4294967296123";
        let shapes = |key: &str| [
            format!("{key} = \"{MARK}\""),
            format!("{key} = [\"{MARK}\"]"),
            format!("{key} = {{ \"{MARK}\" = \"a\" }}"),
            format!("{key} = {NUMBER}"),
            format!("{key} = -{NUMBER}"),
        ];

        let mut cases: Vec<String> = Vec::new();
        for key in CONFIG_TOP_LEVEL_KEYS {
            cases.extend(shapes(key).into_iter().map(|line| format!("{line}\n")));
        }
        for key in HOOKS_KEYS {
            cases.extend(shapes(key).into_iter().map(|line| format!("[hooks]\n{line}\n")));
        }

        let unknown = "an unrecognized key in the top-level config (line 2).";
        let unknown_hooks = "an unrecognized key in the [hooks] table";
        let keyed = [
            (format!("open = true\n{MARK} = 1\n"), unknown),
            (format!("open = true\n\"{MARK}\" = 1\n"), unknown),
            (format!("open = true\n{MARK}.x = 1\n"), unknown),
            (format!("open = true\n[{MARK}]\nx = 1\n"), unknown),
            (format!("open = true\n[[{MARK}]]\nx = 1\n"), unknown),
            (format!("[hooks]\n{MARK} = 1\n"), "an unrecognized key in the [hooks] table (line 2)."),
            (format!("[hooks]\n\"{MARK}\" = \"a\"\n"), "an unrecognized key in the [hooks] table (line 2)."),
            (format!("hooks = {{ {MARK} = 1 }}\n"), unknown_hooks),
            (format!("hooks.{MARK} = 1\n"), unknown_hooks),
        ];

        let mut refused = 0;
        for contents in cases.iter().cloned().map(|c| (c, None)).chain(keyed.into_iter().map(|(c, e)| (c, Some(e)))) {
            let (contents, expected) = contents;
            let path = scratch_config("generated-canary", &contents);
            assert!(!path.to_str().unwrap().contains(MARK));
            let result = parse_config(path.to_str().unwrap());
            let _ = forklift_core::util::file_utils::remove_file(&path);

            match (result, expected) {
                (Err(error), expected) => {
                    refused += 1;
                    assert!(!error.contains(MARK) && !error.contains(NUMBER),
                            "{:?}: the refusal quotes the file: {}", contents, error);
                    if let Some(expected) = expected {
                        assert!(error.contains(expected), "{:?}: {}", contents, error);
                    }
                }
                (Ok(_), None) => {}
                (Ok(_), Some(_)) => panic!("{:?}: a marker key was accepted", contents),
            }
        }

        // The domain is not empty: most values of the wrong shape are refused.
        assert!(refused > cases.len() / 2, "only {} of {} refused", refused, cases.len());
    }

    /// A config file still setting the plaintext `token` is refused, naming what replaced it,
    /// and never echoing the token.
    #[test]
    fn a_plaintext_token_key_is_refused_naming_token_sha256() {
        let path = scratch_config("plaintext-token", "token = \"s3cr3t-value\"\n");
        let error = parse_config(path.to_str().unwrap()).unwrap_err();

        assert!(error.contains("sets \"token\""), "{}", error);
        assert!(error.contains("set token_sha256 to the token's SHA-256 digest"), "{}", error);
        assert!(!error.contains("s3cr3t-value"), "{}", error);

        let _ = forklift_core::util::file_utils::remove_file(&path);
    }

    /// `Debug` of the parsed config prints no digest of the static token: the same rule as
    /// `server.rs`'s `hook_endpoint_debug_never_prints_the_secret`, so a future
    /// `tracing::debug!(?file)` or a panic message leaks nothing. The field's name still prints,
    /// so the redaction cannot pass by the field vanishing.
    #[test]
    fn config_file_debug_never_prints_the_token_digest() {
        let digest = hex_digest("s3cr3t-value");
        let path = scratch_config("debug-redaction", &format!("token_sha256 = \"{}\"\n", digest));
        let file = parse_config(path.to_str().unwrap()).unwrap();
        let formatted = format!("{:?}", file);

        assert!(!formatted.contains(&digest), "{}", formatted);
        assert!(formatted.contains("token_sha256: Some(TokenDigest(<redacted>))"), "{}", formatted);

        let _ = forklift_core::util::file_utils::remove_file(&path);
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

    /// A scratch root for one test, empty.
    fn scratch_root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("forklift-server-main-test-{}-{}", name, std::process::id()));
        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        root
    }

    /// Every path below `root`, sorted.
    fn listing(root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut paths = Vec::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path.clone());
                }
                paths.push(path);
            }
        }
        paths.sort();
        paths
    }

    /// The refusal a command's error carries: its code and next step.
    fn refusal(error: String) -> (forklift_core::error::RefusalCode, String) {
        match forklift_core::error::CoreError::from(error) {
            forklift_core::error::CoreError::Refusal { code, next_step, .. } => (code, next_step),
            other => panic!("not a refusal: {:?}", other),
        }
    }

    /// `bundle` and `gc` open their root through core's gate: an unfinished clone is no
    /// warehouse to work on yet, and neither is a folder whose `.forklift` holds no marker.
    ///
    /// Revert: `server::open_root` without the gate (canonicalize only) → the empty `.forklift` opens → red.
    #[test]
    fn a_root_holding_an_unfinished_franchise_is_refused() {
        let root = scratch_root("clone");
        let forklift = root.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        std::fs::write(forklift_core::util::franchise_utils::record_path(&forklift), b"").unwrap();

        let refused = server::open_root(root.to_str().unwrap()).unwrap_err();
        assert_eq!(refusal(refused).0, forklift_core::error::RefusalCode::CloneIncomplete);

        // Without the clone, the folder holds no marker: still no warehouse.
        forklift_core::util::file_utils::remove_file(forklift_core::util::franchise_utils::record_path(&forklift)).unwrap();
        let refused = server::open_root(root.to_str().unwrap()).unwrap_err();
        assert_eq!(refusal(refused).0, forklift_core::error::RefusalCode::NotAWarehouse);

        std::fs::write(forklift.join(forklift_core::util::warehouse_utils::FILE_NAME_WAREHOUSE_MARKER), forklift_core::util::warehouse_utils::framed_marker_bytes("format 1\nfull\n")).unwrap();
        assert_eq!(server::open_root(root.to_str().unwrap()).unwrap().root, std::fs::canonicalize(&root).unwrap());

        let _ = forklift_core::util::file_utils::remove_dir_all(&root);
    }

    /// `bundle` writes into its root and `gc` deletes from it, so each refuses every root that is
    /// not a warehouse before it touches anything, and names this server's prepare where that is
    /// the remedy.
    ///
    /// Revert: `bundle` or `gc` opening with `server::canonical_root` alone → a root is written to, or the
    /// refusal is not `not_a_warehouse` → red.
    #[test]
    fn bundle_and_gc_refuse_a_root_that_is_not_a_warehouse() {
        let forklift_name = forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT;
        let cases: [(&str, fn(&std::path::Path), bool); 5] = [
            ("none", |_| (), true),
            ("empty", |root| std::fs::create_dir_all(root.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap(), true),
            ("unmarked", |root| std::fs::create_dir_all(root.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT).join("objects")).unwrap(), true),
            ("bay", |root| {
                let forklift = root.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT);
                std::fs::create_dir_all(&forklift).unwrap();
                std::fs::write(forklift.join(forklift_core::util::bay_utils::FILE_NAME_BAY_REDIRECT), b"x").unwrap();
            }, false),
            // A file in the folder's place: the older bay format, or anything else.
            ("file", |root| std::fs::write(root.join(forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT), b"x").unwrap(), true),
        ];

        for (name, make, prepare_is_the_remedy) in cases {
            let root = scratch_root(&format!("gate-{}", name));
            make(&root);
            let before = listing(&root);
            let shown = std::fs::canonicalize(&root).unwrap();

            for (command, error) in [
                ("bundle", bundle(root.to_str().unwrap()).unwrap_err()),
                ("gc", gc(root.to_str().unwrap(), 0).unwrap_err()),
            ] {
                let (code, next_step) = refusal(error);
                assert_eq!(code, forklift_core::error::RefusalCode::NotAWarehouse, "{} {}", command, name);
                assert_eq!(
                    next_step.contains(&format!("\"forklift-server prepare\" with --root \"{}\"", shown.to_string_lossy())),
                    prepare_is_the_remedy,
                    "{} {}: {}", command, name, next_step
                );
                assert!(!next_step.contains("\"forklift prepare\"") && !next_step.contains("git import"), "{} {}: {}", command, name, next_step);
            }
            assert_eq!(listing(&root), before, "{}: nothing written", name);
            assert!(name == "none" || root.join(forklift_name).exists());

            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }
    }

    /// Each next step the server gives for a root that is not a warehouse works when followed:
    /// prepare takes an empty `.forklift`, the warehouse above an empty nested one is the root to
    /// pass, and an unmarked folder or a `.forklift` file moved aside leaves a root prepare takes.
    ///
    /// Revert: the server's step for an empty nested `.forklift` sending the operator to remove it
    /// (the CLI's) → the root to pass is not named → red. Prepare calling a `.forklift` file a bay's
    /// (the CLI's wording), or its step offering the warehouse the file "belongs to" → red.
    #[test]
    fn the_server_next_steps_work_when_followed() {
        let forklift_name = forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT;
        let next_step_of = |root: &std::path::Path| refusal(bundle(root.to_str().unwrap()).unwrap_err()).1;

        let empty = scratch_root("follow-empty");
        std::fs::create_dir_all(empty.join(forklift_name)).unwrap();
        assert!(next_step_of(&empty).starts_with("Run \"forklift-server prepare\" with --root"));
        prepare(empty.to_str().unwrap()).unwrap();
        bundle(empty.to_str().unwrap()).unwrap();

        let warehouse = scratch_root("follow-nested");
        prepare(warehouse.to_str().unwrap()).unwrap();
        let sub = warehouse.join("sub");
        std::fs::create_dir_all(sub.join(forklift_name)).unwrap();
        let next_step = next_step_of(&sub);
        let shown = std::fs::canonicalize(&warehouse).unwrap();
        assert!(next_step.starts_with(&format!("Pass --root \"{}\" to use the warehouse there", shown.to_string_lossy())), "{}", next_step);
        bundle(shown.to_str().unwrap()).unwrap();

        let old = scratch_root("follow-unmarked");
        std::fs::create_dir_all(old.join(forklift_name).join("objects")).unwrap();
        let next_step = next_step_of(&old);
        assert!(next_step.starts_with("Move ") && !next_step.contains("redirect"), "{}", next_step);
        forklift_core::util::file_utils::rename(old.join(forklift_name), old.join("aside")).unwrap();
        prepare(old.to_str().unwrap()).unwrap();
        bundle(old.to_str().unwrap()).unwrap();

        // A `.forklift` file: bundle and prepare give the same step, neither names a bay, and it works.
        let file = scratch_root("follow-file");
        std::fs::write(file.join(forklift_name), "anything").unwrap();
        for (command, next_step) in [("bundle", next_step_of(&file)), ("prepare", refusal(prepare(file.to_str().unwrap()).unwrap_err()).1)] {
            assert!(next_step.starts_with("Move it aside and run \"forklift-server prepare\" with --root"), "{}: {}", command, next_step);
            assert!(!next_step.contains("bay") && !next_step.contains("belongs"), "{}: {}", command, next_step);
        }
        forklift_core::util::file_utils::rename(file.join(forklift_name), file.join("aside")).unwrap();
        prepare(file.to_str().unwrap()).unwrap();
        bundle(file.to_str().unwrap()).unwrap();

        for root in [empty, warehouse, old, file] {
            let _ = forklift_core::util::file_utils::remove_dir_all(&root);
        }
    }
}
