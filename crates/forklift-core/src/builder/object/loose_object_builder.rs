use crate::builder::object::blob::blob_object_builder::BlobObjectBuilder;
use crate::builder::object::chunk::chunk_object_builder::ChunkObjectBuilder;
use crate::builder::object::parcel::parcel_object_builder::ParcelObjectBuilder;
use crate::builder::object::recipe::recipe_object_builder::RecipeObjectBuilder;
use crate::builder::object::tree::tree_object_builder::TreeObjectBuilder;
use crate::enums::object::loose_object_version::LooseObjectVersion;
use crate::enums::object_type::ObjectType;
use crate::model::blob::Blob;
use crate::model::chunk::Chunk;
use crate::model::object::loose_object::LooseObject;
use crate::model::parcel::Parcel;
use crate::model::recipe::Recipe;
use crate::model::tree_item::TreeItem;
use crate::util::{byte_utils, object_utils};

/// Builder for loose objects.
/// Every object must be built using this builder.
pub struct LooseObjectBuilder {
    pub content: Vec<u8>,
    pub object_type: ObjectType,
    pub hash: String
}

impl LooseObjectBuilder {
    /// Build a parcel object.
    ///
    /// # Arguments
    /// * `parcel` - The parcel to build.
    ///
    /// # Returns
    /// The parcel object.
    pub fn build_parcel(parcel: &Parcel) -> LooseObject {
        let builder = LooseObjectBuilder::new(ObjectType::Parcel);
        let object = ParcelObjectBuilder::build_compact(parcel);
        let content = object.content;

        builder
            .write_header(content.len())
            .write_content(content)
            .generate_hash()
            .build()
    }

    // TODO: Move common code between build_* methods to a separate method.
    // For now, I opted to wait until more object types are implemented to see what can be reused.
    /// Build a blob object.
    ///
    /// # Arguments
    /// * `blob` - The blob to build.
    ///
    /// # Returns
    /// The blob object.
    pub fn build_blob(blob: &Blob) -> LooseObject {
        let builder = LooseObjectBuilder::new(ObjectType::Blob);
        let object = BlobObjectBuilder::build(blob);
        let content = object.content;

        builder
            .write_header(content.len())
            .write_content(content)
            .generate_hash()
            .build()
    }

    /// Build a recipe object (the chunk index of a chunked large file).
    ///
    /// # Arguments
    /// * `recipe` - The recipe to build.
    ///
    /// # Returns
    /// The recipe object.
    pub fn build_recipe(recipe: &Recipe) -> LooseObject {
        let builder = LooseObjectBuilder::new(ObjectType::Recipe);
        let object = RecipeObjectBuilder::build(recipe);
        let content = object.content;

        builder
            .write_header(content.len())
            .write_content(content)
            .generate_hash()
            .build()
    }

    /// Build a chunk object (a leaf byte-range of a chunked large file).
    ///
    /// # Arguments
    /// * `chunk` - The chunk to build.
    ///
    /// # Returns
    /// The chunk object.
    pub fn build_chunk(chunk: &Chunk) -> LooseObject {
        let builder = LooseObjectBuilder::new(ObjectType::Chunk);
        let object = ChunkObjectBuilder::build(chunk);
        let content = object.content;

        builder
            .write_header(content.len())
            .write_content(content)
            .generate_hash()
            .build()
    }

    /// Build a tree object.
    ///
    /// # Arguments
    /// * `tree` - The tree to build.
    ///
    /// # Returns
    /// The tree object.
    pub fn build_tree(tree: &TreeItem) -> LooseObject {
        let builder = LooseObjectBuilder::new(ObjectType::Tree);
        let object = TreeObjectBuilder::build(tree);
        let content = object.content;

        builder
            .write_header(content.len())
            .write_content(content)
            .generate_hash()
            .build()
    }

    /// Create a new - EMPTY - object builder.
    ///
    /// # Arguments
    /// * `object_type` - The type of the object.
    ///
    /// # Returns
    /// The new object builder.
    fn new(object_type: ObjectType) -> LooseObjectBuilder {
        LooseObjectBuilder {
            content: Vec::new(),
            object_type,
            hash: String::new()
        }
    }

    /// Write the header into the contents of the object.
    ///
    /// # Arguments
    /// * `content_length` - The length of the content (excluding the header).
    ///
    /// # Returns
    /// The object builder.
    fn write_header(mut self, content_length: usize) -> Self {
        let object_version = LooseObjectVersion::latest();

        self.content.extend(byte_utils::number_to_vlq_bytes(object_version.get_code()));
        self.content.extend(byte_utils::number_to_vlq_bytes(self.object_type.get_code()));
        self.content.extend(byte_utils::number_to_vlq_bytes(content_length as u64));
        object_utils::push_null(&mut self.content);

        self
    }

    /// Write the content into the object.
    ///
    /// # Arguments
    /// * `content` - The content to write.
    ///
    /// # Returns
    /// The object builder.
    fn write_content(mut self, content: Vec<u8>) -> Self {
        self.content.extend(content);

        self
    }


    /// Generate the hash of the object.
    ///
    /// # Returns
    /// The object builder.
    fn generate_hash(mut self) -> Self {
        self.hash = blake3::hash(self.content.as_slice()).to_hex().to_string();

        self
    }

    /// Finalize the object and return it.
    ///
    /// # Returns
    /// The built object.
    fn build(self) -> LooseObject {
        LooseObject {
            content: self.content,
            object_type: self.object_type,
            hash: self.hash
        }
    }
}