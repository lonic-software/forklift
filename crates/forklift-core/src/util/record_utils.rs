//! The shared on-disk form of Forklift's small line-per-entry state files, such as the state of a
//! merge in progress, the load-guard marker, scope files, bay redirects and the inventory
//! metadata. Taint files escape their entries the same way, under a header of their own, but keep
//! their own torn-file reading (see `taint_utils`).
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
//! digest <hex>\n
//! ```
//!
//! Every line ends with `\n`, the last one included. An entry may be empty (the load-guard marker
//! records the warehouse root as the empty key), which is an empty line.
//!
//! Every record ends in one more line, `digest <hex>`, the blake3 hash of every byte before it.
//! That frame is frozen across versions, so damage that still parses, such as a hex digit flipped
//! to another or a cut on a line boundary, reads as damage rather than as true or as a format. A
//! record with no digest line at all is judged by its header: an older version's is that version,
//! since versions from before the digest wrote none, and any other was cut off. Taint files frame
//! their entries the same way (see `taint_utils`); the sealed record keeps a log of CRC-framed
//! entries of its own (see `sealed_utils`).

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

    /// The record does not end in a digest line that matches the bytes before it (see
    /// [`from_bytes`]).
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
pub(crate) fn header(kind: &str, version: u32) -> String {
    format!("/forklift-{} {}", kind, version)
}

