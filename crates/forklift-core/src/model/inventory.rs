use std::collections::BTreeMap;
use std::collections::btree_map::Iter;
use std::fmt::Display;
use std::sync::Arc;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::enums::dir_entry_type::DirEntryType;

/// The inventory contains the full list of objects that have been taken into inventory
/// (using the `load` command). Changes that have not been loaded yet are not reflected in
/// the inventory.
pub struct Inventory {
    items_by_name: BTreeMap<String, Arc<InventoryItem>>,

    /// The rollup hash of this shard's entire subtree (this directory's files plus every
    /// descendant shard, with empty-subtree pruning), from current staged state — directly
    /// comparable to a head subtree hash. `None` when the shard's true subtree hash is not
    /// currently known (never derived independently; only ever stamped from a tree hash the
    /// writer already holds, or cleared). Only a [`RollupStamp`] sets it; the shard-write funnels
    /// (`inventory_utils::shard_write`) clear it where a change invalidates it (DESIGN.html §5.0 D
    /// item 8).
    rollup_hash: Option<String>,
}

/// A tree hash a shard may carry as its rollup. A shard rebuilt from a tree gets one only from
/// `tree_utils::rollup_stamp`, which decides whether the rebuild is a complete materialization of
/// that tree, so no tree-to-shard builder can stamp without that check. Inside this crate it is also
/// minted from a hash already trusted: one read off a shard on disk, or one `stack` just built.
///
/// The type confines who mints a stamp, not whether the hash is right: neither mint can check that
/// the hash names the content the shard holds, so each trusts its caller for that.
pub struct RollupStamp(String);

impl RollupStamp {
    /// A stamp for a hash already trusted as this shard's rollup — one parsed from the shard's own
    /// bytes, or the subtree hash the tree build of the stack being cleaned up produced.
    pub(crate) fn trusted(hash: String) -> RollupStamp {
        RollupStamp(hash)
    }
}

impl Inventory {
    /// Create a new inventory.
    ///
    /// # Returns
    /// * `Inventory` - The new inventory.
    pub fn new() -> Inventory {
        Inventory {
            items_by_name: BTreeMap::new(),
            rollup_hash: None,
        }
    }

    /// Get the rollup hash of this shard's subtree, if currently known.
    ///
    /// # Returns
    /// * `Some(&String)` - The rollup hash.
    /// * `None`          - If the rollup is not currently known.
    pub fn get_rollup_hash(&self) -> Option<&String> {
        self.rollup_hash.as_ref()
    }

    /// Stamp this shard's rollup hash. The model never derives or invalidates it on its own; see
    /// [`RollupStamp`] for where a stamp comes from.
    ///
    /// # Arguments
    /// * `stamp` - The rollup hash to carry.
    pub fn stamp_rollup(&mut self, stamp: RollupStamp) {
        self.rollup_hash = Some(stamp.0);
    }

    /// Clear this shard's rollup hash: its subtree hash is no longer known.
    pub fn clear_rollup(&mut self) {
        self.rollup_hash = None;
    }

    /// Add an item to the inventory. If an item with the same name already exists
    /// (e.g. when re-loading a modified file), the existing entry is replaced.
    ///
    /// # Arguments
    /// * `item` - The item to add.
    pub fn add_item(&mut self, item: InventoryItem) {
        self.items_by_name.insert(item.name.clone(), Arc::new(item));
    }

    /// Get an item from the inventory by its name.
    ///
    /// # Arguments
    /// * `name` - The name of the item.
    ///
    /// # Returns
    /// * `Some<Arc<InventoryItem>>` - The item, if it exists.
    /// * `None`                     - If the item does not exist.
    pub fn get_item_by_name(&self, name: &str) -> Option<Arc<InventoryItem>> {
        self.items_by_name.get(name).cloned()
    }

    /// Get all items in the inventory, sorted by name (ascending).
    ///
    /// # Returns
    /// * `Iter<String, Arc<InventoryItem>>` - The items in the inventory.
    pub fn get_items(&self) -> Iter<'_, String, Arc<InventoryItem>> {
        self.items_by_name.iter()
    }

    /// Get the number of items in the inventory.
    ///
    /// # Returns
    /// * `usize` - The number of items in the inventory.
    pub fn get_items_count(&self) -> usize {
        self.items_by_name.len()
    }

    /// Remove an item from the inventory by name.
    ///
    /// # Arguments
    /// * `name` - The name of the item to remove.
    ///
    /// # Returns
    /// * `true`  - If the item was removed.
    /// * `false` - If no item with the given name existed.
    pub fn remove_item_by_name(&mut self, name: &str) -> bool {
        self.items_by_name.remove(name).is_some()
    }

    /// Mark an item as staged for removal (see `InventoryItemState::Deleted`).
    /// Marking an item that is already staged for removal is a no-op that still reports success.
    ///
    /// # Arguments
    /// * `name` - The name of the item to mark.
    ///
    /// # Returns
    /// * `true`  - If the item was marked (or already was staged for removal).
    /// * `false` - If no item with the given name existed.
    pub fn mark_item_deleted(&mut self, name: &str) -> bool {
        let Some(existing) = self.items_by_name.get(name) else {
            return false;
        };

        if existing.state != InventoryItemState::Deleted {
            let mut item = (**existing).clone();
            item.state = InventoryItemState::Deleted;
            self.add_item(item);
        }

        true
    }

    /// Mark every item in the inventory as staged for removal.
    ///
    /// # Returns
    /// * `true`  - If at least one item changed state.
    /// * `false` - If the inventory was empty or all items were already staged for removal.
    pub fn mark_all_items_deleted(&mut self) -> bool {
        let names_to_mark: Vec<String> = self.items_by_name.iter()
            .filter(|(_, item)| item.state != InventoryItemState::Deleted)
            .map(|(name, _)| name.clone())
            .collect();

        for name in &names_to_mark {
            self.mark_item_deleted(name);
        }

        !names_to_mark.is_empty()
    }
}

