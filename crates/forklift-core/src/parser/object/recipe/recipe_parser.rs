use crate::enums::object::recipe_version::RecipeVersion;
use crate::model::recipe::Recipe;
use crate::util::byte_utils;

/// Parse a recipe object.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the header
///   section.
/// * `content` - The bytes of the recipe object.
///
/// # Returns
/// * `Ok(Recipe)`  - The parsed recipe object.
/// * `Err(String)` - The error message.
pub fn parse_recipe(offset: usize, content: &[u8]) -> Result<Recipe, String> {
    let mut cursor: usize = 0;

    let version_code = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })
        .map_err(|e| format!("Failed to parse recipe version: {}", e))?;

    let version = RecipeVersion::from_code(version_code)?;
    let recipe = version.get_parser()(offset + cursor, content);

    recipe
}
