use crate::enums::inventory_version::InventoryVersion;
use crate::globals;
use crate::model::inventory::Inventory;
use crate::util::byte_utils;

struct ParsedInventoryHeader {
    version: InventoryVersion,
    // TODO: uncomment this once it's used
    //entry_count: u64,
    rollup_hash: Option<String>,
}

/// Parse an inventory file.
///
/// # Arguments
/// * `content` - The content of the inventory file.
///
/// # Returns
/// * `Ok(Inventory)` - If the inventory was parsed successfully.
/// * `Err(String)`   - If an error occurred while parsing the inventory.
pub fn parse_inventory(content: &[u8]) -> Result<Inventory, String> {
    let mut cursor = 0usize;

    let header = parse_inventory_header(content)
        .map(|(header, bytes_read)| {
            cursor += bytes_read;
            header
        })?;

    let mut inventory = header.version.get_parser()(cursor, content)?;
    inventory.set_rollup_hash(header.rollup_hash);

    Ok(inventory)
}

/// Parse only an inventory shard's header and return its rollup hash — never its entries
/// (which can dwarf the header on a large directory). The cheap probe the rollup-based skip
/// (DESIGN.html §5.0 D item 8, stage 2) uses before committing to a skip: a directory with many
/// entries costs the same one small header parse as one with a single entry.
///
/// # Arguments
/// * `content` - The content of the inventory file.
///
/// # Returns
/// * `Ok(Some(String))` - The shard carries a rollup hash.
/// * `Ok(None)`         - The shard carries no rollup (including every old-version shard, which
///                        never has one).
/// * `Err(String)`      - If the header itself could not be parsed.
pub fn peek_rollup_hash(content: &[u8]) -> Result<Option<String>, String> {
    parse_inventory_header(content).map(|(header, _)| header.rollup_hash)
}

/// Parse the header of an inventory file.
///
/// # Arguments
/// * `content` - The content of the inventory file.
///
/// # Returns
/// * `Ok((ParsedInventoryHeader, usize))`:
///    * `ParsedInventoryHeader` - The parsed header.
///    * `usize`                 - The number of bytes read.
/// * `Err(String)` - If an error occurred while parsing the header.
fn parse_inventory_header(content: &[u8]) -> Result<(ParsedInventoryHeader, usize), String> {
    let mut cursor = 0usize;

    let version = byte_utils::number_from_vlq_bytes(cursor, content)
        .and_then(|(version_code, bytes_read)| {
            cursor += bytes_read;
            InventoryVersion::from_code(version_code)
        })?;

    // Read entry count. Currently unused.
    // TODO: handle entry count (e.g. handle a very large amount of entries differently)
    byte_utils::number_from_vlq_bytes(cursor, content)
        .map(|(count, bytes_read)| {
            cursor += bytes_read;
            count
        })?;

    let rollup_hash = version.get_header_parser()(cursor, content)
        .map(|(rollup_hash, bytes_read)| {
            cursor += bytes_read;
            rollup_hash
        })?;

    // Discard everything until the next null byte (indicating the end of the header). The
    // version's header parser above is expected to stop exactly at the null already; this
    // keeps a header that grows again in some future version forward-tolerant, as it always has.
    byte_utils::read_until_byte_value(cursor, content, globals::BYTE_NULL)
        .inspect(|(_, bytes_read)| {
            cursor += bytes_read;
        });

    Ok((ParsedInventoryHeader { version, rollup_hash }, cursor))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::inventory::InventoryBuilder;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::enums::inventory_item_state::InventoryItemState;
    use crate::model::inventory::InventoryItem;

    fn item(name: &str) -> InventoryItem {
        InventoryItem {
            metadata_change_timestamp: 1_700_000_000,
            content_change_timestamp: 1_700_000_001,
            device: 3,
            inode: 12345,
            item_type: DirEntryType::Normal,
            user_id: 501,
            group_id: 20,
            file_size: 42,
            hash: "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc".to_string(),
            file_name_length: name.len() as u64,
            state: InventoryItemState::Normal,
            name: name.to_string(),
        }
    }

    #[test]
    fn round_trip_preserves_all_fields() {
        let mut inventory = crate::model::inventory::Inventory::new();
        inventory.add_item(item("file.txt"));

        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        let parsed_item = parsed.get_item_by_name("file.txt").unwrap();
        assert_eq!(parsed_item.metadata_change_timestamp, 1_700_000_000);
        assert_eq!(parsed_item.content_change_timestamp, 1_700_000_001);
        assert_eq!(parsed_item.device, 3);
        assert_eq!(parsed_item.inode, 12345);
        assert_eq!(parsed_item.user_id, 501);
        assert_eq!(parsed_item.group_id, 20);
        assert_eq!(parsed_item.file_size, 42);
        assert_eq!(parsed_item.hash, "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc");
    }

    #[test]
    fn round_trip_preserves_the_deleted_state() {
        let mut staged_item = item("removed.txt");
        staged_item.state = InventoryItemState::Deleted;

        let mut inventory = crate::model::inventory::Inventory::new();
        inventory.add_item(staged_item);

        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        let parsed_item = parsed.get_item_by_name("removed.txt").unwrap();
        assert!(parsed_item.state == InventoryItemState::Deleted);
    }

    #[test]
    fn round_trip_preserves_hostile_file_names() {
        // File names may legally contain new line and end-of-text bytes on Unix;
        // the serialized name length recovers them exactly.
        let hostile_names = ["with\nnewline", "with\u{3}end-of-text", "emoji 📦 name"];

        let mut inventory = crate::model::inventory::Inventory::new();
        for (i, name) in hostile_names.iter().enumerate() {
            let mut hostile_item = item(name);
            hostile_item.inode += i as u64;
            inventory.add_item(hostile_item);
        }

        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        assert_eq!(parsed.get_items_count(), hostile_names.len());
        for name in hostile_names {
            assert!(parsed.get_item_by_name(name).is_some(), "name not found: {:?}", name);
        }
    }

    #[test]
    fn round_trip_preserves_a_present_rollup_hash() {
        let mut inventory = crate::model::inventory::Inventory::new();
        inventory.add_item(item("file.txt"));
        inventory.set_rollup_hash(Some("a".repeat(64)));

        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        assert_eq!(parsed.get_rollup_hash(), Some(&"a".repeat(64)));
    }

    #[test]
    fn round_trip_preserves_an_absent_rollup_hash() {
        let mut inventory = crate::model::inventory::Inventory::new();
        inventory.add_item(item("file.txt"));
        inventory.set_rollup_hash(None);

        let bytes = InventoryBuilder::build(&inventory);
        let parsed = parse_inventory(&bytes).unwrap();

        assert_eq!(parsed.get_rollup_hash(), None);
    }

    #[test]
    fn an_old_version_shard_parses_with_no_rollup_hash() {
        // A hand-built version-1 (`V2024_09_04`) shard, predating the rollup hash: version
        // code, entry count, and the header's terminating null, immediately followed by the
        // (unchanged) per-item content encoding — exactly what a pre-upgrade forklift wrote.
        let mut inventory = crate::model::inventory::Inventory::new();
        inventory.add_item(item("file.txt"));

        let mut bytes: Vec<u8> = vec![1, 1, 0]; // version=1, entry_count=1, header NULL
        bytes.extend(crate::builder::inventory::version::v2024_09_04::build(&inventory));

        let parsed = parse_inventory(&bytes).unwrap();

        assert_eq!(parsed.get_rollup_hash(), None);
        assert!(parsed.get_item_by_name("file.txt").is_some());
    }
}
