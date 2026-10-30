//! Every production writer of a pallet ref, and why each keeps a take's known-verified set sound.
//!
//! A take judged against a held anchor skips the history of every local user-pallet head: each
//! was signed here or verified by a take, so its ancestry needs no second walk. That holds only
//! while every writer of a user-pallet ref does one of the things listed in [`Class`]. This test
//! finds every call of a ref-write primitive outside test code and fails when the set changes, so
//! a new writer is classified before it lands. The server and the Lambda head write the remote's
//! refs, not a warehouse's, and are not scanned.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Why a writer keeps the known-verified set sound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
    /// Writes a parcel it signed itself while an anchor is held.
    Signs,
    /// Writes a head this warehouse already held: another local pallet's, or one an operation it
    /// undoes replaced.
    CopiesLocal,
    /// Verifies the head against the held anchor and office, and writes under both.
    Verifies,
    /// Writes only while no anchor is held.
    RefusesWhenAnchored,
    /// Writes a meta pallet, which no take treats as verified.
    Meta,
    /// A primitive whose own callers are listed.
    Primitive,
}

/// The ref-write primitives: a call of any of these writes a pallet ref.
const PRIMITIVES: &[&str] = &[
    "update_pallet_head",
    "update_pallet_head_in",
    "update_pallet_head_verified",
    "update_pallet_head_locked",
    "write_head_or_roll_back",
    "recreate_pallet_head",
];

/// Every production writer: the file, the function holding the call, and why it is sound. A
/// function holding several calls is listed once per call.
const WRITERS: &[(&str, &str, Class)] = &[
    ("crates/forklift-core/src/util/deliver_utils.rs", "deliver", Class::Signs),
    // An undone delete's head is verified as the anchor and office stand now.
    ("crates/forklift-core/src/util/journal_utils.rs", "create_deleted", Class::Verifies),
    ("crates/forklift-core/src/util/journal_utils.rs", "put_back_refs", Class::CopiesLocal),
    ("crates/forklift-core/src/util/journal_utils.rs", "un_restore_current", Class::CopiesLocal),
    ("crates/forklift-core/src/util/office_utils.rs", "write_trust_install", Class::Meta),
    // A lower rolled forward under the preconditions its take was verified against, or a
    // consolidation's fast-forward to a local head.
    ("crates/forklift-core/src/util/operation_utils.rs", "recover_operation", Class::Verifies),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_pallet_head", Class::Primitive),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_meta_pallet_head", Class::Meta),
    ("crates/forklift-core/src/util/pallet_utils.rs", "update_pallet_head_checked", Class::Primitive),
    ("crates/forklift-core/src/util/shift_utils.rs", "write_head_or_roll_back", Class::Primitive),
    // Signed whenever an anchor is held; a consolidation's merged head is verified first.
    ("crates/forklift-core/src/util/stack_utils.rs", "write_head", Class::Signs),
    // A restored tombstone's head is verified as the anchor and office stand now, as undo's is.
    ("crates/forklift-core/src/util/tombstone_utils.rs", "restore_pallet", Class::Verifies),
    ("crates/forklift/src/commands/bay.rs", "add", Class::CopiesLocal),
    ("crates/forklift/src/commands/consolidate.rs", "fast_forward", Class::CopiesLocal),
    ("crates/forklift/src/commands/clone.rs", "franchise_into_prepared_target", Class::Verifies),
    ("crates/forklift/src/commands/import_git.rs", "handle_command", Class::RefusesWhenAnchored),
    ("crates/forklift/src/commands/lower.rs", "lower_working_pallet", Class::Verifies),
    ("crates/forklift/src/commands/lower.rs", "lower_named_pallet", Class::Verifies),
    // At a hash, verified; at another pallet, that pallet's head.
    ("crates/forklift/src/commands/pallet.rs", "create_pallet", Class::Verifies),
    ("crates/forklift/src/commands/pallet.rs", "create_pallet", Class::CopiesLocal),
];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().parent().unwrap().to_path_buf()
}

fn rust_files(dir: &Path, files: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir).unwrap().map(|entry| entry.unwrap().path()).collect();
    entries.sort();

    for path in entries {
        if path.is_dir() {
            rust_files(&path, files);
        } else if path.extension().is_some_and(|extension| extension == "rs") {
            files.push(path);
        }
    }
}

