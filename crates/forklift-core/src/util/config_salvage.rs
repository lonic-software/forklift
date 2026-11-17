//! What a damaged configuration file still provably holds.
//!
//! A configuration file has two writers: forklift, through `config_utils`'s one locked writer,
//! and the person editing it by hand. No witness copy can follow the second, so the only evidence
//! of a damaged file's content is the damaged file itself, and this module reads it for the
//! entries the damage provably spared. Everything else is dropped, and [`Salvage::lossless`] says
//! whether the drop can have cost anything. Pure: bytes in, a verdict out.
//!
//! Two simple readings are unsound, and the rules below exist because of them:
//! * **Line by line.** Forklift itself writes a value holding a newline as a multi-line string,
//!   so a line is not an entry.
//! * **Decode lossily, keep what parses.** A damaged newline before a `[remote]` header swallows
//!   the header into the comment above it, and the remote's URL then parses as `mirror.url`.
//!
//! So an entry is kept only when its own text holds no damaged character and it validates. Damage
//! anywhere else lies in a comment, and is read as the newline it may have been: when the rest of
//! its line could be a header or an entry, the section that line would belong to goes. A section
//! that loses one entry loses all of them: every key resolves on its own, warehouse first, so a
//! kept `remote.url` beside a dropped `remote.token` would hand the global file's token to the
//! warehouse's remote.

use std::ops::Range;

/// One entry the salvage keeps: its dotted path and its string value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeptEntry {
    /// The path, e.g. `["remote", "url"]` or `["profile", "work", "identifier"]`.
    pub path: Vec<String>,
    /// The value, exactly as the file holds it.
    pub value: String,
}

/// One entry, or one stretch of text, the salvage drops.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DroppedEntry {
    /// The 1-based line it starts on.
    pub line: usize,
    /// Its dotted key, when the key is still readable; `None` when it is damaged or unknown.
    pub key: Option<String>,
}

/// What a damaged file still provably holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Salvage {
    /// The entries kept, in file order.
    pub kept: Vec<KeptEntry>,
    /// The entries dropped, in file order.
    pub dropped: Vec<DroppedEntry>,
    /// Whether the file was cut short: it does not end in a newline.
    pub cut: bool,
    /// Whether the text left after removing a cut line parses at all. When it does not, nothing
    /// is kept and `dropped` names what still looks like an entry, best effort.
    pub parsed: bool,
    /// Whether writing [`Salvage::kept`] can have lost nothing the file held. Plain `heal` writes
    /// only a lossless salvage; anything else needs the operator to accept the loss.
    pub lossless: bool,
    /// The text the verdict read: the file up to any cut line, every damaged character in it read
    /// as U+FFFD. When the salvage is lossless, it is the file with nothing lost but the damaged
    /// characters, comments included.
    pub text: String,
}

/// Whether `c` is a character TOML never allows anywhere in a document: a control character
/// other than tab, line feed and carriage return. Zeroed bytes read as these.
pub fn is_forbidden_control(c: char) -> bool {
    (c < ' ' && c != '\t' && c != '\n' && c != '\r') || c == '\u{7f}'
}

/// Whether the character `c` at byte `at` of `text` is one TOML never allows where it stands: a
/// forbidden control character ([`is_forbidden_control`]), or a carriage return no line feed
/// follows, as a byte flipped to 0x0D reads. A carriage return that ends the text is left to the
/// cut it may be the start of.
pub fn is_damaged_char(text: &str, at: usize, c: char) -> bool {
    is_forbidden_control(c) || (c == '\r' && at + 1 < text.len() && !text[at + 1..].starts_with('\n'))
}

/// Whether `c` marks damage in the text the verdict reads, where every damaged character already
/// reads as the replacement character a byte that was not UTF-8 decodes to.
fn is_damage(c: char) -> bool {
    c == char::REPLACEMENT_CHARACTER || is_forbidden_control(c)
}

/// What a cut inside a multi-line string leaves unwritten: the string's closing delimiter.
const CLOSERS: [&str; 3] = ["", "\"\"\"\n", "'''\n"];

