use crate::model::recipe::{Recipe, RecipeChunk};
use crate::util::byte_utils;
use crate::util::chunk_utils::MAX_CHUNK_BYTES;

/// The length, in bytes, of an ASCII-hex Blake3 hash as stored in a recipe.
const HASH_HEX_LEN: usize = 64;

/// Parse a recipe object payload (`RECIPE_FORMAT_V1`) and enforce its internal structural
/// invariants at load — before any chunk is fetched.
///
/// The structural checks (cheap, `O(chunk_count)`, zero bytes fetched) catch a broad class of
/// malformed or lying recipes up front: a hash that is not 64 ASCII-hex characters, a chunk whose
/// declared size exceeds the frozen per-chunk ceiling, or (the memo's named check)
/// `sum(chunk_sizes) != total_size`. They do **not** prove `content_hash` correct — only an
/// actual streaming assembly (or a `--full` audit) does that; `content_hash`/sizes stay advisory
/// until then.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the recipe
///   format version code.
/// * `input`   - The bytes of the recipe object.
///
/// # Returns
/// * `Ok(Recipe)`  - The parsed, structurally valid recipe.
/// * `Err(String)` - If the payload is truncated, a hash is malformed, a chunk is over-size, or
///   the sizes do not sum to `total_size`.
pub fn parse(offset: usize, input: &[u8]) -> Result<Recipe, String> {
    let mut cursor = offset;

    let content_hash = read_hash(&mut cursor, input)?;

    let total_size = byte_utils::number_from_vlq_bytes(cursor, input)
        .map(|(value, read)| { cursor += read; value })
        .map_err(|e| format!("Failed to parse recipe total size: {}", e))?;

    let chunk_count = byte_utils::number_from_vlq_bytes(cursor, input)
        .map(|(value, read)| { cursor += read; value })
        .map_err(|e| format!("Failed to parse recipe chunk count: {}", e))?;

    let mut chunks = Vec::with_capacity(chunk_count.min(64 * 1024) as usize);
    let mut size_sum: u64 = 0;

    for index in 0..chunk_count {
        let hash = read_hash(&mut cursor, input)?;

        let size = byte_utils::number_from_vlq_bytes(cursor, input)
            .map(|(value, read)| { cursor += read; value })
            .map_err(|e| format!("Failed to parse the size of chunk {}: {}", index, e))?;

        // A chunk larger than the frozen per-chunk ceiling is malformed (no legitimate writer
        // emits one), so refuse it at load rather than let it inflate the assembly memory bound.
        if size > MAX_CHUNK_BYTES as u64 {
            return Err(format!(
                "Recipe chunk {} declares size {} above the {}-byte chunk ceiling.",
                index, size, MAX_CHUNK_BYTES
            ));
        }

        size_sum = size_sum.checked_add(size).ok_or_else(||
            "Recipe chunk sizes overflow a 64-bit total.".to_string()
        )?;

        chunks.push(RecipeChunk { hash, size });
    }

    // Nothing may trail the last chunk: extra bytes mean a malformed (or maliciously padded)
    // recipe whose declared chunk count does not match its actual content.
    if cursor != input.len() {
        return Err(format!(
            "Recipe has {} trailing bytes after its {} chunks.",
            input.len() - cursor, chunk_count
        ));
    }

    // The named structural check: the declared sizes must sum to the declared total.
    if size_sum != total_size {
        return Err(format!(
            "Recipe is inconsistent: its {} chunk sizes sum to {}, not the declared total {}.",
            chunk_count, size_sum, total_size
        ));
    }

    Ok(Recipe { content_hash, total_size, chunks })
}

