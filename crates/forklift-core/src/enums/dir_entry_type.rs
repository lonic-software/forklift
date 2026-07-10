use std::fmt::Display;

const CODE_TYPE_NORMAL: u64 = 1;
const CODE_TYPE_EXECUTABLE: u64 = 2;
const CODE_TYPE_SYMBOLIC_LINK: u64 = 3;
const CODE_TYPE_TREE: u64 = 4;

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
}

impl Display for DirEntryType {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let type_str = match self {
            DirEntryType::Normal       => "Normal",
            DirEntryType::Executable   => "Executable",
            DirEntryType::SymbolicLink => "Symbolic Link",
            DirEntryType::Tree         => "Tree",
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
            DirEntryType::Normal | DirEntryType::Executable | DirEntryType::SymbolicLink => true,
            DirEntryType::Tree => false,
        }
    }

    /// Get the code of the entry type.
    ///
    /// # Returns
    /// The code of the entry type.
    pub fn get_code(&self) -> u64 {
        match self {
            DirEntryType::Normal       => CODE_TYPE_NORMAL,
            DirEntryType::Executable   => CODE_TYPE_EXECUTABLE,
            DirEntryType::SymbolicLink => CODE_TYPE_SYMBOLIC_LINK,
            DirEntryType::Tree         => CODE_TYPE_TREE,
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
            CODE_TYPE_NORMAL        => Ok(DirEntryType::Normal),
            CODE_TYPE_EXECUTABLE    => Ok(DirEntryType::Executable),
            CODE_TYPE_SYMBOLIC_LINK => Ok(DirEntryType::SymbolicLink),
            CODE_TYPE_TREE          => Ok(DirEntryType::Tree),
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
            DirEntryType::Normal       => "normal    ",
            DirEntryType::Executable   => "executable",
            DirEntryType::SymbolicLink => "symlink   ",
            DirEntryType::Tree         => "tree      ",
        }.to_string()
    }
}