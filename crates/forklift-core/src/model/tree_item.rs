use std::collections::btree_map::Iter;
use std::collections::BTreeMap;
use crate::enums::dir_entry_type::DirEntryType;

/// Represents a tree item (i.e., a directory or file).
///
/// `Clone` is cheap: only one level is ever loaded (see [`object_utils::load_tree`]), so a
/// subtree child's own `tree_children`/`file_children` are always empty maps — cloning a
/// `TreeItem` copies its immediate entries, never a whole subtree recursively.
///
/// [`object_utils::load_tree`]: crate::util::object_utils::load_tree
#[derive(Clone)]
pub struct TreeItem {
    pub name: String,
    pub hash: String,
    pub item_type: DirEntryType,
    tree_children: BTreeMap<String, TreeItem>,
    file_children: BTreeMap<String, TreeItem>
}

impl TreeItem {
    /// Create a new tree item.
    ///
    /// # Arguments
    /// * `name`      - The name of the item.
    /// * `hash`      - The hash of the item.
    /// * `item_type` - The type of the item.
    ///
    /// # Returns
    /// The tree item.
    pub fn new(name: String, hash: String, item_type: DirEntryType) -> TreeItem {
        TreeItem {
            name,
            hash,
            item_type,
            tree_children: BTreeMap::new(),
            file_children: BTreeMap::new(),
        }
    }

    /// Check if the item is a tree (i.e., a directory).
    ///
    /// # Returns
    /// `true` if the item is a tree, `false` otherwise.
    pub fn is_tree(&self) -> bool {
        self.item_type == DirEntryType::Tree
    }

    /// Add a child to the tree item.
    ///
    /// # Arguments
    /// * `child` - The child to add.
    pub fn add_child(&mut self, child: TreeItem) {
        if child.is_tree() {
            self.add_tree_child(child);
        } else {
            self.add_file_child(child);
        }
    }

    /// Add a tree child to the tree item.
    ///
    /// # Arguments
    /// * `child` - The tree child to add.
    fn add_tree_child(&mut self, child: TreeItem) {
        self.tree_children.insert(child.name.clone(), child);
    }

    /// Add a file child to the tree item.
    ///
    /// # Arguments
    /// * `child` - The file child to add.
    fn add_file_child(&mut self, child: TreeItem) {
        self.file_children.insert(child.name.clone(), child);
    }

    /// Get the subtrees of the tree item, sorted by name (ascending).
    ///
    /// # Returns
    /// The subtrees.
    pub fn get_subtrees(&self) -> Iter<'_, String, TreeItem> {
        self.tree_children.iter()
    }

    /// Get the files of the tree item, sorted by name (ascending).
    ///
    /// # Returns
    /// The files.
    pub fn get_files(&self) -> Iter<'_, String, TreeItem> {
        self.file_children.iter()
    }
}