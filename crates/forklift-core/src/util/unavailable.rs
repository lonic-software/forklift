//! Content this warehouse cannot get (FORK-413): a file's content its remote's host withholds, left
//! out of a fetch and absent here.
//!
//! A remote whose host withholds a file's content ([`withholding`]) lists it in the handshake and
//! refuses it on every route. A fetch leaves such content out, and this warehouse records what it
//! left out and lacks, beside the host's statement, so a head naming the file can still be checked
//! out without it. The record widens what a walk tolerates absent, where a file's content sits
//! ([`sealed_utils::Tolerated`](crate::util::sealed_utils::Tolerated)), below a redaction and a
//! withholding and above the sealed record. It is neither of them: the `withheld` record says this
//! warehouse's own head withholds the content, and the sealed record says a remote holds it. No
//! absence recording ever moves a hash this record holds into the sealed record, and a served root
//! is refused while it lacks content the record lists.
//!
//! An entry whose object is present is dropped under the ref lock ([`drop_present`]); an entry whose
//! object is absent is dropped only under exclusive store access, once nothing the live set's roots
//! reach names it, judged only by a walk that was whole ([`prune`]): one that met no absent tree and
//! no absent parcel the sealed record holds. Below either nothing can tell, and a refill would
//! record the object sealed, so a cache or a sparse warehouse holding one keeps its absent entries.
//!
//! The record at `<forklift root>/unavailable` is in the [`record_utils`] form, one
//! `<hash> <digest> <statement>` entry per line, sorted. `<digest>` is what the remote's handshake
//! said of a recipe's chunk list ([`ChunkDigest`]): its digest, `-` when it said none, or `!` once
//! two remotes said different ones. A recorded digest is never replaced by none, a different one
//! makes the entry Conflicted, and nothing takes it out of Conflicted. The record is written whole and atomically under the [`RefLock`],
//! re-read under that lock first, so two writers never drop each other's entries. A record this
//! build cannot read, of another version or with an entry that is not a hash and a statement,
//! fails every read and is never written over: a hash it lists must never read as damage, nor a
//! root lacking it as whole.

use std::collections::BTreeMap;
use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::{RefLock, StoreAccessExclusive};
use crate::util::{file_utils, gc_utils, object_utils, record_utils, withholding};
use crate::util::withholding::ChunkDigest;

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_UNAVAILABLE: &str = "unavailable";
const UNAVAILABLE_RECORD: &str = "unavailable";
const UNAVAILABLE_VERSION: u32 = 2;

fn unavailable_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_UNAVAILABLE)
}

/// One hash to record unavailable: the host's statement, and the chunk-list digest its handshake
/// reported, if any. A `(hash, statement)` pair reports none.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unavailable {
    pub hash: String,
    pub statement: String,
    pub digest: Option<String>,
}

impl From<(String, String)> for Unavailable {
    fn from((hash, statement): (String, String)) -> Unavailable {
        Unavailable { hash, statement, digest: None }
    }
}

/// One recorded hash's entry: the host's statement, and what is known of its chunk list.
#[derive(Clone, Debug, PartialEq, Eq)]
struct UnavailableEntry {
    statement: String,
    digest: ChunkDigest,
}

/// Every hash recorded unavailable, each beside the statement of the host that withholds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnavailableSet {
    entries: BTreeMap<String, UnavailableEntry>,
}

impl UnavailableSet {
    /// Whether `hash` is recorded unavailable.
    pub fn contains(&self, hash: &str) -> bool {
        self.entries.contains_key(hash)
    }

    /// The host's statement `hash` is recorded with, if it is.
    pub fn statement(&self, hash: &str) -> Option<&str> {
        self.entries.get(hash).map(|entry| entry.statement.as_str())
    }

    /// What is known of the chunk list of `hash`, if it is recorded.
    pub fn digest(&self, hash: &str) -> Option<&ChunkDigest> {
        self.entries.get(hash).map(|entry| &entry.digest)
    }

    /// Whether a file of content `hash` is left out here: the record lists it and the store lacks
    /// it. Only a listed hash is probed.
    pub fn lacks(&self, hash: &str) -> Result<bool, String> {
        Ok(self.contains(hash) && !file_utils::does_object_exist(hash)?)
    }