/// Read one 64-character ASCII-hex hash at `*cursor`, advancing the cursor past it. Rejects a
/// truncated or non-hex hash (nothing but lowercase/uppercase hex digits is legal).
fn read_hash(cursor: &mut usize, input: &[u8]) -> Result<String, String> {
    let end = cursor.checked_add(HASH_HEX_LEN)
        .filter(|end| *end <= input.len())
        .ok_or_else(|| "Recipe hash is truncated.".to_string())?;

    let bytes = &input[*cursor..end];

    if !bytes.iter().all(|b| b.is_ascii_hexdigit()) {
        return Err("Recipe hash is not valid ASCII hex.".to_string());
    }

    // Every byte is an ASCII hex digit, so this is always valid UTF-8.
    let hash = String::from_utf8(bytes.to_vec())
        .map_err(|_| "Recipe hash is not valid UTF-8.".to_string())?;

    *cursor = end;
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTENT_HASH: &str = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";
    const CHUNK_A: &str = "1111111111111111111111111111111111111111111111111111111111111111";
    const CHUNK_B: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    /// Serialize a recipe payload by hand from explicit fields, so a test can pin the exact bytes
    /// and craft structurally invalid ones the real builder would never emit. Layout matches
    /// `builder::object::recipe::version::v1::build` (but without going through it).
    fn payload(content_hash: &str, total_size: u64, chunks: &[(&str, u64)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend(byte_utils::number_to_vlq_bytes(1)); // recipe format version
        out.extend(content_hash.as_bytes());
        out.extend(byte_utils::number_to_vlq_bytes(total_size));
        out.extend(byte_utils::number_to_vlq_bytes(chunks.len() as u64));
        for (hash, size) in chunks {
            out.extend(hash.as_bytes());
            out.extend(byte_utils::number_to_vlq_bytes(*size));
        }
        out
    }

    /// Parse a hand-built payload the way `recipe_parser::parse_recipe` does (version VLQ, then
    /// this version's `parse`).
    fn parse_payload(bytes: &[u8]) -> Result<Recipe, String> {
        let (version, read) = byte_utils::number_from_vlq_bytes(0, bytes).unwrap();
        assert_eq!(version, 1);
        parse(read, bytes)
    }

    #[test]
    fn a_valid_recipe_round_trips() {
        let bytes = payload(CONTENT_HASH, 300, &[(CHUNK_A, 100), (CHUNK_B, 200)]);
        let recipe = parse_payload(&bytes).expect("a consistent recipe parses");

        assert_eq!(recipe.content_hash, CONTENT_HASH);
        assert_eq!(recipe.total_size, 300);
        assert_eq!(recipe.chunks.len(), 2);
        assert_eq!(recipe.chunks[0].hash, CHUNK_A);
        assert_eq!(recipe.chunks[0].size, 100);
        assert_eq!(recipe.chunks[1].size, 200);
    }

    #[test]
    fn sizes_that_do_not_sum_to_the_total_are_rejected() {
        // total_size 999 but the chunks sum to 300 — the named structural check (review W3).
        let bytes = payload(CONTENT_HASH, 999, &[(CHUNK_A, 100), (CHUNK_B, 200)]);
        let err = parse_payload(&bytes).expect_err("an inconsistent total must be rejected");
        assert!(err.contains("inconsistent"), "unexpected error: {}", err);
    }

    #[test]
    fn an_over_size_chunk_reference_is_rejected() {
        // A chunk claiming a size above the per-chunk ceiling is malformed at load, before any
        // chunk is fetched (review W2 on the recipe side).
        let over = MAX_CHUNK_BYTES as u64 + 1;
        let bytes = payload(CONTENT_HASH, over, &[(CHUNK_A, over)]);
        let err = parse_payload(&bytes).expect_err("an over-size chunk reference must be rejected");
        assert!(err.contains("chunk ceiling"), "unexpected error: {}", err);
    }

    #[test]
    fn a_non_hex_hash_is_rejected() {
        let bad_hash = "zzzz1111111111111111111111111111111111111111111111111111111111111";
        let bytes = payload(CONTENT_HASH, 100, &[(bad_hash, 100)]);
        let err = parse_payload(&bytes).expect_err("a non-hex chunk hash must be rejected");
        assert!(err.contains("ASCII hex"), "unexpected error: {}", err);
    }

    #[test]
    fn trailing_bytes_after_the_last_chunk_are_rejected() {
        let mut bytes = payload(CONTENT_HASH, 100, &[(CHUNK_A, 100)]);
        bytes.push(0x42); // one byte too many
        let err = parse_payload(&bytes).expect_err("trailing bytes must be rejected");
        assert!(err.contains("trailing"), "unexpected error: {}", err);
    }
}
