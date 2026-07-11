use crate::enums::object::recipe_version::RecipeVersion;
use crate::model::object::recipe_object::RecipeObject;
use crate::model::recipe::Recipe;

/// Builder for recipe objects.
/// This should NOT be used directly. Use `LooseObjectBuilder` instead.
pub struct RecipeObjectBuilder {
    pub content: Vec<u8>,
}

impl RecipeObjectBuilder {
    /// Build a recipe object.
    ///
    /// # Arguments
    /// * `recipe` - The recipe data.
    ///
    /// # Returns
    /// The built recipe object.
    pub fn build(recipe: &Recipe) -> RecipeObject {
        let version = RecipeVersion::latest();
        let builder_fn = version.get_builder();

        RecipeObject {
            content: builder_fn(recipe),
        }
    }
}
