/// A chunk object (without the loose-object header). Its content is the chunk's raw bytes.
pub struct ChunkObject {
    pub content: Vec<u8>,
}
