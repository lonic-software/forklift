use std::fmt::Display;

const CODE_TYPE_NORMAL: u64 = 1;
const CODE_TYPE_EXECUTABLE: u64 = 2;
const CODE_TYPE_SYMBOLIC_LINK: u64 = 3;
const CODE_TYPE_TREE: u64 = 4;
// Codes 5 and 6 are frozen. They are new legal values of the existing tree entry type field —
// no `TREE_OBJECT_FORMAT` version bump — so a pre-chunking binary's `from_code` returns `Err`
// on them and fails loudly (never silently) on any directory that holds a chunked file. This
// is the deliberate B1 fix: the tree-level chunked signal lets a chunk-aware `gc`/`shift`
// dispatch on the entry type with no extra object load, and makes an old reachability walk a
// loud no-op instead of one that silently collects the (recipe-only-reachable) chunks.
const CODE_TYPE_NORMAL_CHUNKED: u64 = 5;
const CODE_TYPE_EXECUTABLE_CHUNKED: u64 = 6;

/// Directory entry type (i.e. type of file or directory).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DirEntryType {
    /// A normal (non-executable) file.
    Normal,

    /// An executable file.
    Executable,

    /// A symbolic link.
    SymbolicLink,

    /// A subtree (directory).
    Tree,

    /// A normal (non-executable) file large enough to be stored chunked: the entry's hash
    /// names a recipe, not a blob.
    NormalChunked,

    /// An executable file stored chunked: the entry's hash names a recipe, not a blob.
    ExecutableChunked,
}

impl Display for DirEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let type_str = match self {
            DirEntryType::Normal            => "Normal",
            DirEntryType::Executable        => "Executable",
            DirEntryType::SymbolicLink      => "Symbolic Link",
            DirEntryType::Tree              => "Tree",
            DirEntryType::NormalChunked     => "Normal (chunked)",
            DirEntryType::ExecutableChunked => "Executable (chunked)",
        };

        write!(f, "{}", type_str)
    }
}

impl DirEntryType {
    /// Check whether the entry is a file.
    ///
    /// # Returns
    /// * `true`  - If the entry is a file.
    /// * `false` - If the entry is a directory.
    pub fn is_file(&self) -> bool {
        match self {
            DirEntryType::Normal
            | DirEntryType::Executable
            | DirEntryType::SymbolicLink
            | DirEntryType::NormalChunked
            | DirEntryType::ExecutableChunked => true,
            DirEntryType::Tree => false,
        }
    }

    /// Whether the entry is a chunked file (its hash names a recipe, not a blob). A symlink
    /// is never chunked (its target is a tiny path string), and a directory is not a file.
    ///
    /// # Returns
    /// * `true`  - If the entry is `NormalChunked` or `ExecutableChunked`.
    /// * `false` - Otherwise.
    pub fn is_chunked(&self) -> bool {
        matches!(self, DirEntryType::NormalChunked | DirEntryType::ExecutableChunked)
    }

    /// The on-disk kind of the entry with the chunked storage decision projected away: a
    /// chunked file is, on disk, an ordinary normal/executable file — chunking is a storage
    /// choice, not a filesystem property a `stat` can see. Two entries with the same on-disk
    /// kind are the same file to the working directory (this is how the stat cache stays a
    /// fast path for an unchanged giant: the fresh `stat` reports `Normal`, the inventory holds
    /// `NormalChunked`, and both normalize to `Normal`).
    ///
    /// # Returns
    /// The entry type as the filesystem sees it (`NormalChunked` → `Normal`,
    /// `ExecutableChunked` → `Executable`, everything else unchanged).
    pub fn on_disk_kind(&self) -> DirEntryType {
        match self {
            DirEntryType::NormalChunked     => DirEntryType::Normal,
            DirEntryType::ExecutableChunked => DirEntryType::Executable,
            other => *other,
        }
    }

    /// The chunked counterpart of a plain file type: `Normal` → `NormalChunked`,
    /// `Executable` → `ExecutableChunked`. Any other type (a symlink, a directory, an already
    /// chunked type) is returned unchanged — a symlink is never chunked.
    ///
    /// # Returns
    /// The chunked entry type for a plain file, or `self` for anything not chunkable.
    pub fn to_chunked(&self) -> DirEntryType {
        match self {
            DirEntryType::Normal     => DirEntryType::NormalChunked,
            DirEntryType::Executable => DirEntryType::ExecutableChunked,
            other => *other,
        }
    }