/// `text` parsed as a document, or, when it ends inside a multi-line string, as one with that
/// string closed. Only a cut leaves a string open at the end of the text it spares, so only a
/// cut's text may be read this way: anywhere else, the closer would swallow what follows into the
/// string.
pub fn parse_closed(text: &str) -> Option<toml_edit::Document<String>> {
    CLOSERS.iter().find_map(|closer| toml_edit::Document::parse(format!("{}{}", text, closer)).ok())
}

/// One entry found in the parsed text, before the verdict on it.
struct Found {
    path: Vec<String>,
    value: Option<String>,
    span: Range<usize>,
}

/// Read what a damaged file still provably holds.
///
/// # Arguments
/// * `bytes` - The damaged file's bytes.
/// * `valid` - Whether one entry (its path and string value) is one the file may hold.
///
/// # Returns
/// * `Salvage` - The entries kept, those dropped, and whether the drop can have cost anything.
pub fn salvage(bytes: &[u8], valid: impl Fn(&[String], &str) -> bool) -> Salvage {
    // A file that does not end in a newline was cut short: its partial last line goes, and
    // whatever followed it is lost beyond telling.
    let cut = !bytes.is_empty() && !bytes.ends_with(b"\n");
    let (body, partial) = match cut {
        true => {
            let at = bytes.iter().rposition(|&b| b == b'\n').map_or(0, |at| at + 1);
            (&bytes[..at], Some(&bytes[at..]))
        }
        false => (bytes, None),
    };

    // A forbidden control character or a lone carriage return stops every TOML reader, wherever
    // it lies; read as the replacement character, it is damage like any byte that was not UTF-8,
    // and the rest of the file still parses around it when it lies in a comment.
    let decoded = String::from_utf8_lossy(body);
    let text: String = decoded.char_indices()
        .map(|(at, c)| if is_damaged_char(&decoded, at, c) { char::REPLACEMENT_CHARACTER } else { c })
        .collect();
    let damage: Vec<Range<usize>> = text.char_indices()
        .filter(|&(_, c)| is_damage(c))
        .map(|(at, c)| at..at + c.len_utf8())
        .collect();

    // A cut inside a multi-line string leaves it open. With damage anywhere, the closing delimiter
    // may be what the damage hid, and closing the string would swallow what followed it.
    let document = match cut && damage.is_empty() {
        true => parse_closed(&text),
        false => toml_edit::Document::parse(text.clone()).ok(),
    };
    let Some(document) = document else {
        let dropped = entry_lines(&text);
        return Salvage { kept: Vec::new(), dropped, cut, parsed: false, lossless: false, text };
    };

    let mut found = Vec::new();
    let mut headers = Vec::new();
    collect(document.as_table(), &[], 0, &mut found, &mut headers);
    found.sort_by_key(|entry| entry.span.start);

    let damaged = |range: &Range<usize>| damage.iter().any(|at| at.start < range.end && range.start < at.end);

    let mut dropped_groups: Vec<Vec<String>> = Vec::new();
    let mut drop_group = |path: &[String]| {
        if !dropped_groups.contains(&group(path)) {
            dropped_groups.push(group(path));
        }
    };
    let mut verdicts = Vec::with_capacity(found.len());
    for entry in &found {
        let keep = !damaged(&entry.span)
            && entry.value.as_deref().is_some_and(|value| valid(&entry.path, value));
        if !keep {
            drop_group(&entry.path);
        }
        verdicts.push(keep);
    }

    // Damage outside every entry lies in a comment. Were the damaged character a newline, the
    // rest of its line would be a line of its own: a hidden entry belongs to the section governing
    // that spot, which goes whole; a hidden header moves every entry after it into that section,
    // which goes whole too. In the root, where no header governs, a hidden line may be of any
    // section, so every one goes.
    //
    // Damage inside a header, in a quoted name, leaves the header's entries under a name it never
    // held: its section goes whole.
    let governing = |at: usize| headers.iter()
        .filter(|(span, _)| span.start <= at)
        .max_by_key(|(span, _)| span.start)
        .map(|(_, path)| path.clone());
    let mut hidden = Vec::new();
    // Where a hidden header or unreadable line may start, and the section governing that spot:
    // the entries after it in that section may belong to another, so their keys are unknown.
    let mut misread_from: Vec<(usize, Option<Vec<String>>)> = Vec::new();
    for at in damage.iter().filter(|at| !found.iter().any(|entry| entry.span.contains(&at.start))) {
        if let Some((_, path)) = headers.iter().find(|(span, _)| span.contains(&at.start)) {
            drop_group(path);
            hidden.push(DroppedEntry { line: line_of(&text, at.start), key: None });
            continue;
        }
        let Some(line) = hidden_line(&text, at.end) else {
            continue;
        };
        let section = governing(at.start);
        let key = line.map(|key| {
            let mut path = section.clone().unwrap_or_default();
            path.extend(key);
            path
        });
        match (&key, &section) {
            (Some(path), _) => drop_group(path),
            (None, Some(section)) => drop_group(section),
            (None, None) => for entry in &found {
                drop_group(&entry.path);
            },
        }
        if key.is_none() {
            misread_from.push((at.start, section.clone()));
        }
        hidden.push(DroppedEntry {
            line: line_of(&text, at.start),
            key: key.map(|path| path.join(".")).filter(|key| !key.chars().any(is_damage)),
        });
    }

    // The cut line belongs to the section the last header still visible governs, and that
    // section may have run on past the cut: it goes whole. A cut header fragment ends the
    // section above it, so that one is complete.
    let fragment = partial.is_some_and(|line| String::from_utf8_lossy(line).trim_start().starts_with('['));
    if cut && !fragment {
        match headers.iter().max_by_key(|(span, _)| span.start) {
            Some((_, path)) => drop_group(path),
            // No header at all: the cut lies in the root, where any entry may be of any section.
            None => for entry in &found {
                drop_group(&entry.path);
            },
        }
    }

    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (entry, keep) in found.iter().zip(verdicts) {
        if keep && !dropped_groups.contains(&group(&entry.path)) {
            kept.push(KeptEntry { path: entry.path.clone(), value: entry.value.clone().unwrap_or_default() });
        } else {
            let key = entry.path.join(".");
            let misread = misread_from.iter()
                .any(|(from, section)| entry.span.start > *from && governing(entry.span.start) == *section);
            dropped.push(DroppedEntry {
                line: line_of(&text, entry.span.start),
                key: (!misread && !key.chars().any(is_damage)).then_some(key),
            });
        }
    }
    dropped.extend(hidden);
    dropped.sort_by_key(|entry| entry.line);

    let lossless = !cut && dropped.is_empty();

    Salvage { kept, dropped, cut, parsed: true, lossless, text }
}

