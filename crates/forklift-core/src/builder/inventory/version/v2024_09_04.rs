use crate::model::inventory::Inventory;
use crate::util::{byte_utils, object_utils};

/// Build an inventory object with version `V2024_09_04`.
///
/// # Arguments
/// * `version`   - The version of the object.
/// * `inventory` - The inventory data.
///
/// # Returns
/// The bytes of the inventory object.
pub fn build(inventory: &Inventory) -> Vec<u8> {
    let mut content: Vec<u8> = Vec::new();

    for (_, item) in inventory.get_items() {
        content.extend(byte_utils::number_to_vlq_bytes(item.metadata_change_timestamp));
        content.extend(byte_utils::number_to_vlq_bytes(item.content_change_timestamp));
        content.extend(byte_utils::number_to_vlq_bytes(item.device));
        content.extend(byte_utils::number_to_vlq_bytes(item.inode));
        content.extend(byte_utils::number_to_vlq_bytes(item.item_type.get_code()));
        content.extend(byte_utils::number_to_vlq_bytes(item.user_id));
        content.extend(byte_utils::number_to_vlq_bytes(item.group_id));
        content.extend(byte_utils::number_to_vlq_bytes(item.file_size));

        content.extend(item.hash.as_bytes());
        object_utils::push_end_of_text(&mut content);

        content.extend(byte_utils::number_to_vlq_bytes(item.file_name_length));
        content.extend(byte_utils::number_to_vlq_bytes(item.state.get_code()));

        content.extend(item.name.as_bytes());
        object_utils::push_new_line(&mut content);
    }

    content
}

/// Build the inventory header's extra bytes for version `V2024_09_04`: none — this version
/// predates the rollup hash, so its header ends right after the entry count.
///
/// # Arguments
/// * `inventory` - The inventory data (unused: this version has no header extras).
///
/// # Returns
/// The (empty) extra header bytes.
pub fn build_header(_inventory: &Inventory) -> Vec<u8> {
    Vec::new()
}