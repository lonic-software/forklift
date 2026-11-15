//! The Run screen's fields and what each one accepts. The checks are made as the fields are typed
//! into, so a field shows its error before Run is pressed, and Run stays disabled while any field
//! has one. Nothing here draws.

use std::path::{Path, PathBuf};

use proof_load::run::RunOptions;

/// The Run screen's fields, as typed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Form {
    pub forklift: String,
    pub forklift_server: String,
    pub git: String,
    pub source: String,
    pub out: String,
    pub only: String,
}

/// Which field a label, hint and check belong to, in the order the screen lays them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Field {
    Forklift,
    ForkliftServer,
    Git,
    Source,
    Out,
    Only,
}

impl Field {
    pub const ALL: [Field; 6] = [Field::Forklift, Field::ForkliftServer, Field::Git, Field::Source, Field::Out, Field::Only];

    pub fn label(self) -> &'static str {
        match self {
            Field::Forklift => "forklift binary",
            Field::ForkliftServer => "forklift-server binary",
            Field::Git => "git binary",
            Field::Source => "Source tree",
            Field::Out => "Report folder",
            Field::Only => "Only",
        }
    }

    /// What the field is for, shown under it while it has no error.
    pub fn hint(self) -> &'static str {
        match self {
            Field::Forklift => "The forklift under test. Without it the scenarios are not run.",
            Field::ForkliftServer => "Without it a scenario that hosts a warehouse is not built.",
            Field::Git => "Lays out exported records. Without it damaged files are not run.",
            Field::Source => "The version under test's source tree, read only, for its census.",
            Field::Out => "An empty or new folder for the scratch areas and the report.",
            Field::Only => "Run only the scenario with this id. Empty runs every scenario.",
        }
    }
}

impl Form {
    pub fn value(&mut self, field: Field) -> &mut String {
        match field {
            Field::Forklift => &mut self.forklift,
            Field::ForkliftServer => &mut self.forklift_server,
            Field::Git => &mut self.git,
            Field::Source => &mut self.source,
            Field::Out => &mut self.out,
            Field::Only => &mut self.only,
        }
    }

    /// What is wrong with `field` as typed, if anything.
    pub fn error(&self, field: Field) -> Option<&'static str> {
        match field {
            Field::Forklift => binary(&self.forklift),
            Field::ForkliftServer => binary(&self.forklift_server),
            Field::Git => binary(&self.git),
            Field::Source => source(&self.source),
            Field::Out => out(&self.out),
            Field::Only => only(&self.only),
        }
    }

    /// Every field's error, in `Field::ALL`'s order.
    pub fn errors(&self) -> [Option<&'static str>; 6] {
        Field::ALL.map(|field| self.error(field))
    }

    /// The run these fields ask for, or `None` while any field has an error.
    pub fn options(&self) -> Option<RunOptions> {
        if Field::ALL.into_iter().any(|field| self.error(field).is_some()) {
            return None;
        }
        let path = |text: &str| (!text.trim().is_empty()).then(|| PathBuf::from(text.trim()));
        Some(RunOptions {
            source: PathBuf::from(self.source.trim()),
            out: PathBuf::from(self.out.trim()),
            seed: 1,
            only: (!self.only.trim().is_empty()).then(|| self.only.trim().to_string()),
            forklift: path(&self.forklift),
            git: path(&self.git),
            forklift_server: path(&self.forklift_server),
            ..RunOptions::default()
        })
    }
}

/// A binary may be left out; given, it is a full path to a file.
fn binary(text: &str) -> Option<&'static str> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    let path = Path::new(text);
    if !path.is_absolute() {
        Some("Give the binary's full path.")
    } else if !path.is_file() {
        Some("No file at this path.")
    } else {
        None
    }
}

/// The source tree is needed, and is a folder.
fn source(text: &str) -> Option<&'static str> {
    let text = text.trim();
    if text.is_empty() {
        Some("A run needs the source tree.")
    } else if !Path::new(text).is_absolute() {
        Some("Give the folder's full path.")
    } else if !Path::new(text).is_dir() {
        Some("No folder at this path.")
    } else {
        None
    }
}