/// The section an entry's path belongs to, the unit the salvage keeps or drops whole: a top-level
/// section, or one named profile.
fn group(path: &[String]) -> Vec<String> {
    let depth = if path.first().is_some_and(|section| section == "profile") { 2 } else { 1 };
    path.iter().take(depth).cloned().collect()
}

/// Gather every leaf entry under `table`, and every header with its span (`[` to `]`) and path.
/// `header_start`, the start of the header governing `table`, stands in for a span the parser did
/// not keep.
fn collect(table: &toml_edit::Table,
           prefix: &[String],
           header_start: usize,
           found: &mut Vec<Found>,
           headers: &mut Vec<(Range<usize>, Vec<String>)>) {
    for (keys, value) in table.get_values() {
        let mut path = prefix.to_vec();
        path.extend(keys.iter().map(|key| key.get().to_string()));
        let start = keys.first().and_then(|key| key.span()).map_or(header_start, |span| span.start);
        collect_value(value, path, start, found);
    }

    for (name, item) in table.iter() {
        let mut path = prefix.to_vec();
        path.push(name.to_string());
        match item {
            toml_edit::Item::Table(sub) if !sub.is_dotted() => {
                let span = sub.span().unwrap_or(header_start..header_start);
                let start = match sub.is_implicit() {
                    true => header_start,
                    false => span.start,
                };
                if !sub.is_implicit() {
                    headers.push((span, path.clone()));
                }
                collect(sub, &path, start, found, headers);
            }
            // An array of tables is nothing a configuration file holds: each of its tables is a
            // dropped entry, so its section goes.
            toml_edit::Item::ArrayOfTables(array) => for sub in array.iter() {
                let span = sub.span().unwrap_or(header_start..header_start);
                let start = span.start;
                headers.push((span, path.clone()));
                found.push(Found { path: path.clone(), value: None, span: start..start + 1 });
            },
            _ => {}
        }
    }
}