/// The `content_change_timestamp` of an entry whose stat does not prove its content: one taken
/// too close to its anchor to rule out a same-second edit, an entry with no stat at all, or one
/// read from a shard format that predates per-entry proof. No whole-second mtime reaches it, so
/// such an entry never matches a stat and the file is always hashed.
pub const UNPROVEN_MTIME: u64 = u64::MAX;

/// An item (i.e. file) in the inventory.
/// It contains metadata about a file that has been taken into inventory.
/// This metadata can be used to detect changes in the actual file.
#[derive(Clone)]
pub struct InventoryItem {
    /// Unix: ctime, Windows: reuse the content_change_timestamp
    pub metadata_change_timestamp: u64,
    /// Unix: mtime, Windows: LastWriteTime
    pub content_change_timestamp: u64,
    /// Unix: device number / ID, Windows: VolumeSerialNumber
    pub device: u64,
    /// Unix: inode, Windows: FileIndex
    pub inode: u64,
    pub item_type: DirEntryType,
    /// Unix: user ID, Windows: ignore
    pub user_id: u64,
    /// Unix: group ID, Windows: ignore
    pub group_id: u64,
    pub file_size: u64,
    pub hash: String,
    pub file_name_length: u64,
    pub state: InventoryItemState,
    pub name: String,
}

impl Display for InventoryItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "name:    {}", self.name)?;
        writeln!(f, "hash:    {}", self.hash)?;
        writeln!(f, "type:    {}", self.item_type)?;
        writeln!(f, "state:   {}", self.state)?;
        writeln!(f, "size:    {}", self.file_size)?;
        writeln!(f, "namelen: {}", self.file_name_length)?;
        writeln!(f, "MCT:     {}", self.metadata_change_timestamp)?;
        writeln!(f, "CCT:     {}", self.content_change_timestamp)?;
        writeln!(f, "devid:   {}", self.device)?;
        writeln!(f, "fileid:  {}", self.inode)?;
        writeln!(f, "userid:  {}", self.user_id)?;
        write!(f, "groupid: {}", self.group_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(name: &str, inode: u64, hash: &str) -> InventoryItem {
        InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 1,
            inode,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: hash.to_string(),
            file_name_length: name.len() as u64,
            state: InventoryItemState::Normal,
            name: name.to_string(),
        }
    }

    #[test]
    fn add_item_replaces_an_existing_entry_with_the_same_name() {
        let mut inventory = Inventory::new();

        inventory.add_item(item("file.txt", 42, "old-hash"));
        // Re-loading a modified file must update the staged entry.
        inventory.add_item(item("file.txt", 42, "new-hash"));

        assert_eq!(inventory.get_items_count(), 1);
        assert_eq!(inventory.get_item_by_name("file.txt").unwrap().hash, "new-hash");
    }

    #[test]
    fn remove_item_by_name_reports_whether_an_item_was_removed() {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", 42, "hash"));

        assert!(inventory.remove_item_by_name("file.txt"));
        assert!(!inventory.remove_item_by_name("file.txt"));
        assert_eq!(inventory.get_items_count(), 0);
    }

    #[test]
    fn mark_item_deleted_keeps_the_entry_and_flips_its_state() {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", 42, "hash"));

        assert!(inventory.mark_item_deleted("file.txt"));
        // Marking again is a no-op, but still reports success.
        assert!(inventory.mark_item_deleted("file.txt"));
        assert!(!inventory.mark_item_deleted("missing.txt"));

        let marked = inventory.get_item_by_name("file.txt").unwrap();
        assert_eq!(inventory.get_items_count(), 1);
        assert!(marked.state == InventoryItemState::Deleted);
        assert_eq!(marked.hash, "hash");
    }

    #[test]
    fn mark_all_items_deleted_marks_every_entry() {
        let mut inventory = Inventory::new();
        inventory.add_item(item("a.txt", 1, "hash-a"));
        inventory.add_item(item("b.txt", 2, "hash-b"));

        assert!(inventory.mark_all_items_deleted());
        // A second call changes nothing.
        assert!(!inventory.mark_all_items_deleted());

        for (_, marked) in inventory.get_items() {
            assert!(marked.state == InventoryItemState::Deleted);
        }
    }
}
