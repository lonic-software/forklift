//! The shared on-disk form of Forklift's small line-per-entry state files: the out-of-scope
//! skeleton and consolidation state of a merge in progress, the load-guard marker, scope files,
//! bay redirects and the inventory metadata. Taint files escape their entries the same way, under
//! a header of their own, but keep their own torn-file reading (see `taint_utils`).
//!
//! The entries of these files are names: warehouse paths, pallet labels, filesystem paths. A tree
//! entry name may hold any character but `/` and NUL, line breaks included, so an entry cannot be
//! written raw, one per line. Each entry is escaped instead: `\` is written `\\`, a line feed `\n`
//! and a carriage return `\r` (which [`str::lines`] would strip from a line's end). An escaped
//! entry holds no line break, so a line is exactly one entry, and the file stays readable by eye.
//!
//! A file opens with a header line naming its kind and format version, such as
//! `/forklift-marker 1`. The header is always the first line and only the first line is read as
//! one, so an entry is never mistaken for a header, even an absolute path that happens to begin
//! `/forklift-`. A reader refuses a file whose header is missing or names another version, rather
//! than guessing at a format it does not know.
//!
//! ## Format
//!
//! ```text
//! /forklift-<kind> <version>\n
//! <escaped entry>\n
//! ...
//! ```
//!
//! Every line ends with `\n`, the last one included. An entry may be empty (the load-guard marker
//! records the warehouse root as the empty key), which is an empty line.

/// Why a record file could not be read. [`RecordError::describe`] phrases it to follow
/// "The <file> ", so each caller names its own file and adds its own way out, if it has one.
#[derive(Debug, PartialEq, Eq)]
pub enum RecordError {
    /// The bytes are not UTF-8.
    NotUtf8,

    /// The file does not open with a header of its kind: it was written by an older Forklift,
    /// before the format was versioned, or it is not this kind of file at all.
    Unversioned,

    /// The header names this kind of file, but a version this build does not know.
    OtherVersion(String),

    /// The last line has no closing line feed, so the file was cut short.
    Unterminated,

    /// An entry holds a raw carriage return, a lone `\` at its end, or a `\` before a character
    /// that is not escaped. A writer never produces one.
    BadEscape { line: usize },
}

impl RecordError {
    /// The reason as a phrase that follows "The <file> ".
    pub fn describe(&self) -> String {
        match self {
            RecordError::NotUtf8 => "is not valid UTF-8.".to_string(),
            RecordError::Unversioned => "has no format header; an older version of Forklift wrote it.".to_string(),
            RecordError::OtherVersion(header) => format!(
                "is in a format this version of Forklift does not know (\"{}\").", header.escape_debug()
            ),
            RecordError::Unterminated => "is cut short: its last line has no line ending.".to_string(),
            RecordError::BadEscape { line } => format!("has a malformed escape on line {}.", line),
        }
    }
}

/// The header line of a record of `kind` at `version`, such as `/forklift-marker 1`.
pub fn header(kind: &str, version: u32) -> String {
    format!("/forklift-{} {}", kind, version)
}

/// Escape one entry so that it holds no line break (see the module doc comment).
pub fn escape(entry: &str) -> String {
    let mut escaped = String::with_capacity(entry.len());

    for c in entry.chars() {
        match c {
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(c),
        }
    }

    escaped
}

/// Undo [`escape`]. `None` for anything [`escape`] cannot have produced.
pub fn unescape(line: &str) -> Option<String> {
    let mut entry = String::with_capacity(line.len());
    let mut chars = line.chars();

    while let Some(c) = chars.next() {
        match c {
            '\\' => match chars.next()? {
                '\\' => entry.push('\\'),
                'n' => entry.push('\n'),
                'r' => entry.push('\r'),
                _ => return None,
            },
            '\r' => return None,
            _ => entry.push(c),
        }
    }

    Some(entry)
}

