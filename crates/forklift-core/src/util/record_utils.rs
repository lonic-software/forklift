//! The shared on-disk form of Forklift's small line-per-entry state files: the out-of-scope
//! skeleton and consolidation state of a merge in progress, the load-guard marker, scope files,
//! bay redirects, the inventory metadata and the git mirror's roots. Taint files escape their entries the same way, under
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
//!
//! A digested record ([`to_bytes_digested`]) ends in one more line, `digest <hex>`, the blake3
//! hash of every byte before it. That frame is frozen across versions, so damage reads as damage
//! rather than as a format: the warehouse marker and the parked-work record are digested.

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

    /// A digested record (see [`from_bytes_digested`]) does not end in its digest line, or the
    /// digest does not match the bytes before it.
    Damaged,
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
            RecordError::Damaged => "is damaged: it does not match its digest.".to_string(),
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

/// Whether `bytes` open with the header of `kind` at a version newer than `version`, written as
/// [`to_bytes`] writes one: the tag, one space, and a number without a leading zero. Judged on the
/// first line's own bytes, so damage elsewhere, such as a byte that is not UTF-8, does not hide
/// it. A record that does not read and passes this is a newer build's, not damage.
pub fn written_by_newer(bytes: &[u8], kind: &str, version: u32) -> bool {
    let first = bytes.split(|byte| *byte == b'\n').next().unwrap_or_default();

    std::str::from_utf8(first).ok()
        .and_then(|header| header.strip_prefix(&format!("/forklift-{} ", kind)))
        .and_then(|text| text.parse::<u32>().ok().filter(|found| found.to_string() == text))
        .is_some_and(|found| found > version)
}

/// The line prefix of a digested record's last line.
const DIGEST_PREFIX: &str = "digest ";

/// Serialize `entries` as [`to_bytes`] does, then append the digest line [`from_bytes_digested`]
/// checks: `digest ` and the blake3 hash, in hex, of every byte before that line. This frame is
/// frozen across versions: a record of any version ends in it, so a reader of any build tells a
/// damaged record from one of another version before it reads the header.
pub fn to_bytes_digested<I, S>(kind: &str, version: u32, entries: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    with_digest(to_bytes(kind, version, entries))
}

/// `bytes` followed by the digest line of every byte before it.
pub fn with_digest(mut bytes: Vec<u8>) -> Vec<u8> {
    let digest = blake3::hash(&bytes).to_hex();
    bytes.extend_from_slice(format!("{}{}\n", DIGEST_PREFIX, digest).as_bytes());
    bytes
}

/// The bytes of a digested record before its digest line, when its last line is a digest line
/// that matches them; `None` otherwise. Judged before anything else, at any version.
fn verified_body(bytes: &[u8]) -> Option<&[u8]> {
    let (body, last) = last_line(bytes);
    let hex = last.strip_suffix(b"\n")?.strip_prefix(DIGEST_PREFIX.as_bytes())?;

    (blake3::hash(body).to_hex().as_bytes() == hex).then_some(body)
}

/// `bytes` split before its last line, which keeps its newline when it has one.
fn last_line(bytes: &[u8]) -> (&[u8], &[u8]) {
    let text = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    let start = text.iter().rposition(|byte| *byte == b'\n').map_or(0, |index| index + 1);

    bytes.split_at(start)
}

/// Parse a record written by [`to_bytes_digested`] for `kind` at `version`. The digest is checked
/// first: a record whose last line is no digest line, or whose digest does not match, is
/// [`RecordError::Damaged`] whatever its header says. Only then is the header read, as
/// [`from_bytes`] reads it.
pub fn from_bytes_digested(bytes: &[u8], kind: &str, version: u32) -> Result<Vec<String>, RecordError> {
    from_bytes(verified_body(bytes).ok_or(RecordError::Damaged)?, kind, version)
}

/// Whether a digested record was written by a newer build: its digest verifies, and its header
/// names `kind` at a version newer than `version` (see [`written_by_newer`]). A record whose
/// digest does not verify is damage, whatever its header says.
pub fn written_by_newer_digested(bytes: &[u8], kind: &str, version: u32) -> bool {
    verified_body(bytes).is_some_and(|body| written_by_newer(body, kind, version))
}

/// `bytes` without a digested record's last line, when that line opens as a digest line, matching
/// or not, cut short or not: what [`salvage`] reads of a damaged digested record.
pub fn without_digest_line(bytes: &[u8]) -> &[u8] {
    let (body, last) = last_line(bytes);

    match last.starts_with(DIGEST_PREFIX.as_bytes()) {
        true => body,
        false => bytes,
    }
}

