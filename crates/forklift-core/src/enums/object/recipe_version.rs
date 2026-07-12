use crate::builder::object::recipe::version;
use crate::model::recipe::Recipe;

/// `RECIPE_FORMAT_V1` — the first (and only) recipe object format. This code, together with the
/// chunking constants and gear table in `chunk_utils`, freezes forever: it feeds the recipe hash
/// and through it the signed tree hash. A future version may change the payload layout or the
/// chunking scheme; `V1` then stays available for reading, exactly as an old tree-format parser
/// does.
const CODE_VERSION_1: u64 = 1;

/// Versions of the recipe object format.
pub enum RecipeVersion {
    /// The original recipe format (`RECIPE_FORMAT_V1`).
    V1,
}

impl RecipeVersion {
    /// Get the code of the version.
    ///
    /// # Returns
    /// * `u64` - The code of the version.
    pub fn get_code(&self) -> u64 {
        match self {
            RecipeVersion::V1 => CODE_VERSION_1,
        }
    }

    /// Get the version for the given code.
    ///
    /// # Arguments
    /// * `code` - The code of the version.
    ///
    /// # Returns
    /// * `Ok(RecipeVersion)` - The version associated with the given code.
    /// * `Err(String)`       - The error message if the version is unknown.
    pub fn from_code(code: u64) -> Result<RecipeVersion, String> {
        match code {
            CODE_VERSION_1 => Ok(RecipeVersion::V1),
            _ => Err(format!("Unknown recipe version: {}", code)),
        }
    }

    /// Get the object builder function for the version.
    ///
    /// # Returns
    /// * `impl Fn(&Recipe) -> Vec<u8>` - The object builder function.
    pub fn get_builder(&self) -> impl Fn(&Recipe) -> Vec<u8> + '_ {
        let builder_fn = match self {
            RecipeVersion::V1 => version::v1::build,
        };

        move |recipe: &Recipe| builder_fn(self.get_code(), recipe)
    }

    /// Get the object parser function for the version.
    ///
    /// # Returns
    /// * `impl Fn(usize, &[u8]) -> Result<Recipe, String>` - The object parser function.
    pub fn get_parser(&self) -> impl Fn(usize, &[u8]) -> Result<Recipe, String> + '_ {
        let parser_fn = match self {
            RecipeVersion::V1 =>
                crate::parser::object::recipe::version::v1::parse,
        };

        move |offset, content| parser_fn(offset, content)
    }

    /// Get the latest version of the recipe object format.
    ///
    /// # Returns
    /// The latest version of the recipe object format.
    pub fn latest() -> RecipeVersion {
        RecipeVersion::V1
    }
}
