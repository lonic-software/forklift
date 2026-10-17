use std::collections::btree_map::Iter;
use crate::model::tree_item::TreeItem;
use crate::util::{byte_utils, object_utils};

/// Build a tree object with version `V2026_07_02`.
///
/// This version length-prefixes entry names instead of terminating them with an EOT byte:
/// file names may legally contain almost any byte (EOT and new line included; the parser
/// refuses only `/` and NUL, see `tree_parser::check_entry_name`), so a terminator byte cannot
/// delimit them safely.
///
/// # Arguments
/// * `version` - The version of the object.
/// * `tree`    - The tree item data.
///
/// # Returns
/// The bytes of the tree object.
pub fn build(version: u64, tree: &TreeItem) -> Vec<u8> {
    let mut content: Vec<u8> = Vec::new();

    content.extend(byte_utils::number_to_vlq_bytes(version));

    add_items(&mut content, tree.get_subtrees());
    add_items(&mut content, tree.get_files());

    content
}

/// Add the given tree items to the content.
///
/// # Arguments
/// * `content` - The content to add the items to.
/// * `items`   - The items to add.
fn add_items(content: &mut Vec<u8>, items: Iter<'_, String, TreeItem>) {
    for (name, item) in items {
        content.extend(byte_utils::number_to_vlq_bytes(item.item_type.get_code()));
        content.extend(byte_utils::number_to_vlq_bytes(name.len() as u64));
        content.extend(name.as_bytes());
        content.extend(item.hash.as_bytes());
        object_utils::push_new_line(content);
    }
}
