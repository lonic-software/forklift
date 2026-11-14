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
//! damaged or redacted: an origin move counts it as not held by the new server, and a cache's
//! eviction or a sparse warehouse's prune keeps the copy here. A served head deletes its copy as it
//! deletes an applied redaction's ([`redaction_deleter`](crate::util::redaction_deleter)), under a
//! census that history never holds a copy for.
//!
//! A chunked file is withheld by its recipe's hash. Its chunks are not refused by hash, since other
//! content may share them; until the deletion is done, a server serves the chunks only withheld or
//! redacted recipes name as absent, which
//! [`redaction_deleter`](crate::util::redaction_deleter) also calls "withheld", for how they are
//! served.
//!
//! The list is head-local configuration, not history: the record at `<forklift root>/withheld`
//! lists each withheld hash beside its statement, in the [`record_utils`] form, one
//! `<hash> <digest> <statement>` entry per line, sorted. For a chunked file's recipe, `<digest>`
//! is the [`chunk_list_digest`](redaction_utils::chunk_list_digest) a chunk-list redaction
//! compares, taken while a copy of the recipe was at hand ([`ChunkDigest`]); it is `-` while none
//! was, and for a blob. A recorded digest is never taken off: once the host deletes the recipe it
//! is the only way left to tell whether the file repeats a redacted recipe's chunk list. It is written whole and atomically under the
//! [`RefLock`], re-read under that lock first, so two writers never drop each other's entries. A
//! record this build cannot read, of another version or with an entry that is not a hash and a
//! statement, fails every read with [`RefusalCode::WithheldDamaged`] and is never written over:
//! withheld content must never read as served. Nor may a lost record: a root's first serve writes
//! an empty one before the served mark ([`tombstone_utils::mark_served`]), so in a served root a
//! missing record is damage too, and only a root never served reads none as nothing withheld. A
//! record lost before the first serve cannot be told from none, but nothing was served yet. The
//! operator repairs a damaged record with `forklift-server withhold repair`
//! ([`withheld_repair`](crate::util::withheld_repair)).

use std::collections::BTreeMap;
use std::path::PathBuf;
use crate::globals;
use crate::util::applied_redactions::{self, AppliedSet};
use crate::util::lock_utils::RefLock;
use crate::enums::object_type::ObjectType;
use crate::error::{CoreError, RefusalCode};
use crate::util::tombstone_utils::{self, ServedState};
use crate::util::{bundle_utils, file_utils, object_utils, record_utils, redaction_utils};

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_WITHHELD: &str = "withheld";
const WITHHELD_RECORD: &str = "withheld";
const WITHHELD_VERSION: u32 = 2;

/// The longest statement a takedown may carry, in characters: every handshake carries it.
pub const STATEMENT_MAX_CHARS: usize = 1000;

fn withheld_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_WITHHELD)
}

/// What a record of withheld content knows of a withheld recipe's chunk list: the digest a
/// chunk-list redaction compares ([`redaction_utils::chunk_list_digest`]), taken while a copy of the
/// recipe was at hand. The hash fixes the chunk list, so two honest digests of one hash are equal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ChunkDigest {
    /// The digest of the recipe's chunk list.
    Known(String),

    /// No copy of a recipe of this hash was at hand when it was recorded, or it is a blob's hash.
    Unknown,

    /// Two remotes reported different digests for the hash, so one of them is wrong. Only a client's
    /// record of unavailable content holds it, and no later recording takes the entry out of it.
    Conflicted,
}

impl ChunkDigest {
    /// The digest, when it is known.
    pub fn known(&self) -> Option<&str> {
        match self {
            ChunkDigest::Known(digest) => Some(digest),
            ChunkDigest::Unknown | ChunkDigest::Conflicted => None,
        }
    }

