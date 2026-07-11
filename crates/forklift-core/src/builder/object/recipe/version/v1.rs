use crate::model::recipe::Recipe;
use crate::util::byte_utils;

/// Build a recipe object payload (`RECIPE_FORMAT_V1`).
///
/// Layout — a format freeze (this feeds the recipe hash, and through it the signed tree hash):
/// ```text
/// [recipe_format_version_vlq]
/// [content_hash]              64 ASCII-hex bytes — Blake3 of the assembled file
/// [total_size_vlq]
/// [chunk_count_vlq]
/// ( [chunk_hash] 64 ASCII-hex bytes [chunk_size_vlq] ) * chunk_count
/// ```
/// Hashes are ASCII-hex (fixed 64 bytes each, so no delimiter is needed), consistent with every
/// other Forklift object format. Chunk offsets are the running prefix sum of the sizes and are
/// therefore not stored.
///
/// # Arguments
/// * `version` - The recipe format version code.
/// * `recipe`  - The recipe to serialize.
///
/// # Returns
/// The bytes of the recipe object payload (without the loose-object header).
pub fn build(version: u64, recipe: &Recipe) -> Vec<u8> {
    let mut content: Vec<u8> = Vec::new();

    content.extend(byte_utils::number_to_vlq_bytes(version));
    content.extend(recipe.content_hash.as_bytes());
    content.extend(byte_utils::number_to_vlq_bytes(recipe.total_size));
    content.extend(byte_utils::number_to_vlq_bytes(recipe.chunks.len() as u64));

    for chunk in &recipe.chunks {
        content.extend(chunk.hash.as_bytes());
        content.extend(byte_utils::number_to_vlq_bytes(chunk.size));
    }

    content
}