/// The report folder is needed, and is new or an empty folder. A link is judged as itself, not
/// followed, as the run claims the folder.
fn out(text: &str) -> Option<&'static str> {
    let text = text.trim();
    let path = Path::new(text);
    if text.is_empty() {
        return Some("A run needs a folder for its report.");
    }
    if !path.is_absolute() {
        return Some("Give the folder's full path.");
    }
    match std::fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => Some("This folder cannot be read."),
        Ok(metadata) if metadata.is_symlink() => Some("This is a link. Give the folder itself."),
        Ok(metadata) if !metadata.is_dir() => Some("This is a file, not a folder."),
        Ok(_) => match std::fs::read_dir(path).map(|mut entries| entries.next().is_none()) {
            Ok(true) => None,
            Ok(false) => Some("This folder is not empty. A run needs an empty or new one."),
            Err(_) => Some("This folder cannot be read."),
        },
    }
}

/// Only may be left empty; given, it names a scenario the run can build.
fn only(text: &str) -> Option<&'static str> {
    let text = text.trim();
    (!text.is_empty() && !proof_load::scenario::known(text)).then_some("No scenario has this id.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let folder = std::env::temp_dir().join(format!("proof-load-desktop-form-{}-{}", name, std::process::id()));
        std::fs::create_dir_all(&folder).expect("a scratch folder");
        folder
    }

    /// A valid form gives the run it describes; empty binaries are left out.
    #[test]
    fn a_valid_form_gives_its_run() {
        let folder = scratch("valid");
        let binary = folder.join("forklift");
        std::fs::write(&binary, "").expect("written");
        let form = Form {
            forklift: format!(" {} ", binary.display()),
            source: folder.display().to_string(),
            out: folder.join("new").display().to_string(),
            only: "signed".into(),
            ..Form::default()
        };
        let options = form.options().expect("the form is valid");
        assert_eq!(options.forklift.as_deref(), Some(binary.as_path()));
        assert_eq!(options.git, None);
        assert_eq!(options.only.as_deref(), Some("signed"));
        assert_eq!(options.out, folder.join("new"));
    }

    /// Each field refuses what the run would refuse, and any error keeps Run from starting.
    ///
    /// Revert: follow links → a link to an empty folder passes, and the run refuses it after Run.
    #[test]
    fn each_field_refuses_what_the_run_cannot_use() {
        let folder = scratch("errors");
        let file = folder.join("a-file");
        std::fs::write(&file, "").expect("written");
        assert_eq!(binary("forklift"), Some("Give the binary's full path."));
        assert_eq!(binary(&folder.join("missing").display().to_string()), Some("No file at this path."));
        assert_eq!(binary(&folder.display().to_string()), Some("No file at this path."));
        assert_eq!(binary(""), None);
        assert_eq!(source(""), Some("A run needs the source tree."));
        assert_eq!(source(&file.display().to_string()), Some("No folder at this path."));
        assert_eq!(out(&folder.display().to_string()), Some("This folder is not empty. A run needs an empty or new one."));
        assert_eq!(out(&file.display().to_string()), Some("This is a file, not a folder."));
        assert_eq!(out("relative"), Some("Give the folder's full path."));
        let empty = scratch("empty");
        assert_eq!(out(&empty.display().to_string()), None);
        // A link to an empty folder, or to nothing, is refused as the run refuses it.
        #[cfg(unix)]
        {
            let link = folder.join("link");
            std::os::unix::fs::symlink(&empty, &link).expect("a link");
            assert_eq!(out(&link.display().to_string()), Some("This is a link. Give the folder itself."));
            let dangling = folder.join("dangling");
            std::os::unix::fs::symlink(folder.join("nothing"), &dangling).expect("a link");
            assert_eq!(out(&dangling.display().to_string()), Some("This is a link. Give the folder itself."));
        }
        assert_eq!(only("signd"), Some("No scenario has this id."));
        assert_eq!(only("signed"), None);
        assert_eq!(only(" "), None);
        let form = Form { source: folder.display().to_string(), out: folder.display().to_string(), ..Form::default() };
        assert!(form.options().is_none());
    }
}
