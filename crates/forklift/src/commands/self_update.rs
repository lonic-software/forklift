use std::process::Command;
use serde::Serialize;
use crate::commands::alias;
use crate::output::{self, CommandOutput};

/// The version this binary was built as.
const CURRENT: &str = env!("CARGO_PKG_VERSION");

/// The GitHub repository releases are published to.
const REPO: &str = "lonic-software/forklift";

/// The canonical install one-liners (the same verified path used to install).
const INSTALL_SH: &str =
    "curl -fsSL https://raw.githubusercontent.com/lonic-software/forklift/main/install.sh | sh";
const INSTALL_PS1: &str =
    "irm https://raw.githubusercontent.com/lonic-software/forklift/main/install.ps1 | iex";

/// Handle the self-update command: check the latest published release against this binary
/// and, for a script/manual install, update in place by re-running the verified install
/// script. A **package-manager** install is never overwritten — self-update defers to the
/// package manager with the right command, so it can't fight `cargo`/`brew`/the system.
///
/// Forklift deliberately keeps this to the *client*. A server is a deployed artifact — it
/// is redeployed (new container, new Lambda version, new package), never self-mutating; a
/// network service that rewrites its own binary is an anti-pattern and an attack surface.
///
/// # Arguments
/// * `check` - Only report status and how to update; never modify anything.
///
/// # Returns
/// * `Ok(())`      - Whether or not an update was applied (a no-op is success).
/// * `Err(String)` - If the latest release could not be reached, or the update failed.
pub async fn handle_command(check: bool) -> Result<(), String> {
    let latest = fetch_latest().await?;
    let method = InstallMethod::detect();

    let update_available = latest.as_deref().map(|latest| is_newer(latest, CURRENT)).unwrap_or(false);

    let report = SelfUpdate {
        current: CURRENT.to_string(),
        latest: latest.clone(),
        update_available,
        install_method: method.as_str().to_string(),
        update_command: method.update_command().to_string(),
        applied: false,
    };

    // Nothing to do: unknown latest, or already current.
    if !update_available {
        output::emit("self-update", &report);
        return Ok(());
    }

    // A check, or a package-manager binary we must not overwrite: report the command and stop.
    if check || method != InstallMethod::Script {
        output::emit("self-update", &report);
        return Ok(());
    }

    // A script/manual install: update in place by re-running the install script, pinned to
    // the exact version we resolved. `update_available` guarantees `latest` is set.
    let target = latest.expect("an available update has a latest version");

    // The install script replaces the binary at the same path (atomic rename), so an
    // existing `fl` alias keeps working without any action. This is defense in depth for
    // the case it doesn't (e.g. a relocated install dir): if the alias was present before,
    // make sure it still is after — best-effort, never fails the update over it.
    let had_alias = alias::exists(alias::DEFAULT_NAME);

    perform_update(&target)?;

    if had_alias {
        alias::reinstall_silently(alias::DEFAULT_NAME);
    }

    output::emit("self-update", &SelfUpdate { applied: true, latest: Some(target), ..report });

    Ok(())
}

/// The latest published release version (without a leading `v`), or `None` if there are no
/// releases yet. `FORKLIFT_SELFUPDATE_LATEST` overrides the lookup (used by tests, and as
/// an offline escape hatch): empty means "no releases".
async fn fetch_latest() -> Result<Option<String>, String> {
    if let Ok(injected) = std::env::var("FORKLIFT_SELFUPDATE_LATEST") {
        let trimmed = injected.trim().trim_start_matches('v');
        return Ok((!trimmed.is_empty()).then(|| trimmed.to_string()));
    }

    let url = format!("https://api.github.com/repos/{}/releases/latest", REPO);

    let client = reqwest::Client::builder()
        .user_agent("forklift-self-update")
        .build()
        .map_err(|e| format!("Could not build an HTTP client: {}", e))?;

    let response = client.get(&url).send().await
        .map_err(|e| format!("Could not reach GitHub to check for updates: {}", e))?;

    // A repository with no releases yet answers 404 — that is "nothing to update to", not
    // an error.
    if !response.status().is_success() {
        return Ok(None);
    }

    let body: serde_json::Value = response.json().await
        .map_err(|e| format!("Could not read GitHub's response: {}", e))?;

    Ok(body["tag_name"].as_str().map(|tag| tag.trim_start_matches('v').to_string()))
}

/// Whether `candidate` is a strictly newer x.y.z version than `current` (a missing or
/// unparseable component counts as 0; non-numeric versions never compare as newer).
fn is_newer(candidate: &str, current: &str) -> bool {
    parts(candidate) > parts(current)
}