    /// Whether nothing is recorded.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every recorded hash beside its statement, sorted by hash.
    pub fn entries(&self) -> impl Iterator<Item = (&String, &String)> {
        self.entries.iter().map(|(hash, entry)| (hash, &entry.statement))
    }
}

/// Read the unavailable record. No record reads as nothing unavailable.
///
/// # Returns
/// * `Ok(UnavailableSet)` - The recorded hashes and their statements.
/// * `Err(String)`        - The record exists but cannot be read, is of a version this build does
///                          not know, or holds an entry that is not a hash and a statement.
pub fn read_unavailable() -> Result<UnavailableSet, String> {
    let path = unavailable_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(UnavailableSet::default()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let entries = record_utils::from_bytes(&bytes, UNAVAILABLE_RECORD, UNAVAILABLE_VERSION).map_err(|e| format!(
        "The record of content this warehouse cannot get \"{}\" {} This version of Forklift never writes over it.",
        path.to_string_lossy(), e.describe()
    ))?;

    let mut set = UnavailableSet::default();

    for entry in entries {
        let parsed = entry.split_once(' ')
            .and_then(|(hash, rest)| rest.split_once(' ').map(|(column, statement)| (hash, column, statement)))
            .and_then(|(hash, column, statement)| Some((hash, ChunkDigest::parse_column(column)?, statement)));

        match parsed {
            Some((hash, digest, statement)) if object_utils::is_object_hash(hash) && withholding::valid_statement(statement) => {
                set.entries.insert(hash.to_string(), UnavailableEntry { statement: statement.to_string(), digest });
            }
            _ => return Err(format!(
                "The record of content this warehouse cannot get \"{}\" holds an entry that is not an \
                object hash, a chunk-list digest and a statement: \"{}\". This version of Forklift never \
                writes over it.",
                path.to_string_lossy(), entry.escape_debug()
            )),
        }
    }

    Ok(set)
}

/// How many of the record's entries the store lacks: content its history may name that this
/// warehouse cannot serve. An entry whose object is present counts nothing.
///
/// # Returns
/// * `Ok(usize)`   - The count.
/// * `Err(String)` - The record cannot be read, or an object only an unreadable pack index might
///                   hold, or the store could not be consulted.
pub fn absent_count() -> Result<usize, String> {
    let mut absent = 0;

    for (hash, _) in read_unavailable()?.entries() {
        if !file_utils::is_present_for_sweep(hash)? {
            absent += 1;
        }
    }

    Ok(absent)
}

/// The `content_withheld` refusal of a read that found `hash` held nowhere in the store, when it is
/// absent because the record lists it: this warehouse's remote withholds the content, so the read
/// is refused with the host's statement rather than reported as a missing object. `details` carries
/// the hash and the statement.
///
/// # Returns
/// * `Ok(Some(String))` - The refusal: the record lists `hash`.
/// * `Ok(None)`         - It does not; the absence is the caller's to report.
/// * `Err(String)`      - The record cannot be read (see [`read_unavailable`]).
pub(crate) fn absent_read_refusal(hash: &str) -> Result<Option<String>, String> {
    let record = read_unavailable()?;
    let Some(statement) = record.statement(hash) else { return Ok(None) };

    Ok(Some(crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentWithheld,
        format!(
            "Cannot read object {}: this warehouse does not hold it; it was left out because the host \
            of a remote withholds it. The host's statement: {}",
            hash, statement
        ),
        "Only a warehouse that holds the content can read it or send it on.",
        serde_json::json!({ "hash": hash, "statement": statement }),
    ).into()))
}

/// Write `set` whole as the record, or remove the record when `set` is empty. The caller holds the
/// ref lock and read `set` under it.
fn write_locked(_lock: &RefLock, set: &UnavailableSet) -> Result<(), String> {
    if set.is_empty() {
        return file_utils::remove_unfollowed(&unavailable_path());
    }

    let entries = set.entries.iter()
        .map(|(hash, entry)| format!("{} {} {}", hash, entry.digest.column(), entry.statement));
    let bytes = record_utils::to_bytes(UNAVAILABLE_RECORD, UNAVAILABLE_VERSION, entries);
    file_utils::replace_file_atomically(&unavailable_path(), &bytes)
}

