use std::env;
use std::io;
use std::path::{Path, PathBuf};
use serde::Serialize;
use crate::cli::AliasAction;
use crate::output::{self, CommandOutput};

/// The alias name every installer creates by default (DESIGN.html §5.0 milestone F):
/// home-row-friendly, and short enough that `fl` collisions are rare.
pub const DEFAULT_NAME: &str = "fl";

/// Overrides the directory the alias is created next to (default: this binary's own
/// directory, resolved via `current_exe`). This is the same pattern as `FORKLIFT_GLOBAL_CONFIG`
/// / `FORKLIFT_KEYS_DIR`: it keeps tests away from the real build output directory, and
/// doubles as an escape hatch when the binary's own directory is not writable.
const ENV_ALIAS_DIR: &str = "FORKLIFT_ALIAS_DIR";

/// Handle the "alias" command: manage the `fl` short alias next to this binary.
///
/// * `alias` / `alias status`      - Report whether the alias exists and where it points.
/// * `alias install [name]`       - Create the alias (default name: "fl").
/// * `alias uninstall [name]`     - Remove the alias.
///
/// # Arguments
/// * `action` - The alias subcommand (`None` reports status).
///
/// # Returns
/// * `Ok(())`      - If the command completed.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(action: Option<AliasAction>) -> Result<(), String> {
    match action {
        Some(AliasAction::Install { name }) => install(&name.unwrap_or_else(|| DEFAULT_NAME.to_string())),
        Some(AliasAction::Uninstall { name }) => uninstall(&name.unwrap_or_else(|| DEFAULT_NAME.to_string())),
        Some(AliasAction::Status) | None => status(),
    }
}

/// Create `name` next to this binary, pointing at it. Idempotent: an alias that already
/// points here succeeds without change. Refuses to touch anything it did not create itself
/// (a real file, or a symlink/shim to a different target) — there is no `--force`, on
/// purpose: silently clobbering an unrelated file is exactly the kind of surprise an
/// installer must never cause.
fn install(name: &str) -> Result<(), String> {
    let dir = alias_dir()?;
    let target = this_binary()?;
    let alias_path = platform_alias_path(&dir, name);

    match inspect(&alias_path, &target) {
        Existing::PointsHere => {
            output::emit("alias", &Installed {
                name: name.to_string(),
                path: display(&alias_path),
                target: display(&target),
                already_installed: true,
            });

            return Ok(());
        }
        Existing::PointsElsewhere(other) => return Err(format!(
            "\"{}\" already exists and points at \"{}\" (not this binary, \"{}\"). Refusing to \
            overwrite it — remove it yourself first if that's what you want.",
            display(&alias_path), display(&other), display(&target)
        )),
        Existing::Foreign => return Err(format!(
            "\"{}\" already exists and is not an alias forklift manages (not a symlink to a \
            forklift binary). Refusing to overwrite it.",
            display(&alias_path)
        )),
        Existing::None => {}
    }

    create_alias(&alias_path, &target).map_err(|e| io_hint("create", &alias_path, e))?;

    output::emit("alias", &Installed {
        name: name.to_string(),
        path: display(&alias_path),
        target: display(&target),
        already_installed: false,
    });

    Ok(())
}

/// Remove `name` from next to this binary. A no-op (success) if it is not installed.
/// Refuses if the name exists but forklift did not create it (a real file, e.g.). A symlink
/// (Unix) or a recognized shim (Windows) is removed even if it points at a *different*
/// forklift binary — deleting a symlink can never lose data, unlike a real file.
fn uninstall(name: &str) -> Result<(), String> {
    let dir = alias_dir()?;
    let target = this_binary()?;
    let alias_path = platform_alias_path(&dir, name);

    match inspect(&alias_path, &target) {
        Existing::None => {
            output::emit("alias", &Uninstalled {
                name: name.to_string(),
                path: display(&alias_path),
                removed: false,
            });

            Ok(())
        }
        Existing::Foreign => Err(format!(
            "\"{}\" exists but is not an alias forklift manages. Refusing to remove it.",
            display(&alias_path)
        )),
        Existing::PointsHere | Existing::PointsElsewhere(_) => {
            remove_alias(&alias_path).map_err(|e| io_hint("remove", &alias_path, e))?;

            output::emit("alias", &Uninstalled {
                name: name.to_string(),
                path: display(&alias_path),
                removed: true,
            });

            Ok(())
        }
    }
}

