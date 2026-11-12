//! The copy-only rule: every run works inside a scratch area the harness created, and every tool
//! runs in a scrubbed environment that points it there.
//!
//! Paths are half of "never touches the user's data"; the tools also find the user's files through
//! the environment. So each tool starts from an empty environment plus PATH (and, on Windows, the
//! few system variables no program runs without: see [`system_variables_when`]), with HOME,
//! XDG_CONFIG_HOME and TMPDIR inside the scratch area, every `FORKLIFT_*` variable unset unless the
//! scenario sets it on purpose (and the report records it), and git reading harness-written global
//! and system config. An inherited FORKLIFT_FSYNC=0 would otherwise void every power-loss grade.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, PartialEq, Eq)]
pub enum CopyOnlyError {
    /// The output directory exists and holds something.
    OutputNotEmpty(PathBuf),
    /// The output directory is not a directory.
    OutputNotADirectory(PathBuf),
    /// A scratch area would be created over something that already exists.
    ScratchExists(PathBuf),
    /// A scenario asked to set a variable the scrub owns.
    ReservedVariable(String),
    Io { path: PathBuf, reason: String },
}

impl fmt::Display for CopyOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyOnlyError::OutputNotEmpty(path) => write!(f, "the output directory {} is not empty; give an empty or new one", path.display()),
            CopyOnlyError::OutputNotADirectory(path) => write!(f, "the output path {} is not a directory", path.display()),
            CopyOnlyError::ScratchExists(path) => write!(f, "the scratch area {} already exists; a run never reuses one", path.display()),
            CopyOnlyError::ReservedVariable(name) => write!(f, "{} is set by the harness and cannot be set by a scenario", name),
            CopyOnlyError::Io { path, reason } => write!(f, "{}: {}", path.display(), reason),
        }
    }
}

impl std::error::Error for CopyOnlyError {}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> CopyOnlyError + '_ {
    move |error| CopyOnlyError::Io { path: path.to_path_buf(), reason: error.to_string() }
}

/// The output directory of a run, claimed only when it is new or empty.
#[derive(Debug)]
pub struct Output {
    root: PathBuf,
    next_scratch: u64,
}

impl Output {
    /// Claims `path` as the run's output directory, creating it if absent. Anything already inside
    /// refuses the claim: the harness never accepts a path to existing data.
    pub fn claim(path: &Path) -> Result<Output, CopyOnlyError> {
        match std::fs::symlink_metadata(path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(path).map_err(io(path))?;
            }
            Err(error) => return Err(io(path)(error)),
            Ok(metadata) if !metadata.is_dir() => return Err(CopyOnlyError::OutputNotADirectory(path.to_path_buf())),
            Ok(_) => {
                if std::fs::read_dir(path).map_err(io(path))?.next().is_some() {
                    return Err(CopyOnlyError::OutputNotEmpty(path.to_path_buf()));
                }
            }
        }

        Ok(Output { root: path.to_path_buf(), next_scratch: 0 })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Creates a fresh scratch area for one scenario run, inside the output directory.
    pub fn scratch(&mut self, label: &str) -> Result<Scratch, CopyOnlyError> {
        let label: String = label.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' }).collect();
        let root = self.root.join("scratch").join(format!("{:04}-{}", self.next_scratch, label));
        self.next_scratch += 1;
        Scratch::create(&root)
    }
}

/// One run's scratch area: its warehouses, HOME, TMPDIR and git config all live inside it.
#[derive(Debug)]
pub struct Scratch {
    root: PathBuf,
}

impl Scratch {
    /// Creates the area at `root`, which must not exist yet.
    pub fn create(root: &Path) -> Result<Scratch, CopyOnlyError> {
        if std::fs::symlink_metadata(root).is_ok() {
            return Err(CopyOnlyError::ScratchExists(root.to_path_buf()));
        }
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(io(parent))?;
        }
        std::fs::create_dir(root).map_err(io(root))?;

        let scratch = Scratch { root: root.to_path_buf() };
        for dir in [scratch.home(), scratch.tmp(), scratch.config_home(), scratch.work()] {
            std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        }
        for (path, contents) in [(scratch.git_global_config(), GIT_CONFIG), (scratch.git_system_config(), GIT_CONFIG)] {
            std::fs::write(&path, contents).map_err(io(&path))?;
        }
        Ok(scratch)
    }

    pub fn root(&self) -> &Path { &self.root }
    pub fn home(&self) -> PathBuf { self.root.join("home") }
    pub fn tmp(&self) -> PathBuf { self.root.join("tmp") }
    pub fn config_home(&self) -> PathBuf { self.root.join("home/.config") }
    /// Where the scenario's warehouses and working folders go.
    pub fn work(&self) -> PathBuf { self.root.join("work") }
    pub fn git_global_config(&self) -> PathBuf { self.root.join("gitconfig-global") }
    pub fn git_system_config(&self) -> PathBuf { self.root.join("gitconfig-system") }
}