/// Every line of a damaged record that still reads whole, unescaped, in file order, and how many
/// did not. A line is whole only when it is valid UTF-8, its escapes read, it ends in its newline
/// and `parse` accepts it: a record cut inside its last entry would otherwise keep an altered one.
/// Empty lines are skipped, and so is a first line that opens as a record header, of any kind or
/// version.
pub fn salvage<T>(bytes: &[u8], parse: impl Fn(&str) -> Option<T>) -> (Vec<T>, usize) {
    let mut whole = Vec::new();
    let mut dropped = 0;

    let lines: Vec<&[u8]> = bytes.split(|byte| *byte == b'\n').collect();
    for (index, raw) in lines.iter().enumerate() {
        if raw.is_empty() || (index == 0 && raw.starts_with(b"/forklift-")) {
            continue;
        }

        // The piece after the last newline is a line the record was cut inside.
        let terminated = index + 1 < lines.len();
        let parsed = std::str::from_utf8(raw).ok()
            .filter(|_| terminated)
            .and_then(unescape)
            .and_then(|entry| parse(&entry));

        match parsed {
            Some(entry) => whole.push(entry),
            None => dropped += 1,
        }
    }

    (whole, dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A newer header is told apart from this version's, an older one, another kind's, a padded
    /// number and a first line that is not UTF-8, whatever follows it.
    ///
    /// Revert: compare with `>=` → this version's own header reads as newer.
    #[test]
    fn only_a_newer_header_of_the_same_kind_reads_as_written_by_newer() {
        assert!(written_by_newer(b"/forklift-k 3\n\xff\n", "k", 2));
        assert!(written_by_newer(b"/forklift-k 10", "k", 2));
        for bytes in [&b"/forklift-k 2\n"[..], b"/forklift-k 1\n", b"/forklift-j 3\n", b"/forklift-k 03\n",
                      b"/forklift-k  3\n", b"/forklift-k 3x\n", b"\xff/forklift-k 3\n", b"", b"/forklift-k \n"] {
            assert!(!written_by_newer(bytes, "k", 2), "{:?}", String::from_utf8_lossy(bytes));
        }
    }

    /// Only a terminated, valid, parsed line is kept; the header and empty lines count nothing.
    ///
    /// Revert: keep the unterminated last piece → the cut "cc" is kept.
    #[test]
    fn salvage_keeps_only_whole_lines_the_parser_accepts() {
        let parse = |line: &str| (line.len() == 2).then(|| line.to_string());
        let bytes = b"/forklift-k 9\naa\n\n\xffb\nb\\q\nccc\nd\\\\\ncc";

        assert_eq!(salvage(bytes, parse), (vec!["aa".to_string(), "d\\".to_string()], 4));
    }

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

    /// The digest is checked before the header, at any version, and covers every byte before it.
    ///
    /// Revert: read the header before the digest → the digit flipped to a newer version reads as
    /// `OtherVersion`, not damage.
    #[test]
    fn a_digested_record_is_judged_by_its_digest_first() {
        let bytes = to_bytes_digested("test", 2, ["a", "b"]);
        assert_eq!(from_bytes_digested(&bytes, "test", 2).unwrap(), vec!["a", "b"]);
        assert!(!written_by_newer_digested(&bytes, "test", 2));

        // A digit flip to another version, a flipped entry byte, a cut and a missing digest.
        let mut newer = bytes.clone();
        newer[b"/forklift-test ".len()] = b'3';
        let mut flipped = bytes.clone();
        flipped[b"/forklift-test 2\n".len()] = b'z';
        for damaged in [&newer[..], &flipped[..], &bytes[..bytes.len() - 1], &bytes[..bytes.len() - 10],
                        &to_bytes("test", 2, ["a", "b"])[..], b""] {
            assert_eq!(from_bytes_digested(damaged, "test", 2), Err(RecordError::Damaged),
                "{:?}", String::from_utf8_lossy(damaged));
            assert!(!written_by_newer_digested(damaged, "test", 2));
        }

        // A newer build's record, whole, reads as newer, not as damage.
        let theirs = to_bytes_digested("test", 3, ["a"]);
        assert_eq!(from_bytes_digested(&theirs, "test", 2),
            Err(RecordError::OtherVersion("/forklift-test 3".to_string())));
        assert!(written_by_newer_digested(&theirs, "test", 2));
    }

    /// Salvage reads a digested record without its digest line, whole or cut.
    #[test]
    fn the_digest_line_is_left_out_of_a_salvage() {
        let bytes = to_bytes_digested("test", 2, ["aa", "bb"]);
        let parse = |line: &str| Some(line.to_string());
        assert_eq!(salvage(without_digest_line(&bytes), parse), (vec!["aa".to_string(), "bb".to_string()], 0));
        assert_eq!(salvage(without_digest_line(&bytes[..bytes.len() - 5]), parse),
            (vec!["aa".to_string(), "bb".to_string()], 0));
        assert_eq!(without_digest_line(b"/forklift-test 2\naa\n"), b"/forklift-test 2\naa\n");
    }

    #[test]
    fn invalid_utf8_is_refused() {
        assert_eq!(from_bytes(&[0xFF, b'\n'], "test", 1), Err(RecordError::NotUtf8));
    }
}
