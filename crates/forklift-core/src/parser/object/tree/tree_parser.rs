use crate::enums::dir_entry_type::DirEntryType;
use crate::enums::object::tree_version::TreeVersion;
use crate::model::tree_item::TreeItem;
use crate::util::{byte_utils, object_utils};

/// Parse a tree object.
///
/// Every entry is validated here, once, for every format version: a tree may have arrived from a
/// remote, and checkout joins its entry names into filesystem paths, so no consumer may see a
/// name that could step outside its directory (see [`check_entry_name`]), an entry hash that is
/// not an object hash, or two entries sharing a name.
///
/// # Arguments
/// * `offset`  - The offset to start parsing at. This should be the byte *after*
/// the header section.
/// * `content` - The bytes of the tree object.
///
/// # Returns
/// * `Ok(TreeItem)`  - The parsed tree object.
/// * `Err(String)`   - The error message.
pub fn parse_tree(offset: usize, content: &[u8]) -> Result<TreeItem, String> {
    let mut cursor = 0;

    let version_code = byte_utils::number_from_vlq_bytes(offset, content)
        .map(|(value, bytes_read)| {
            cursor += bytes_read;
            value
        }).map_err(|e| format!("Failed to parse version code: {}", e))?;

    let version = TreeVersion::from_code(version_code)?;
    let entries = version.get_parser()(offset + cursor, content)?;

    let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
    let mut names = std::collections::HashSet::with_capacity(entries.len());

    for entry in entries {
        check_entry_name(&entry.name)?;

        // Debug-quoted, so the refusal shows the bad value with any hidden character escaped.
        if !object_utils::is_object_hash(&entry.hash) {
            return Err(format!("The tree entry {:?} has hash {:?}, which is not an object hash.", entry.name, entry.hash));
        }

        // Files and subtrees are kept apart, so a repeated name would otherwise survive as one of
        // each, or silently replace the earlier entry.
        if !names.insert(entry.name.clone()) {
            return Err(format!("The tree has more than one entry named {:?}.", entry.name));
        }

        tree.add_child(entry);
    }

    Ok(tree)
}

