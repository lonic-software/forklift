use crate::model::chunk::Chunk;
use crate::model::object::chunk_object::ChunkObject;

/// Builder for chunk objects.
/// This should NOT be used directly. Use `LooseObjectBuilder` instead.
///
/// A chunk object's content is the chunk's raw bytes verbatim — no inner format version (the
/// recipe format version governs the chunking scheme). The distinct `Chunk` object type in the
/// loose-object header is what keeps a chunk from ever colliding with a same-bytes blob.
pub struct ChunkObjectBuilder {
    pub content: Vec<u8>,
}

impl ChunkObjectBuilder {
    /// Build a chunk object.
    ///
    /// # Arguments
    /// * `chunk` - The chunk data.
    ///
    /// # Returns
    /// The built chunk object.
    pub fn build(chunk: &Chunk) -> ChunkObject {
        ChunkObject {
            content: chunk.content.clone(),
        }
    }
}
