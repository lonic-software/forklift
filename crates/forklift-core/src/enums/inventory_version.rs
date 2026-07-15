use crate::{builder, parser};
use crate::model::inventory::Inventory;

const CODE_VERSION_2024_09_04: u64 = 1;
const CODE_VERSION_2026_07_15: u64 = 2;

/// Versions of the inventory object format.
pub enum InventoryVersion {
    /// The original version, defined on 09/04/2024.
    V2024_09_04,

    /// Adds an optional per-shard rollup hash to the header (DESIGN.html §5.0 D item 8): the
    /// hash `stack` would build for this directory's entire staged subtree, when known.
    V2026_07_15,
}

impl InventoryVersion {
    /// Get the code of the inventory version.
    ///
    /// # Returns
    /// * `u64` - The code of the inventory version.
    pub fn get_code(&self) -> u64 {
        match self {
            InventoryVersion::V2024_09_04 => CODE_VERSION_2024_09_04,
            InventoryVersion::V2026_07_15 => CODE_VERSION_2026_07_15,
        }
    }

    /// Get the inventory version for the given code.
    ///
    /// # Arguments
    /// * `code` - The code of the inventory version.
    ///
    /// # Returns
    /// * `Ok(InventoryVersion)` - The inventory version.
    /// * `Err(String)`          - If the code is not recognized.
    pub fn from_code(code: u64) -> Result<InventoryVersion, String> {
        match code {
            CODE_VERSION_2024_09_04 => Ok(InventoryVersion::V2024_09_04),
            CODE_VERSION_2026_07_15 => Ok(InventoryVersion::V2026_07_15),
            _ => Err(format!("Inventory version code {} not found.", code)),
        }
    }

    /// Get the function for building inventory files with the given version.
    ///
    /// # Returns
    /// * `impl Fn(&Inventory) -> Vec<u8>` - The builder function.
    pub fn get_builder(&self) -> impl Fn(&Inventory) -> Vec<u8> + '_ {
        let builder_fn = match self {
            InventoryVersion::V2024_09_04 => builder::inventory::version::v2024_09_04::build,
            InventoryVersion::V2026_07_15 => builder::inventory::version::v2026_07_15::build,
        };

        move |inventory| builder_fn(inventory)
    }

    /// Get the function for parsing inventory files with the given version.
    ///
    /// # Returns
    /// * `impl Fn(usize, &[u8]) -> Result<Inventory, String>` - The parser function.
    pub fn get_parser(&self) -> impl Fn(usize, &[u8]) -> Result<Inventory, String> + '_ {
        let parser_fn = match self {
            InventoryVersion::V2024_09_04 => parser::inventory::version::v2024_09_04::parse,
            InventoryVersion::V2026_07_15 => parser::inventory::version::v2026_07_15::parse,
        };

        move |offset, content| parser_fn(offset, content)
    }

    /// Get the function for building an inventory file's header extra bytes with the given
    /// version (written after the entry count, before the header's terminating null byte).
    ///
    /// # Returns
    /// * `impl Fn(&Inventory) -> Vec<u8>` - The header builder function.
    pub fn get_header_builder(&self) -> impl Fn(&Inventory) -> Vec<u8> + '_ {
        let builder_fn = match self {
            InventoryVersion::V2024_09_04 => builder::inventory::version::v2024_09_04::build_header,
            InventoryVersion::V2026_07_15 => builder::inventory::version::v2026_07_15::build_header,
        };

        move |inventory| builder_fn(inventory)
    }

    /// Get the function for parsing an inventory file's header extra bytes with the given
    /// version.
    ///
    /// # Returns
    /// * `impl Fn(usize, &[u8]) -> Result<(Option<String>, usize), String>` - The header parser
    ///   function: the rollup hash (if any), and the number of bytes read.
    pub fn get_header_parser(&self) -> impl Fn(usize, &[u8]) -> Result<(Option<String>, usize), String> + '_ {
        let parser_fn = match self {
            InventoryVersion::V2024_09_04 => parser::inventory::version::v2024_09_04::parse_header,
            InventoryVersion::V2026_07_15 => parser::inventory::version::v2026_07_15::parse_header,
        };

        move |offset, content| parser_fn(offset, content)
    }

    /// Get the latest inventory version.
    ///
    /// # Returns
    /// * `InventoryVersion` - The latest inventory version.
    pub fn latest() -> InventoryVersion {
        InventoryVersion::V2026_07_15
    }
}