/// Refuse a tree entry name that is not a single path component: the empty name, `.`, `..`, and
/// any name containing `/` or NUL. Checkout joins names with `/` under the working directory, so
/// each of these would name a different place than the entry claims, including one outside it.
/// A file system walk never produces such a name, so a valid tree never carries one.
///
/// Other characters, control characters included, are legal: file names may hold them, and every
/// human render sanitizes them.
///
/// # Arguments
/// * `name` - The entry name.
///
/// # Returns
/// * `Ok(())`      - If the name is a single path component.
/// * `Err(String)` - The refusal, naming the entry Debug-quoted.
pub fn check_entry_name(name: &str) -> Result<(), String> {
    let reason = if name.is_empty() {
        Some("is empty")
    } else if name == "." || name == ".." {
        Some("names a directory step, not an entry")
    } else if name.contains('/') {
        Some("contains a path separator")
    } else if name.contains('\0') {
        Some("contains a NUL character")
    } else {
        None
    };

    match reason {
        Some(reason) => Err(format!("The tree entry name {:?} {}, so it is refused.", name, reason)),
        None => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::object::tree::tree_object_builder::TreeObjectBuilder;
    use crate::enums::dir_entry_type::DirEntryType;

    #[test]
    fn round_trip_preserves_entries_and_hostile_names() {
        let hash = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("normal.txt".to_string(), hash.to_string(), DirEntryType::Normal));
        // Names containing new line and EOT bytes must survive the round trip.
        tree.add_child(TreeItem::new("with\nnewline".to_string(), hash.to_string(), DirEntryType::Executable));
        tree.add_child(TreeItem::new("with\u{3}end-of-text".to_string(), hash.to_string(), DirEntryType::Normal));
        tree.add_child(TreeItem::new("subdir".to_string(), hash.to_string(), DirEntryType::Tree));

        let object = TreeObjectBuilder::build(&tree);
        let parsed = parse_tree(0, &object.content).unwrap();

        assert_eq!(parsed.get_files().count(), 3);
        assert_eq!(parsed.get_subtrees().count(), 1);

        for (name, item) in parsed.get_files().chain(parsed.get_subtrees()) {
            assert_eq!(item.hash, hash, "hash mismatch for entry {:?}", name);
        }

        let file_names: Vec<&String> = parsed.get_files().map(|(name, _)| name).collect();
        assert!(file_names.iter().any(|n| *n == "with\nnewline"));
        assert!(file_names.iter().any(|n| *n == "with\u{3}end-of-text"));
    }

    #[test]
    fn chunked_entry_types_round_trip_through_a_tree() {
        // A tree carrying the chunked entry types (codes 5/6) parses back to exactly those types.
        let hash = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("big.bin".to_string(), hash.to_string(), DirEntryType::NormalChunked));
        tree.add_child(TreeItem::new("run.sh".to_string(), hash.to_string(), DirEntryType::ExecutableChunked));

        let object = TreeObjectBuilder::build(&tree);
        let parsed = parse_tree(0, &object.content).unwrap();

        let types: std::collections::BTreeMap<String, DirEntryType> = parsed.get_files()
            .map(|(name, item)| (name.clone(), item.item_type))
            .collect();
        assert_eq!(types["big.bin"], DirEntryType::NormalChunked);
        assert_eq!(types["run.sh"], DirEntryType::ExecutableChunked);
    }

    #[test]
    fn an_unknown_entry_type_code_fails_the_whole_tree_parse() {
        // Old-binary behaviour can't be run directly, but this pins the mechanism that protects
        // it: a tree object whose entry carries a type code this build does not know (7 here)
        // fails to parse *entirely* — the loud failure the B1 fix relies on, so a pre-chunk
        // binary meeting a chunked (code 5/6) entry refuses the whole tree rather than
        // mis-reading it. Codes 5 and 6 are the reserved chunked types; 7 stands in for
        // "unknown to this build" the way 5/6 read to a pre-chunk build.
        let hash = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";

        // Build a valid tree, then rewrite the single entry's leading type code to 7. The latest
        // tree format is `[version][type_vlq][name_len_vlq][name][hash][NL]` per entry, so the
        // type code is the first byte after the version VLQ.
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        tree.add_child(TreeItem::new("f".to_string(), hash.to_string(), DirEntryType::Normal));
        let mut bytes = TreeObjectBuilder::build(&tree).content;

        // The version is a single-byte VLQ; the next byte is the entry type code — set it to 7.
        let type_index = 1;
        assert_eq!(bytes[type_index], DirEntryType::Normal.get_code() as u8, "sanity: found the type byte");
        bytes[type_index] = 7;

        assert!(parse_tree(0, &bytes).is_err(), "an unknown entry type code must fail the parse");
    }

    /// A tree holding `entries` (name, hash, type), as raw object bytes. The
    /// builder writes whatever it is given, which is how a hostile peer would make one.
    fn tree_bytes(entries: &[(&str, &str, DirEntryType)]) -> Vec<u8> {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash, item_type) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
        }

        TreeObjectBuilder::build(&tree).content
    }

    const HASH: &str = "9028a15ad613bcd9853a3e780cfe3c78361b56ce95a2430484ba75ade5198cdc";

    #[test]
    fn names_that_are_not_one_path_component_are_refused() {
        for name in ["", ".", "..", "../x", "a/b", "/abs", "a\0b"] {
            for item_type in [DirEntryType::Normal, DirEntryType::Tree] {
                let error = parse_tree(0, &tree_bytes(&[(name, HASH, item_type)]))
                    .err()
                    .unwrap_or_else(|| panic!("{:?} ({:?}) must be refused", name, item_type));

                assert!(error.contains(&format!("{:?}", name)), "the refusal names the entry: {}", error);
            }
        }
    }

    #[test]
    fn ordinary_and_unusual_names_are_accepted() {
        // Dots inside a name, a leading dot, non-ASCII, and control characters (legal in file
        // names; renders sanitize them) all stay valid.
        for name in ["a.txt", ".hidden", "...", "..a", "café", "with\nnewline", "tab\there", "x\u{1f}y"] {
            let parsed = parse_tree(0, &tree_bytes(&[(name, HASH, DirEntryType::Normal)]))
                .unwrap_or_else(|e| panic!("{:?} must be accepted: {}", name, e));

            assert_eq!(parsed.get_files().next().map(|(n, _)| n.as_str()), Some(name));
        }
    }

    #[test]
    fn an_entry_hash_that_is_not_an_object_hash_is_refused() {
        let bad = "../../outside";
        let error = parse_tree(0, &tree_bytes(&[("a.txt", bad, DirEntryType::Normal)])).err().expect("the tree must be refused");

        assert!(error.contains(&format!("{:?}", bad)), "the refusal names the hash: {}", error);
        assert!(error.contains("\"a.txt\""), "the refusal names the entry: {}", error);
    }

    #[test]
    fn a_file_and_a_subtree_sharing_a_name_are_refused() {
        let bytes = tree_bytes(&[("x", HASH, DirEntryType::Normal), ("x", HASH, DirEntryType::Tree)]);
        let error = parse_tree(0, &bytes).err().expect("the tree must be refused");

        assert!(error.contains("more than one entry named \"x\""), "{}", error);

        // The positive twin: two different names parse.
        let bytes = tree_bytes(&[("x", HASH, DirEntryType::Normal), ("y", HASH, DirEntryType::Tree)]);
        assert!(parse_tree(0, &bytes).is_ok());
    }
}