/// The git config the harness writes for both the global and the system level: an identity, so git
/// can record, and nothing else, so "git at its defaults" means git's defaults, not the user's.
const GIT_CONFIG: &str = "[user]\n\tname = Proof Load\n\temail = proof-load@invalid\n";

/// The variables the scrub sets itself; a scenario may not override them.
const RESERVED: &[&str] = &["PATH", "HOME", "XDG_CONFIG_HOME", "TMPDIR", "TMP", "TEMP", "USERPROFILE", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM", "GIT_CONFIG_NOSYSTEM"];

/// The exact environment a tool runs under.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Environment {
    /// Every variable the tool sees, and nothing else.
    pub variables: BTreeMap<String, String>,
    /// The `FORKLIFT_*` variables the scenario set on purpose, for the report.
    pub scenario_set: BTreeMap<String, String>,
}

impl Environment {
    /// The scrubbed environment for a tool run in `scratch`, keeping only `path` from the
    /// harness's own environment, plus what the scenario sets on purpose.
    pub fn scrubbed(scratch: &Scratch, path: Option<OsString>, scenario: &BTreeMap<String, String>) -> Result<Environment, CopyOnlyError> {
        let text = |path: PathBuf| path.to_string_lossy().into_owned();
        let mut variables = BTreeMap::new();

        if let Some(path) = path {
            variables.insert("PATH".to_string(), path.to_string_lossy().into_owned());
        }
        variables.insert("HOME".to_string(), text(scratch.home()));
        variables.insert("XDG_CONFIG_HOME".to_string(), text(scratch.config_home()));
        variables.insert("TMPDIR".to_string(), text(scratch.tmp()));
        variables.extend(system_variables_when(cfg!(windows), |name| std::env::var_os(name)));
        if cfg!(windows) {
            variables.insert("USERPROFILE".to_string(), text(scratch.home()));
            variables.insert("TMP".to_string(), text(scratch.tmp()));
            variables.insert("TEMP".to_string(), text(scratch.tmp()));
        }
        variables.insert("GIT_CONFIG_GLOBAL".to_string(), text(scratch.git_global_config()));
        variables.insert("GIT_CONFIG_SYSTEM".to_string(), text(scratch.git_system_config()));

        let mut scenario_set = BTreeMap::new();
        for (name, value) in scenario {
            if RESERVED.contains(&name.as_str()) {
                return Err(CopyOnlyError::ReservedVariable(name.clone()));
            }
            variables.insert(name.clone(), value.clone());
            if name.starts_with("FORKLIFT_") {
                scenario_set.insert(name.clone(), value.clone());
            }
        }

        Ok(Environment { variables, scenario_set })
    }

    /// Applies this environment to a command, clearing everything it inherited first.
    pub fn apply(&self, command: &mut std::process::Command) {
        command.env_clear();
        command.envs(&self.variables);
    }
}

/// The inherited variables a Windows program cannot run without, when `windows`: without
/// SystemRoot, Winsock fails to start (`forklift-server` cannot bind, os error 10106), and git for
/// Windows and the shell need the rest. None of them points at the user's files. Elsewhere, none.
pub fn system_variables_when(windows: bool, inherited: impl Fn(&str) -> Option<OsString>) -> Vec<(String, String)> {
    if !windows {
        return Vec::new();
    }
    ["SystemRoot", "SystemDrive", "windir", "ComSpec", "PATHEXT"].into_iter()
        .filter_map(|name| Some((name.to_string(), inherited(name)?.to_string_lossy().into_owned())))
        .collect()
}

