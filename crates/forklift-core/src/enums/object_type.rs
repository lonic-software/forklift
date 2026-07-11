use std::fmt::Display;
use std::str::FromStr;

const OBJECT_TYPE_BLOB: &str = "blob";
const OBJECT_TYPE_PARCEL: &str = "parcel";
const OBJECT_TYPE_TREE: &str = "tree";
const OBJECT_TYPE_RECIPE: &str = "recipe";
const OBJECT_TYPE_CHUNK: &str = "chunk";

const CODE_BLOB: u64 = 1;
const CODE_PARCEL: u64 = 2;
const CODE_TREE: u64 = 3;
// Codes 4 and 5 are frozen: they are hashed into every recipe/chunk object's identity (the
// type code sits inside the bytes the object hashes to, see `LooseObjectBuilder::write_header`).
const CODE_RECIPE: u64 = 4;
const CODE_CHUNK: u64 = 5;

/// Types of objects recognized by Forklift.
#[derive(PartialEq)]
pub enum ObjectType {
    Blob,
    Parcel,
    Tree,

    /// A chunk index: the whole-file content hash, total size, and the ordered list of
    /// (chunk hash, chunk size) a chunked large file assembles from. A chunked file's tree
    /// entry points at a recipe (see `DirEntryType::NormalChunked`/`ExecutableChunked`).
    Recipe,

    /// A leaf byte-range of a chunked file. Its content is the raw chunk bytes; a chunk is
    /// never larger than `chunk_utils::MAX_CHUNK_BYTES` (enforced on store and read).
    Chunk,
}

impl FromStr for ObjectType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            OBJECT_TYPE_BLOB => Ok(ObjectType::Blob),
            OBJECT_TYPE_PARCEL => Ok(ObjectType::Parcel),
            OBJECT_TYPE_TREE => Ok(ObjectType::Tree),
            OBJECT_TYPE_RECIPE => Ok(ObjectType::Recipe),
            OBJECT_TYPE_CHUNK => Ok(ObjectType::Chunk),
            _ => Err(format!("Object type \"{}\" not found.", s)),
        }
    }
}

impl Display for ObjectType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let str = match self {
            ObjectType::Blob => OBJECT_TYPE_BLOB.to_string(),
            ObjectType::Parcel => OBJECT_TYPE_PARCEL.to_string(),
            ObjectType::Tree => OBJECT_TYPE_TREE.to_string(),
            ObjectType::Recipe => OBJECT_TYPE_RECIPE.to_string(),
            ObjectType::Chunk => OBJECT_TYPE_CHUNK.to_string(),
        };
        write!(f, "{}", str)
    }
}

impl ObjectType {
    /// Get the code of the object type.
    ///
    /// # Returns
    /// * `u64` - The code of the object type.
    pub fn get_code(&self) -> u64 {
        match self {
            ObjectType::Blob => CODE_BLOB,
            ObjectType::Parcel => CODE_PARCEL,
            ObjectType::Tree => CODE_TREE,
            ObjectType::Recipe => CODE_RECIPE,
            ObjectType::Chunk => CODE_CHUNK,
        }
    }

    /// Get the object type for the given code.
    ///
    /// # Arguments
    /// * `code` - The code of the object type.
    ///
    /// # Returns
    /// * `Ok(ObjectType)` - The object type associated with the given code.
    /// * `Err(String)`    - The error message if the object type is unknown.
    pub fn from_code(code: u64) -> Result<ObjectType, String> {
        match code {
            CODE_BLOB => Ok(ObjectType::Blob),
            CODE_PARCEL => Ok(ObjectType::Parcel),
            CODE_TREE => Ok(ObjectType::Tree),
            CODE_RECIPE => Ok(ObjectType::Recipe),
            CODE_CHUNK => Ok(ObjectType::Chunk),
            _ => Err(format!("Unknown object type: {}", code)),
        }
    }
}