    /// The digest `known` reports, or [`ChunkDigest::Unknown`] when it reports none.
    ///
    /// # Returns
    /// * `Ok(ChunkDigest)` - The reported state.
    /// * `Err(String)`     - The report is not a digest's form: it is refused, never taken for none.
    pub fn of_report(known: Option<&str>) -> Result<ChunkDigest, String> {
        match known {
            None => Ok(ChunkDigest::Unknown),
            Some(digest) => checked_digest(digest).map(|digest| ChunkDigest::Known(digest.to_string())),
        }
    }

    /// The entry's state column: the digest, `-` for Unknown, `!` for Conflicted.
    pub(crate) fn column(&self) -> &str {
        match self {
            ChunkDigest::Known(digest) => digest,
            ChunkDigest::Unknown => "-",
            ChunkDigest::Conflicted => "!",
        }
    }

    /// The state a column says, or `None` when it is none of [`Self::column`]'s forms.
    pub(crate) fn parse_column(column: &str) -> Option<ChunkDigest> {
        match column {
            "-" => Some(ChunkDigest::Unknown),
            "!" => Some(ChunkDigest::Conflicted),
            digest if is_digest(digest) => Some(ChunkDigest::Known(digest.to_string())),
            _ => None,
        }
    }
}

/// Whether `digest` has a chunk-list digest's form: 64 lowercase hex digits.
fn is_digest(digest: &str) -> bool {
    digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `digest`, when it is a chunk-list digest's form (64 lowercase hex digits).
///
/// # Returns
/// * `Ok(&str)`    - The digest.
/// * `Err(String)` - It is not one.
pub fn checked_digest(digest: &str) -> Result<&str, String> {
    match is_digest(digest) {
        true => Ok(digest),
        false => Err(format!("\"{}\" is not a chunk-list digest.", digest)),
    }
}

/// The chunk-list digest of the recipe `hash` from `bytes`, a copy of it read from the store, or
/// [`ChunkDigest::Unknown`] when the bytes do not hash to `hash` or are not a recipe: a digest is
/// only ever recorded from the recipe's own bytes.
pub(crate) fn verified_digest(hash: &str, bytes: &[u8]) -> ChunkDigest {
    recipe_digest(hash, bytes).map_or(ChunkDigest::Unknown, ChunkDigest::Known)
}

/// The chunk-list digest of the recipe `hash` from `bytes`, a copy of it from anywhere, once the
/// bytes are shown to hash to `hash` and to parse as a recipe.
///
/// # Returns
/// * `Ok(String)`  - The digest.
/// * `Err(String)` - The bytes do not hash to `hash`, or are not a recipe.
pub fn recipe_digest(hash: &str, bytes: &[u8]) -> Result<String, String> {
    object_utils::verify_object_bytes(hash, bytes).map_err(|_| format!("The bytes do not hash to {}.", hash))?;

    let recipe = object_utils::parse_recipe_bytes(hash, bytes)?;

    Ok(redaction_utils::chunk_list_digest(&recipe.chunks.into_iter().map(|chunk| chunk.hash).collect::<Vec<_>>()))
}

/// One withheld hash's entry: the operator's statement, and what is known of its chunk list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WithheldEntry {
    statement: String,
    digest: ChunkDigest,
}

/// Every hash this head withholds, each beside the operator's statement.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WithheldSet {
    entries: BTreeMap<String, WithheldEntry>,
}

impl WithheldSet {
    /// The set of `entries`, each a hash beside its statement and what is known of its chunk list:
    /// what a head that keeps its record somewhere other than this module's file reads it as.
    pub fn of(entries: impl IntoIterator<Item = (String, String, ChunkDigest)>) -> WithheldSet {
        WithheldSet {
            entries: entries.into_iter()
                .map(|(hash, statement, digest)| (hash, WithheldEntry { statement, digest }))
                .collect(),
        }
    }

    /// Whether `hash` is withheld.
    pub fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    /// The statement `hash` is withheld with, if it is.
    pub fn statement(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(|entry| entry.statement.as_str())
    }