/// Report whether the default alias exists next to this binary, and where it points.
fn status() -> Result<(), String> {
    let dir = alias_dir()?;
    let target = this_binary()?;
    let alias_path = platform_alias_path(&dir, DEFAULT_NAME);

    let (installed, target_display) = match inspect(&alias_path, &target) {
        Existing::None | Existing::Foreign => (false, None),
        Existing::PointsHere => (true, Some(display(&target))),
        Existing::PointsElsewhere(other) => (true, Some(display(&other))),
    };

    output::emit("alias", &StatusReport {
        name: DEFAULT_NAME.to_string(),
        path: display(&alias_path),
        installed,
        target: target_display,
    });

    Ok(())
}

/// Whether `name` currently exists next to this binary (any recognized alias, not
/// necessarily pointing here) — used by `self-update` to decide whether to restore it after
/// an in-place update. Best-effort: any resolution failure reads as "not installed".
pub(crate) fn exists(name: &str) -> bool {
    match (alias_dir(), this_binary()) {
        (Ok(dir), Ok(target)) => matches!(
            inspect(&platform_alias_path(&dir, name), &target),
            Existing::PointsHere | Existing::PointsElsewhere(_)
        ),
        _ => false,
    }
}

/// Silently (re)create `name` next to this binary if it is currently missing. Used by
/// `self-update`: if the alias existed before an in-place update, make sure it still does
/// afterwards. Never overwrites a foreign file or an alias pointing elsewhere — same
/// conservative rule as `install` — and never fails the update it's called from; a failure
/// here is swallowed, since self-update already reported its own result.
pub(crate) fn reinstall_silently(name: &str) {
    if let (Ok(dir), Ok(target)) = (alias_dir(), this_binary()) {
        let alias_path = platform_alias_path(&dir, name);

        if matches!(inspect(&alias_path, &target), Existing::None) {
            let _ = create_alias(&alias_path, &target);
        }
    }
}

/// What, if anything, already sits at the alias path.
enum Existing {
    /// Nothing there.
    None,

    /// An alias forklift manages, already pointing at `target`.
    PointsHere,

    /// An alias forklift manages, but pointing somewhere else.
    PointsElsewhere(PathBuf),

    /// Something is there, but it is not an alias forklift recognizes as its own (a real
    /// file, a directory, or unrecognized content) — never touched.
    Foreign,
}

/// The directory the alias is created next to: `FORKLIFT_ALIAS_DIR` when set, otherwise this
/// binary's own directory.
fn alias_dir() -> Result<PathBuf, String> {
    if let Ok(dir) = env::var(ENV_ALIAS_DIR) {
        if !dir.trim().is_empty() {
            return Ok(PathBuf::from(dir));
        }
    }

    this_binary()?
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "This binary has no parent directory.".to_string())
}

/// This binary's own resolved (canonical) path — the alias's target.
fn this_binary() -> Result<PathBuf, String> {
    let exe = env::current_exe()
        .map_err(|e| format!("Could not resolve this binary's own path: {}", e))?;

    Ok(exe.canonicalize().unwrap_or(exe))
}

fn display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Add a suggestion to a filesystem error when it looks like a permissions problem.
fn io_hint(verb: &str, path: &Path, error: io::Error) -> String {
    if error.kind() == io::ErrorKind::PermissionDenied {
        format!(
            "Could not {} \"{}\": permission denied. Try again with elevated privileges (e.g. \
            sudo), or use an install directory you own.",
            verb, display(path)
        )
    } else {
        format!("Could not {} \"{}\": {}", verb, display(path), error)
    }
}

// ── Unix: a real symlink ─────────────────────────────────────────────────

#[cfg(unix)]
fn platform_alias_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[cfg(unix)]
fn inspect(alias_path: &Path, target: &Path) -> Existing {
    let metadata = match std::fs::symlink_metadata(alias_path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Existing::None,
        Err(_) => return Existing::Foreign,
    };

    if !metadata.file_type().is_symlink() {
        return Existing::Foreign;
    }

    match std::fs::canonicalize(alias_path) {
        Ok(resolved) if resolved == target => Existing::PointsHere,
        Ok(resolved) => Existing::PointsElsewhere(resolved),
        // A dangling symlink (or an unreadable target): can't be pointing at this binary.
        Err(_) => Existing::PointsElsewhere(
            std::fs::read_link(alias_path).unwrap_or_default()
        ),
    }
}

#[cfg(unix)]
fn create_alias(alias_path: &Path, target: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, alias_path)
}