/// Record each of `entries`, a hash beside its host's statement and the digest its handshake
/// reported, as unavailable, under the ref lock this takes. A hash recorded already takes the
/// statement given, and its digest is merged ([`merged`]).
///
/// # Returns
/// * `Ok(())`      - Every entry is recorded.
/// * `Err(String)` - As [`record_locked`].
pub fn record<E: Into<Unavailable>>(entries: impl IntoIterator<Item = E>) -> Result<(), String> {
    let lock = RefLock::acquire()?;
    record_locked(&lock, entries)
}

/// [`record`], under the ref lock `lock` the caller holds. The record is re-read under it, so an
/// entry another writer recorded is kept.
///
/// # Returns
/// * `Ok(())`      - Every entry is recorded; nothing is written when each was already, with its
///                   statement.
/// * `Err(String)` - An entry is not an object hash and a statement of one plain line, the record
///                   cannot be read, or it could not be written. A write that failed after its
///                   rename has still landed: read the record back to learn which.
pub fn record_locked<E: Into<Unavailable>>(lock: &RefLock, entries: impl IntoIterator<Item = E>) -> Result<(), String> {
    let mut set = read_unavailable()?;
    let mut changed = false;

    for entry in entries {
        let Unavailable { hash, statement, digest } = entry.into();

        if !object_utils::is_object_hash(&hash) {
            return Err(format!("\"{}\" is not an object hash; nothing was recorded unavailable.", hash));
        }

        let statement = withholding::checked_statement(&statement)?.to_string();
        let digest = merged(set.digest(&hash), ChunkDigest::of_report(digest.as_deref())?);
        let entry = UnavailableEntry { statement, digest };

        if set.entries.get(&hash) != Some(&entry) {
            set.entries.insert(hash, entry);
            changed = true;
        }
    }

    match changed {
        true => write_locked(lock, &set),
        false => Ok(()),
    }
}

/// What an entry recorded as `recorded` (none, when it is new) knows of its chunk list once a
/// handshake reported `reported`: a known digest is never replaced by an unknown one, and a second,
/// different known digest makes the entry Conflicted, since the hash fixes the chunk list and so
/// one of the two remotes is wrong. Conflicted stays.
fn merged(recorded: Option<&ChunkDigest>, reported: ChunkDigest) -> ChunkDigest {
    match (recorded, reported) {
        (None | Some(ChunkDigest::Unknown), reported) => reported,
        (Some(ChunkDigest::Conflicted), _) => ChunkDigest::Conflicted,
        (Some(known), ChunkDigest::Unknown | ChunkDigest::Conflicted) => known.clone(),
        (Some(known), reported) if *known == reported => reported,
        (Some(_), _) => ChunkDigest::Conflicted,
    }
}

/// Drop every entry whose object `drop` says to, under the ref lock this takes, re-reading the
/// record under it. Takes the lock only when the record holds an entry.
///
/// # Returns
/// * `Ok(usize)`   - How many entries were dropped.
/// * `Err(String)` - This thread holds the ref lock already, the record cannot be read, `drop`
///                   failed, or the record could not be written.
fn drop_where(mut drop: impl FnMut(&str) -> Result<bool, String>) -> Result<usize, String> {
    if read_unavailable()?.is_empty() {
        return Ok(0);
    }

    // The ref lock waits on any holder, this thread's own hold too.
    if RefLock::held_by_this_thread() {
        return Err("The record of content this warehouse cannot get is not shrunk while this thread holds the ref lock.".to_string());
    }

    let lock = RefLock::acquire()?;
    let mut set = read_unavailable()?;
    let before = set.entries.len();
    let mut kept = BTreeMap::new();

    for (hash, entry) in std::mem::take(&mut set.entries) {
        if !drop(&hash)? {
            kept.insert(hash, entry);
        }
    }

    set.entries = kept;
    let dropped = before - set.entries.len();

    if dropped > 0 {
        write_locked(&lock, &set)?;
    }

    Ok(dropped)
}

/// Drop every entry whose object the store holds: the content is no longer unavailable here. An
/// object only an unreadable pack index might hold is not present, and fails the step.
///
/// # Returns
/// * `Ok(usize)`   - How many entries were dropped.
/// * `Err(String)` - As [`drop_where`], or the store could not be consulted.
pub fn drop_present() -> Result<usize, String> {
    drop_where(file_utils::is_present_for_sweep)
}

