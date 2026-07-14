use std::path::PathBuf;
use std::process::Stdio;
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use forklift_core::globals::forklift_root;
use forklift_core::util::config_utils;
use crate::output::{self, CommandOutput};

/// The folder under the warehouse root where `peer` keeps its local, never-shared state — the
/// access token and the persistent onion key. It lives inside `.forklift`, so it is neither part
/// of the working tree nor ever uploaded by a lift.
const PEER_STATE_DIR: &str = "peer";
const TOKEN_FILE: &str = "token";
const ONION_KEY_FILE: &str = "onion.key";

/// The marker `forklift-server` prints on stdout in front of the published onion URL. Matched
/// (not reconstructed) so the two stay coupled through one string.
const ONION_MARKER: &str = "onion service at ";

/// Handle the `peer` command: share this warehouse peer-to-peer over Tor with one command.
///
/// It runs the `forklift-server` head against this warehouse — bound to loopback and published
/// as a Tor onion service — and, once the address is up, prints the single line to hand a peer.
/// The head runs as a **child process** (the client never links the server): the client stays
/// the MIT/Apache tool, the server head stays its own binary. Serving continues until the child
/// exits or Ctrl-C, which tears the onion down.
///
/// # Arguments
/// * `token`                - The access token peers must present (default: minted once, reused).
/// * `ephemeral`            - Use a fresh throwaway `.onion` each run, not the saved stable one.
/// * `server`               - Path to the `forklift-server` binary (default: found automatically).
/// * `tor_control`          - The Tor control address (default: the server's `127.0.0.1:9051`).
/// * `tor_control_password` - The Tor control password, for a `HashedControlPassword` control port.
///
/// # Returns
/// * `Ok(())`      - When sharing ends cleanly (Ctrl-C, or the server stopped on its own).
/// * `Err(String)` - If the server binary is missing, or the onion could not be published.
pub async fn handle_command(token: Option<String>,
                            ephemeral: bool,
                            server: Option<String>,
                            tor_control: Option<String>,
                            tor_control_password: Option<String>) -> Result<(), String> {
    // `peer` runs inside the warehouse (the CLI entered it), so the child inherits this cwd and
    // `--root .` resolves to it. The local peer state lives under the warehouse's `.forklift`.
    let peer_dir = ensure_peer_dir()?;
    let server_bin = resolve_server_binary(server)?;
    let token = resolve_token(token, &peer_dir)?;

    let mut command = Command::new(&server_bin);
    command
        .arg("serve")
        .arg("--root").arg(".")
        // Port 0: the OS picks a free loopback port; only Tor reaches it, via the onion.
        .arg("--addr").arg("127.0.0.1:0")
        .arg("--tor")
        .arg("--token").arg(&token)
        .stdin(Stdio::null())
        // The onion URL is parsed from stdout; the head's request logs and any error go to
        // stderr, which is inherited so the operator sees them live.
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());

    if !ephemeral {
        command.arg("--tor-onion-key").arg(peer_dir.join(ONION_KEY_FILE));
    }

    if let Some(control) = &tor_control {
        command.arg("--tor-control").arg(control);
    }

    if let Some(password) = &tor_control_password {
        command.arg("--tor-control-password").arg(password);
    }

    let mut child = command.spawn().map_err(|e| spawn_error(&server_bin, e))?;

    // Take the stdout pipe; the child no longer borrows it, so it can be waited on concurrently.
    let stdout = child.stdout.take()
        .ok_or("The server produced no output stream.".to_string())?;
    let mut lines = BufReader::new(stdout).lines();

    // Read startup output until the onion address appears. The head either prints it or, on a
    // Tor failure, reports on stderr and exits — closing stdout, which ends this loop.
    let onion_url = loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if let Some(url) = line.split(ONION_MARKER).nth(1) {
                    break url.trim().to_string();
                }
                // Any other startup line (e.g. "listening on …") is the head's own; ignore it.
            }
            Ok(None) => {
                // stdout closed before the onion was announced: the head exited. Its reason is
                // already on the inherited stderr; surface a terminal error with the exit status.
                let status = child.wait().await
                    .map_err(|e| format!("The server process failed: {}", e))?;

                return Err(format!(
                    "The server stopped before it could publish an onion address ({}). Check the \
                    messages above: is `tor` running with a ControlPort, and is forklift-server \
                    recent enough to support --tor? See docs/guide/p2p-tor.md.",
                    status
                ));
            }
            Err(e) => return Err(format!("Error while reading the server's output: {}", e)),
        }
    };

    output::emit("peer", &PeerReport {
        address: onion_url.clone(),
        token: token.clone(),
        franchise_command: format!("forklift franchise {} <dir> --token {}", onion_url, token),
        stable: !ephemeral,
    });

    // Flush now: unlike an ordinary command, `peer` does not return after emitting — it blocks
    // in the serve loop below. On a pipe (block-buffered) stdout, the share details would
    // otherwise sit unflushed behind the long-running server. The one thing to share must appear
    // immediately.
    use std::io::Write;
    std::io::stdout().flush().ok();

    // Keep draining the head's stdout so its pipe never fills (it writes little more after
    // startup, but a blocked pipe would stall it). Runs until the child closes stdout.
    tokio::spawn(async move {
        while let Ok(Some(_)) = lines.next_line().await {}
    });

    // Serve until the operator stops us, or the head exits on its own. Ctrl-C kills the child,
    // which closes its Tor control connection and removes the onion address.
    tokio::select! {
        status = child.wait() => {
            let status = status.map_err(|e| format!("The server process failed: {}", e))?;

            if !status.success() {
                return Err(format!("The server stopped unexpectedly ({}).", status));
            }
        }
        _ = tokio::signal::ctrl_c() => {
            let _ = child.kill().await;
            println!("\nStopped sharing. The onion address is offline until you run \"peer\" again.");
        }
    }

    Ok(())
}

