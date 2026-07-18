use std::path::{Component, Path, PathBuf};
use crate::util::warehouse_utils;

/// Whether `ancestor` "covers" `key` under the warehouse's subtree-membership rule: `ancestor`
/// is `key` itself, a strict ancestor of it (`key` starts with `"{ancestor}/"`), or the empty
/// key (the warehouse root, which covers everything). Both arguments are raw path keys, not
/// [`WarehousePath`] instances, because every current caller already holds one as a `&str` (an
/// inventory metadata entry, or a recorded marker path) rather than a freshly-normalized path —
/// forcing a `WarehousePath` round-trip at each call site would just re-validate input that was
/// already normalized once, upstream.
///
/// The one subtree-membership test shared by every walk that needs to know "is this key at or
/// under this path": the incomplete-load marker's covers-based update rule
/// (`load_guard_utils`), `load`'s dirty-inventory-path pass (`inventory_utils`), and `restore`'s
/// directory walk all use this single predicate instead of each hand-rolling their own copy of
/// the same three-way check.
///
/// # Arguments
/// * `ancestor` - The candidate ancestor-or-self key.
/// * `key`      - The key being tested.
pub fn covers(ancestor: &str, key: &str) -> bool {
    ancestor.is_empty() || key == ancestor || key.starts_with(&format!("{}/", ancestor))
}

/// A normalized path inside the warehouse, relative to the warehouse root.
///
/// Invariants:
/// * Components are joined with `/` on every platform.
/// * There is no leading `./`, no trailing `/`, and no `.` or `..` component.
/// * The warehouse root itself is represented as the empty string.
///
/// Every user-supplied path must be converted to a `WarehousePath` at the command boundary,
/// so that the storage layer only ever sees one canonical representation of a given path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WarehousePath {
    relative: String,
}

impl WarehousePath {
    /// Create a warehouse path from raw user input.
    /// Relative paths are resolved against the directory the user invoked forklift from;
    /// absolute paths must point inside the warehouse.
    ///
    /// # Arguments
    /// * `raw` - The path as the user entered it.
    ///
    /// # Returns
    /// * `Ok(WarehousePath)` - The normalized path.
    /// * `Err(String)`       - If the path points outside the warehouse, is not valid UTF-8,
    ///                        or contains an ASCII control character.
    pub fn from_user_input(raw: &str) -> Result<WarehousePath, String> {
        // A control character is never legitimate in a warehouse path (defense in depth,
        // §7.6 u7): beyond being nonsensical input, the §7.4 machine-interface envelope and
        // the scope-refusal framing (`scope_utils::REFUSAL_PREFIX`) both use a reserved
        // control byte (ASCII Unit Separator) to delimit their fields — a path containing one
        // would mangle those messages downstream. Reject at the boundary instead.
        if let Some(control) = raw.chars().find(|c| c.is_control()) {
            return Err(format!(
                "Path \"{}\" contains a control character ({:?}), which is never valid in a \
                warehouse path.",
                raw.escape_debug(), control
            ));
        }

        let raw_path = Path::new(raw);

        let root_relative = if raw_path.is_absolute() {
            // The process runs from the warehouse root (see `enter_warehouse`),
            // so the current directory is the root the absolute path must be inside of.
            let root = std::env::current_dir()
                .map_err(|e| format!("Error while getting the current directory: {}", e))?;

            raw_path.strip_prefix(&root)
                .map_err(|_| format!("Path \"{}\" is outside of the warehouse.", raw))?
                .to_path_buf()
        } else {
            warehouse_utils::cwd_relative_to_root().join(raw_path)
        };

        Self::from_root_relative(&root_relative, raw)
    }

    /// Create a warehouse path from a path that is already relative to the warehouse root,
    /// resolving `.` and `..` components lexically.
    ///
    /// # Arguments
    /// * `path` - The root-relative path to normalize.
    /// * `raw`  - The original user input (only used in error messages).
    ///
    /// # Returns
    /// * `Ok(WarehousePath)` - The normalized path.
    /// * `Err(String)`       - If the path escapes the warehouse root or is not valid UTF-8.
    fn from_root_relative(path: &Path, raw: &str) -> Result<WarehousePath, String> {
        let mut parts: Vec<String> = Vec::new();

        for component in path.components() {
            match component {
                Component::Normal(part) => {
                    let part = part.to_str()
                        .ok_or("Error while converting path to UTF-8.".to_string())?;
                    parts.push(part.to_string());
                }
                Component::CurDir => {}
                Component::ParentDir => {
                    if parts.pop().is_none() {
                        return Err(format!("Path \"{}\" is outside of the warehouse.", raw));
                    }
                }
                Component::RootDir | Component::Prefix(_) => {
                    return Err(format!("Path \"{}\" is outside of the warehouse.", raw));
                }
            }
        }

        Ok(WarehousePath { relative: parts.join("/") })
    }

    /// Check if this path is the warehouse root itself.
    pub fn is_root(&self) -> bool {
        self.relative.is_empty()
    }

