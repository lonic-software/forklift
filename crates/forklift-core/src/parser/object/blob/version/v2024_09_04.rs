use crate::model::blob::Blob;

/// Parse a blob object with version `V2024_09_04`.
///
/// # Arguments
/// * `offset` - The offset to start parsing at This should be the byte *after*
/// the blob format version code.
/// * `input`  - The bytes of the blob object.
///
/// # Returns
/// * `Ok(Blob)`    - The parsed blob object.
/// * `Err(String)` - The error message.
pub fn parse(offset: usize, input: &[u8]) -> Result<Blob, String> {
    let payload = input.get(offset..).ok_or_else(|| format!(
        "Blob object is truncated: the header ends past the object's {} bytes.", input.len()
    ))?;

    Ok (
        Blob {
            content: payload.to_vec()
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An `offset` past the end of `input` must be refused with an `Err`, not panic (the parser
    /// sits on an untrusted-input boundary).
    #[test]
    fn an_offset_past_the_end_is_refused_not_a_panic() {
        let input = vec![1u8, 2, 3];
        let error = match parse(input.len() + 1, &input) {
            Err(e) => e,
            Ok(_) => panic!("an offset past the end must be refused"),
        };
        assert!(error.contains("truncated"), "unexpected error: {}", error);
    }
}
