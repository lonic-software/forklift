use chrono::{DateTime, Utc};
use crate::globals;

/// Read a line from a byte array. The content of the line is returned exactly as it appears
/// in the byte array.
///
/// # Arguments
/// * `offset`  - The offset to start reading from.
/// * `content` - The byte array to read from.
///
/// # Returns
/// * `Some((Vec<u8>, usize))` - If a line was successfully read (even if it is empty).
/// The tuple contains the line (excluding the new line character) and the number of bytes read
/// (including the new line character).
/// * `None`                   - If there were no remaining bytes to read.
pub fn read_line(offset: usize, content: &[u8]) -> Option<(Vec<u8>, usize)> {
    read_until_byte_value(offset, content, globals::BYTE_NEW_LINE)
}

/// Read the given byte array until a specific byte value is found.
///
/// # Arguments
/// * `offset`     - The offset to start reading from.
/// * `content`    - The byte array to read from.
/// * `byte_value` - The byte value to read until.
///
/// # Returns
/// * `Some((Vec<u8>, usize))` - If the byte value was found:
///    * `Vec<u8>` - The bytes read until the byte value (excluding that byte).
///    * `usize`   - The number of bytes read (including that byte).
pub fn read_until_byte_value(offset: usize,
                             content: &[u8],
                             byte_value: u8) -> Option<(Vec<u8>, usize)> {
    let mut cursor = 0;
    let mut read_bytes = Vec::new();

    while offset + cursor < content.len() {
        let byte = content[offset + cursor];
        cursor += 1;

        if byte == byte_value {
            break;
        }

        read_bytes.push(byte);
    }

    // Return "None" if there were no remaining bytes to read.
    if offset >= content.len() { None } else { Some((read_bytes, cursor)) }
}

/// Encode a number as a variable-length quantity (VLQ) byte sequence.
///
/// # Arguments
/// * `number` - The number to encode.
///
/// # Returns
/// The VLQ byte sequence.
pub fn number_to_vlq_bytes(mut number: u64) -> Vec<u8> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let mut byte = (number & 0b0111_1111) as u8; // Take the last 7 bits
        number >>= 7; // Shift the number 7 bits to the right
        if number > 0 {
            byte |= 0b1000_0000;
        }
        bytes.push(byte);
        if number == 0 {
            break;
        }
    }
    bytes
}

/// Try to decode a VLQ encoded number from a byte sequence.
/// The byte sequence is consumed until the number is fully decoded.
/// If we don't find a number in the first 10 bytes, further bytes are not consumed.
///
/// Only the shortest encoding of a number is accepted, the one [`number_to_vlq_bytes`] writes: a
/// multi-byte encoding whose last byte is zero (`80 00`, `81 80 00`) carries a group of seven zero
/// bits that adds nothing, and is refused. Objects are named by the hash of their bytes, so a
/// second spelling of one number would give one object a second name.
///
/// # Arguments
/// * `offset`  - The offset to start decoding at.
/// * `content` - The byte sequence.
/// There should be a valid VLQ encoded number in the first 10 (or less) bytes.
///
/// # Returns
/// * `Ok(u64)`    - The decoded number.
/// * `Err(String)`- The error message if the number could not be decoded, or is not in its
///                  shortest encoding.
pub fn number_from_vlq_bytes(offset: usize, content: &[u8]) -> Result<(u64, usize), String> {
    let mut cursor = 0;
    let mut value: u64 = 0;
    let mut shift = 0;

    while offset + cursor < content.len() {
        let byte = content[offset + cursor];
        cursor += 1;

        // At shift 63, only the least significant bit of the 7 payload bits still fits into
        // the u64: any higher bit means the encoded value does not fit and must be rejected
        // (instead of silently truncating it).
        if shift == 63 && byte & 0b0111_1110 != 0 {
            return Err("VLQ encoded value does not fit into 64 bits".to_string());
        }

        // Add the 7 least significant bits to the value
        value |= ((byte & 0x7F) as u64) << shift;

        // If the MSB is not set, the number is fully decoded
        if byte & 0x80 == 0 {
            if byte == 0 && cursor > 1 {
                return Err("VLQ encoded value is not in its shortest encoding".to_string());
            }

            return Ok((value, cursor));
        }

        shift += 7;

        // Prevent overflow for very large numbers
        if shift > 63 {
            return Err("Shift overflow".to_string());
        }
    }

    Err("Unexpected end of input".to_string())
}