    /// What is known of the chunk list of `hash`, if it is withheld.
    pub fn digest(&self, hash: &str) -> Option<&ChunkDigest> {
        self.entries.get(hash).map(|entry| &entry.digest)
    }

    /// Whether nothing is withheld.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every withheld hash beside its statement, sorted by hash.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &String)> {
        self.entries.iter().map(|(hash, entry)| (hash, &entry.statement))
    }

    /// Write this set whole as the record. The caller holds the ref lock and read the set under it.
    pub(crate) fn write(&self, _lock: &RefLock) -> Result<(), String> {
        let entries = self.entries.iter()
            .map(|(hash, entry)| format!("{} {} {}", hash, entry.digest.column(), entry.statement));
        let bytes = record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, entries);
        file_utils::replace_file_atomically(&withheld_path(), &bytes)
    }
}

/// The command that repairs the record of withheld content of the warehouse at the current storage
/// root.
fn repair_command() -> String {
    // The storage root may be relative to where the command runs, and the remedy is run elsewhere.
    let root = std::path::absolute(globals::forklift_root()).unwrap_or_else(|_| globals::forklift_root());
    let warehouse = root.parent().unwrap_or(&root);

    format!("forklift-server withhold repair --root {}", crate::util::franchise_utils::quote(&warehouse.to_string_lossy()))
}

/// The refusal for a lost or damaged record of withheld content, `detail` saying what is wrong.
fn damaged(detail: &str) -> String {
    CoreError::refusal(
        RefusalCode::WithheldDamaged,
        format!(
            "{} What this warehouse's head withholds cannot be told, so nothing was served or \
            changed, and every read of the record refuses until it is repaired.",
            detail
        ),
        repair_command(),
    ).into()
}

/// Whether `error` is the refusal of a lost or damaged record of withheld content ([`damaged`]).
pub fn is_damage(error: &str) -> bool {
    crate::error::deframe(error).is_some_and(|(code, _, _)| code == RefusalCode::WithheldDamaged.as_str())
}

/// Why a record's bytes do not read as a set of withheld content.
pub(crate) enum Unparsed {
    /// The file as a whole does not read as a record of this kind and version.
    Record(record_utils::RecordError),
    /// An entry is not a hash, a chunk-list digest and a statement.
    Entry(String),
}

impl Unparsed {
    /// The reason, naming the record.
    pub(crate) fn describe(&self) -> String {
        match self {
            Unparsed::Record(e) => format!("The record of withheld content {}", e.describe()),
            Unparsed::Entry(entry) => format!(
                "The record of withheld content holds an entry that is not an object hash, a chunk-list \
                digest and a statement: \"{}\".",
                entry.escape_debug()
            ),
        }
    }
}

/// Parse one entry, `None` when it is not a hash, a chunk-list digest and a statement. A host never
/// records Conflicted: every digest it fills comes from bytes of the entry's hash.
pub(crate) fn parse_entry(entry: &str) -> Option<(&str, &str, ChunkDigest)> {
    let (hash, rest) = entry.split_once(' ')?;
    let (column, statement) = rest.split_once(' ')?;
    let digest = ChunkDigest::parse_column(column)?;

    (object_utils::is_object_hash(hash) && valid_statement(statement) && digest != ChunkDigest::Conflicted)
        .then_some((hash, statement, digest))
}

/// Parse the record's bytes whole.
pub(crate) fn parse(bytes: &[u8]) -> Result<WithheldSet, Unparsed> {
    let entries = record_utils::from_bytes(bytes, WITHHELD_RECORD, WITHHELD_VERSION).map_err(Unparsed::Record)?;
    let mut set = WithheldSet::default();

    for entry in entries {
        match parse_entry(&entry) {
            Some((hash, statement, digest)) => {
                set.entries.insert(hash.to_string(), WithheldEntry { statement: statement.to_string(), digest });
            }
            None => return Err(Unparsed::Entry(entry)),
        }
    }

    Ok(set)
}