    /// Get the code of the entry type.
    ///
    /// # Returns
    /// The code of the entry type.
    pub fn get_code(&self) -> u64 {
        match self {
            DirEntryType::Normal            => CODE_TYPE_NORMAL,
            DirEntryType::Executable        => CODE_TYPE_EXECUTABLE,
            DirEntryType::SymbolicLink      => CODE_TYPE_SYMBOLIC_LINK,
            DirEntryType::Tree              => CODE_TYPE_TREE,
            DirEntryType::NormalChunked     => CODE_TYPE_NORMAL_CHUNKED,
            DirEntryType::ExecutableChunked => CODE_TYPE_EXECUTABLE_CHUNKED,
        }
    }

    /// Get the entry type from the code.
    ///
    /// # Arguments
    /// * `code` - The code of the entry type.
    ///
    /// # Returns
    /// * `Ok(DirEntryType)` - The entry type.
    /// * `Err(String)`      - If the code does not match any entry type.
    pub fn from_code(code: u64) -> Result<Self, String> {
        match code {
            CODE_TYPE_NORMAL            => Ok(DirEntryType::Normal),
            CODE_TYPE_EXECUTABLE        => Ok(DirEntryType::Executable),
            CODE_TYPE_SYMBOLIC_LINK     => Ok(DirEntryType::SymbolicLink),
            CODE_TYPE_TREE              => Ok(DirEntryType::Tree),
            CODE_TYPE_NORMAL_CHUNKED    => Ok(DirEntryType::NormalChunked),
            CODE_TYPE_EXECUTABLE_CHUNKED => Ok(DirEntryType::ExecutableChunked),
            _ => Err(format!("Directory entry type with code \"{}\" not found.", code)),
        }
    }

    /// Get the name of the entry type for peeking.
    /// The name may have some padding at the end to make sure that all names have the same length.
    ///
    /// # Returns
    /// * `String` - The name of the entry type.
    pub fn get_name_for_peek(&self) -> String {
        match self {
            DirEntryType::Normal            => "normal    ",
            DirEntryType::Executable        => "executable",
            DirEntryType::SymbolicLink      => "symlink   ",
            DirEntryType::Tree              => "tree      ",
            DirEntryType::NormalChunked     => "chunked   ",
            DirEntryType::ExecutableChunked => "chunked-x ",
        }.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_codes_5_and_6_are_the_reserved_chunked_types() {
        // Codes 5 and 6 are the frozen chunked entry types. Before chunk support they were
        // unknown codes an old parser rejected (fail-loud) — the property that makes the B1 fix
        // route an old binary through a loud no-op rather than silent data loss.
        assert_eq!(DirEntryType::from_code(5).unwrap(), DirEntryType::NormalChunked);
        assert_eq!(DirEntryType::from_code(6).unwrap(), DirEntryType::ExecutableChunked);
        assert_eq!(DirEntryType::NormalChunked.get_code(), 5);
        assert_eq!(DirEntryType::ExecutableChunked.get_code(), 6);
    }

    #[test]
    fn an_unknown_entry_type_code_is_rejected_not_guessed() {
        // The dormant defensive parsing that protects an old binary: any code past the known set
        // is an error, never silently mapped. (Code 7 stands in for "a future type this build
        // does not know" — exactly how codes 5/6 read to a pre-chunk binary.)
        assert!(DirEntryType::from_code(7).is_err());
        assert!(DirEntryType::from_code(0).is_err());
        assert!(DirEntryType::from_code(9999).is_err());
    }

    #[test]
    fn on_disk_kind_projects_chunking_away_and_to_chunked_reverses_it() {
        assert_eq!(DirEntryType::NormalChunked.on_disk_kind(), DirEntryType::Normal);
        assert_eq!(DirEntryType::ExecutableChunked.on_disk_kind(), DirEntryType::Executable);
        assert_eq!(DirEntryType::Normal.on_disk_kind(), DirEntryType::Normal);

        assert_eq!(DirEntryType::Normal.to_chunked(), DirEntryType::NormalChunked);
        assert_eq!(DirEntryType::Executable.to_chunked(), DirEntryType::ExecutableChunked);
        // A symlink is never chunked, so `to_chunked` leaves it untouched.
        assert_eq!(DirEntryType::SymbolicLink.to_chunked(), DirEntryType::SymbolicLink);
    }

    #[test]
    fn chunked_entries_are_files() {
        assert!(DirEntryType::NormalChunked.is_file());
        assert!(DirEntryType::ExecutableChunked.is_file());
        assert!(DirEntryType::NormalChunked.is_chunked());
        assert!(DirEntryType::ExecutableChunked.is_chunked());
        assert!(!DirEntryType::Normal.is_chunked());
        assert!(!DirEntryType::Tree.is_chunked());
    }
}