/// The PATH the tools run with: the readback git's own directory first, then the harness's PATH,
/// so a `git` that forklift itself starts (as `git export` does) is the one the run records.
pub fn search_path(git: Option<&Path>, inherited: Option<OsString>) -> Option<OsString> {
    let first = git.and_then(Path::parent).map(Path::to_path_buf);
    let rest = inherited.iter().flat_map(std::env::split_paths).collect::<Vec<_>>();
    if first.is_none() && rest.is_empty() {
        return inherited;
    }
    std::env::join_paths(first.into_iter().chain(rest)).ok().or(inherited)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("proof-load-{}-{}-{}", name, std::process::id(), nanos));
        assert!(std::fs::symlink_metadata(&path).is_err(), "the test's path is fresh");
        path
    }

    /// Revert: accept an existing non-empty directory → the user's data becomes a target.
    #[test]
    fn an_output_directory_with_anything_in_it_is_refused() {
        let root = fresh("output-full");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("precious"), b"user data").unwrap();
        assert_eq!(Output::claim(&root).unwrap_err(), CopyOnlyError::OutputNotEmpty(root.clone()));
        assert_eq!(std::fs::read(root.join("precious")).unwrap(), b"user data");

        let file = root.join("precious");
        assert_eq!(Output::claim(&file).unwrap_err(), CopyOnlyError::OutputNotADirectory(file));

        let empty = root.join("empty");
        std::fs::create_dir(&empty).unwrap();
        assert!(Output::claim(&empty).is_ok());
        let new = root.join("new");
        assert!(Output::claim(&new).is_ok());
        assert!(new.is_dir());
    }

    /// Revert: let a scratch area reuse an existing path → a second run inherits the first's state.
    #[test]
    fn scratch_areas_are_always_new() {
        let root = fresh("scratch");
        let mut output = Output::claim(&root).unwrap();
        let first = output.scratch("stack kill").unwrap();
        let second = output.scratch("stack kill").unwrap();
        assert_ne!(first.root(), second.root());
        assert!(first.home().is_dir() && first.tmp().is_dir() && first.work().is_dir());
        assert_eq!(Scratch::create(first.root()).unwrap_err(), CopyOnlyError::ScratchExists(first.root().to_path_buf()));
    }

    /// Each variable that leads a tool to the user's files points into the scratch area, and
    /// nothing inherited survives except PATH.
    ///
    /// Revert: drop `env_clear` → the inherited FORKLIFT_FSYNC=0 reaches the child; point HOME at
    /// the harness's own → the HOME assertion.
    #[cfg(unix)]
    #[test]
    fn a_tool_sees_only_the_scrubbed_environment() {
        let root = fresh("scrub");
        let scratch = Scratch::create(&root).unwrap();
        let environment = Environment::scrubbed(&scratch, Some(OsString::from("/usr/bin:/bin")), &BTreeMap::new()).unwrap();

        let mut command = std::process::Command::new("/usr/bin/env");
        command.env("FORKLIFT_FSYNC", "0").env("FORKLIFT_KEYS_DIR", "/home/user/keys").env("HOME", "/home/user");
        environment.apply(&mut command);
        let seen = String::from_utf8(command.output().unwrap().stdout).unwrap();
        let seen: BTreeMap<String, String> = seen.lines()
            .filter_map(|line| line.split_once('=')).map(|(k, v)| (k.to_string(), v.to_string())).collect();

        assert_eq!(seen, environment.variables);
        assert!(!seen.keys().any(|name| name.starts_with("FORKLIFT_")));
        for name in ["HOME", "XDG_CONFIG_HOME", "TMPDIR", "GIT_CONFIG_GLOBAL", "GIT_CONFIG_SYSTEM"] {
            assert!(Path::new(&seen[name]).starts_with(scratch.root()), "{} points into the scratch area: {}", name, seen[name]);
        }
    }

    /// On Windows the system variables survive the scrub, taken from the harness; elsewhere none
    /// is added.
    ///
    /// Revert: return nothing on Windows → SystemRoot is lost (forklift-server cannot bind there).
    #[test]
    fn windows_keeps_the_system_variables_and_nothing_else_does() {
        let inherited = |name: &str| (name != "PATHEXT").then(|| OsString::from(format!("v-{}", name)));
        let windows: BTreeMap<String, String> = system_variables_when(true, inherited).into_iter().collect();
        assert_eq!(windows.get("SystemRoot").map(String::as_str), Some("v-SystemRoot"));
        assert_eq!(windows.keys().map(String::as_str).collect::<Vec<_>>(), vec!["ComSpec", "SystemDrive", "SystemRoot", "windir"]);
        assert!(system_variables_when(false, inherited).is_empty());
    }

    /// The readback git's directory leads PATH, ahead of everything inherited.
    ///
    /// Revert: keep the inherited PATH as it is → a `git` earlier on it is the one forklift starts.
    #[test]
    fn the_readback_gits_directory_leads_path() {
        let joined = search_path(Some(Path::new("/opt/git/bin/git")), Some(std::env::join_paths(["/usr/bin", "/bin"]).unwrap())).unwrap();
        let parts: Vec<PathBuf> = std::env::split_paths(&joined).collect();
        assert_eq!(parts, vec![PathBuf::from("/opt/git/bin"), PathBuf::from("/usr/bin"), PathBuf::from("/bin")]);
        assert_eq!(search_path(None, Some(OsString::from("/usr/bin"))), Some(OsString::from("/usr/bin")));
    }

    /// A scenario's own FORKLIFT_* setting reaches the tool and is recorded; one that would move
    /// HOME or git's config out of the scratch area is refused.
    ///
    /// Revert: skip the reserved check → a scenario could point HOME back at the user's.
    #[test]
    fn a_scenario_sets_only_what_it_may_and_it_is_recorded() {
        let root = fresh("scenario-env");
        let scratch = Scratch::create(&root).unwrap();

        let set = BTreeMap::from([("FORKLIFT_FSYNC".to_string(), "0".to_string())]);
        let environment = Environment::scrubbed(&scratch, None, &set).unwrap();
        assert_eq!(environment.variables["FORKLIFT_FSYNC"], "0");
        assert_eq!(environment.scenario_set, set);

        for name in ["HOME", "GIT_CONFIG_GLOBAL", "TMPDIR"] {
            let set = BTreeMap::from([(name.to_string(), "/home/user".to_string())]);
            assert_eq!(Environment::scrubbed(&scratch, None, &set).unwrap_err(), CopyOnlyError::ReservedVariable(name.to_string()));
        }
    }
}
