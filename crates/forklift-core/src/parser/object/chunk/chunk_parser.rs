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
    let payload = content.get(offset..).ok_or_else(|| format!(
        "Chunk object is truncated: the header ends past the object's {} bytes.", content.len()
    ))?;

    if payload.len() > MAX_CHUNK_BYTES {
        return Err(format!(
            "Chunk object payload is {} bytes, above the {}-byte chunk ceiling.",
            payload.len(), MAX_CHUNK_BYTES
        ));
    }

    Ok(Chunk { content: payload.to_vec() })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `offset` past the end of `content` must be refused with an `Err`, not panic (the
    /// parser sits on an untrusted-input boundary).
    #[test]
    fn an_offset_past_the_end_is_refused_not_a_panic() {
        let content = vec![1u8, 2, 3];
        let error = match parse_chunk(content.len() + 1, &content) {
            Err(e) => e,
            Ok(_) => panic!("an offset past the end must be refused"),
        };
        assert!(error.contains("truncated"), "unexpected error: {}", error);
    }
}
