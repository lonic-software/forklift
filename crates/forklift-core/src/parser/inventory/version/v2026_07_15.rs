use crate::globals;
use crate::model::inventory::Inventory;
use crate::util::byte_utils;

/// Parse an inventory object's content with version `V2026_07_15`: byte-identical per-item
/// encoding to `V2024_09_04` — the rollup hash added by this version lives in the header, not
/// the content, so the item body is reused rather than duplicated.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after* the inventory
/// header.
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// * `Ok(Inventory)` - The parsed inventory (its rollup hash is not set here — the caller
///   attaches it from the parsed header).
/// * `Err(String)`   - The error message.
pub fn parse(offset: usize, content: &[u8]) -> Result<Inventory, String> {
    super::v2024_09_04::parse(offset, content)
}

/// Parse the inventory header's extra bytes for version `V2026_07_15`: a presence flag (VLQ
/// `0`/`1`) followed by the rollup hash bytes when present, terminated the same way an item's
/// hash is (an end-of-text byte, not a length prefix).
///
/// # Arguments
/// * `offset`  - The offset to start parsing at (right after the entry count).
/// * `content` - The bytes of the inventory file.
///
/// # Returns
/// * `Ok((Some(String), usize))` - The rollup hash, and the number of bytes read.
/// * `Ok((None, usize))`         - No rollup hash was present, and the number of bytes read.
/// * `Err(String)`               - If the extra header bytes are malformed: a presence flag
///   other than `0`/`1`, a hash with no terminator, or a present-but-empty hash. A shard this
///   malformed is corrupt and must fail loudly, not silently swallow the rest of the file as
///   the "hash" (losing every entry after it) or accept a hash no real writer ever produces.
pub fn parse_header(offset: usize, content: &[u8]) -> Result<(Option<String>, usize), String> {
    let mut cursor = 0usize;

    let present = byte_utils::number_from_vlq_bytes(offset + cursor, content)
        .map(|(flag, bytes_read)| {
            cursor += bytes_read;
            flag
        })?;

    match present {
        0 => Ok((None, cursor)),
        1 => {
            let (hash_bytes, bytes_read) =
                byte_utils::read_until_byte_value(offset + cursor, content, globals::BYTE_END_OF_TEXT)
                    .ok_or("Expected inventory rollup hash, but not found.".to_string())?;

            // `read_until_byte_value` also returns a result when it simply runs out of input
            // before ever finding the terminator (see its own doc comment) — that is not a
            // header any well-formed writer produces, so it must fail loudly here rather than
            // silently swallow the rest of the file as the "hash" (and lose every entry after
            // it). A genuine match always consumes the terminator as the last byte read.
            if content.get(offset + cursor + bytes_read - 1) != Some(&globals::BYTE_END_OF_TEXT) {
                return Err("Inventory rollup hash is not terminated.".to_string());
            }

            if hash_bytes.is_empty() {
                return Err("Inventory rollup presence flag is set, but the rollup hash is empty.".to_string());
            }

            cursor += bytes_read;

            let hash = String::from_utf8(hash_bytes)
                .map_err(|_| "Failed to parse inventory rollup hash.".to_string())?;

            Ok((Some(hash), cursor))
        }
        _ => Err(format!(
            "Unexpected inventory rollup presence flag {} (expected 0 or 1).", present
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_present_rollup_parses() {
        let mut content = vec![1u8];
        content.extend_from_slice(b"abcd");
        content.push(globals::BYTE_END_OF_TEXT);

        let (rollup, bytes_read) = parse_header(0, &content).unwrap();

        assert_eq!(rollup, Some("abcd".to_string()));
        assert_eq!(bytes_read, content.len());
    }

    #[test]
    fn an_absent_rollup_parses() {
        let content = [0u8];

        let (rollup, bytes_read) = parse_header(0, &content).unwrap();

        assert_eq!(rollup, None);
        assert_eq!(bytes_read, 1);
    }

    #[test]
    fn a_presence_flag_other_than_0_or_1_is_a_loud_error() {
        let content = [2u8];

        let error = parse_header(0, &content).unwrap_err();

        assert!(error.contains("presence flag"), "{}", error);
    }

    #[test]
    fn a_missing_terminator_is_a_loud_error_not_a_silent_swallow() {
        // Presence = 1, followed by hash bytes with no end-of-text terminator anywhere in the
        // rest of the buffer — `read_until_byte_value` still returns `Some` here (it just runs
        // out of input), so this must be caught explicitly rather than accepted as the hash.
        let content = [1u8, b'a', b'b', b'c'];

        let error = parse_header(0, &content).unwrap_err();

        assert!(error.contains("not terminated"), "{}", error);
    }

    #[test]
    fn a_present_but_empty_hash_is_a_loud_error() {
        // Presence = 1, immediately followed by the terminator: a zero-length hash.
        let content = [1u8, globals::BYTE_END_OF_TEXT];

        let error = parse_header(0, &content).unwrap_err();

        assert!(error.contains("empty"), "{}", error);
    }

    #[test]
    fn no_bytes_at_all_after_the_presence_flag_is_a_loud_error() {
        let content = [1u8];

        assert!(parse_header(0, &content).is_err());
    }
}
