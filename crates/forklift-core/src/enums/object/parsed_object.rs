use crate::enums::object_type::ObjectType;
use crate::model::blob::Blob;
use crate::model::chunk::Chunk;
use crate::model::parcel::Parcel;
use crate::model::recipe::Recipe;
use crate::model::tree_item::TreeItem;

/// A parsed loose object.
pub enum ParsedObject {
    /// A blob object.
    Blob(Blob),

    /// A parcel object.
    Parcel(Parcel),

    /// A tree object.
    Tree(TreeItem),

    /// A recipe object (the chunk index of a chunked large file).
    Recipe(Recipe),

    /// A chunk object (a leaf byte-range of a chunked large file).
    Chunk(Chunk),
}

impl ParsedObject {
    /// Get the type of the object.
    ///
    /// # Returns
    /// The type of the object.
    pub fn get_type(&self) -> ObjectType {
        match self {
            ParsedObject::Blob(_)   => ObjectType::Blob,
            ParsedObject::Parcel(_) => ObjectType::Parcel,
            ParsedObject::Tree(_)   => ObjectType::Tree,
            ParsedObject::Recipe(_) => ObjectType::Recipe,
            ParsedObject::Chunk(_)  => ObjectType::Chunk,
        }
    }
}