/// Create (and return) the peer-state folder under `.forklift`, canonicalized so the paths passed
/// to the child are absolute regardless of the child's own resolution.
fn ensure_peer_dir() -> Result<PathBuf, String> {
    let dir = forklift_root().join(PEER_STATE_DIR);

    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("Error while creating the peer state folder \"{}\": {}", dir.display(), e))?;

    std::fs::canonicalize(&dir)
        .map_err(|e| format!("Error while resolving the peer state folder \"{}\": {}", dir.display(), e))
}

/// Resolve the access token: the explicit one, else the one minted on a previous run, else a
/// freshly minted UUID persisted (owner-only) so the same warehouse keeps one stable token.
fn resolve_token(explicit: Option<String>, peer_dir: &std::path::Path) -> Result<String, String> {
    if let Some(token) = explicit {
        return Ok(token);
    }

    let path = peer_dir.join(TOKEN_FILE);

    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }

    let token = config_utils::mint_uuid_v4();

    std::fs::write(&path, &token)
        .map_err(|e| format!("Error while saving the peer token \"{}\": {}", path.display(), e))?;

    restrict_to_owner(&path)?;

    Ok(token)
}

/// Restrict a file to owner-read/write (`0600`) on Unix — the token and onion key are secrets.
/// A no-op elsewhere, where the file inherits the default ACL.
fn restrict_to_owner(path: &std::path::Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("Error while restricting permissions on \"{}\": {}", path.display(), e))?;
    }

    let _ = path;
    Ok(())
}

/// Find the `forklift-server` binary: the explicit path when given (and present), otherwise a
/// sibling of the running `forklift` executable (the common install shape — both in the same
/// `bin` folder), otherwise the bare name for the OS to resolve on `PATH`.
fn resolve_server_binary(explicit: Option<String>) -> Result<PathBuf, String> {
    if let Some(path) = explicit {
        let path = PathBuf::from(path);

        if !path.exists() {
            return Err(format!(
                "No forklift-server binary at \"{}\". Point --server at it, or install it \
                (install.sh server).",
                path.display()
            ));
        }

        return Ok(path);
    }

    let binary_name = if cfg!(windows) { "forklift-server.exe" } else { "forklift-server" };

    if let Ok(exe) = std::env::current_exe() {
        if let Some(sibling) = exe.parent().map(|dir| dir.join(binary_name)) {
            if sibling.exists() {
                return Ok(sibling);
            }
        }
    }

    // Not beside us: let the OS resolve it on PATH (a clear error follows if it is absent).
    Ok(PathBuf::from(binary_name))
}

/// Turn a failed spawn into a helpful error — a missing binary is the common case and gets an
/// install pointer rather than a bare OS error.
fn spawn_error(server_bin: &std::path::Path, error: std::io::Error) -> String {
    if error.kind() == std::io::ErrorKind::NotFound {
        format!(
            "Could not find the forklift-server binary (\"{}\"). Peer sharing runs the server \
            head locally — install it with \"install.sh server\" (or \"all\"), or pass its path \
            with --server. See docs/guide/p2p-tor.md.",
            server_bin.display()
        )
    } else {
        format!("Error while starting the server \"{}\": {}", server_bin.display(), error)
    }
}

/// What `peer` announces once the warehouse is live: the address and token to share, and the
/// exact command a peer runs to clone it.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct PeerReport {
    /// The Tor onion URL to give a peer (their `remote.url`).
    address: String,

    /// The access token peers must present.
    token: String,

    /// The exact command a peer runs to franchise (clone) this warehouse.
    franchise_command: String,

    /// Whether the address is stable across runs (`false` for an `--ephemeral` share).
    stable: bool,
}

impl CommandOutput for PeerReport {
    fn render_human(&self) {
        println!();
        println!("  Your warehouse is live and shared over Tor — no server needed.");
        println!();
        println!("  Give your peer these two things:");
        println!();
        println!("    address   {}", self.address);
        println!("    token     {}", self.token);
        println!();
        println!("  They clone it with:");
        println!();
        println!("    {}", self.franchise_command);
        println!();

        if self.stable {
            println!("  This address is saved — it stays the same next time you share.");
        } else {
            println!("  This is a throwaway address — it changes each run (--ephemeral).");
        }

        println!("  Peers can lift and lower while this runs. Press Ctrl-C to stop sharing.");
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("PeerReport", schemars::schema_for!(PeerReport)),
    ]
}
