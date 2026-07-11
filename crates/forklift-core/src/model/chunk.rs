/// A chunk: a leaf byte-range of a chunked large file. Its content is the chunk's raw bytes,
/// with no inner format version — the recipe format version governs the whole chunking scheme
/// (including how chunks are encoded), so a chunk object is just its raw bytes under a distinct
/// object type. A chunk's raw content is never larger than `chunk_utils::MAX_CHUNK_BYTES`.
pub struct Chunk {
    pub content: Vec<u8>,
}
