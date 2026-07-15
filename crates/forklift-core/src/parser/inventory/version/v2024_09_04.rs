use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::globals;
use crate::model::inventory::{Inventory, InventoryItem};
use crate::util::byte_utils;

/// Parse an inventory object from the given inventory file bytes (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the inventory header.
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// * `Ok(Inventory)` - The parsed inventory.
/// * `Err(String)`   - The error message.
pub fn parse(offset: usize, content: &[u8]) -> Result<Inventory, String> {
    let mut cursor = offset;
    let mut inventory = Inventory::new();

    while cursor < content.len() {
        let (item, bytes_read) = parse_item(cursor, content)?;
        cursor += bytes_read;

        inventory.add_item(item);
    }

    Ok(inventory)
}

/// Parse an inventory item.
///
/// # Arguments
/// * `offset`  - The offset in the content where the item starts.
/// * `content` - The content of the inventory file.
///
/// # Returns
/// * `Ok((InventoryItem, usize))`:
///    * `InventoryItem` - The parsed inventory item.
///    * `usize`         - The number of bytes read.
/// * `Err(String)` - If an error occurred while parsing the item.
fn parse_item(offset: usize, content: &[u8]) -> Result<(InventoryItem, usize), String> {
    let mut cursor = 0usize;

    let metadata_change_timestamp = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(timestamp, bytes_read)| {
            cursor += bytes_read;
            timestamp
        })?;

    let content_change_timestamp = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(timestamp, bytes_read)| {
            cursor += bytes_read;
            timestamp
        })?;

    let device = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(device, bytes_read)| {
            cursor += bytes_read;
            device
        })?;

    let inode = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(inode, bytes_read)| {
            cursor += bytes_read;
            inode
        })?;

    let item_type = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .and_then(|(item_type_code, bytes_read)| {
            cursor += bytes_read;
            DirEntryType::from_code(item_type_code)
        })?;

    let user_id = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(user_id, bytes_read)| {
            cursor += bytes_read;
            user_id
        })?;

    let group_id = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(group_id, bytes_read)| {
            cursor += bytes_read;
            group_id
        })?;

    let file_size = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(file_size, bytes_read)| {
            cursor += bytes_read;
            file_size
        })?;

    let hash = byte_utils::read_until_byte_value(offset + cursor, content, globals::BYTE_END_OF_TEXT)
        .ok_or("Expected inventory item hash, but not found.".to_string())
        .and_then(|(hash, bytes_read)| {
            cursor += bytes_read;
            String::from_utf8(hash).map_err(|_| "Failed to parse hash.".to_string())
        })?;

    let file_name_length = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(file_name_length, bytes_read)| {
            cursor += bytes_read;
            file_name_length
        })?;

    let state = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .and_then(|(state_code, bytes_read)| {
            cursor += bytes_read;
            InventoryItemState::from_code(state_code)
        })?;

    // The name is read using the serialized name length instead of reading until the next
    // new line byte: file names may legally contain new line bytes, and the length field
    // recovers them exactly.
    let name_start = offset + cursor;
    // `file_name_length` is an untrusted length prefix, so fold the overflow guard into the bounds
    // check: a huge value is reported as truncated rather than panicking on the addition (an
    // `attempt to add with overflow` in debug, a wrapped out-of-range slice in release).
    let name_end = name_start.checked_add(file_name_length as usize)
        .filter(|end| *end <= content.len())
        .ok_or_else(|| "Inventory item name is truncated.".to_string())?;

    let name = String::from_utf8(content[name_start..name_end].to_vec())
        .map_err(|_| "Failed to parse name.".to_string())?;
    cursor += file_name_length as usize;

    if content.get(offset + cursor) != Some(&globals::BYTE_NEW_LINE) {
        return Err("Expected a new line byte after the inventory item name.".to_string());
    }
    cursor += 1;

    Ok((
        InventoryItem {
            metadata_change_timestamp,
            content_change_timestamp,
            device,
            inode,
            item_type,
            user_id,
            group_id,
            file_size,
            hash,
            file_name_length,
            state,
            name,
        },
        cursor,
    ))
}

/// Parse the inventory header's extra bytes for version `V2024_09_04`: none — this version
/// predates the rollup hash.
///
/// # Arguments
/// * `_offset`  - Unused: this version has no header extras to parse.
/// * `_content` - Unused: this version has no header extras to parse.
///
/// # Returns
/// * `Ok((None, 0))` - Always: no rollup hash, no bytes consumed.
pub fn parse_header(_offset: usize, _content: &[u8]) -> Result<(Option<String>, usize), String> {
    Ok((None, 0))
}