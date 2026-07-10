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
        Command::Serve { root, warehouses, addr, token, tokens, max_body_mb, rebuild_after_lifts, config } =>
            serve(root, warehouses, addr, token, tokens, max_body_mb, rebuild_after_lifts, config).await,
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
    };

    server::serve(options).await
}

/// The serve keys of the TOML config file (same names as the flags). The hooks
/// (`docs/format/HOOK_PROTOCOL.md`) are config-file-only — they come in URL+secret
/// pairs, which flags handle poorly:
///
/// ```toml
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
    token: Option<String>,
    tokens: Option<String>,
    max_body_mb: Option<u64>,
    rebuild_after_lifts: Option<u32>,
    authentication_hook: Option<server::HookEndpoint>,
    admission_hook: Option<server::HookEndpoint>,
    events_hook: Option<server::HookEndpoint>,
    resolution_hook: Option<server::HookEndpoint>,
    authentication_cache_secs: Option<u64>,
}

/// Parse the serve config file.
fn parse_config(path: &str) -> Result<ConfigFile, String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("Error while reading the config file \"{}\": {}", path, e))?;

    let doc: toml_edit::DocumentMut = content.parse()
        .map_err(|e| format!("The config file \"{}\" is not valid TOML: {}", path, e))?;

    let string_of = |key: &str| doc.get(key).and_then(|item| item.as_str()).map(|s| s.to_string());
    let integer_of = |key: &str| doc.get(key).and_then(|item| item.as_integer());

    let hooks = doc.get("hooks").and_then(|item| item.as_table());

    let hook_of = |name: &str| -> Result<Option<server::HookEndpoint>, String> {
        let Some(hooks) = hooks else {
            return Ok(None);
        };

        let field = |suffix: &str| hooks.get(&format!("{}_{}", name, suffix))
            .and_then(|item| item.as_str())
            .map(|s| s.to_string());

        match (field("url"), field("secret")) {
            (None, None) => Ok(None),
            (Some(url), Some(secret)) => Ok(Some(server::HookEndpoint { url, secret })),
            _ => Err(format!(
                "The config file \"{}\" configures the {} hook with only one of \
                {}_url and {}_secret; hook requests are signed, both are required.",
                path, name, name, name
            )),
        }
    };

    Ok(ConfigFile {
        root: string_of("root"),
        warehouses: string_of("warehouses"),
        addr: string_of("addr"),
        token: string_of("token"),
        tokens: string_of("tokens"),
        max_body_mb: integer_of("max_body_mb").map(|v| v as u64),
        rebuild_after_lifts: integer_of("rebuild_after_lifts").map(|v| v as u32),
        authentication_hook: hook_of("authentication")?,
        admission_hook: hook_of("admission")?,
        events_hook: hook_of("events")?,
        resolution_hook: hook_of("resolution")?,
        authentication_cache_secs: hooks
            .and_then(|table| table.get("authentication_cache_secs"))
            .and_then(|item| item.as_integer())
            .map(|v| v as u64),
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
