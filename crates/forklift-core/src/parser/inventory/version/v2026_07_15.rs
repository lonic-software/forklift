use crate::globals;
use crate::model::inventory::Inventory;
use crate::util::byte_utils;

/// Parse an inventory object's content with version `V2026_07_15`: byte-identical per-item
/// encoding to `V2024_09_04` — the rollup hash added by this version lives in the header, not
/// the content, so the item body is reused rather than duplicated.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the inventory
/// header.
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// * `Ok(Inventory)` - The parsed inventory (its rollup hash is not set here — the caller
///   attaches it from the parsed header).
/// * `Err(String)`   - The error message.
pub fn parse(offset: usize, content: &[u8]) -> Result<Inventory, String> {
    super::v2024_09_04::parse(offset, content)
}

/// Parse the inventory header's extra bytes for version `V2026_07_15`: a presence flag (VLQ
/// `0`/`1`) followed by the rollup hash bytes when present, terminated the same way an item's
/// hash is (an end-of-text byte, not a length prefix).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at (right after the entry count).
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// * `Ok((Some(String), usize))` - The rollup hash, and the number of bytes read.
/// * `Ok((None, usize))`         - No rollup hash was present, and the number of bytes read.
/// * `Err(String)`               - If the extra header bytes are malformed.
pub fn parse_header(offset: usize, content: &[u8]) -> Result<(Option<String>, usize), String> {
    let mut cursor = 0usize;

    let present = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(flag, bytes_read)| {
            cursor += bytes_read;
            flag
        })?;

    if present == 0 {
        return Ok((None, cursor));
    }

    let hash = byte_utils::read_until_byte_value(offset + cursor, content, globals::BYTE_END_OF_TEXT)
        .ok_or("Expected inventory rollup hash, but not found.".to_string())
        .and_then(|(hash, bytes_read)| {
            cursor += bytes_read;
            String::from_utf8(hash).map_err(|_| "Failed to parse inventory rollup hash.".to_string())
        })?;

    Ok((Some(hash), cursor))
}
