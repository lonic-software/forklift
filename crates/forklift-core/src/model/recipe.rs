/// One entry in a recipe's ordered chunk list: a chunk object's hash and its (raw) byte size.
/// The chunk's offset in the assembled file is the running prefix sum of the sizes before it
/// (derivable, so it is not stored).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecipeChunk {
    /// The hash of the `Chunk`-typed object holding this chunk's raw bytes.
    pub hash: String,

    /// The chunk's raw byte size (never above `chunk_utils::MAX_CHUNK_BYTES`).
    pub size: u64,
}

/// A recipe: the chunk index a chunked large file's tree entry points at. Its own object hash is
/// what the tree commits; the assembled file's whole-content hash lives inside as `content_hash`.
///
/// The `content_hash` and the sizes are **advisory until assembly** — the true content is defined
/// solely by the individually content-addressed chunk list. A lying `content_hash`/size cannot
/// substitute bytes (each chunk still content-addresses); it can only misreport size or fail the
/// one-shot post-assembly integrity check. Nothing may key identity or dedup off `content_hash`
/// before an actual assembly re-derives it (an out-of-scope recipe's `content_hash` is never
/// verified — sparse content is sealed, not checked).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    /// Blake3 of the assembled (whole-file) bytes, 64 ASCII-hex characters. Verified only at
    /// materialization (streaming assembly) or a `--full` audit, never trusted at rest.
    pub content_hash: String,

    /// The total assembled file size. A cheap structural check at load enforces that this equals
    /// the sum of the chunk sizes.
    pub total_size: u64,

    /// The ordered chunk list. Assembling the file concatenates each chunk's raw bytes in order.
    pub chunks: Vec<RecipeChunk>,
}