/// Drop every entry whose object is present, and, when the live walk was whole
/// ([`gc_utils::LiveSet::whole`]), every one
/// whose object is absent and that nothing the live set's roots reach names (every pallet, meta and
/// tombstoned head, haul head, mirror root, bay root, trust pin and staged shard), read
/// fail-closed. It takes exclusive store access, held
/// by the caller, so no writer, fetch or checkout that recorded an entry and has not yet written
/// its head runs meanwhile.
///
/// A host's step calls this after its own work and ignores its error: the record then only stays
/// larger than it needs to be, which a later step shrinks.
///
/// # Returns
/// * `Ok(usize)`   - How many entries were dropped.
/// * `Err(String)` - The live set could not be computed, or as [`drop_present`].
pub fn prune(hold: &StoreAccessExclusive) -> Result<usize, String> {
    if read_unavailable()?.is_empty() {
        return Ok(0);
    }

    prune_against(hold, &gc_utils::collect_live_set()?)
}

/// [`prune`], against `live`, a live set the caller computed under `_hold`. When its walk was not
/// whole, stopped at an absent tree or at an absent parcel the sealed record holds, an entry it did
/// not mark may sit below that boundary, and a refill there pushes absent children down into the
/// sealed record, which must never hold unavailable content: so only present entries are dropped
/// then.
pub(crate) fn prune_against(_hold: &StoreAccessExclusive, live: &gc_utils::LiveSet) -> Result<usize, String> {
    drop_where(|hash| Ok(file_utils::is_present_for_sweep(hash)? || (live.whole && !live.contains(hash))))
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
            let root = std::env::temp_dir().join(format!("forklift-unavailable-test-{}-{}", name, std::process::id()));
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

    // Revert: drop the re-read under the lock → the second record loses the first.
    #[test]
    fn recording_keeps_every_entry_and_its_statement() {
        let _scratch = Scratch::new("keeps");
        assert!(read_unavailable().unwrap().is_empty());

        record([(A.to_string(), "DMCA notice 1".to_string())]).unwrap();
        record([(B.to_string(), "  court order 7  ".to_string())]).unwrap();
        record([(A.to_string(), "DMCA notice 2".to_string())]).unwrap();

        let set = read_unavailable().unwrap();
        assert_eq!(set.statement(A), Some("DMCA notice 2"));
        assert_eq!(set.statement(B), Some("court order 7"));
        assert_eq!(set.entries().count(), 2);
    }

    /// An entry for `hash` reporting `digest`.
    fn reported(hash: &str, digest: Option<&str>) -> Unavailable {
        Unavailable { hash: hash.to_string(), statement: "statement".to_string(), digest: digest.map(str::to_string) }
    }

    // Revert: let a report without a digest replace a known one → Unknown again; keep the first of
    // two different digests → the remote that lied wins; let a later report end Conflicted → it
    // reads as known again.
    #[test]
    fn a_digest_is_kept_and_two_different_ones_conflict_for_good() {
        let _scratch = Scratch::new("digest");
        let (one, two) = ("1".repeat(64), "2".repeat(64));

        record([reported(A, None)]).unwrap();
        assert_eq!(read_unavailable().unwrap().digest(A), Some(&ChunkDigest::Unknown));
        record([reported(A, Some(&one))]).unwrap();
        record([reported(A, None)]).unwrap();
        assert_eq!(read_unavailable().unwrap().digest(A), Some(&ChunkDigest::Known(one.clone())));
        record([reported(A, Some(&one))]).unwrap();
        assert_eq!(read_unavailable().unwrap().digest(A), Some(&ChunkDigest::Known(one.clone())));

        record([reported(A, Some(&two))]).unwrap();
        assert_eq!(read_unavailable().unwrap().digest(A), Some(&ChunkDigest::Conflicted));

        for digest in [Some(one.as_str()), Some(two.as_str()), None] {
            record([reported(A, digest)]).unwrap();
            assert_eq!(read_unavailable().unwrap().digest(A), Some(&ChunkDigest::Conflicted));
        }

        // A report that is not a digest's form is refused, and nothing is recorded.
        assert!(record([reported(B, Some("not a digest"))]).unwrap_err().contains("not a chunk-list digest"));
        assert!(!read_unavailable().unwrap().contains(B));
    }

    // Revert: drop a validity check → the non-hash or the two-line statement is recorded.
    #[test]
    fn an_entry_needs_a_hash_and_a_statement() {
        let _scratch = Scratch::new("refuses");

        assert!(record([("not-a-hash".to_string(), "statement".to_string())]).is_err());
        assert!(record([(A.to_string(), "two\nlines".to_string())]).is_err());
        assert!(record([(A.to_string(), "   ".to_string())]).is_err());
        assert!(read_unavailable().unwrap().is_empty());
    }

    // Revert: read an unknown version or a malformed entry as nothing unavailable → the hash reads
    // as damage, and a served root lacking it as whole.
    #[test]
    fn an_unreadable_record_fails_every_read() {
        let _scratch = Scratch::new("unreadable");

        std::fs::write(unavailable_path(), record_utils::to_bytes(UNAVAILABLE_RECORD, UNAVAILABLE_VERSION + 1, [A])).unwrap();
        assert!(read_unavailable().is_err());

        std::fs::write(unavailable_path(), record_utils::to_bytes(UNAVAILABLE_RECORD, UNAVAILABLE_VERSION, [A])).unwrap();
        assert!(read_unavailable().unwrap_err().contains("not an object hash, a chunk-list digest and a statement"));
        std::fs::write(unavailable_path(), record_utils::to_bytes(UNAVAILABLE_RECORD, UNAVAILABLE_VERSION, [format!("{} statement", A)])).unwrap();
        assert!(read_unavailable().is_err(), "a version-1 entry has no digest column");
        assert!(record([(B.to_string(), "statement".to_string())]).is_err(), "never written over");
        assert!(drop_present().is_err(), "never written over");
    }

    // Revert: drop the presence check → the present entry stays; drop every entry → the absent
    // one goes too.
    #[test]
    fn a_present_entry_is_dropped_and_an_absent_one_kept() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::model::blob::Blob;

        let _scratch = Scratch::new("present");
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: b"the bytes came back".to_vec() });
        blob.store().unwrap();
        record([(blob.hash.clone(), "statement".to_string()), (A.to_string(), "statement".to_string())]).unwrap();

        assert_eq!(drop_present().unwrap(), 1);
        let set = read_unavailable().unwrap();
        assert!(!set.contains(&blob.hash) && set.contains(A));
        assert_eq!(drop_present().unwrap(), 0);
    }

    // Revert: drop the liveness check → the named absent entry goes; keep every absent entry →
    // the unnamed one stays; ignore `whole` → the walk that stopped at a boundary drops B.
    #[test]
    fn the_prune_drops_an_absent_entry_only_when_a_whole_walk_names_it_nowhere() {
        let _scratch = Scratch::new("prune");
        std::fs::create_dir_all(file_utils::get_path_objects_root()).unwrap();
        record([(A.to_string(), "statement".to_string()), (B.to_string(), "statement".to_string())]).unwrap();
        let hold = crate::util::lock_utils::StoreAccessLock::try_exclusive().unwrap();

        let live = |whole| gc_utils::LiveSet { hashes: [A.to_string()].into(), whole };
        assert_eq!(prune_against(&hold, &live(false)).unwrap(), 0, "B may sit below an absent tree");
        assert_eq!(prune_against(&hold, &live(true)).unwrap(), 1);
        let set = read_unavailable().unwrap();
        assert!(set.contains(A) && !set.contains(B));

        assert_eq!(prune_against(&hold, &gc_utils::LiveSet { hashes: Default::default(), whole: true }).unwrap(), 1);
        assert!(!unavailable_path().exists(), "an empty record is removed");
    }

    // Revert: drop the own-hold check → the step waits on this thread's own ref lock forever.
    #[test]
    fn the_record_is_not_shrunk_under_this_threads_ref_lock() {
        let _scratch = Scratch::new("own-lock");
        record([(A.to_string(), "statement".to_string())]).unwrap();
        let _lock = RefLock::acquire().unwrap();

        assert!(drop_present().is_err());
    }
}