#[cfg(unix)]
fn remove_alias(alias_path: &Path) -> io::Result<()> {
    std::fs::remove_file(alias_path)
}

// ── Windows: symlinks need elevated privilege, so drop a `.cmd` shim instead ──────────────

#[cfg(windows)]
const SHIM_MARKER: &str = "rem forklift-alias: managed by \"forklift alias\" \u{2014} do not edit by hand";

#[cfg(windows)]
fn platform_alias_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{}.cmd", name))
}

#[cfg(windows)]
fn shim_content(target: &Path) -> String {
    format!("@echo off\r\n{}\r\n\"{}\" %*\r\n", SHIM_MARKER, target.display())
}

/// Pull the quoted target path back out of a shim this command wrote.
#[cfg(windows)]
fn shim_target(content: &str) -> Option<PathBuf> {
    content.lines()
        .find(|line| !line.trim_start().starts_with("rem") && line.contains('"'))
        .and_then(|line| line.split('"').nth(1))
        .map(PathBuf::from)
}

#[cfg(windows)]
fn inspect(alias_path: &Path, target: &Path) -> Existing {
    let content = match std::fs::read_to_string(alias_path) {
        Ok(content) => content,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Existing::None,
        Err(_) => return Existing::Foreign,
    };

    if !content.contains(SHIM_MARKER) {
        return Existing::Foreign;
    }

    match shim_target(&content) {
        Some(existing) => {
            // Canonicalize before comparing: the shim was written with `target.display()`,
            // but `target` here is already canonical, so a raw comparison misclassifies
            // PointsHere as PointsElsewhere on case-insensitive filesystems or when short
            // (8.3) vs. long paths differ. Fall back to the raw path if it no longer
            // resolves (e.g. a dangling target after the binary moved).
            let resolved = existing.canonicalize().unwrap_or_else(|_| existing.clone());

            if resolved == *target {
                Existing::PointsHere
            } else {
                Existing::PointsElsewhere(resolved)
            }
        }
        None => Existing::PointsElsewhere(PathBuf::new()),
    }
}

#[cfg(windows)]
fn create_alias(alias_path: &Path, target: &Path) -> io::Result<()> {
    std::fs::write(alias_path, shim_content(target))
}

#[cfg(windows)]
fn remove_alias(alias_path: &Path) -> io::Result<()> {
    std::fs::remove_file(alias_path)
}

// ── Anything else: no supported mechanism yet ─────────────────────────────

#[cfg(not(any(unix, windows)))]
fn platform_alias_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(name)
}

#[cfg(not(any(unix, windows)))]
fn inspect(_alias_path: &Path, _target: &Path) -> Existing {
    Existing::None
}

#[cfg(not(any(unix, windows)))]
fn create_alias(_alias_path: &Path, _target: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the fl alias is not supported yet on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
fn remove_alias(_alias_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "the fl alias is not supported yet on this platform",
    ))
}

/// The result of creating (or confirming) the alias.
#[derive(Serialize)]
struct Installed {
    name: String,
    path: String,
    target: String,

    /// Whether the alias already existed and pointed here (idempotent no-op) vs. was just
    /// created by this run.
    already_installed: bool,
}

impl CommandOutput for Installed {
    fn render_human(&self) {
        if self.already_installed {
            println!("\"{}\" already points at \"{}\" — nothing to do.", self.path, self.target);
        } else {
            println!("Created \"{}\" -> \"{}\".", self.path, self.target);
        }
    }
}

/// The result of removing (or confirming the absence of) the alias.
#[derive(Serialize)]
struct Uninstalled {
    name: String,
    path: String,

    /// Whether anything was actually removed (`false` if it was already absent).
    removed: bool,
}

impl CommandOutput for Uninstalled {
    fn render_human(&self) {
        if self.removed {
            println!("Removed \"{}\".", self.path);
        } else {
            println!("\"{}\" was not installed — nothing to do.", self.path);
        }
    }
}

/// Whether the alias exists, and where it points.
#[derive(Serialize)]
struct StatusReport {
    name: String,
    path: String,
    installed: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<String>,
}

impl CommandOutput for StatusReport {
    fn render_human(&self) {
        match &self.target {
            Some(target) => println!("\"{}\" -> \"{}\"", self.path, target),
            None => println!(
                "No \"{}\" alias installed at \"{}\". Run \"forklift alias install\" to create it.",
                self.name, self.path
            ),
        }
    }
}