    /// The canonical key of this path. This is the value used for naming inventory folders
    /// and for entries in the inventory metadata file. The warehouse root is the empty string.
    pub fn as_key(&self) -> &str {
        &self.relative
    }

    /// The path to use for file system operations, relative to the warehouse root
    /// (which is the current directory of the process, see `enter_warehouse`).
    pub fn to_fs_path(&self) -> PathBuf {
        if self.is_root() {
            PathBuf::from(".")
        } else {
            PathBuf::from(&self.relative)
        }
    }

    /// Split the path into its parent and the final component.
    ///
    /// # Returns
    /// * `Ok((WarehousePath, String))` - The parent path and the name of the final component.
    /// * `Err(String)`                 - If this path is the warehouse root (it has no parent).
    pub fn split_parent(&self) -> Result<(WarehousePath, String), String> {
        if self.is_root() {
            return Err("The warehouse root has no parent folder.".to_string());
        }

        match self.relative.rsplit_once('/') {
            Some((parent, name)) => Ok((
                WarehousePath { relative: parent.to_string() },
                name.to_string(),
            )),
            None => Ok((
                WarehousePath { relative: String::new() },
                self.relative.clone(),
            )),
        }
    }

    /// Create the warehouse path of an entry inside this directory.
    ///
    /// # Arguments
    /// * `name` - The name of the entry.
    pub fn child(&self, name: &str) -> WarehousePath {
        if self.is_root() {
            WarehousePath { relative: name.to_string() }
        } else {
            WarehousePath { relative: format!("{}/{}", self.relative, name) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_equivalent_spellings_to_the_same_key() {
        for spelling in ["src", "./src", "src/", "./src/", "sub/../src"] {
            let path = WarehousePath::from_user_input(spelling).unwrap();
            assert_eq!(path.as_key(), "src", "spelling: {}", spelling);
        }
    }

    #[test]
    fn the_root_is_the_empty_key() {
        for spelling in [".", "./", "src/.."] {
            let path = WarehousePath::from_user_input(spelling).unwrap();
            assert!(path.is_root(), "spelling: {}", spelling);
            assert_eq!(path.as_key(), "");
            assert_eq!(path.to_fs_path(), PathBuf::from("."));
        }
    }

    #[test]
    fn rejects_paths_that_escape_the_warehouse() {
        assert!(WarehousePath::from_user_input("..").is_err());
        assert!(WarehousePath::from_user_input("../sibling").is_err());
        assert!(WarehousePath::from_user_input("src/../../sibling").is_err());
    }

    #[test]
    fn rejects_ascii_control_characters() {
        // A control character is never legitimate in a warehouse path (§7.6 u7): beyond being
        // nonsensical input, one particular control byte (ASCII Unit Separator, 0x1F) is the
        // field delimiter of the scope-refusal envelope (`scope_utils::REFUSAL_PREFIX`) — a
        // path carrying it into a refusal message would mangle that framing.
        assert!(WarehousePath::from_user_input("src/\u{1f}api").is_err());
        assert!(WarehousePath::from_user_input("src/\u{0}api").is_err());
        assert!(WarehousePath::from_user_input("src/\tapi").is_err());
        assert!(WarehousePath::from_user_input("src/\napi").is_err());
        assert!(WarehousePath::from_user_input("\u{7f}").is_err());

        // Ordinary paths (including non-ASCII, non-control Unicode) are unaffected.
        assert_eq!(WarehousePath::from_user_input("src/café").unwrap().as_key(), "src/café");
    }

    #[test]
    fn rejects_absolute_paths_outside_the_warehouse() {
        assert!(WarehousePath::from_user_input("/definitely/not/the/warehouse").is_err());
    }

    #[test]
    fn splits_into_parent_and_name() {
        let (parent, name) = WarehousePath::from_user_input("src/util/file.rs")
            .unwrap()
            .split_parent()
            .unwrap();
        assert_eq!(parent.as_key(), "src/util");
        assert_eq!(name, "file.rs");

        let (parent, name) = WarehousePath::from_user_input("file.rs")
            .unwrap()
            .split_parent()
            .unwrap();
        assert!(parent.is_root());
        assert_eq!(name, "file.rs");

        assert!(WarehousePath::from_user_input(".").unwrap().split_parent().is_err());
    }

    #[test]
    fn builds_child_paths() {
        let root = WarehousePath::from_user_input(".").unwrap();
        assert_eq!(root.child("src").as_key(), "src");
        assert_eq!(root.child("src").child("util").as_key(), "src/util");
    }

    #[test]
    fn the_empty_ancestor_covers_everything() {
        assert!(covers("", ""));
        assert!(covers("", "src"));
        assert!(covers("", "src/api"));
    }

    #[test]
    fn a_key_covers_itself_and_its_descendants_but_not_its_ancestors_or_siblings() {
        assert!(covers("src", "src"));
        assert!(covers("src", "src/api"));
        assert!(covers("src", "src/api/mod.rs"));

        assert!(!covers("src/api", "src"));
        assert!(!covers("src", "src-other"));
        assert!(!covers("src/a", "src/b"));
    }
}
