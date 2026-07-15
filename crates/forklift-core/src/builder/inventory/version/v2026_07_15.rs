use crate::model::inventory::Inventory;
use crate::util::{byte_utils, object_utils};

/// Build an inventory object's content with version `V2026_07_15`: byte-identical per-item
/// encoding to `V2024_09_04` — the rollup hash added by this version lives in the header, not
/// the content, so the item body is reused rather than duplicated.
///
/// # Arguments
/// * `inventory` - The inventory data.
///
/// # Returns
/// The bytes of the inventory object's content.
pub fn build(inventory: &Inventory) -> Vec<u8> {
    super::v2024_09_04::build(inventory)
}

/// Build the inventory header's extra bytes for version `V2026_07_15`: a presence flag (VLQ
/// `0`/`1`) followed by the rollup hash bytes when present, terminated the same way an item's
/// hash is (an end-of-text byte, not a length prefix) so the header stays self-delimiting up to
/// its own terminating null byte.
///
/// # Arguments
/// * `inventory` - The inventory data.
///
/// # Returns
/// The extra header bytes (empty when there is no rollup).
pub fn build_header(inventory: &Inventory) -> Vec<u8> {
    let mut extra: Vec<u8> = Vec::new();

    match inventory.get_rollup_hash() {
        Some(hash) => {
            extra.extend(byte_utils::number_to_vlq_bytes(1));
            extra.extend(hash.as_bytes());
            object_utils::push_end_of_text(&mut extra);
        }
        None => extra.extend(byte_utils::number_to_vlq_bytes(0)),
    }

    extra
}