/// Gather one value: a string is an entry; an inline table is read through for its own entries;
/// anything else is an entry no configuration file holds, so it is dropped.
fn collect_value(value: &toml_edit::Value, path: Vec<String>, start: usize, found: &mut Vec<Found>) {
    let end = value.span().map_or(start + 1, |span| span.end);
    match value {
        toml_edit::Value::InlineTable(inline) => for (keys, inner) in inline.get_values() {
            let mut inner_path = path.clone();
            inner_path.extend(keys.iter().map(|key| key.get().to_string()));
            // The whole inline table is one span from its key to its closing brace: damage
            // anywhere in it drops every entry of it.
            found.push(Found {
                path: inner_path,
                value: inner.as_str().map(str::to_string),
                span: start..end,
            });
        },
        _ => found.push(Found { path, value: value.as_str().map(str::to_string), span: start..end }),
    }
}

/// The 1-based line of byte offset `at`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

/// What the comment text after a damaged character, from `after` to the end of its line, may
/// hide: were that character a newline, the rest would be a line of its own.
///
/// # Returns
/// * `None`             - Nothing: the rest is no header and no entry.
/// * `Some(Some(key))`  - An entry, whose key (dotted, relative to its section) is `key`.
/// * `Some(None)`       - A header, or something no reading can tell, since the rest holds
///                        damage of its own.
fn hidden_line(text: &str, after: usize) -> Option<Option<Vec<String>>> {
    let rest = &text[after..];
    let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
    if rest.chars().any(is_damage) {
        return Some(None);
    }

    let line = rest.trim_start();
    if line.starts_with('[') {
        return Some(None);
    }
    let (key, _) = line.split_once('=')?;
    match toml_edit::Key::parse(key.trim()).ok().filter(|key| !key.is_empty()) {
        Some(key) => Some(Some(key.iter().map(|part| part.get().to_string()).collect())),
        // A quoted key may hold `=` itself, so the first `=` need not end the key: read the rest
        // as a document of one line, its value closed should it open a multi-line string.
        None => one_line_key(line).map(Some),
    }
}

/// The key of the entry `line` holds, read as a document of its own, when it holds one.
fn one_line_key(line: &str) -> Option<Vec<String>> {
    let document = parse_closed(&format!("{}\n", line))?;
    let (mut found, mut headers) = (Vec::new(), Vec::new());
    collect(document.as_table(), &[], 0, &mut found, &mut headers);
    found.into_iter().next().map(|entry| entry.path)
}

/// Whether `line` begins as an entry does: a key, then `=`.
fn looks_like_entry(line: &str) -> bool {
    line.split_once('=').is_some_and(|(key, _)| !key.trim().is_empty() && toml_edit::Key::parse(key.trim()).is_ok())
}

