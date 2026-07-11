use crate::model::chunk::Chunk;
use crate::util::chunk_utils::MAX_CHUNK_BYTES;

/// Parse a chunk object: its content is the chunk's raw bytes, verbatim (no inner format
/// version). The per-chunk ceiling is enforced on read here as well as on store — a `Chunk`
/// object whose payload exceeds `MAX_CHUNK_BYTES` is refused, so a malicious recipe cannot
/// reference an over-size chunk to inflate the assembly memory bound (review W2).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the header
///   section.
/// * `content` - The bytes of the chunk object.
///
/// # Returns
/// * `Ok(Chunk)`   - The parsed chunk object.
/// * `Err(String)` - If the chunk's payload exceeds the per-chunk ceiling.
pub fn parse_chunk(offset: usize, content: &[u8]) -> Result<Chunk, String> {
    let payload = &content[offset..];

    if payload.len() > MAX_CHUNK_BYTES {
        return Err(format!(
            "Chunk object payload is {} bytes, above the {}-byte chunk ceiling.",
            payload.len(), MAX_CHUNK_BYTES
        ));
    }

    Ok(Chunk { content: payload.to_vec() })
}