/// Serialize `entries` under the header of `kind` at `version`.
pub fn to_bytes<I, S>(kind: &str, version: u32, entries: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut text = header(kind, version);
    text.push('\n');

    for entry in entries {
        text.push_str(&escape(entry.as_ref()));
        text.push('\n');
    }

    text.into_bytes()
}

/// Parse a file written by [`to_bytes`] for `kind` at `version`, returning its entries in file
/// order.
pub fn from_bytes(bytes: &[u8], kind: &str, version: u32) -> Result<Vec<String>, RecordError> {
    let text = std::str::from_utf8(bytes).map_err(|_| RecordError::NotUtf8)?;

    let expected = header(kind, version);
    let tag = format!("/forklift-{} ", kind);

    let first = text.split('\n').next().unwrap_or_default();
    if first != expected {
        return Err(if first.starts_with(&tag) {
            RecordError::OtherVersion(first.to_string())
        } else {
            RecordError::Unversioned
        });
    }

    let body = text.strip_suffix('\n').ok_or(RecordError::Unterminated)?;

    body.split('\n')
        .enumerate()
        .skip(1)
        .map(|(index, line)| unescape(line).ok_or(RecordError::BadEscape { line: index + 1 }))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every character an entry could hold that the escape has to handle, alone and together.
    const ENTRIES: [&str; 11] = [
        "", "plain", "with space", "a\nb", "\n", "c\rd", "e\r", "\r\n", "\\", "f\\ng", "\\\\n\n\r\\",
    ];

    #[test]
    fn entries_round_trip_whatever_characters_they_hold() {
        let bytes = to_bytes("test", 1, ENTRIES);

        assert_eq!(from_bytes(&bytes, "test", 1).unwrap(), ENTRIES.to_vec());
    }

    #[test]
    fn an_escaped_entry_holds_no_line_break() {
        for entry in ENTRIES {
            let escaped = escape(entry);
            assert!(!escaped.contains('\n') && !escaped.contains('\r'), "{:?} -> {:?}", entry, escaped);
        }
    }

    #[test]
    fn the_file_is_the_header_then_one_line_per_entry() {
        assert_eq!(to_bytes("test", 1, ["a\nb", "", "c\\"]), b"/forklift-test 1\na\\nb\n\nc\\\\\n".to_vec());
        assert_eq!(to_bytes("test", 1, Vec::<String>::new()), b"/forklift-test 1\n".to_vec());
        assert_eq!(from_bytes(b"/forklift-test 1\n", "test", 1).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn a_missing_or_foreign_header_is_refused() {
        assert_eq!(from_bytes(b"", "test", 1), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"src\n", "test", 1), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"/forklift-other 1\nsrc\n", "test", 1), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"/forklift-test 1\r\nsrc\n", "test", 1),
            Err(RecordError::OtherVersion("/forklift-test 1\r".to_string())));
        assert_eq!(from_bytes(b"/forklift-test 2\nsrc\n", "test", 1),
            Err(RecordError::OtherVersion("/forklift-test 2".to_string())));
    }

    #[test]
    fn a_file_cut_short_is_refused() {
        assert_eq!(from_bytes(b"/forklift-test 1", "test", 1), Err(RecordError::Unterminated));
        assert_eq!(from_bytes(b"/forklift-test 1\nsrc", "test", 1), Err(RecordError::Unterminated));
    }

    #[test]
    fn an_escape_no_writer_produces_is_refused() {
        for (line, bad) in [("a\\", 2), ("a\\t", 2), ("a\rb", 2), ("\\x41", 2)] {
            let bytes = format!("/forklift-test 1\n{}\n", line);
            assert_eq!(from_bytes(bytes.as_bytes(), "test", 1), Err(RecordError::BadEscape { line: bad }), "{:?}", line);
        }
        let later = from_bytes(b"/forklift-test 1\nok\nok\nbad\\\n", "test", 1);
        assert_eq!(later, Err(RecordError::BadEscape { line: 4 }));
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert_eq!(from_bytes(&[0xFF, b'\n'], "test", 1), Err(RecordError::NotUtf8));
    }
}
