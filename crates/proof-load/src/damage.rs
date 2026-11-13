//! Damaged-file faults and their completeness check over the census of the version under test.
//!
//! Every name a census row lists, where the row's class needs damage, needs a binding whose
//! scenario creates that path before damaging it, or an exemption naming why it cannot be damaged
//! meaningfully. Completeness is per name, not per row. A binding whose target does not exist when
//! the fault is due fails rather than passing vacuously (see `apply`).

use std::path::Path;

use crate::census::{Census, DamageName};

/// One way to damage a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Kind {
    /// Cut to half its length. An empty file has nothing to cut, so it is refused, as for a flip.
    Truncate,
    /// Invert one byte, chosen by the seed.
    FlipByte,
    /// Overwrite every byte with zero, keeping the length.
    Zero,
    /// Remove it; a folder is removed with everything in it.
    Delete,
    /// Create it holding the PID of a process that has exited, as a holder killed hard leaves a lock
    /// file that only exists while a command holds it: planting it is how its scenario creates the
    /// path, so its binding is not checked against what the scenario built.
    DeadPidLock,
}

impl Kind {
    /// Whether the kind creates its target rather than damaging one the scenario built.
    pub fn plants(self) -> bool {
        matches!(self, Kind::DeadPidLock)
    }
}

/// The path a planting binding's `name` names in the main bay: the name with `{bay}` dropped. A
/// planted path is absent by definition, so it is never resolved against the walk.
pub fn planted(name: &str) -> String {
    name.replace("{bay}", "")
}

/// The PID of a process this harness started and has already reaped: a holder that no longer
/// runs. The harness's own binary, asked for its help, is the one program certainly present.
fn dead_pid() -> std::io::Result<u32> {
    let mut child = std::process::Command::new(std::env::current_exe()?).arg("--help")
        .stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null())
        .spawn()?;
    let pid = child.id();
    child.wait()?;
    Ok(pid)
}

/// The warehouse a binding damages: the client's, beside the work tree, or the host's a scenario
/// serves.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Warehouse {
    Client,
    Host,
}

/// A damage binding: the census name it covers, the scenario that creates the path, the warehouse
/// it is damaged in, the kinds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Binding {
    pub name: &'static str,
    pub scenario: &'static str,
    pub warehouse: Warehouse,
    pub kinds: &'static [Kind],
}

/// A census name that is not damaged, with the reason.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Exemption {
    pub name: &'static str,
    pub reason: &'static str,
}

/// The names of `census` that neither a binding nor an exemption covers.
pub fn uncovered(census: &Census, bindings: &[Binding], exemptions: &[Exemption]) -> Vec<DamageName> {
    census.damage_names().into_iter()
        .filter(|name| !bindings.iter().any(|binding| binding.name == name.path)
            && !exemptions.iter().any(|exemption| exemption.name == name.path))
        .collect()
}

/// Bindings and exemptions naming something this census does not list: stale, and reported as
/// missing so the bindings file follows the census rather than drifting from it.
pub fn stale<'a>(census: &Census, bindings: &'a [Binding], exemptions: &'a [Exemption]) -> Vec<&'a str> {
    let names: Vec<String> = census.damage_names().into_iter().map(|name| name.path).collect();
    bindings.iter().map(|binding| binding.name).chain(exemptions.iter().map(|exemption| exemption.name))
        .filter(|name| !names.iter().any(|listed| listed == name))
        .collect()
}

#[derive(Debug, PartialEq, Eq)]
pub enum DamageError {
    /// The target does not exist when the fault is due: the binding's scenario never created it.
    TargetAbsent(String),
    Io(String),
}

