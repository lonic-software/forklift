use crate::enums::inventory_version::InventoryVersion;
use crate::model::inventory::Inventory;
use crate::util::{byte_utils, object_utils};

pub mod version;

/// A builder for inventory objects.
pub struct InventoryBuilder {
    pub version: InventoryVersion,
    pub content: Vec<u8>,
}

impl InventoryBuilder {
    /// Build an inventory object.
    ///
    /// # Arguments
    /// * `inventory` - The inventory to build.
    ///
    /// # Returns
    /// * `Vec<u8>` - The bytes of the inventory object (including the header).
    pub fn build(inventory: &Inventory) -> Vec<u8> {
        let builder = InventoryBuilder::new(InventoryVersion::latest());

        builder.write_header(inventory).write_content(inventory).content
    }

    /// Create a new inventory builder.
    ///
    /// # Arguments
    /// * `version` - The inventory file version to use.
    ///
    /// # Returns
    /// * `InventoryBuilder` - The inventory builder.
    fn new(version: InventoryVersion) -> InventoryBuilder {
        InventoryBuilder {
            content: Vec::new(),
            version,
        }
    }

    /// Write the header to the bytes of the inventory object: the version code, the entry
    /// count, then the version's header extras (e.g. the rollup hash, since `V2026_07_15`),
    /// terminated by a null byte.
    ///
    /// # Arguments
    /// * `inventory` - The inventory to write the header for.
    ///
    /// # Returns
    /// * `InventoryBuilder` - The inventory builder.
    fn write_header(mut self, inventory: &Inventory) -> Self {
        self.content.extend(byte_utils::number_to_vlq_bytes(self.version.get_code()));
        self.content.extend(byte_utils::number_to_vlq_bytes(inventory.get_items_count() as u64));
        self.content.extend(self.version.get_header_builder()(inventory));
        object_utils::push_null(&mut self.content);

        self
    }

    /// Write the content to the bytes of the inventory object.
    ///
    /// # Arguments
    /// * `inventory` - The inventory to write to the object.
    ///
    /// # Returns
    /// * `InventoryBuilder` - The inventory builder.
    fn write_content(mut self, inventory: &Inventory) -> Self {
        self.content.extend(self.version.get_builder()(inventory));

        self
    }
}