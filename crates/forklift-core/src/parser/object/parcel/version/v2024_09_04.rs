use crate::enums::parcel_action_type::ParcelActionType;
use crate::globals;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::util::{byte_utils};

/// Parse a parcel object with version `V2024_09_04`.
///
/// # Arguments
/// * `offset` - The offset to start parsing at. This should be the byte *after*
/// the parcel format version code.
/// * `input`  - The bytes of the parcel object.
///
/// # Returns
/// * `Ok(Parcel)`  - The parsed blob object.
/// * `Err(String)` - The error message.
pub fn parse(offset: usize, input: &[u8]) -> Result<Parcel, String> {
    let mut cursor = offset;

    let tree_hash = byte_utils::read_line(cursor, input)
        .ok_or_else(|| "Tree hash not found.".to_string())
        .and_then(|(line, bytes_read)| {
            cursor += bytes_read;
            String::from_utf8(line).map_err(|_| "Failed to parse tree hash.".to_string())
        })?;

    let parents = read_parents(cursor, input)
        .map(|(p, bytes_read)| {
            cursor += bytes_read;
            p
        })?;

    let actions = read_actions(cursor, input)
        .map(|(a, bytes_read)| {
            cursor += bytes_read;
            a
        })?;

    // The description is optional: the builder writes nothing for a missing description,
    // so an empty remainder must parse back to `None` to keep round-trips symmetric.
    let description = if cursor < input.len() {
        Some(
            String::from_utf8(input[cursor..].to_vec())
                .map_err(|_| "Failed to parse description.".to_string())?
        )
    } else {
        None
    };

    Ok(
        Parcel {
            tree_hash,
            parents,
            actions,
            description,
        }
    )
}

/// Read the parents section of the parcel object.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the tree hash.
/// * `content` - The bytes of the parcel object.
///
/// # Returns
/// * `Ok((Vec<String>, usize))`:
///    * `Vec<String>` - The parent hashes.
///    * `usize`       - The number of bytes read.
/// * `Err(String)` - The error message.
fn read_parents(offset: usize, content: &[u8]) -> Result<(Vec<String>, usize), String> {
    let mut cursor = 0;
    let mut parents: Vec<String> = Vec::new();

    while offset + cursor < content.len() {
        // A zero byte indicates the end of the parents section.
        if content[offset + cursor] == globals::BYTE_NULL {
            cursor += 1;
            break;
        }

        let parent = byte_utils::read_line(offset + cursor, content)
            .ok_or_else(|| "Expected parent but not found.".to_string())
            .and_then(|(line, bytes_read)| {
                cursor += bytes_read;
                String::from_utf8(line).map_err(|_| "Failed to parse parent.".to_string())
            })?;

        parents.push(parent);
    }

    Ok((parents, cursor))
}

/// Read the actions section of the parcel object.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the parents section.
/// * `content` - The bytes of the parcel object.
///
/// # Returns
/// * `Ok((Vec<ParcelAction>, usize))`:
///    * `Vec<ParcelAction>` - The parcel actions.
///    * `usize`             - The number of bytes read.
/// * `Err(String)` - The error message.
fn read_actions(offset: usize, content: &[u8]) -> Result<(Vec<ParcelAction>, usize), String> {
    let mut cursor = 0;
    let mut actions: Vec<ParcelAction> = Vec::new();

    while offset + cursor < content.len() {
        // A zero byte indicates the end of the actions section.
        if content[offset + cursor] == globals::BYTE_NULL {
            cursor += 1;
            break;
        }

        let action_type_code = byte_utils::number_from_vlq_bytes(
            offset + cursor,
            content
        ).map(|(action_type_code, bytes_read)| {
            cursor += bytes_read;
            action_type_code
        }).map_err(|e| format!("Failed to parse action type code: {}", e))?;

        let action_type = ParcelActionType::from_code(action_type_code)?;
        let action_time =
            byte_utils::read_date_time_from_vlq_timestamp_bytes(offset + cursor, content)
                .map(|(date, bytes_read)| {
                    cursor += bytes_read;
                    date
                })?;

        let operator_identifier =
            byte_utils::read_until_byte_value(offset + cursor, content, globals::BYTE_END_OF_TEXT)
                .ok_or_else(|| "Expected operator identifier but not found.".to_string())
                .and_then(|(line, bytes_read)| {
                    cursor += bytes_read;
                    String::from_utf8(line)
                        .map_err(|_| "Failed to parse operator identifier.".to_string())
                })?;

        // The action description is optional: the builder writes an empty line for a missing
        // description, so an empty line must parse back to `None`.
        let description = byte_utils::read_line(offset + cursor, content)
            .map(|(line, bytes_read)| {
                cursor += bytes_read;
                String::from_utf8(line).map_err(|_| "Failed to parse description.".to_string())
            }).transpose()?
            .filter(|description| !description.is_empty());

        actions.push(ParcelAction {
            operator: Operator { identifier: operator_identifier },
            action: action_type,
            description,
            timestamp: action_time,
        })
    }

    Ok((actions, cursor))
}