/// Parse `x.y.z` into a comparable tuple (extra components and a pre-release suffix are
/// ignored; anything unparseable becomes 0).
fn parts(version: &str) -> (u64, u64, u64) {
    let mut it = version.split(['.', '-', '+'])
        .map(|component| component.parse::<u64>().unwrap_or(0));

    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Re-run the platform install script to install exactly `version`.
///
/// We pin `FORKLIFT_VERSION` to the version we already resolved from the GitHub API,
/// instead of letting the install script resolve "latest" itself. The script's own
/// `latest` uses the `releases/latest/download/` redirect, which GitHub CDN-caches and can
/// serve a *stale* release right after a publish (the reason a self-update could reinstall
/// the old version, checksum and all). Pinning downloads from the stable per-tag URL.
fn perform_update(version: &str) -> Result<(), String> {
    println!("Installing forklift {} via the install script…", version);

    let tag = format!("v{}", version);

    let status = if cfg!(windows) {
        Command::new("powershell")
            .args(["-NoProfile", "-Command", INSTALL_PS1])
            .env("FORKLIFT_VERSION", &tag)
            .status()
    } else {
        Command::new("sh").arg("-c").arg(INSTALL_SH)
            .env("FORKLIFT_VERSION", &tag)
            .status()
    };

    match status {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(format!("The install script exited with status {}.", status)),
        Err(e) => Err(format!("Could not run the install script (need curl + sh on PATH): {}", e)),
    }
}

/// How this binary appears to have been installed — heuristic, from its path.
#[derive(PartialEq)]
enum InstallMethod {
    Cargo,
    Homebrew,
    Script,
}

impl InstallMethod {
    /// Guess the install method from the running executable's path.
    fn detect() -> InstallMethod {
        let path = std::env::current_exe()
            .map(|path| path.to_string_lossy().to_lowercase())
            .unwrap_or_default();

        if path.contains("/.cargo/") || path.contains("\\.cargo\\") {
            InstallMethod::Cargo
        } else if path.contains("homebrew") || path.contains("cellar") {
            InstallMethod::Homebrew
        } else {
            InstallMethod::Script
        }
    }

    fn as_str(&self) -> &'static str {
        match self {
            InstallMethod::Cargo => "cargo",
            InstallMethod::Homebrew => "homebrew",
            InstallMethod::Script => "script",
        }
    }

    /// The command that updates a binary installed this way.
    fn update_command(&self) -> &'static str {
        match self {
            InstallMethod::Cargo =>
                "cargo install --git https://github.com/lonic-software/forklift forklift --force",
            InstallMethod::Homebrew => "brew upgrade forklift",
            InstallMethod::Script => if cfg!(windows) { INSTALL_PS1 } else { INSTALL_SH },
        }
    }
}

/// The result of a self-update check (or update).
#[derive(Serialize)]
struct SelfUpdate {
    current: String,

    /// The latest published release, or `None` if there are none yet / GitHub was silent.
    #[serde(skip_serializing_if = "Option::is_none")]
    latest: Option<String>,

    update_available: bool,

    /// How this binary was installed (`cargo`, `homebrew`, `script`).
    install_method: String,

    /// The command that updates a binary installed this way.
    update_command: String,

    /// Whether this run actually applied the update.
    applied: bool,
}

impl CommandOutput for SelfUpdate {
    fn render_human(&self) {
        let Some(latest) = &self.latest else {
            println!(
                "You are on forklift {}. No published release was found (none yet, or GitHub \
                was unreachable).",
                self.current
            );
            return;
        };

        if !self.update_available {
            println!("forklift {} is up to date (latest release: {}).", self.current, latest);
            return;
        }

        if self.applied {
            // The install script above printed the path and version it actually installed —
            // don't restate (or fabricate) it. Point the user at the ground truth.
            println!("Ran the installer for forklift {}.", latest);
            println!("Run \"forklift version\" to confirm — if a different copy is first on your PATH, that one is unchanged.");
            return;
        }

        println!("forklift {} is available — you have {}.", latest, self.current);

        if self.install_method == "script" {
            println!("Run \"forklift self-update\" to update in place, or:");
        } else {
            println!(
                "This looks like a {}-managed install, so self-update won't overwrite it. Update with:",
                self.install_method
            );
        }

        println!("  {}", self.update_command);
    }
}

#[cfg(test)]
mod tests {
    use super::is_newer;

    #[test]
    fn compares_versions() {
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("0.1.10", "0.1.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(!is_newer("garbage", "0.1.0"));
    }
}
