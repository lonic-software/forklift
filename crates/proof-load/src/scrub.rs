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
    /// The work tree was moved off the scratch volume once already; a run moves it once.
    WorkMoved(PathBuf),
    /// A scratch area whose work tree lives off its root is not copied: the copy would share it.
    MovedWorkNotCopied(PathBuf),
    Io { path: PathBuf, reason: String },
}

impl fmt::Display for CopyOnlyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CopyOnlyError::OutputNotEmpty(path) => write!(f, "the output directory {} is not empty; give an empty or new one", path.display()),
            CopyOnlyError::OutputNotADirectory(path) => write!(f, "the output path {} is not a directory", path.display()),
            CopyOnlyError::ScratchExists(path) => write!(f, "the scratch area {} already exists; a run never reuses one", path.display()),
            CopyOnlyError::ReservedVariable(name) => write!(f, "{} is set by the harness and cannot be set by a scenario", name),
            CopyOnlyError::WorkMoved(path) => write!(f, "the work tree already moved to {}; a run moves it once", path.display()),
            CopyOnlyError::MovedWorkNotCopied(path) => write!(f, "the scratch area's work tree lives at {}, off its root; such an area is not copied", path.display()),
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

/// One run's scratch area: its warehouses, HOME, TMPDIR and git config all live inside it, except
/// a work tree a clone moved onto a folding volume (see [`Scratch::move_work`]).
#[derive(Debug)]
pub struct Scratch {
    root: PathBuf,
    /// The folder on a volume of its own that a clone moves the work tree into, when the run puts
    /// it there.
    folding: Option<PathBuf>,
    /// Where the work tree moved to, once a clone moved it.
    moved: std::sync::OnceLock<PathBuf>,
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

        let scratch = Scratch { root: root.to_path_buf(), folding: None, moved: std::sync::OnceLock::new() };
        for dir in [scratch.home(), scratch.tmp(), scratch.config_home(), scratch.work()] {
            std::fs::create_dir_all(&dir).map_err(io(&dir))?;
        }
        for (path, contents) in [(scratch.git_global_config(), GIT_CONFIG), (scratch.git_system_config(), GIT_CONFIG)] {
            std::fs::write(&path, contents).map_err(io(&path))?;
        }
        Ok(scratch)
    }

    /// A copy of this whole area, HOME and TMPDIR included, at `root`, which must not exist yet: the
    /// fresh start an injected run gets, so runs on copies of one area are independent of each
    /// other and of it. Symlinks are copied as links, never followed. An area whose work tree moved
    /// off its root is refused: the copy would hold none of it.
    pub fn copy_to(&self, root: &Path) -> Result<Scratch, CopyOnlyError> {
        if let Some(moved) = self.moved.get() {
            return Err(CopyOnlyError::MovedWorkNotCopied(moved.clone()));
        }
        if std::fs::symlink_metadata(root).is_ok() {
            return Err(CopyOnlyError::ScratchExists(root.to_path_buf()));
        }
        copy_tree(&self.root, root)?;
        Ok(Scratch { root: root.to_path_buf(), folding: None, moved: std::sync::OnceLock::new() })
    }

    /// This area, with `folder` (on a volume of its own) as where a clone moves its work tree.
    pub fn with_folding_volume(self, folder: &Path) -> Scratch {
        Scratch { folding: Some(folder.to_path_buf()), ..self }
    }

    /// Moves the work tree off the scratch volume, for a clone to make it there: to a fresh
    /// subfolder of the folding volume, never the volume's own folder, named after this area so
    /// areas sharing the volume never meet. None without a folding volume, where the work tree
    /// stays at `root/work`. A second move is refused, as is a target that already exists.
    ///
    /// Only the clone moves it: the history before it is recorded on the scratch volume, since a
    /// file first tracked on a volume without an executable bit is recorded without one.
    pub fn move_work(&self) -> Result<Option<PathBuf>, CopyOnlyError> {
        let Some(folding) = &self.folding else { return Ok(None) };
        if let Some(moved) = self.moved.get() {
            return Err(CopyOnlyError::WorkMoved(moved.clone()));
        }
        let name = self.root.file_name().map(|name| name.to_string_lossy().into_owned()).unwrap_or_default();
        let target = folding.join(format!("{}-work", name));
        if std::fs::symlink_metadata(&target).is_ok() {
            return Err(CopyOnlyError::ScratchExists(target));
        }
        self.moved.set(target.clone()).map_err(|_| CopyOnlyError::WorkMoved(target.clone()))?;
        Ok(Some(target))
    }

    /// The folders a tool may run in and a remedy may remove under: the scratch root, and the work
    /// tree once it moved off it. Nothing else.
    pub fn roots(&self) -> Vec<&Path> {
        std::iter::once(self.root.as_path()).chain(self.moved.get().map(PathBuf::as_path)).collect()
    }

    /// Whether the work tree moved off the scratch volume.
    pub fn work_moved(&self) -> bool {
        self.moved.get().is_some()
    }

    pub fn root(&self) -> &Path { &self.root }
    pub fn home(&self) -> PathBuf { self.root.join("home") }
    pub fn tmp(&self) -> PathBuf { self.root.join("tmp") }
    pub fn config_home(&self) -> PathBuf { self.root.join("home/.config") }
    /// Where the scenario's work tree is: `root/work`, or where a clone moved it. Read at each use,
    /// never bound once, since a clone in the history moves it.
    pub fn work(&self) -> PathBuf { self.moved.get().cloned().unwrap_or_else(|| self.root.join("work")) }
    pub fn git_global_config(&self) -> PathBuf { self.root.join("gitconfig-global") }
    pub fn git_system_config(&self) -> PathBuf { self.root.join("gitconfig-system") }
}

