//! Content a host withholds (FORK-359): a legal takedown, the host's act rather than the
//! warehouse's.
//!
//! A redaction is recorded by the warehouse's admins, signed, on `@redactions`, and applies only
//! once nothing names its content. A takedown is neither: the operator of the head lists a file's
//! content hash, with a statement saying why, and the head stops serving and taking it at once,
//! whatever names it. Heads keep naming the file: a takedown adds no check on what a ref write may
//! name, and relaxes one, since the head tolerates the absence of withheld content where a file
//! names it ([`sealed_utils::Tolerated`](crate::util::sealed_utils::Tolerated)). So the upload
//! negotiation answers it as withheld rather than asking for it. The handshake's withheld list
//! publishes it, so a client can tell the content is unavailable from that remote rather than
//! damaged or redacted; no client reads the list yet. A served head deletes its copy as it deletes
//! an applied redaction's ([`redaction_deleter`](crate::util::redaction_deleter)), under a census
//! that history never holds a copy for.
//!
//! (The chunks a redaction's deletion serves as absent are a different thing, named "withheld" in
//! [`redaction_deleter`](crate::util::redaction_deleter) only for how they are served.)
//!
//! The list is head-local configuration, not history: the record at `<forklift root>/withheld`
//! lists each withheld hash beside its statement, in the [`record_utils`] form, one
//! `<hash> <statement>` entry per line, sorted. It is written whole and atomically under the
//! [`RefLock`], re-read under that lock first, so two writers never drop each other's entries. A
//! record this build cannot read, of another version or with an entry that is not a hash and a
//! statement, fails every read and is never written over: withheld content must never read as
//! served.

use std::collections::BTreeMap;
use std::path::PathBuf;
use crate::globals;
use crate::util::applied_redactions::{self, AppliedSet};
use crate::util::lock_utils::RefLock;
use crate::enums::object_type::ObjectType;
use crate::util::{bundle_utils, file_utils, object_utils, record_utils, redaction_utils};

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_WITHHELD: &str = "withheld";
const WITHHELD_RECORD: &str = "withheld";
const WITHHELD_VERSION: u32 = 1;

/// The longest statement a takedown may carry, in characters: every handshake carries it.
pub const STATEMENT_MAX_CHARS: usize = 1000;

fn withheld_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_WITHHELD)
}

/// Every hash this head withholds, each beside the operator's statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WithheldSet {
    statements: BTreeMap<String, String>,
}

impl WithheldSet {
    /// Whether `hash` is withheld.
    pub fn contains(&self, hash: &str) -> bool {
        self.statements.contains_key(hash)
    }

    /// The statement `hash` is withheld with, if it is.
    pub fn statement(&self, hash: &str) -> Option<&str> {
        self.statements.get(hash).map(String::as_str)
    }

    /// Whether nothing is withheld.
    pub fn is_empty(&self) -> bool {
        self.statements.is_empty()
    }

    /// Every withheld hash beside its statement, sorted by hash.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &String)> {
        self.statements.iter()
    }
}

/// Read the withheld record. No record reads as nothing withheld.
///
/// # Returns
/// * `Ok(WithheldSet)` - The withheld hashes and their statements.
/// * `Err(String)`     - The record exists but cannot be read, is of a version this build does not
///                       know, or holds an entry that is not a hash and a statement.
pub fn read_withheld() -> Result<WithheldSet, String> {
    let path = withheld_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(WithheldSet::default()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let entries = record_utils::from_bytes(&bytes, WITHHELD_RECORD, WITHHELD_VERSION).map_err(|e| format!(
        "The record of withheld content \"{}\" {} This version of Forklift never writes over it.",
        path.to_string_lossy(), e.describe()
    ))?;

    let mut set = WithheldSet::default();

    for entry in entries {
        match entry.split_once(' ') {
            Some((hash, statement)) if object_utils::is_object_hash(hash) && valid_statement(statement) => {
                set.statements.insert(hash.to_string(), statement.to_string());
            }
            _ => return Err(format!(
                "The record of withheld content \"{}\" holds an entry that is not an object hash and \
                a statement: \"{}\". This version of Forklift never writes over it.",
                path.to_string_lossy(), entry.escape_debug()
            )),
        }
    }

    Ok(set)
}

/// Whether `statement` may be a takedown's: not blank, at most [`STATEMENT_MAX_CHARS`] long, and
/// one plain line, since every terminal that shows a refusal prints it: no control character, line
/// or paragraph separator, or direction mark, override or isolate, which would reorder its display.
fn valid_statement(statement: &str) -> bool {
    let plain = |c: char| !c.is_control() && !matches!(
        c, '\u{2028}' | '\u{2029}' | '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
    );

    !statement.trim().is_empty() && statement.chars().count() <= STATEMENT_MAX_CHARS && statement.chars().all(plain)
}

/// The content this warehouse never serves or takes: what a redaction applied here names, and
/// what its head withholds. A walk that must leave out both, such as the bundle builder's, reads
/// this one set.
///
/// # Returns
/// * `Ok(AppliedSet)` - Both, as one set of hashes.
/// * `Err(String)`    - Either record cannot be read.
pub fn removed() -> Result<AppliedSet, String> {
    let applied = applied_redactions::read_applied()?;
    let withheld = read_withheld()?;

    Ok(AppliedSet::of(applied.hashes().cloned().chain(withheld.statements.into_keys())))
}

/// What [`withhold`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withheld {
    /// The hash was not withheld, and now is.
    Added,

    /// The hash was withheld already; the statement now is the one given.
    Restated,

    /// The hash was withheld already, with this statement; nothing was written.
    Unchanged,
}