/// Read the withheld record. No record reads as nothing withheld only in a warehouse never served:
/// its first serve writes an empty record before it marks the root served
/// ([`tombstone_utils::mark_served`]), so a served root without one has lost it, and what its
/// head withholds cannot be told.
///
/// # Returns
/// * `Ok(WithheldSet)` - The withheld hashes and their statements.
/// * `Err(String)`     - A [`RefusalCode::WithheldDamaged`] refusal: the record cannot be read, is
///                       of a version this build does not know, holds an entry that is not a hash,
///                       a digest and a statement, or is missing from a served root; or the
///                       tombstones' damage, when a missing record makes the served state matter.
pub fn read_withheld() -> Result<WithheldSet, String> {
    let bytes = match std::fs::read(withheld_path()) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return match tombstone_utils::served_state()? {
            ServedState::Never => Ok(WithheldSet::default()),
            ServedState::Unfinished | ServedState::Served => Err(damaged(
                "This warehouse has been served, but its record of withheld content is missing."
            )),
        },
        Err(e) => return Err(damaged(&format!("The record of withheld content could not be read: {}.", e))),
    };

    parse(&bytes).map_err(|e| damaged(&format!("{} This version of Forklift never writes over it.", e.describe())))
}

/// Write an empty record of withheld content unless one is there, under the ref lock `lock` the
/// caller holds: the first serve's witness that a served root has a record ([`read_withheld`]).
///
/// # Returns
/// * `Ok(())`      - A record is there.
/// * `Err(String)` - Whether one is there could not be told, or it could not be written.
pub(crate) fn create_if_absent(lock: &RefLock) -> Result<(), String> {
    match std::fs::symlink_metadata(withheld_path()) {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => WithheldSet::default().write(lock),
        Err(e) => Err(format!("Error while reading \"{}\": {}", withheld_path().to_string_lossy(), e)),
    }
}

/// `statement`, trimmed, when it may be a takedown's ([`valid_statement`]), as every head that
/// withholds checks it before recording it.
///
/// # Returns
/// * `Ok(&str)`    - The trimmed statement.
/// * `Err(String)` - It is not one plain line of at most [`STATEMENT_MAX_CHARS`], or is blank.
pub fn checked_statement(statement: &str) -> Result<&str, String> {
    let statement = statement.trim();

    match valid_statement(statement) {
        true => Ok(statement),
        false => Err(format!(
            "The statement must be one plain line, not blank, with no control character, separator \
            or direction mark, and at most {} characters long.", STATEMENT_MAX_CHARS
        )),
    }
}

/// Whether `statement` may be a takedown's: not blank, at most [`STATEMENT_MAX_CHARS`] long, and
/// one plain line, since every terminal that shows a refusal prints it: no control character, line
/// or paragraph separator, or direction mark, override or isolate, which would reorder its display.
pub(crate) fn valid_statement(statement: &str) -> bool {
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
    Ok(removed_from(&applied_redactions::read_applied()?, &read_withheld()?))
}

/// The content [`removed`] names, of `applied` and `withheld` already read.
pub fn removed_from(applied: &AppliedSet, withheld: &WithheldSet) -> AppliedSet {
    AppliedSet::of(applied.hashes().chain(withheld.entries.keys()).cloned())
}

/// What [`withhold`] did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Withheld {
    /// The hash was not withheld, and now is.
    Added,

    /// The hash was withheld already; the statement now is the one given.
    Restated,

    /// The hash was withheld already, with this statement. Nothing was written, unless the entry
    /// lacked the digest of a recipe the store now holds, which is filled in.
    Unchanged,
}