/// Applies one damage kind to `path`. `seed` picks the flipped byte.
pub fn apply(path: &Path, kind: Kind, seed: u64) -> Result<(), DamageError> {
    let io = |error: std::io::Error| DamageError::Io(format!("{}: {}", path.display(), error));
    let exists = std::fs::symlink_metadata(path).is_ok();

    match kind {
        Kind::DeadPidLock => {
            if exists {
                return Err(DamageError::Io(format!("{}: already present, so planting it creates nothing", path.display())));
            }
            std::fs::write(path, format!("{}\n", dead_pid().map_err(io)?)).map_err(io)
        }
        _ if !exists => Err(DamageError::TargetAbsent(path.display().to_string())),
        Kind::Delete if std::fs::symlink_metadata(path).map_err(io)?.is_dir() => {
            #[allow(clippy::disallowed_methods)] // a harness fault, on a scratch copy; no retry is wanted
            std::fs::remove_dir_all(path).map_err(io)
        }
        Kind::Delete => {
            #[allow(clippy::disallowed_methods)] // a harness fault, on a scratch copy; no retry is wanted
            std::fs::remove_file(path).map_err(io)
        }
        Kind::Truncate => {
            let length = std::fs::metadata(path).map_err(io)?.len();
            if length == 0 {
                return Err(DamageError::Io(format!("{}: empty, so there is nothing to cut", path.display())));
            }
            std::fs::OpenOptions::new().write(true).open(path).and_then(|file| file.set_len(length / 2)).map_err(io)
        }
        Kind::FlipByte | Kind::Zero => {
            let mut bytes = std::fs::read(path).map_err(io)?;
            if bytes.is_empty() {
                return Err(DamageError::Io(format!("{}: empty, so there is no byte to damage", path.display())));
            }
            if kind == Kind::Zero {
                bytes.iter_mut().for_each(|byte| *byte = 0);
            } else {
                let at = (seed % bytes.len() as u64) as usize;
                bytes[at] = !bytes[at];
            }
            std::fs::write(path, bytes).map_err(io)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn census() -> Census {
        let doc = "<!-- census:start -->\n| Path | Class | Names |\n|---|---|---|\n\
            | `trust` | rewritable | `a::A` |\n\
            | `store.lock`, `serve.lock` | lock | `a::B` |\n\
            | `clock` | probe | `a::C` |\n<!-- census:end -->\n";
        Census::parse(doc, PathBuf::from("x")).unwrap().unwrap()
    }

    /// Revert: match a binding to its row instead of its name → `serve.lock` reads covered by the
    /// `store.lock` binding.
    #[test]
    fn completeness_is_per_listed_name() {
        let bindings = [Binding { name: "store.lock", scenario: "stack", warehouse: Warehouse::Client, kinds: &[Kind::DeadPidLock] }];
        let exemptions = [Exemption { name: "trust", reason: "test" }];
        let missing: Vec<String> = uncovered(&census(), &bindings, &exemptions).into_iter().map(|name| name.path).collect();
        assert_eq!(missing, vec!["serve.lock"]);
    }

    #[test]
    fn a_binding_for_a_name_the_census_lacks_is_stale() {
        let bindings = [Binding { name: "gone", scenario: "stack", warehouse: Warehouse::Client, kinds: &[Kind::Delete] }];
        assert_eq!(stale(&census(), &bindings, &[]), vec!["gone"]);
    }

    /// Revert: return Ok for an absent target → a binding whose scenario never made the path passes.
    #[test]
    fn damage_to_an_absent_target_fails() {
        let dir = std::env::temp_dir().join(format!("proof-load-damage-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("never-made");
        for kind in [Kind::Truncate, Kind::FlipByte, Kind::Zero, Kind::Delete] {
            assert_eq!(apply(&target, kind, 3), Err(DamageError::TargetAbsent(target.display().to_string())));
        }

        apply(&target, Kind::DeadPidLock, 0).unwrap();
        assert!(target.exists());
        assert!(apply(&target, Kind::DeadPidLock, 0).is_err());

        std::fs::write(&target, b"abcd").unwrap();
        apply(&target, Kind::FlipByte, 5).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), [b'a', !b'b', b'c', b'd']);
        apply(&target, Kind::Truncate, 0).unwrap();
        assert_eq!(std::fs::read(&target).unwrap().len(), 2);
    }

    /// A planted lock holds the PID of a process that has exited, one line, as forklift writes its
    /// own; `{bay}` names the main bay's path.
    ///
    /// Revert: plant an empty file → the PID line is missing; plant the live harness's own PID →
    /// it is not a dead holder.
    #[test]
    fn a_planted_lock_names_a_dead_holder() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("proof-load-damage-plant-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(planted("{bay}lock"));
        assert_eq!(target, dir.join("lock"));
        apply(&target, Kind::DeadPidLock, 0).unwrap();
        let text = std::fs::read_to_string(&target).unwrap();
        assert!(text.ends_with('\n') && text.lines().count() == 1, "{:?}", text);
        let pid: u32 = text.trim().parse().unwrap();
        assert_ne!(pid, std::process::id());
        #[cfg(target_os = "linux")]
        assert!(!std::path::Path::new(&format!("/proc/{}", pid)).exists(), "process {} still runs", pid);
        assert!(Kind::DeadPidLock.plants() && !Kind::Delete.plants());
    }

    /// No kind but Delete applies to an empty file (as the lock files are): each would leave it
    /// as it was, a fault that damages nothing.
    ///
    /// Revert: truncate an empty file to half of nothing → it succeeds and changes nothing.
    #[test]
    fn an_empty_file_has_nothing_to_truncate_flip_or_zero() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("proof-load-damage-empty-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("format.lock");
        std::fs::write(&target, b"").unwrap();
        for kind in [Kind::Truncate, Kind::FlipByte, Kind::Zero] {
            assert!(matches!(apply(&target, kind, 1), Err(DamageError::Io(_))), "{:?} of an empty file", kind);
        }
        apply(&target, Kind::Delete, 0).unwrap();
        assert!(std::fs::symlink_metadata(&target).is_err());
    }

    /// A folder binding's Delete removes the folder and what it holds; a file's removes the file.
    ///
    /// Revert: delete a folder with remove_file → every folder binding errors at run time.
    #[test]
    fn delete_removes_a_folder_whole() {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("proof-load-damage-folder-{}-{}", std::process::id(), nanos));
        std::fs::create_dir_all(dir.join("objects/ab")).unwrap();
        std::fs::write(dir.join("objects/ab/cd"), b"object").unwrap();
        std::fs::write(dir.join("warehouse"), b"marker").unwrap();

        apply(&dir.join("objects"), Kind::Delete, 0).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("objects")).is_err());
        apply(&dir.join("warehouse"), Kind::Delete, 0).unwrap();
        assert!(std::fs::symlink_metadata(dir.join("warehouse")).is_err());
    }
}