/// Withhold `hash`, the content of a file, with the operator's `statement`.
///
/// Under the ref lock, before the record is written, the warehouse's bundle is withdrawn: it may
/// hold the content, and a build publishes only under that lock and only when nothing was
/// withheld or redacted since it began ([`bundle_utils::build_bundle`]). So no bundle holding
/// withheld content is published once the record is; a statement restated leaves the bundle, which
/// already leaves the content out. A hash the store holds as a tree, a parcel or a chunk is
/// refused: a takedown removes one file's content, as a redaction does. So is a chunked file's
/// recipe, for now: its chunks would still be served. One the store lacks is withheld all the
/// same, so it is never taken; should it be a chunked file's recipe, nothing here can tell, and its
/// chunks are still taken and served, as they are for a recipe uploaded while this runs (an
/// upload takes no ref lock).
///
/// # Returns
/// * `Ok(Withheld)` - What changed.
/// * `Err(String)`  - `hash` is not an object hash, or the store holds it as other than a file's
///                    unchunked content, `statement` is not one plain line of at most
///                    [`STATEMENT_MAX_CHARS`], the record cannot be read, or the bundle could not
///                    be withdrawn or the record written. A write that failed after its rename
///                    has still landed: read the record back to learn which.
pub fn withhold(hash: &str, statement: &str) -> Result<Withheld, String> {
    if !object_utils::is_object_hash(hash) {
        return Err(format!("\"{}\" is not an object hash.", hash));
    }

    let statement = statement.trim();

    if !valid_statement(statement) {
        return Err(format!(
            "The statement must be one plain line, not blank, with no control character, separator \
            or direction mark, and at most {} characters long.", STATEMENT_MAX_CHARS
        ));
    }

    match redaction_utils::stored_type(hash)? {
        None | Some(ObjectType::Blob) => {}
        Some(ObjectType::Recipe) => return Err(format!(
            "Object {} is a chunked file's content. Withholding one is not supported yet: its chunks \
            would still be served.", hash
        )),
        Some(other) => return Err(format!(
            "Object {} is a {:?}, not a file's content: a takedown withholds one file's content.", hash, other
        )),
    }

    let _lock = RefLock::acquire()?;
    let mut set = read_withheld()?;

    let outcome = match set.statement(hash) {
        Some(held) if held == statement => return Ok(Withheld::Unchanged),
        Some(_) => Withheld::Restated,
        None => Withheld::Added,
    };

    if outcome == Withheld::Added {
        file_utils::remove_unfollowed(&bundle_utils::get_latest_bundle_path()).map_err(|e| format!(
            "Cannot withhold the content: the bundle, which may hold it, could not be withdrawn. {}", e
        ))?;
    }

    set.statements.insert(hash.to_string(), statement.to_string());
    let entries = set.statements.iter().map(|(hash, statement)| format!("{} {}", hash, statement));
    let bytes = record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, entries);
    file_utils::replace_file_atomically(&withheld_path(), &bytes)?;

    Ok(outcome)
}