/// Parse a VLQ encoded timestamp as a `DateTime<Utc>` from the given byte array.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at.
/// * `content` - The bytes to read.
///
/// # Returns
/// * `Ok((DateTime<Utc>, usize))`:
///    * `DateTime<Utc>` - The parsed date and time.
///    * `usize`         - The number of bytes read.
/// * `Err(String)` - The error message.
pub fn read_date_time_from_vlq_timestamp_bytes(offset: usize,
                                               content: &[u8]) -> Result<(DateTime<Utc>, usize), String> {
    let (timestamp, bytes_read) = number_from_vlq_bytes(offset, content)
        .map_err(|e| format!("Failed to parse timestamp: {}", e))?;

    let date = chrono::DateTime::from_timestamp(timestamp as i64, 0)
        .ok_or("Failed to parse timestamp".to_string())?;

    Ok((date, bytes_read))
}
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vlq_round_trip() {
        for number in [0u64, 1, 127, 128, 300, 16_384, u32::MAX as u64, u64::MAX] {
            let bytes = number_to_vlq_bytes(number);
            let (decoded, bytes_read) = number_from_vlq_bytes(0, &bytes).unwrap();

            assert_eq!(decoded, number);
            assert_eq!(bytes_read, bytes.len());
        }
    }

    #[test]
    fn vlq_decoding_rejects_values_over_64_bits() {
        // The encoding of u64::MAX with the last byte replaced so it carries more than
        // the single bit that still fits at shift 63.
        let mut bytes = number_to_vlq_bytes(u64::MAX);
        *bytes.last_mut().unwrap() = 0b0000_0011;

        assert!(number_from_vlq_bytes(0, &bytes).is_err());
    }

    /// A number has one spelling: a trailing group of zero bits is refused at every length, while
    /// a lone zero byte is the number zero.
    ///
    /// Revert: accept a final zero byte after a continuation → the padded spellings decode.
    #[test]
    fn vlq_decoding_rejects_an_overlong_encoding() {
        assert_eq!(number_from_vlq_bytes(0, &[0x00]), Ok((0, 1)));
        assert_eq!(number_from_vlq_bytes(0, &[0x81, 0x01]), Ok((129, 2)));

        for overlong in [&[0x80, 0x00][..], &[0x81, 0x00], &[0x81, 0x80, 0x00], &[0xFF, 0x80, 0x80, 0x00]] {
            let error = number_from_vlq_bytes(0, overlong).expect_err("an overlong VLQ must be refused");
            assert!(error.contains("shortest encoding"), "{:02x?}: {}", overlong, error);
        }

        // The longest legal spelling, u64::MAX, still ends in a non-zero byte.
        let max = number_to_vlq_bytes(u64::MAX);
        assert_eq!(max.len(), 10);
        let mut padded = max.clone();
        *padded.last_mut().unwrap() = 0x00;
        assert!(number_from_vlq_bytes(0, &padded).is_err(), "a ten-byte spelling of a smaller number");
    }

    #[test]
    fn vlq_decoding_rejects_truncated_input() {
        let mut bytes = number_to_vlq_bytes(u64::MAX);
        bytes.pop();

        assert!(number_from_vlq_bytes(0, &bytes).is_err());
    }

    #[test]
    fn read_line_returns_content_exactly() {
        let content = b"first\n \t \nlast";

        let (line1, read1) = read_line(0, content).unwrap();
        assert_eq!(line1, b"first");

        // A whitespace-only line is returned exactly as stored.
        let (line2, read2) = read_line(read1, content).unwrap();
        assert_eq!(line2, b" \t ");

        let (line3, _) = read_line(read1 + read2, content).unwrap();
        assert_eq!(line3, b"last");

        assert_eq!(read_line(content.len(), content), None);
    }

}
