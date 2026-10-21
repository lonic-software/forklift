use crate::model::inventory::Inventory;

/// Parse an inventory object's content with version `V2026_10_21`: byte-identical to
/// `V2026_10_19`. What changes is the meaning of a proven entry: its anchor was stamped on the
/// entry's own device, so entries are read as written.
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
    super::v2024_09_04::parse_items(offset, content, false)
}

/// Parse the inventory header's extra bytes for version `V2026_10_21`: unchanged from
/// `V2026_10_19` (the optional rollup hash).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at (right after the entry count).
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// See `V2026_07_15`'s `parse_header`.
pub fn parse_header(offset: usize, content: &[u8]) -> Result<(Option<String>, usize), String> {
    super::v2026_10_19::parse_header(offset, content)
}