/// The [`RefusalCode::ContentWithheld`] refusal a server answers a request for `hash` with, read
/// or upload, while its head withholds it, carrying the operator's `statement`.
///
/// [`RefusalCode::ContentWithheld`]: crate::error::RefusalCode::ContentWithheld
pub fn served_refusal(hash: &str, statement: &str) -> String {
    crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentWithheld,
        format!(
            "Object {} is withheld by the host of this remote: it is not served, and cannot be \
            uploaded. The host's statement: {}",
            hash, statement
        ),
        "Keep any copy you hold; this remote cannot supply the content.",
        serde_json::json!({ "hash": hash, "statement": statement }),
    ).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    struct Scratch {
        root: PathBuf,
        _scope: StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-withheld-test-{}-{}", name, std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    // Revert: drop the re-read under the lock → the second withhold loses the first.
    #[test]
    fn withholding_keeps_every_entry_and_its_statement() {
        let _scratch = Scratch::new("keeps");
        assert!(read_withheld().unwrap().is_empty());

        assert_eq!(withhold(A, "DMCA notice 1 from X").unwrap(), Withheld::Added);
        assert_eq!(withhold(B, "  court order 7  ").unwrap(), Withheld::Added);
        assert_eq!(withhold(B, "court order 7").unwrap(), Withheld::Unchanged);
        assert_eq!(withhold(A, "DMCA notice 2").unwrap(), Withheld::Restated);

        let set = read_withheld().unwrap();
        assert_eq!(set.statement(A), Some("DMCA notice 2"));
        assert_eq!(set.statement(B), Some("court order 7"));
        assert_eq!(set.entries().count(), 2);

        let removed = removed().unwrap();
        assert!(removed.contains(A) && removed.contains(B));
    }

    // Revert: drop a validity check → the blank, the overlong or the non-hash is recorded.
    #[test]
    fn a_withholding_needs_a_hash_and_a_statement() {
        let _scratch = Scratch::new("refuses");

        assert!(withhold("not-a-hash", "statement").is_err());
        assert!(withhold(A, "   ").is_err());
        assert!(withhold(A, "two\nlines").is_err());
        assert!(withhold(A, "\u{1b}[2Kforged").is_err());
        assert!(withhold(A, "line\u{2028}separated").is_err());
        assert!(withhold(A, "reordered \u{202e}txt.exe").is_err());
        assert!(withhold(A, &"x".repeat(STATEMENT_MAX_CHARS + 1)).is_err());
        assert!(withhold(A, &"x".repeat(STATEMENT_MAX_CHARS)).is_ok());
        assert_eq!(read_withheld().unwrap().entries().count(), 1);
    }

    // Revert: accept a recipe → the chunked file is withheld while its chunks are served; accept a
    // chunk → a chunk other files share is withheld.
    #[test]
    fn a_chunked_file_or_a_chunk_is_never_withheld() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::chunk::Chunk;
        use crate::model::recipe::{Recipe, RecipeChunk};

        let _scratch = Scratch::new("chunked");
        let chunk = Chunk { content: b"a chunk of a large file".to_vec() };
        let mut chunk_object = LooseObjectBuilder::build_chunk(&chunk);
        chunk_object.store().unwrap();
        let recipe = Recipe {
            content_hash: object_utils::hash_object_bytes(&chunk.content),
            total_size: chunk.content.len() as u64,
            chunks: vec![RecipeChunk { hash: chunk_object.hash.clone(), size: chunk.content.len() as u64 }],
        };
        let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);
        recipe_object.store().unwrap();

        assert!(withhold(&recipe_object.hash, "statement").unwrap_err().contains("chunked file"));
        assert!(withhold(&chunk_object.hash, "statement").unwrap_err().contains("not a file's content"));
        assert!(read_withheld().unwrap().is_empty());
    }

    // Revert: read an unknown version or a malformed entry as nothing withheld → served again.
    #[test]
    fn an_unreadable_record_fails_every_read() {
        let _scratch = Scratch::new("unreadable");

        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION + 1, [A])).unwrap();
        assert!(read_withheld().is_err());
        assert!(removed().is_err());

        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [A])).unwrap();
        assert!(read_withheld().unwrap_err().contains("not an object hash and a statement"));
        assert!(withhold(B, "statement").is_err(), "never written over");
    }

    // Revert: skip the withdrawal → the bundle stays published.
    #[test]
    fn withholding_withdraws_the_bundle() {
        let _scratch = Scratch::new("withdraws");
        let bundle = bundle_utils::get_latest_bundle_path();
        std::fs::create_dir_all(bundle.parent().unwrap()).unwrap();
        std::fs::write(&bundle, b"bundle").unwrap();

        withhold(A, "statement").unwrap();
        assert!(!bundle.exists());

        // A restated statement changes no content, so the bundle stays.
        std::fs::write(&bundle, b"bundle").unwrap();
        assert_eq!(withhold(A, "restated").unwrap(), Withheld::Restated);
        assert!(bundle.exists());
    }
}
