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
//! So an entry is kept only when its own text holds no damaged character, no damaged character
//! lies between it and the header that governs it (a header can only vanish at one), and it
//! validates. A section that loses one entry loses all of them: every key resolves on its own,
//! warehouse first, so a kept `remote.url` beside a dropped `remote.token` would hand the global
//! file's token to the warehouse's remote.

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
}

/// Whether `c` is a character TOML never allows anywhere in a document: a control character
/// other than tab, line feed and carriage return. Zeroed bytes read as these.
pub fn is_forbidden_control(c: char) -> bool {
    (c < ' ' && c != '\t' && c != '\n' && c != '\r') || c == '\u{7f}'
}

/// Whether `c` marks damage in a lossily decoded file: the replacement character a byte that was
/// not UTF-8 decodes to, or a control character TOML never allows.
fn is_damage(c: char) -> bool {
    c == char::REPLACEMENT_CHARACTER || is_forbidden_control(c)
}

/// One entry found in the parsed text, before the verdict on it.
struct Found {
    path: Vec<String>,
    value: Option<String>,
    span: Range<usize>,
    header_start: usize,
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

    let text = String::from_utf8_lossy(body).into_owned();
    let damage: Vec<Range<usize>> = text.char_indices()
        .filter(|&(_, c)| is_damage(c))
        .map(|(at, c)| at..at + c.len_utf8())
        .collect();

    let Ok(document) = toml_edit::Document::parse(text.clone()) else {
        return Salvage { kept: Vec::new(), dropped: entry_lines(&text), cut, parsed: false, lossless: false };
    };

    let mut found = Vec::new();
    let mut headers = Vec::new();
    collect(document.as_table(), &mut Vec::new(), 0, &mut found, &mut headers);
    found.sort_by_key(|entry| entry.span.start);

    let damaged = |range: Range<usize>| damage.iter().any(|at| at.start < range.end && range.start < at.end);

    let mut dropped_groups: Vec<Vec<String>> = Vec::new();
    let mut verdicts = Vec::with_capacity(found.len());
    for entry in &found {
        let keep = !damaged(entry.header_start..entry.span.end)
            && entry.value.as_deref().is_some_and(|value| valid(&entry.path, value));
        if !keep && !dropped_groups.contains(&group(&entry.path)) {
            dropped_groups.push(group(&entry.path));
        }
        verdicts.push(keep);
    }

    // The cut line belongs to the section the last header still visible governs, and that
    // section may have run on past the cut: it goes whole. A cut header fragment ends the
    // section above it, so that one is complete.
    let fragment = partial.is_some_and(|line| String::from_utf8_lossy(line).trim_start().starts_with('['));
    let mut cut_dropped = false;
    if cut && !fragment {
        match headers.iter().max_by_key(|(start, _)| *start) {
            Some((_, path)) => {
                let path: &Vec<String> = path;
                if !dropped_groups.contains(&group(path)) {
                    dropped_groups.push(group(path));
                }
            }
            // No header at all: the cut lies in the root, where any entry may be of any section.
            None => for entry in &found {
                if !dropped_groups.contains(&group(&entry.path)) {
                    dropped_groups.push(group(&entry.path));
                }
            },
        }
        cut_dropped = true;
    }

    let mut kept = Vec::new();
    let mut dropped = Vec::new();
    for (entry, keep) in found.iter().zip(verdicts) {
        if keep && !dropped_groups.contains(&group(&entry.path)) {
            kept.push(KeptEntry { path: entry.path.clone(), value: entry.value.clone().unwrap_or_default() });
        } else {
            let key = entry.path.join(".");
            dropped.push(DroppedEntry {
                line: line_of(&text, entry.span.start),
                key: (!key.chars().any(is_damage)).then_some(key),
            });
        }
    }

    let lossless = !cut && !cut_dropped && dropped.is_empty()
        && !damage.iter().any(|at| on_last_line(&text, at.start))
        && !damage.iter().any(|at| {
            !found.iter().any(|entry| entry.span.contains(&at.start)) && may_hide_a_line(&text, at.end)
        });

    Salvage { kept, dropped, cut, parsed: true, lossless }
}