/// Whether `line` calls `primitive`: the name, not part of a longer identifier, followed by `(`.
fn calls(line: &str, primitive: &str) -> bool {
    let call = format!("{}(", primitive);

    line.match_indices(&call).any(|(at, _)| {
        !line[..at].ends_with(|c: char| c.is_alphanumeric() || c == '_') && !line[..at].ends_with("fn ")
    })
}

/// The name of the function a line at `index` sits in: the nearest `fn` above it.
fn enclosing_fn(lines: &[&str], index: usize) -> String {
    lines[..=index].iter().rev()
        .find_map(|line| {
            let (_, rest) = line.split_once("fn ")?;
            let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
            (!name.is_empty() && line.trim_start().split("fn ").next().unwrap().split_whitespace()
                .all(|word| ["pub", "pub(crate)", "async", "unsafe", "const"].contains(&word))).then_some(name)
        })
        .unwrap_or_default()
}

/// Every call of a ref-write primitive outside test code, as (file, function), once per call.
fn found_writers() -> Vec<(String, String)> {
    let root = workspace_root();
    let mut files = Vec::new();
    rust_files(&root.join("crates/forklift/src"), &mut files);
    rust_files(&root.join("crates/forklift-core/src"), &mut files);

    let mut writers = Vec::new();
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let lines: Vec<&str> = text.lines().collect();

        // A file's test module comes last; nothing from it on is production code.
        let end = (0..lines.len())
            .find(|&i| lines[i].trim() == "#[cfg(test)]" && lines.get(i + 1).is_some_and(|next| {
                let next = next.trim_start();
                next.starts_with("mod ") || next.starts_with("pub mod ") || next.starts_with("pub(crate) mod ")
            }))
            .unwrap_or(lines.len());

        let relative = file.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
        for index in 0..end {
            let line = lines[index];
            if line.trim_start().starts_with("//") {
                continue;
            }

            for primitive in PRIMITIVES {
                if calls(line, primitive) {
                    writers.push((relative.clone(), enclosing_fn(&lines, index)));
                }
            }
        }
    }

    writers.sort();
    writers
}

fn counted<'a>(writers: impl Iterator<Item = (&'a str, &'a str)>) -> BTreeMap<(String, String), usize> {
    let mut counts = BTreeMap::new();
    for (file, function) in writers {
        *counts.entry((file.to_string(), function.to_string())).or_insert(0) += 1;
    }
    counts
}

/// The production writers of a pallet ref are exactly the classified ones. Revert: add a call of
/// `update_pallet_head` to any production function (or remove one), and this fails naming it.
#[test]
fn every_pallet_ref_writer_is_classified() {
    let found = found_writers();
    let found = counted(found.iter().map(|(file, function)| (file.as_str(), function.as_str())));
    let expected = counted(WRITERS.iter().map(|(file, function, _)| (*file, *function)));

    let unlisted: Vec<_> = found.iter()
        .filter(|(key, count)| expected.get(*key) != Some(count))
        .map(|((file, function), count)| format!("{} {} ({} call(s))", file, function, count))
        .collect();
    let gone: Vec<_> = expected.iter()
        .filter(|(key, count)| found.get(*key) != Some(count))
        .map(|((file, function), count)| format!("{} {} ({} listed)", file, function, count))
        .collect();

    assert!(
        unlisted.is_empty() && gone.is_empty(),
        "The writers of a pallet ref changed. Classify each new one in WRITERS: a take skips the \
        history of every local user-pallet head, which is sound only while each writer signs, \
        copies a local head, verifies, or refuses while an anchor is held.\nFound but not listed: \
        {:#?}\nListed but not found: {:#?}",
        unlisted, gone
    );
}

/// The scan finds a call however it is spelled, and skips definitions, comments and test code.
#[test]
fn the_scan_finds_calls_and_skips_definitions() {
    assert!(calls("    pallet_utils::update_pallet_head(name, None, &head, &none)?;", "update_pallet_head"));
    assert!(calls("let x = update_pallet_head(a)", "update_pallet_head"));
    assert!(!calls("pub fn update_pallet_head(name: &str,", "update_pallet_head"));
    assert!(!calls("    update_meta_pallet_head(name)", "update_pallet_head"));
    assert!(!calls("    update_pallet_head_in(name)", "update_pallet_head"));

    let lines = ["pub(crate) async fn outer() {", "    let f = |x| x;", "    update_pallet_head(a);", "}"];
    assert_eq!(enclosing_fn(&lines, 2), "outer");
}