/// Withhold `hash`, the content of a file, with the operator's `statement`.
///
/// Under the ref lock, before the record is written, the warehouse's bundle is withdrawn: it may
/// hold the content, and a build publishes only under that lock and only when nothing was
/// withheld or redacted since it began ([`bundle_utils::build_bundle`]). So no bundle holding
/// withheld content is published once the record is; a statement restated leaves the bundle, which
/// already leaves the content out. A hash the store holds as a tree, a parcel or a chunk is
/// refused: a takedown removes one file's content, as a redaction does. A chunked file's recipe is
/// a file's content, and is withheld; a server then serves the chunks no other object names as
/// absent ([`redaction_deleter::withheld_chunks`]). One the store lacks is withheld all the same,
/// so it is never taken; should it be a chunked file's recipe, nothing here can tell, and its
/// chunks are still taken and served, since no recipe the store holds names them.
///
/// A recipe the store holds has its chunk-list digest recorded with it ([`ChunkDigest`]). An entry
/// restated keeps the digest it has, even once the recipe is deleted; one without a digest takes
/// the held recipe's.
///
/// [`redaction_deleter::withheld_chunks`]: crate::util::redaction_deleter::withheld_chunks
///
/// # Returns
/// * `Ok(Withheld)` - What changed.
/// * `Err(String)`  - `hash` is not an object hash, or the store holds it as other than a file's
///                    content, `statement` is not one plain line of at most
///                    [`STATEMENT_MAX_CHARS`], the record cannot be read, or the bundle could not
///                    be withdrawn or the record written. A write that failed after its rename
///                    has still landed: read the record back to learn which.
pub fn withhold(hash: &str, statement: &str) -> Result<Withheld, String> {
    if !object_utils::is_object_hash(hash) {
        return Err(format!("\"{}\" is not an object hash.", hash));
    }

    let statement = checked_statement(statement)?;

    // The bytes are pinned by the hash, so a digest read before the lock is the recipe's still. A
    // copy that cannot be read whole and verified leaves the entry without one: the takedown is
    // not held up by a damaged copy, and a digest is never recorded from bytes that are not the
    // recipe's.
    let digest = match redaction_utils::stored_type(hash)? {
        None | Some(ObjectType::Blob) => ChunkDigest::Unknown,
        Some(ObjectType::Recipe) => file_utils::retrieve_object_by_hash_shared(hash)
            .map_or(ChunkDigest::Unknown, |bytes| verified_digest(hash, &bytes)),
        Some(other) => return Err(format!(
            "Object {} is a {:?}, not a file's content: a takedown withholds one file's content.", hash, other
        )),
    };

    let lock = RefLock::acquire()?;
    let mut set = read_withheld()?;

    let (outcome, digest) = match set.entries.get(hash) {
        Some(held) => {
            let digest = match &held.digest {
                ChunkDigest::Unknown => digest,
                recorded => recorded.clone(),
            };

            match held.statement == statement {
                true if digest == held.digest => return Ok(Withheld::Unchanged),
                true => (Withheld::Unchanged, digest),
                false => (Withheld::Restated, digest),
            }
        }
        None => (Withheld::Added, digest),
    };

    if outcome == Withheld::Added {
        file_utils::remove_unfollowed(&bundle_utils::get_latest_bundle_path()).map_err(|e| format!(
            "Cannot withhold the content: the bundle, which may hold it, could not be withdrawn. {}", e
        ))?;
    }

    set.entries.insert(hash.to_string(), WithheldEntry { statement: statement.to_string(), digest });
    set.write(&lock)?;

    Ok(outcome)
}

/// Record `digests`, each a withheld hash beside the chunk-list digest of a recipe of that hash at
/// hand, under the ref lock `lock` the caller holds, for every entry that has no digest yet. The
/// caller computed each digest from bytes it verified hash to its hash ([`verified_digest`]). A hash not withheld, or whose
/// entry has a digest already, is left as it is.
///
/// # Returns
/// * `Ok(usize)`   - How many entries took a digest; nothing is written when none did.
/// * `Err(String)` - A digest is not one, the record cannot be read, or it could not be written.
///                   A write that failed after its rename has still landed.
pub fn record_digests_locked(lock: &RefLock, digests: impl IntoIterator<Item = (String, String)>) -> Result<usize, String> {
    let mut set = read_withheld()?;
    let mut filled = 0;

    for (hash, digest) in digests {
        if !is_digest(&digest) {
            return Err(format!("\"{}\" is not a chunk-list digest; nothing was recorded.", digest));
        }

        if let Some(entry) = set.entries.get_mut(&hash) {
            if entry.digest == ChunkDigest::Unknown {
                entry.digest = ChunkDigest::Known(digest);
                filled += 1;
            }
        }
    }

    if filled > 0 {
        set.write(lock)?;
    }

    Ok(filled)
}

