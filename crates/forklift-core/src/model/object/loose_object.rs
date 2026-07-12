use std::ops::Add;
use std::path::Path;
use crate::enums::object_type::ObjectType;
use crate::util::{file_utils, object_utils};

/// A loose object.
/// Compress it before saving it to the object store.
pub struct LooseObject {
    pub content: Vec<u8>,
    pub object_type: ObjectType,
    pub hash: String
}

impl LooseObject {
    // TODO: handle in buffers instead of all at once. use zstd::stream::Encoder
    /// Compress the object.
    ///
    /// # Returns
    /// The compressed bytes of the object.
    pub fn compress(&mut self) -> Result<Vec<u8>, String> {
        zstd::encode_all(self.content.as_slice(), 0)
            .map_err(|e| format!("Error while compressing object: {}", e))
    }

    /// Compress and save the object to the object store.
    ///
    /// # Returns
    /// * `Ok(String, bool)`:
    ///    * `String`: The full path (relative to the root of the warehouse)
    /// where the object is stored.
    ///    * `bool`: True if a new object was stored, false if the object already existed.
    /// * `Err(String)`- The error message, if the operation failed.
    pub fn store(&mut self) -> Result<(String, bool), String> {
        // The whole-object ceiling, on the way in from local authorship (`stack`, `import-git`, a
        // meta write). Only a tree or a recipe can legitimately approach it; blobs and chunks are
        // bounded well below it by construction. Reads never re-store an object, so a grandfathered
        // giant authored before this policy stays readable — this gates new authorship only.
        object_utils::check_object_ceiling(&self.object_type, self.content.len())?;

        let does_exist = file_utils::does_object_exist(&self.hash)?;
        let (path, file_name) = file_utils::get_path_for_object(&self.hash)?;

        if !does_exist {
            let compressed = self.compress()?;
            file_utils::write_object_to_file(Path::new(&path), &file_name, compressed)?;
        }

        Ok((path.add(file_utils::PATH_SEPARATOR).add(&file_name), !does_exist))
    }
}