/// Best effort over text that does not parse: every line that still looks like an entry, named by
/// its key while the key holds no damage.
fn entry_lines(text: &str) -> Vec<DroppedEntry> {
    let mut section = String::new();
    let mut dropped = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if let Some(header) = trimmed.strip_prefix('[') {
            section = header.trim_start_matches('[').split(']').next().unwrap_or("").trim().to_string();
            continue;
        }
        if looks_like_entry(trimmed) {
            let key = trimmed.split_once('=').map(|(key, _)| key.trim()).unwrap_or("");
            let full = match section.is_empty() { true => key.to_string(), false => format!("{}.{}", section, key) };
            dropped.push(DroppedEntry {
                line: index + 1,
                key: (!full.chars().any(is_damage)).then_some(full),
            });
        }
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;

    fn any(_: &[String], _: &str) -> bool { true }

    fn keys(salvage: &Salvage) -> Vec<String> {
        salvage.kept.iter().map(|entry| entry.path.join(".")).collect()
    }

    const FILE: &str = "# Forklift warehouse configuration.\n\n[operator]\nidentifier = \"op-1\"\n\n\
                        [mirror]\npallets = \"main\"\n# the remote\n[remote]\nurl = \"https://remote.example\"\n\
                        token = \"secret\"\n";

    fn flip(at: usize) -> Vec<u8> {
        let mut bytes = FILE.as_bytes().to_vec();
        bytes[at] = !bytes[at];
        bytes
    }

    #[test]
    fn a_sound_file_keeps_everything_losslessly() {
        let salvage = salvage(FILE.as_bytes(), any);
        assert_eq!(keys(&salvage), ["operator.identifier", "mirror.pallets", "remote.url", "remote.token"]);
        assert!(salvage.lossless && salvage.dropped.is_empty());
    }

    /// Revert: make a hidden header drop nothing in hidden_line's caller → the URL is kept as
    /// mirror.url → red.
    #[test]
    fn a_swallowed_header_never_moves_an_entry_into_another_section() {
        let salvage = salvage(&flip(FILE.find("\n[remote]").unwrap()), any);
        assert!(!keys(&salvage).iter().any(|key| key == "mirror.url"), "{:?}", keys(&salvage));
        assert_eq!(keys(&salvage), ["operator.identifier"]);
        assert!(!salvage.lossless);
    }

    /// Revert: keep entries of a section that lost another → url kept beside a dropped token
    /// → red.
    #[test]
    fn a_section_that_loses_one_entry_loses_all() {
        let salvage = salvage(&flip(FILE.find("secret").unwrap()), any);
        assert_eq!(keys(&salvage), ["operator.identifier", "mirror.pallets"]);
        assert_eq!(salvage.dropped.iter().map(|d| d.key.clone()).collect::<Vec<_>>(),
                   [Some("remote.url".to_string()), Some("remote.token".to_string())]);
        assert!(!salvage.lossless);
    }

    /// A flipped byte in the comment block above every header hides nothing: the rest of its line
    /// is no header and no entry.
    #[test]
    fn damage_in_a_preamble_comment_is_lossless() {
        let salvage = salvage(&flip(FILE.find("warehouse").unwrap()), any);
        assert_eq!(keys(&salvage).len(), 4);
        assert!(salvage.lossless);
    }

    /// A hidden entry mid-file costs its whole section: the remote's URL is not kept without the
    /// `tor` setting that went with it. Revert: make hidden_line answer None → url kept → red.
    #[test]
    fn an_entry_hidden_mid_file_drops_its_section() {
        let text = "[remote]\nurl = \"https://private\"\n# t\ntor = \"on\"\n[operator]\nidentifier = \"x\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("\ntor").unwrap()] = 0xF5;
        let salvage = salvage(&bytes, any);
        assert_eq!(keys(&salvage), ["operator.identifier"]);
        assert!(salvage.dropped.contains(&DroppedEntry { line: 3, key: Some("remote.tor".to_string()) }),
                "{:?}", salvage.dropped);
        assert!(!salvage.lossless);
    }

    /// In the root no header says whose a hidden line is, so every section goes.
    #[test]
    fn a_header_hidden_in_the_root_drops_everything() {
        let text = "# top\n[remote]\nurl = \"u\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("\n[remote]").unwrap()] = 0;
        let salvage = salvage(&bytes, any);
        assert!(salvage.parsed && salvage.kept.is_empty(), "{:?}", salvage);
        // The url was read as a root entry only because the header was hidden: its key is unknown.
        // Revert: drop the misread rule → Some("url") → red.
        assert_eq!(salvage.dropped, [DroppedEntry { line: 1, key: None },
                                     DroppedEntry { line: 2, key: None }]);
    }

    /// Entries a hidden header governs are misread only up to the next real header, and only in
    /// the section the hidden header lies in.
    #[test]
    fn a_hidden_header_unnames_only_the_entries_it_governs() {
        let text = "[mirror]\npallets = \"main\"\n# x\n[remote]\nurl = \"u\"\n[operator]\nidentifier = \"o\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("\n[remote]").unwrap()] = 0xFF;
        let salvage = salvage(&bytes, any);
        assert_eq!(keys(&salvage), ["operator.identifier"]);
        assert_eq!(salvage.dropped, [DroppedEntry { line: 2, key: Some("mirror.pallets".to_string()) },
                                     DroppedEntry { line: 3, key: None },
                                     DroppedEntry { line: 4, key: None }]);
    }

    /// A key quoted around `=` is still a key. Revert: answer None when the split key does not
    /// parse → the remote is kept and the salvage reads as lossless → red.
    #[test]
    fn a_hidden_entry_with_a_quoted_equals_sign_drops_its_section() {
        let text = "[remote]\nurl = \"u\"\n# x\"a=b\" = \"v\"\n[operator]\nidentifier = \"o\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("x\"a=b").unwrap()] = 0xFF;
        let salvage = salvage(&bytes, any);
        assert_eq!(keys(&salvage), ["operator.identifier"]);
        assert!(salvage.dropped.contains(&DroppedEntry { line: 3, key: Some("remote.a=b".to_string()) }),
                "{:?}", salvage.dropped);
        assert!(!salvage.lossless);
    }

    /// Damage in a quoted header name leaves the entries under a name they never had. Revert:
    /// skip the header-span check → profile entries kept under the damaged name → red.
    #[test]
    fn damage_in_a_quoted_header_drops_its_section() {
        let text = "[profile.\"work\"]\nidentifier = \"w\"\n[operator]\nidentifier = \"o\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("ork").unwrap()] = 0xFF;
        let read = salvage(&bytes, any);
        assert_eq!(keys(&read), ["operator.identifier"]);
        assert!(!read.lossless);

        // A damaged header with no entries under it still costs the header, so it is not lossless.
        let text = "[operator]\nidentifier = \"o\"\n[\"remote\"]\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("mote").unwrap()] = 0xFF;
        let salvage = salvage(&bytes, any);
        assert_eq!(salvage.dropped, [DroppedEntry { line: 3, key: None }]);
        assert!(!salvage.lossless);
    }

    /// A cut inside a multi-line string still spares the sections above it. Revert: try no closer
    /// in parse_closed → the text does not parse → nothing kept → red.
    #[test]
    fn a_cut_inside_a_multi_line_string_keeps_the_sections_above() {
        let text = "[operator]\nidentifier = \"o\"\n[remote]\ntoken = \"\"\"\nline1\nlin";
        let salvage = salvage(text.as_bytes(), any);
        assert!(salvage.cut && salvage.parsed && !salvage.lossless, "{:?}", salvage);
        assert_eq!(keys(&salvage), ["operator.identifier"]);
        assert!(salvage.dropped.contains(&DroppedEntry { line: 4, key: Some("remote.token".to_string()) }),
                "{:?}", salvage.dropped);
    }

    /// With damage beside a cut, the closer could swallow later sections into a string whose real
    /// end the damage hid, so it is not tried. Revert: try the closer whenever cut → the operator
    /// vanishes into the token, named nowhere → red.
    #[test]
    fn a_cut_beside_damage_never_closes_a_string() {
        let text = "[remote]\ntoken = \"\"\"\nx\n\"\"\"\n[operator]\nidentifier = \"o\"\n[mirror]\npall";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("\"\"\"\n[operator]").unwrap() + 2] = 0xFF;
        let salvage = salvage(&bytes, any);
        assert!(!salvage.parsed && salvage.kept.is_empty(), "{:?}", salvage);
        assert!(salvage.dropped.iter().any(|entry| entry.key.as_deref() == Some("operator.identifier")),
                "{:?}", salvage.dropped);
    }

    /// A byte flipped to a carriage return is damage like any other. Revert: map only forbidden
    /// controls → the text does not parse → red.
    #[test]
    fn a_lone_carriage_return_is_damage() {
        let text = "[remote]\n# a\rb\nurl = \"u\"\r\n";
        let salvage = salvage(text.as_bytes(), any);
        assert_eq!(keys(&salvage), ["remote.url"]);
        assert!(salvage.lossless && salvage.text.contains("# a\u{FFFD}b\n") && salvage.text.ends_with("\r\n"),
                "{:?}", salvage);
    }

    /// Damage in a comment whose rest is plain words hides nothing, even between a header and
    /// its entries, and a zeroed byte reads like any other damage. Revert: check every damage
    /// against its governing section regardless of hidden_line → the remote goes → red.
    #[test]
    fn damage_that_hides_nothing_keeps_its_section() {
        let text = "[remote]\n# Mate's server\nurl = \"u\"\ntoken = \"t\"\n";
        for byte in [0xE9u8, 0] {
            let mut bytes = text.as_bytes().to_vec();
            bytes[text.find("'").unwrap()] = byte;
            let salvage = salvage(&bytes, any);
            assert_eq!(keys(&salvage), ["remote.url", "remote.token"], "{:#x}", byte);
            assert!(salvage.lossless && salvage.text.contains("# Mate\u{FFFD}s server"), "{:#x}", byte);
        }
    }

    /// Revert: skip the cut-section drop → url kept while the token was cut off → red.
    #[test]
    fn a_cut_drops_the_section_it_lands_in() {
        let at = FILE.find("token").unwrap() + 4;
        let salvage = salvage(&FILE.as_bytes()[..at], any);
        assert!(salvage.cut && !salvage.lossless);
        assert_eq!(keys(&salvage), ["operator.identifier", "mirror.pallets"]);
    }

    #[test]
    fn a_cut_header_fragment_keeps_the_section_above() {
        let at = FILE.find("[remote]").unwrap() + 4;
        let salvage = salvage(&FILE.as_bytes()[..at], any);
        assert_eq!(keys(&salvage), ["operator.identifier", "mirror.pallets"]);
        assert!(!salvage.lossless);
    }

    #[test]
    fn a_zeroed_file_keeps_nothing() {
        let salvage = salvage(&vec![0u8; FILE.len()], any);
        assert!(salvage.kept.is_empty() && !salvage.lossless);
    }

    #[test]
    fn an_unparsable_file_names_what_looks_like_an_entry() {
        let text = "[remote]\nurl = \"x\ntoken = \"y\"\n";
        let salvage = salvage(text.as_bytes(), any);
        assert!(!salvage.parsed && salvage.kept.is_empty());
        assert_eq!(salvage.dropped[0], DroppedEntry { line: 2, key: Some("remote.url".to_string()) });
    }

    /// A multi-line value forklift itself writes is one entry, kept whole.
    #[test]
    fn a_multi_line_value_is_one_entry() {
        let text = "[remote]\ntoken = \"\"\"\nline1\nline2\"\"\"\n";
        let salvage = salvage(text.as_bytes(), any);
        assert_eq!(salvage.kept, [KeptEntry { path: vec!["remote".into(), "token".into()], value: "line1\nline2".into() }]);
        assert!(salvage.lossless);
    }

    /// Revert: keep entries `valid` refuses → the unknown key survives → red.
    #[test]
    fn an_entry_that_does_not_validate_drops_its_section() {
        let salvage = salvage(FILE.as_bytes(), |path, _| path.join(".") != "remote.token");
        assert_eq!(keys(&salvage), ["operator.identifier", "mirror.pallets"]);
        assert!(!salvage.lossless);
    }

    #[test]
    fn a_profile_is_its_own_section() {
        let text = "[profile.work]\nidentifier = \"w\"\n[profile.home]\nidentifier = \"h\"\n";
        let salvage = salvage(text.as_bytes(), |path, _| path[1] != "home");
        assert_eq!(keys(&salvage), ["profile.work.identifier"]);
    }

    #[test]
    fn an_inline_table_is_read_through() {
        let text = "remote = { url = \"u\", token = \"t\" }\n";
        let salvage = salvage(text.as_bytes(), any);
        assert_eq!(keys(&salvage), ["remote.url", "remote.token"]);
        assert!(salvage.lossless);
    }

    /// Damage on the last line is judged like damage anywhere: by what the rest of its line hides.
    #[test]
    fn damage_on_the_last_line_is_judged_by_what_it_hides() {
        let text = "[operator]\nidentifier = \"op\"\n# end\n";
        let mut bytes = text.as_bytes().to_vec();
        let at = text.find("end").unwrap();
        bytes[at] = !bytes[at];
        assert!(salvage(&bytes, any).lossless);

        let text = "[operator]\nidentifier = \"op\"\n# xidentifier = \"other\"\n";
        let mut bytes = text.as_bytes().to_vec();
        bytes[text.find("xidentifier").unwrap()] = 0xFF;
        let read = salvage(&bytes, any);
        assert!(!read.lossless && read.kept.is_empty(), "{:?}", read);
    }
}
