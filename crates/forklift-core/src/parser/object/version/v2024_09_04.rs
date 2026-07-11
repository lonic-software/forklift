use crate::enums::object_type::ObjectType;
use crate::{globals, parser};
use crate::enums::object::parsed_object::ParsedObject;
use crate::model::blob::Blob;
use crate::model::parcel::Parcel;
use crate::model::tree_item::TreeItem;
use crate::util::byte_utils;

/// The header of a loose object (version `v2024_09_04`).
struct ObjectHeader {
    object_type: ObjectType,
    // The content length is currently unused. Uncomment it once it is needed.
    //content_length: u64,
}

/// Parse a parcel object from the given loose object bytes (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the loose object format version.
/// * `content` - The bytes of the loose object.
///
/// # Returns
/// * `Ok(Parcel)`  - The parsed parcel object.
/// * `Err(String)` - The error message.
pub fn parse_parcel(offset: usize, content: &[u8]) -> Result<Parcel, String> {
    let mut cursor: usize = 0;

    validate_header(offset + cursor, content, ObjectType::Parcel)
        .inspect(|bytes_read| cursor += bytes_read)?;

    let parcel = parser::object::parcel::compact_parcel_parser::parse_compact_parcel(
        offset + cursor,
        content
    );

    parcel
}

/// Parse a blob object from the given loose object bytes (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the loose object format version.
/// * `content` - The bytes of the loose object.
///
/// # Returns
/// * `Ok(Blob)`    - The parsed blob object.
/// * `Err(String)` - The error message.
pub fn parse_blob(offset: usize, content: &[u8]) -> Result<Blob, String> {
    let mut cursor: usize = 0;

    validate_header(offset + cursor, content, ObjectType::Blob)
        .inspect(|bytes_read| cursor += bytes_read)?;

    let blob = parser::object::blob::blob_parser::parse_blob(
        offset + cursor,
        content
    );

    blob
}

/// Parse a tree object from the given loose object bytes (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the loose object format version.
/// * `content` - The bytes of the loose object.
///
/// # Returns
/// * `Ok(TreeItem)` - The parsed tree object.
/// * `Err(String)`  - The error message.
pub fn parse_tree(offset: usize, content: &[u8]) -> Result<TreeItem, String> {
    let mut cursor = 0;

    validate_header(offset + cursor, content, ObjectType::Tree)
        .inspect(|bytes_read| cursor += bytes_read)?;

    let tree = parser::object::tree::tree_parser::parse_tree(
        offset + cursor,
        content
    );

    tree
}

/// Parse any loose object from the given bytes (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the loose object format version.
/// * `content` - The bytes of the loose object.
///
/// # Returns
/// * `Ok(ParsedObject)` - The parsed object.
/// * `Err(String)`      - The error message.
pub fn parse(offset: usize, content: &[u8]) -> Result<ParsedObject, String> {
    let mut cursor = 0;

    let object_header = read_header(offset + cursor, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })?;

    let parsed_object = match object_header.object_type {
        ObjectType::Blob =>
            parser::object::blob::blob_parser::parse_blob(
                offset + cursor,
                content
            ).map(ParsedObject::Blob),
        ObjectType::Parcel =>
            parser::object::parcel::compact_parcel_parser::parse_compact_parcel(
                offset + cursor,
                content
            ).map(ParsedObject::Parcel),
        ObjectType::Tree =>
            parser::object::tree::tree_parser::parse_tree(
                offset + cursor,
                content
            ).map(ParsedObject::Tree),
        ObjectType::Recipe =>
            parser::object::recipe::recipe_parser::parse_recipe(
                offset + cursor,
                content
            ).map(ParsedObject::Recipe),
        ObjectType::Chunk =>
            parser::object::chunk::chunk_parser::parse_chunk(
                offset + cursor,
                content
            ).map(ParsedObject::Chunk),
    };

    parsed_object
}

/// Read the header part of a loose object (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the object format version.
/// * `content` - The bytes of the loose object.
///
/// # Returns
/// * `Ok((ObjectHeader, usize))`:
///    * `ObjectHeader` - The parsed object header.
///    * `usize`        - The number of bytes read.
/// * `Err(String)` - The error message.
fn read_header(offset: usize, content: &[u8]) -> Result<(ObjectHeader, usize), String> {
    let mut cursor: usize = 0;

    let object_type_code = byte_utils::number_from_vlq_bytes(offset, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })
        .map_err(|e| format!("Failed to parse object type: {}", e))?;

    let object_type = ObjectType::from_code(object_type_code)?;

    // We discard the content length, as it is currently unused.
    byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })
        .map_err(|e| format!("Failed to parse object content length: {}", e))?;

    // Discard everything until the next null byte, which indicates the end of the header.
    byte_utils::read_until_byte_value(offset + cursor, content, globals::BYTE_NULL)
        .inspect(|(_, bytes_read)| cursor += bytes_read);

    let header = ObjectHeader {
        object_type,
    };

    Ok((header, cursor))
}

/// Validate the header of a loose object (version `v2024_09_04`).
///
/// # Arguments
/// * `offset`        - The offset to start parsing at
/// (this should be the byte *after* the object type).
/// * `content`       - The bytes of the loose object.
/// * `expected_type` - The expected object type.
///
/// # Returns
/// * `usize`       - The number of bytes read.
/// * `Err(String)` - The error message.
fn validate_header(offset: usize,
                   content: &[u8],
                   expected_type: ObjectType) -> Result<usize, String> {
    let mut cursor: usize = 0;

    let object_header = read_header(offset, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        })?;

    if object_header.object_type != expected_type {
        return Err(format!(
            "Expected object type {} but got {}",
            expected_type,
            object_header.object_type
        ));
    }

    Ok(cursor)
}