/// Escape one entry so that it holds no line break (see the module doc comment).
pub(crate) fn escape(entry: &str) -> String {
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
pub(crate) fn unescape(line: &str) -> Option<String> {
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

/// Serialize `entries` under the header of `kind` at `version`, then append the digest line
/// [`from_bytes`] checks: `digest ` and the blake3 hash, in hex, of every byte before that line.
/// This frame is frozen across versions: a record of any version ends in it, so a reader of any
/// build tells a damaged record from one of another version.
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

    with_digest(text.into_bytes())
}

/// Parse a record written by [`to_bytes`] for `kind` at `version`, returning its entries in file
/// order.
///
/// The digest is checked first. A record whose last line opens as a digest line that does not
/// match is [`RecordError::Damaged`], whatever its header says. A record with no digest line at
/// all is judged by its header alone ([`read_header`]): another kind's file, or none, is
/// [`RecordError::Unversioned`], and an older version of this kind is
/// [`RecordError::OtherVersion`], since versions from before the digest wrote none. Any other
/// record without one is damaged: every version since writes it, so it was cut off.
pub fn from_bytes(bytes: &[u8], kind: &str, version: u32) -> Result<Vec<String>, RecordError> {
    match verified_body(bytes) {
        Some(body) => parse_body(body, kind, version),
        None if has_digest_line(bytes) => Err(RecordError::Damaged),
        None => Err(match read_header(bytes, kind, version) {
            Err(RecordError::OtherVersion(found)) if header_number(&found, kind).is_some_and(|n| n < version) =>
                RecordError::OtherVersion(found),
            Err(RecordError::Unversioned) => RecordError::Unversioned,
            _ => RecordError::Damaged,
        }),
    }
}

/// Judge the first line of `bytes` alone as the header of `kind` at `version`, with no digest:
/// `Ok` for this version's header, [`RecordError::OtherVersion`] for another version of this
/// kind, [`RecordError::Unversioned`] for anything else. Only for a reader that looks at a
/// record's header without reading the record, such as a gate that refuses another version.
pub fn read_header(bytes: &[u8], kind: &str, version: u32) -> Result<(), RecordError> {
    let first = bytes.split(|byte| *byte == b'\n').next().unwrap_or_default();
    let Ok(first) = std::str::from_utf8(first) else {
        return Err(RecordError::Unversioned);
    };

    if first == header(kind, version) {
        Ok(())
    } else if first.starts_with(&format!("/forklift-{} ", kind)) {
        Err(RecordError::OtherVersion(first.to_string()))
    } else {
        Err(RecordError::Unversioned)
    }
}

/// The body of a verified record parsed under the header of `kind` at `version`.
fn parse_body(bytes: &[u8], kind: &str, version: u32) -> Result<Vec<String>, RecordError> {
    let text = std::str::from_utf8(bytes).map_err(|_| RecordError::NotUtf8)?;
    read_header(bytes, kind, version)?;

    let body = text.strip_suffix('\n').ok_or(RecordError::Unterminated)?;

    body.split('\n')
        .enumerate()
        .skip(1)
        .map(|(index, line)| unescape(line).ok_or(RecordError::BadEscape { line: index + 1 }))
        .collect()
}

/// The version a header of `kind` names, when it is written as [`to_bytes`] writes one: the tag,
/// one space, and a number without a leading zero.
fn header_number(header: &str, kind: &str) -> Option<u32> {
    header.strip_prefix(&format!("/forklift-{} ", kind))
        .and_then(|text| text.parse::<u32>().ok().filter(|found| found.to_string() == text))
}

/// Whether a record was written by a newer build: its digest verifies, and its header names
/// `kind` at a version newer than `version`. A record whose digest does not verify is damage,
/// whatever its header says. A record that does not read and passes this is a newer build's.
pub fn written_by_newer(bytes: &[u8], kind: &str, version: u32) -> bool {
    verified_body(bytes).is_some_and(|body| {
        let first = body.split(|byte| *byte == b'\n').next().unwrap_or_default();
        std::str::from_utf8(first).ok()
            .and_then(|header| header_number(header, kind))
            .is_some_and(|found| found > version)
    })
}

/// The line prefix of a record's last line.
const DIGEST_PREFIX: &str = "digest ";

/// `bytes` followed by the digest line of every byte before it.
pub fn with_digest(mut bytes: Vec<u8>) -> Vec<u8> {
    let digest = blake3::hash(&bytes).to_hex();
    bytes.extend_from_slice(format!("{}{}\n", DIGEST_PREFIX, digest).as_bytes());
    bytes
}

/// The bytes of a record before its digest line, when its last line is a digest line that
/// matches them; `None` otherwise. Judged before anything else, at any version.
pub(crate) fn verified_body(bytes: &[u8]) -> Option<&[u8]> {
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

/// Whether `bytes` ends with a line that opens as a digest line, matching or not: a record that
/// failed its digest rather than one that never had one, such as a record from before its digest.
pub fn has_digest_line(bytes: &[u8]) -> bool {
    without_digest_line(bytes).len() != bytes.len()
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
pub(crate) fn salvage<T>(bytes: &[u8], parse: impl Fn(&str) -> Option<T>) -> (Vec<T>, usize) {
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
        assert!(written_by_newer(&with_digest(b"/forklift-k 3\n\xff\n".to_vec()), "k", 2));
        assert!(written_by_newer(&with_digest(b"/forklift-k 10\n".to_vec()), "k", 2));
        for bytes in [&b"/forklift-k 2\n"[..], b"/forklift-k 1\n", b"/forklift-j 3\n", b"/forklift-k 03\n",
                      b"/forklift-k  3\n", b"/forklift-k 3x\n", b"\xff/forklift-k 3\n", b"", b"/forklift-k \n"] {
            assert!(!written_by_newer(&with_digest(bytes.to_vec()), "k", 2), "{:?}", String::from_utf8_lossy(bytes));
        }

        // A newer header without its digest is damage, never a newer build's.
        assert!(!written_by_newer(b"/forklift-k 3\n", "k", 2));
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
    fn the_file_is_the_header_then_one_line_per_entry_then_the_digest() {
        let body = b"/forklift-test 1\na\\nb\n\nc\\\\\n".to_vec();
        let digest = format!("digest {}\n", blake3::hash(&body).to_hex());
        assert_eq!(to_bytes("test", 1, ["a\nb", "", "c\\"]), [body, digest.into_bytes()].concat());
        assert_eq!(from_bytes(&to_bytes("test", 1, Vec::<String>::new()), "test", 1).unwrap(), Vec::<String>::new());
    }

    /// With no digest line at all, the header alone is judged: another kind's file or none is
    /// unversioned, and an older version of this kind is that version, since no version from
    /// before the digest wrote one. Anything else without one was cut off: damage.
    ///
    /// Revert: read a record with no digest line as `OtherVersion` whatever its version → the
    /// newer header without a digest reads as a newer build's.
    #[test]
    fn a_record_without_a_digest_line_is_judged_by_its_header() {
        assert_eq!(from_bytes(b"", "test", 2), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"src\n", "test", 2), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"\xff\nsrc\n", "test", 2), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"/forklift-other 2\nsrc\n", "test", 2), Err(RecordError::Unversioned));
        assert_eq!(from_bytes(b"/forklift-test 1\nsrc\n", "test", 2),
            Err(RecordError::OtherVersion("/forklift-test 1".to_string())));
        for damaged in [&b"/forklift-test 2\nsrc\n"[..], b"/forklift-test 3\nsrc\n", b"/forklift-test 01\n",
                        b"/forklift-test 1\r\nsrc\n", b"/forklift-test 2"] {
            assert_eq!(from_bytes(damaged, "test", 2), Err(RecordError::Damaged), "{:?}", String::from_utf8_lossy(damaged));
        }
    }

    /// The header alone, with no digest: for a gate that refuses another version unread.
    #[test]
    fn read_header_judges_the_first_line_alone() {
        assert_eq!(read_header(b"/forklift-test 2\n", "test", 2), Ok(()));
        assert_eq!(read_header(b"/forklift-test 3\nanything", "test", 2),
            Err(RecordError::OtherVersion("/forklift-test 3".to_string())));
        assert_eq!(read_header(b"/forklift-tests 2\n", "test", 2), Err(RecordError::Unversioned));
    }

    /// Inside a verified frame, the body is still read strictly: a body no writer produces is
    /// refused, never guessed at.
    #[test]
    fn a_verified_body_no_writer_produces_is_refused() {
        let framed = |body: &[u8]| with_digest(body.to_vec());
        assert_eq!(from_bytes(&framed(b"/forklift-test 1\r\nsrc\n"), "test", 1),
            Err(RecordError::OtherVersion("/forklift-test 1\r".to_string())));
        assert_eq!(from_bytes(&framed(b"/forklift-other 1\nsrc\n"), "test", 1), Err(RecordError::Unversioned));
        for (line, bad) in [("a\\", 2), ("a\\t", 2), ("a\rb", 2), ("\\x41", 2)] {
            let bytes = framed(format!("/forklift-test 1\n{}\n", line).as_bytes());
            assert_eq!(from_bytes(&bytes, "test", 1), Err(RecordError::BadEscape { line: bad }), "{:?}", line);
        }
        let later = from_bytes(&framed(b"/forklift-test 1\nok\nok\nbad\\\n"), "test", 1);
        assert_eq!(later, Err(RecordError::BadEscape { line: 4 }));
        assert_eq!(from_bytes(&framed(b"/forklift-test 1\n\xff\n"), "test", 1), Err(RecordError::NotUtf8));
    }

    /// The digest is checked before the header, at any version, and covers every byte before it:
    /// both shapes of damage that still parse, a hex digit flipped to another and a cut on a line
    /// boundary, read as damage.
    ///
    /// Revert: read the header before the digest → the digit flipped to a newer version reads as
    /// `OtherVersion`, not damage.
    #[test]
    fn a_record_is_judged_by_its_digest_first() {
        let bytes = to_bytes("test", 2, ["aa", "bb"]);
        assert_eq!(from_bytes(&bytes, "test", 2).unwrap(), vec!["aa", "bb"]);
        assert!(!written_by_newer(&bytes, "test", 2));

        let at = b"/forklift-test 2\n".len();
        let mut newer = bytes.clone();
        newer[b"/forklift-test ".len()] = b'3';
        let mut hex_flip = bytes.clone();
        hex_flip[at] = b'b';
        let line_cut = [&bytes[..at + 3], &bytes[at + 6..]].concat();
        for damaged in [&newer[..], &hex_flip[..], &line_cut[..], &bytes[..bytes.len() - 1],
                        &bytes[..bytes.len() - 10], &bytes[..at + 6]] {
            assert_eq!(from_bytes(damaged, "test", 2), Err(RecordError::Damaged),
                "{:?}", String::from_utf8_lossy(damaged));
            assert!(!written_by_newer(damaged, "test", 2));
        }

        // A newer build's record, whole, reads as newer, not as damage.
        let theirs = to_bytes("test", 3, ["a"]);
        assert_eq!(from_bytes(&theirs, "test", 2),
            Err(RecordError::OtherVersion("/forklift-test 3".to_string())));
        assert!(written_by_newer(&theirs, "test", 2));
    }

    /// Salvage reads a digested record without its digest line, whole or cut.
    #[test]
    fn the_digest_line_is_left_out_of_a_salvage() {
        let bytes = to_bytes("test", 2, ["aa", "bb"]);
        let parse = |line: &str| Some(line.to_string());
        assert_eq!(salvage(without_digest_line(&bytes), parse), (vec!["aa".to_string(), "bb".to_string()], 0));
        assert_eq!(salvage(without_digest_line(&bytes[..bytes.len() - 5]), parse),
            (vec!["aa".to_string(), "bb".to_string()], 0));
        assert_eq!(without_digest_line(b"/forklift-test 2\naa\n"), b"/forklift-test 2\naa\n");
    }
}
