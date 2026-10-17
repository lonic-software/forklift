use crate::enums::dir_entry_type::DirEntryType;
use crate::globals;
use crate::model::tree_item::TreeItem;
use crate::util::byte_utils;

/// Parse a tree object with version `V2026_07_02`.
/// Entry names are length-prefixed (see the builder for the reasoning).
///
/// # Arguments
/// * `offset` - The offset to start parsing at. This should be the byte *after*
/// the tree format version code.
/// * `input`  - The bytes of the tree object.
///
/// # Returns
/// * `Ok(Vec<TreeItem>)` - The entries, in the order they are stored. They are not validated
///                         here: [`tree_parser::parse_tree`] does that for every version.
/// * `Err(String)`       - The error message.
///
/// [`tree_parser::parse_tree`]: crate::parser::object::tree::tree_parser::parse_tree
pub fn parse(offset: usize, input: &[u8]) -> Result<Vec<TreeItem>, String> {
    let mut cursor = offset;
    let mut entries = Vec::new();

    while cursor < input.len() {
        let (item, bytes_read) = parse_entry(cursor, input)?;

        cursor += bytes_read;
        entries.push(item);
    }

    Ok(entries)
}

/// Parse a single entry in the tree object.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the last entry
/// (or after the version code if it's the first entry).
/// * `content` - The bytes of the tree object.
///
/// # Returns
/// * `Ok((TreeItem, usize))`:
///    * `TreeItem` - The parsed tree item.
///    * `usize`    - The number of bytes read.
/// * `Err(String)` - The error message.
fn parse_entry(offset: usize, content: &[u8]) -> Result<(TreeItem, usize), String> {
    let mut cursor = 0;

    let type_code = byte_utils::number_from_vlq_bytes(offset, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })?;

    let item_type = DirEntryType::from_code(type_code)?;

    let name_length = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })? as usize;

    let name_start = offset + cursor;
    // `name_length` is an attacker-controlled length prefix, so fold the overflow guard into the
    // bounds check: a huge value is reported as truncated rather than panicking on the addition
    // (an `attempt to add with overflow` in debug, a wrapped out-of-range slice in release).
    let name_end = name_start.checked_add(name_length)
        .filter(|end| *end <= content.len())
        .ok_or_else(|| "Tree entry name is truncated.".to_string())?;

    let name = String::from_utf8(content[name_start..name_end].to_vec())
        .map_err(|_| "Failed to parse the name of the tree item.".to_string())?;
    cursor += name_length;

    let hash = byte_utils::read_until_byte_value(
        offset + cursor,
        content,
        globals::BYTE_NEW_LINE
    ).ok_or(
        "Expected hash of tree item, but not found.".to_string()
    ).and_then(|(value, bytes_read)| {
        cursor += bytes_read;
        String::from_utf8(value)
            .map_err(|_| "Failed to parse the hash of the tree item.".to_string())
    })?;

    let result = TreeItem::new(name, hash, item_type);

    Ok((result, cursor))
}