/// Copies the tree at `from` to `to`: folders with their permissions, files with their bytes and
/// permissions, symlinks as links.
fn copy_tree(from: &Path, to: &Path) -> Result<(), CopyOnlyError> {
    let metadata = std::fs::symlink_metadata(from).map_err(io(from))?;
    if metadata.file_type().is_symlink() {
        let target = std::fs::read_link(from).map_err(io(from))?;
        #[cfg(unix)]
        return std::os::unix::fs::symlink(&target, to).map_err(io(to));
        #[cfg(not(unix))]
        return Err(CopyOnlyError::Io { path: from.to_path_buf(), reason: format!("a symlink to {} is not copied on this platform", target.display()) });
    }
    if metadata.is_dir() {
        std::fs::create_dir(to).map_err(io(to))?;
        let mut entries: Vec<_> = std::fs::read_dir(from).map_err(io(from))?.collect::<Result<_, _>>().map_err(io(from))?;
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            copy_tree(&entry.path(), &to.join(entry.file_name()))?;
        }
        return std::fs::set_permissions(to, metadata.permissions()).map_err(io(to));
    }
    std::fs::copy(from, to).map(|_| ()).map_err(io(from))
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

    /// A copy holds every folder, file and link of the area, is independent of it, and is never
    /// made over something that exists.
    ///
    /// Revert: follow symlinks → the link's target is copied as a file; skip the existence check →
    /// a copy lands inside an existing area.
    #[cfg(unix)]
    #[test]
    fn a_scratch_copy_is_whole_and_independent() {
        let scratch = Scratch::create(&fresh("copy-source")).unwrap();
        std::fs::create_dir_all(scratch.work().join(".forklift/objects/ab")).unwrap();
        std::fs::write(scratch.work().join(".forklift/objects/ab/cd"), b"object").unwrap();
        std::fs::write(scratch.home().join("config"), b"home").unwrap();
        std::os::unix::fs::symlink("objects/ab/cd", scratch.work().join(".forklift/link")).unwrap();
        let copy = scratch.copy_to(&fresh("copy-target")).unwrap();
        assert_eq!(std::fs::read(copy.work().join(".forklift/objects/ab/cd")).unwrap(), b"object");
        assert_eq!(std::fs::read(copy.home().join("config")).unwrap(), b"home");
        assert_eq!(std::fs::read(copy.git_global_config()).unwrap(), GIT_CONFIG.as_bytes());
        assert_eq!(std::fs::read_link(copy.work().join(".forklift/link")).unwrap(), PathBuf::from("objects/ab/cd"));
        std::fs::write(copy.work().join(".forklift/objects/ab/cd"), b"changed").unwrap();
        assert_eq!(std::fs::read(scratch.work().join(".forklift/objects/ab/cd")).unwrap(), b"object");
        assert!(matches!(scratch.copy_to(copy.root()), Err(CopyOnlyError::ScratchExists(_))));
    }

    /// Without a folding volume the work tree never moves; with one, it moves once, to a fresh
    /// subfolder named after the area, which every later read of the work tree then names, and
    /// the area is no longer copied.
    ///
    /// Revert: target the folding folder itself → the work tree is the volume's own folder; drop
    /// the second-move refusal → a later clone moves it again, away from what earlier steps wrote.
    #[test]
    fn the_work_tree_moves_once_and_only_onto_a_folding_volume() {
        let plain = Scratch::create(&fresh("unmoved")).unwrap();
        assert_eq!(plain.move_work().unwrap(), None);
        assert_eq!(plain.work(), plain.root().join("work"));
        assert_eq!(plain.roots(), vec![plain.root()]);

        let folding = fresh("folding");
        std::fs::create_dir_all(&folding).unwrap();
        let scratch = Scratch::create(&fresh("moved")).unwrap().with_folding_volume(&folding);
        assert_eq!(scratch.work(), scratch.root().join("work"), "the history before the clone stays on the scratch volume");
        let moved = scratch.move_work().unwrap().unwrap();
        assert_eq!(moved.parent(), Some(folding.as_path()));
        assert_ne!(moved, folding);
        assert_eq!(scratch.work(), moved);
        assert_eq!(scratch.roots(), vec![scratch.root(), moved.as_path()]);
        assert_eq!(scratch.move_work().unwrap_err(), CopyOnlyError::WorkMoved(moved.clone()));
        assert_eq!(scratch.copy_to(&fresh("moved-copy")).unwrap_err(), CopyOnlyError::MovedWorkNotCopied(moved.clone()));

        // An area whose target already exists is refused rather than shared.
        let clash = Scratch::create(&fresh("clash")).unwrap().with_folding_volume(&folding);
        let name = clash.root().file_name().unwrap().to_string_lossy().into_owned();
        std::fs::create_dir(folding.join(format!("{}-work", name))).unwrap();
        assert!(matches!(clash.move_work(), Err(CopyOnlyError::ScratchExists(_))));
        assert_eq!(clash.work(), clash.root().join("work"));
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