/// Record `digest`, the chunk-list digest of the withheld recipe `hash` from a copy shown to be
/// it ([`recipe_digest`]), under the ref lock, when its entry has no digest yet: the digest intake
/// a head offers, so a client holding a recipe the host never held can supply what a chunk-list
/// redaction compares. Nothing of the copy is kept. A hash not withheld, or whose entry has a
/// digest already, is left as it is.
///
/// # Returns
/// * `Ok(bool)`    - Whether the entry took the digest.
/// * `Err(String)` - See [`record_digests_locked`], or the ref lock could not be taken.
pub fn record_digest(hash: &str, digest: &str) -> Result<bool, String> {
    let lock = RefLock::acquire()?;

    Ok(record_digests_locked(&lock, [(hash.to_string(), digest.to_string())])? > 0)
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
            let _ = file_utils::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = file_utils::remove_dir_all(&self.root);
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

    // Revert: refuse a recipe → the chunked file is not withheld; accept a chunk → a chunk other
    // files share is withheld.
    #[test]
    fn a_chunked_file_is_withheld_and_a_chunk_never_is() {
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

        assert_eq!(withhold(&recipe_object.hash, "statement").unwrap(), Withheld::Added);
        assert!(withhold(&chunk_object.hash, "statement").unwrap_err().contains("not a file's content"));
        let set = read_withheld().unwrap();
        assert_eq!(set.entries().map(|(hash, _)| hash.as_str()).collect::<Vec<_>>(), [recipe_object.hash.as_str()]);
    }

    /// A recipe of one chunk of `content`, stored, with its chunk-list digest.
    fn stored_recipe(content: &[u8]) -> (String, String) {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::chunk::Chunk;
        use crate::model::recipe::{Recipe, RecipeChunk};

        let chunk = Chunk { content: content.to_vec() };
        let mut chunk_object = LooseObjectBuilder::build_chunk(&chunk);
        chunk_object.store().unwrap();
        let recipe = Recipe {
            content_hash: object_utils::hash_object_bytes(&chunk.content),
            total_size: chunk.content.len() as u64,
            chunks: vec![RecipeChunk { hash: chunk_object.hash.clone(), size: chunk.content.len() as u64 }],
        };
        let mut recipe_object = LooseObjectBuilder::build_recipe(&recipe);
        recipe_object.store().unwrap();

        (recipe_object.hash.clone(), redaction_utils::chunk_list_digest(&[chunk_object.hash]))
    }

    // Revert: record Unknown for a held recipe → no digest; let a restatement take the stored
    // type's (absent) digest → the digest is lost once the recipe is deleted; let a fill replace a
    // Known digest → the entry changes.
    #[test]
    fn a_withheld_recipe_keeps_its_chunk_list_digest() {
        let _scratch = Scratch::new("digest");
        let (recipe, digest) = stored_recipe(b"a chunk of a withheld film");

        withhold(&recipe, "DMCA notice 1").unwrap();
        withhold(A, "a blob or an absent hash").unwrap();
        let set = read_withheld().unwrap();
        assert_eq!(set.digest(&recipe), Some(&ChunkDigest::Known(digest.clone())));
        assert_eq!(set.digest(A), Some(&ChunkDigest::Unknown));

        // Deleted, then restated: the digest stays.
        let (folder, name) = file_utils::get_path_for_object(&recipe).unwrap();
        let path = std::path::Path::new(&folder).join(name);
        file_utils::remove_file(&path).unwrap();
        assert_eq!(withhold(&recipe, "DMCA notice 2").unwrap(), Withheld::Restated);
        assert_eq!(read_withheld().unwrap().digest(&recipe), Some(&ChunkDigest::Known(digest.clone())));

        // A fill takes only an entry without one, and never one not withheld.
        let other = "c".repeat(64);
        let lock = RefLock::acquire().unwrap();
        assert_eq!(record_digests_locked(&lock, [(recipe.clone(), other.clone()), (A.to_string(), other.clone()), (B.to_string(), other.clone())]).unwrap(), 1);
        assert!(record_digests_locked(&lock, [(A.to_string(), "not a digest".to_string())]).is_err());
        drop(lock);
        let set = read_withheld().unwrap();
        assert_eq!(set.digest(&recipe), Some(&ChunkDigest::Known(digest)));
        assert_eq!(set.digest(A), Some(&ChunkDigest::Known(other)));
        assert!(!set.contains(B));
    }

    // Revert: compute the digest without verifying the bytes → the other recipe's chunk list is
    // taken for this one's.
    #[test]
    fn a_digest_is_only_ever_taken_from_the_recipes_own_bytes() {
        let _scratch = Scratch::new("verified");
        let (recipe, digest) = stored_recipe(b"a chunk of a film");
        let (other, _) = stored_recipe(b"a chunk of another film");
        let bytes = |hash: &str| file_utils::retrieve_object_by_hash_shared(hash).unwrap();

        assert_eq!(verified_digest(&recipe, &bytes(&recipe)), ChunkDigest::Known(digest));
        assert_eq!(verified_digest(&recipe, &bytes(&other)), ChunkDigest::Unknown);
        assert_eq!(verified_digest(&recipe, b"not an object"), ChunkDigest::Unknown);
    }

    // Revert: return Unchanged before the fill → an entry recorded while the recipe was absent
    // never takes its digest from withhold() again.
    #[test]
    fn withholding_again_fills_the_digest_of_a_recipe_held_since() {
        let _scratch = Scratch::new("refill");
        let (recipe, digest) = stored_recipe(b"a chunk of a film withheld before it arrived");
        let (folder, name) = file_utils::get_path_for_object(&recipe).unwrap();
        let path = std::path::Path::new(&folder).join(name);
        let bytes = std::fs::read(&path).unwrap();
        file_utils::remove_file(&path).unwrap();

        withhold(&recipe, "statement").unwrap();
        assert_eq!(read_withheld().unwrap().digest(&recipe), Some(&ChunkDigest::Unknown));

        std::fs::write(&path, bytes).unwrap();
        assert_eq!(withhold(&recipe, "statement").unwrap(), Withheld::Unchanged);
        assert_eq!(read_withheld().unwrap().digest(&recipe), Some(&ChunkDigest::Known(digest)));
    }

    // Revert: read an unknown version or a malformed entry as nothing withheld → served again.
    #[test]
    fn an_unreadable_record_fails_every_read() {
        let _scratch = Scratch::new("unreadable");

        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION + 1, [A])).unwrap();
        assert!(read_withheld().is_err());
        assert!(removed().is_err());

        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [A])).unwrap();
        assert!(read_withheld().unwrap_err().contains("not an object hash, a chunk-list digest and a statement"));

        // A version-1 entry, with no digest column, and a host entry recorded Conflicted.
        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [format!("{} statement", A)])).unwrap();
        assert!(read_withheld().is_err());
        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [format!("{} ! statement", A)])).unwrap();
        assert!(read_withheld().is_err());
        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [format!("{} - statement", A)])).unwrap();
        assert_eq!(read_withheld().unwrap().digest(A), Some(&ChunkDigest::Unknown));

        std::fs::write(withheld_path(), record_utils::to_bytes(WITHHELD_RECORD, WITHHELD_VERSION, [A])).unwrap();
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
