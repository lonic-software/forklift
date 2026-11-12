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
    /// Cut to half its length (an empty file is cut to nothing and recorded as such).
    Truncate,
    /// Invert one byte, chosen by the seed.
    FlipByte,
    /// Overwrite every byte with zero, keeping the length.
    Zero,
    /// Remove it.
    Delete,
    /// Create it, for a lock file that only exists while a command holds it: planting it is how its
    /// scenario creates the path.
    Plant,
}

/// A damage binding: the census name it covers, the scenario that creates the path, the kinds.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct Binding {
    pub name: &'static str,
    pub scenario: &'static str,
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
        Kind::Plant => {
            if exists {
                return Err(DamageError::Io(format!("{}: already present, so planting it creates nothing", path.display())));
            }
            std::fs::write(path, b"").map_err(io)
        }
        _ if !exists => Err(DamageError::TargetAbsent(path.display().to_string())),
        Kind::Delete => {
            #[allow(clippy::disallowed_methods)] // a harness fault, on a scratch copy; no retry is wanted
            std::fs::remove_file(path).map_err(io)
        }
        Kind::Truncate => {
            let length = std::fs::metadata(path).map_err(io)?.len();
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
        let bindings = [Binding { name: "store.lock", scenario: "stack", kinds: &[Kind::Plant] }];
        let exemptions = [Exemption { name: "trust", reason: "test" }];
        let missing: Vec<String> = uncovered(&census(), &bindings, &exemptions).into_iter().map(|name| name.path).collect();
        assert_eq!(missing, vec!["serve.lock"]);
    }

    #[test]
    fn a_binding_for_a_name_the_census_lacks_is_stale() {
        let bindings = [Binding { name: "gone", scenario: "stack", kinds: &[Kind::Delete] }];
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

        apply(&target, Kind::Plant, 0).unwrap();
        assert!(target.exists());
        assert!(apply(&target, Kind::Plant, 0).is_err());

        std::fs::write(&target, b"abcd").unwrap();
        apply(&target, Kind::FlipByte, 5).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), [b'a', !b'b', b'c', b'd']);
        apply(&target, Kind::Truncate, 0).unwrap();
        assert_eq!(std::fs::read(&target).unwrap().len(), 2);
    }
}
