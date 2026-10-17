use crate::enums::dir_entry_type::DirEntryType;
use crate::globals;
use crate::model::tree_item::TreeItem;
use crate::util::byte_utils;

/// Parse a tree object with version `V2024_09_04`.
///
/// # Arguments
/// * `offset` - The offset to start parsing at This should be the byte *after*
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

    let name = byte_utils::read_until_byte_value(
        offset + cursor,
        content,
        globals::BYTE_END_OF_TEXT
    ).ok_or(
        "Expected name of tree item, but not found.".to_string()
    ).and_then(|(value, bytes_read)| {
        cursor += bytes_read;
        String::from_utf8(value)
            .map_err(|_| "Failed to parse the name of the tree item.".to_string())
    })?;

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