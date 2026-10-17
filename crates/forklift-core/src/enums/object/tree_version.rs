use crate::model::tree_item::TreeItem;

const CODE_VERSION_2024_09_04: u64 = 1;
const CODE_VERSION_2026_07_02: u64 = 2;

/// Versions of the tree object format.
pub enum TreeVersion {
    /// The original version, defined on 09/04/2024.
    V2024_09_04,

    /// Entry names are length-prefixed instead of EOT-terminated, so that names containing
    /// EOT or new line bytes cannot corrupt the object. Defined on 07/02/2026.
    V2026_07_02,
}

impl TreeVersion {
    /// Get the code of the version.
    ///
    /// # Returns
    /// * `u64` - The code of the version.
    pub fn code(&self) -> u64 {
        match self {
            TreeVersion::V2024_09_04 => CODE_VERSION_2024_09_04,
            TreeVersion::V2026_07_02 => CODE_VERSION_2026_07_02,
        }
    }

    /// Get the version for the given code.
    ///
    /// # Arguments
    /// * `code` - The code of the version.
    ///
    /// # Returns
    /// * `Ok(TreeVersion)` - The version associated with the given code.
    /// * `Err(String)`     - The error message if the version is unknown.
    pub fn from_code(code: u64) -> Result<Self, String> {
        match code {
            CODE_VERSION_2024_09_04 => Ok(TreeVersion::V2024_09_04),
            CODE_VERSION_2026_07_02 => Ok(TreeVersion::V2026_07_02),
            _ => Err(format!("Unknown tree version: {}", code)),
        }
    }

    /// Get the object builder function for the version.
    ///
    /// # Returns
    /// * `impl Fn(&TreeItem) -> Vec<u8>` - The object builder function.
    pub fn get_builder(&self) -> impl Fn(&TreeItem) -> Vec<u8> + '_ {
        let builder_fn = match self {
            TreeVersion::V2024_09_04 => crate::builder::object::tree::version::v2024_09_04::build,
            TreeVersion::V2026_07_02 => crate::builder::object::tree::version::v2026_07_02::build,
        };

        move |t: &TreeItem| builder_fn(self.code(), t)
    }

    /// Get the entry parser function for the version. It returns the raw entries; callers go
    /// through `tree_parser::parse_tree`, which validates them.
    ///
    /// # Returns
    /// * `impl Fn(usize, &[u8]) -> Result<Vec<TreeItem>, String>` - The entry parser function.
    pub fn get_parser(&self) -> impl Fn(usize, &[u8]) -> Result<Vec<TreeItem>, String> + '_ {
        let parser_fn = match self {
            TreeVersion::V2024_09_04 =>
                crate::parser::object::tree::version::v2024_09_04::parse,
            TreeVersion::V2026_07_02 =>
                crate::parser::object::tree::version::v2026_07_02::parse,
        };

        move |offset, content| parser_fn(offset, content)
    }

    /// Get the latest version of the tree object format.
    ///
    /// # Returns
    /// The latest version of the tree object format.
    pub fn latest() -> Self {
        TreeVersion::V2026_07_02
    }
}