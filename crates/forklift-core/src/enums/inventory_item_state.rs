use std::fmt::Display;

const CODE_NORMAL: u64 = 1;
const CODE_FIRST_PARENT_CONFLICT: u64 = 2;
const CODE_SECOND_PARENT_CONFLICT: u64 = 3;
const CODE_THIRD_PARENT_CONFLICT: u64 = 4;
const CODE_DELETED: u64 = 5;

/// The state of an inventory item.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum InventoryItemState {
    /// Normal state. The item has been loaded (taken into inventory).
    Normal,

    /// The item is from the first parent and is in conflict.
    FirstParentConflict,

    /// The item is from the second parent and is in conflict.
    SecondParentConflict,

    /// The item is from the third parent and is in conflict.
    ThirdParentConflict,

    /// The item is staged for removal: it will not be part of the next parcel.
    /// The entry is kept in the inventory (instead of being erased) so that the staged
    /// removal survives subsequent loads and can be reported by status-like commands.
    Deleted,
}

impl Display for InventoryItemState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        let state_str = match self {
            InventoryItemState::Normal               => "Loaded",
            InventoryItemState::FirstParentConflict  => "In conflict - From 1st parent",
            InventoryItemState::SecondParentConflict => "In Conflict - From 2nd parent",
            InventoryItemState::ThirdParentConflict  => "In Conflict - From 3rd parent",
            InventoryItemState::Deleted              => "Staged for removal",
        };

        write!(f, "{}", state_str)
    }
}

impl InventoryItemState {
    /// Get the code of the inventory item state.
    ///
    /// # Returns
    /// * `u64` - The code of the inventory item state.
    pub fn get_code(&self) -> u64 {
        match self {
            InventoryItemState::Normal => CODE_NORMAL,
            InventoryItemState::FirstParentConflict => CODE_FIRST_PARENT_CONFLICT,
            InventoryItemState::SecondParentConflict => CODE_SECOND_PARENT_CONFLICT,
            InventoryItemState::ThirdParentConflict => CODE_THIRD_PARENT_CONFLICT,
            InventoryItemState::Deleted => CODE_DELETED,
        }
    }

    /// Get the inventory item state for the given code.
    ///
    /// # Arguments
    /// * `code` - The code of the inventory item state.
    ///
    /// # Returns
    /// * `Ok(InventoryItemState)` - The inventory item state.
    /// * `Err(String)`            - If the code is not recognized.
    pub fn from_code(code: u64) -> Result<InventoryItemState, String> {
        match code {
            CODE_NORMAL                 => Ok(InventoryItemState::Normal),
            CODE_FIRST_PARENT_CONFLICT  => Ok(InventoryItemState::FirstParentConflict),
            CODE_SECOND_PARENT_CONFLICT => Ok(InventoryItemState::SecondParentConflict),
            CODE_THIRD_PARENT_CONFLICT  => Ok(InventoryItemState::ThirdParentConflict),
            CODE_DELETED                => Ok(InventoryItemState::Deleted),
            _ => Err(format!("Inventory item state code {} not found.", code)),
        }
    }
}