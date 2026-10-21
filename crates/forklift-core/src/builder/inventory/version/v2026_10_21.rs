use crate::model::inventory::Inventory;

/// Build an inventory object's content with version `V2026_10_21`: byte-identical to
/// `V2026_10_19`. The version marks shards whose entries were proven on their own device (see
/// the parser).
///
/// # Arguments
/// * `inventory` - The inventory data.
///
/// # Returns
/// The bytes of the inventory object's content.
pub fn build(inventory: &Inventory) -> Vec<u8> {
    super::v2026_10_19::build(inventory)
}

/// Build the inventory header's extra bytes for version `V2026_10_21`: unchanged from
/// `V2026_10_19` (the optional rollup hash).
///
/// # Arguments
/// * `inventory` - The inventory data.
///
/// # Returns
/// The extra header bytes.
pub fn build_header(inventory: &Inventory) -> Vec<u8> {
    super::v2026_10_19::build_header(inventory)
}