/// The section an entry's path belongs to, the unit the salvage keeps or drops whole: a top-level
/// section, or one named profile.
fn group(path: &[String]) -> Vec<String> {
    let depth = if path.first().is_some_and(|section| section == "profile") { 2 } else { 1 };
    path.iter().take(depth).cloned().collect()
}

/// Gather every leaf entry under `table`, with the start of the header that governs it, and every
/// header with its path.
fn collect(table: &toml_edit::Table,
           prefix: &mut Vec<String>,
           header_start: usize,
           found: &mut Vec<Found>,
           headers: &mut Vec<(usize, Vec<String>)>) {
    for (keys, value) in table.get_values() {
        let mut path = prefix.clone();
        path.extend(keys.iter().map(|key| key.get().to_string()));
        let start = keys.first().and_then(|key| key.span()).map_or(header_start, |span| span.start);
        collect_value(value, path, start, header_start, found);
    }

    for (name, item) in table.iter() {
        let mut path = prefix.clone();
        path.push(name.to_string());
        match item {
            toml_edit::Item::Table(sub) if !sub.is_dotted() => {
                let start = match sub.is_implicit() {
                    true => header_start,
                    false => sub.span().map_or(header_start, |span| span.start),
                };
                if !sub.is_implicit() {
                    headers.push((start, path.clone()));
                }
                collect(sub, &mut path, start, found, headers);
            }
            // An array of tables is nothing a configuration file holds: each of its tables is a
            // dropped entry, so its section goes.
            toml_edit::Item::ArrayOfTables(array) => for sub in array.iter() {
                let start = sub.span().map_or(header_start, |span| span.start);
                headers.push((start, path.clone()));
                found.push(Found { path: path.clone(), value: None, span: start..start + 1, header_start: start });
            },
            _ => {}
        }
    }
}

/// Gather one value: a string is an entry; an inline table is read through for its own entries;
/// anything else is an entry no configuration file holds, so it is dropped.
fn collect_value(value: &toml_edit::Value, path: Vec<String>, start: usize, header_start: usize, found: &mut Vec<Found>) {
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
                header_start,
            });
        },
        _ => found.push(Found { path, value: value.as_str().map(str::to_string), span: start..end, header_start }),
    }
}

/// The 1-based line of byte offset `at`.
fn line_of(text: &str, at: usize) -> usize {
    text[..at.min(text.len())].matches('\n').count() + 1
}

/// Whether byte offset `at` lies on the text's last line, the one its final newline ends.
fn on_last_line(text: &str, at: usize) -> bool {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    trimmed.rfind('\n').map_or(true, |last| at > last)
}

/// Whether the comment text after a damaged character, from `after` to the end of its line, may
/// be a line the damage hid: were that character a newline, the rest would be a line of its own,
/// and a header or an entry there is lost without a dropped entry to show for it. Another damaged
/// character in the rest counts as may: nothing can be said of it.
fn may_hide_a_line(text: &str, after: usize) -> bool {
    let rest = &text[after..];
    let rest = &rest[..rest.find('\n').unwrap_or(rest.len())];
    if rest.chars().any(is_damage) {
        return true;
    }

    let line = rest.trim_start();
    line.starts_with('[') || looks_like_entry(line)
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

    /// Revert: drop the header-to-entry range from the damage test (check only the entry's own
    /// span) → the URL is kept as mirror.url → red.
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

    /// Revert: make may_hide_a_line answer false → a comment that may have been an entry reads
    /// as lossless → red.
    #[test]
    fn damage_that_may_have_hidden_an_entry_is_not_lossless() {
        let text = "# note\n[operator]\nidentifier = \"op-1\"\n# x\nprofile = \"work\"\n";
        let mut bytes = text.as_bytes().to_vec();
        let at = text.find("\nprofile").unwrap();
        bytes[at] = !bytes[at];
        let salvage = salvage(&bytes, any);
        assert!(!salvage.lossless, "{:?}", salvage);
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

    #[test]
    fn damage_on_the_last_line_is_not_lossless() {
        let text = "[operator]\nidentifier = \"op\"\n# end\n";
        let mut bytes = text.as_bytes().to_vec();
        let at = text.find("end").unwrap();
        bytes[at] = !bytes[at];
        assert!(!salvage(&bytes, any).lossless);
    }
}
