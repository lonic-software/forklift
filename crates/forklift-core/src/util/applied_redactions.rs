//! Which redactions this warehouse has applied (FORK-359), and the scan that decides it.
//!
//! A redaction recorded on `@redactions` is pending until it applies. It applies once a scan made
//! after its cell committed finds no pallet head naming its content and no meta pallet's history
//! naming its hash, and, in a warehouse with a remote, once the remote reports it applied too,
//! since this warehouse's meta history may lag the remote's. No head written after the cell may
//! name the content anew, so a clean scan stays clean, and the scan needs no fence over other ref
//! writes. Meta history holds a redaction pending for good: a meta pallet's verification loads
//! every cell blob its chain adds, so that blob must stay. Bay state (staged, parked, consolidation
//! and cherry-pick state) never holds a redaction pending: it is no pallet head, and what of it
//! names the content is kept by the deletion's own census, not by this scan.
//!
//! Applying is local state, not history: the record at `<forklift root>/redactions-applied` lists
//! every applied hash, in the [`record_utils`] form, one hash per line, sorted. It only ever grows,
//! since an applied redaction is never taken back. It is written whole and atomically under the
//! [`RefLock`], re-read under that lock first, so two writers never drop each other's hashes. A
//! record this build cannot read, of another version or with an entry that is not an object hash,
//! fails every read and is never written over: an applied redaction must never read as pending.

use std::collections::BTreeSet;
use std::path::PathBuf;
use crate::globals;
use crate::util::lock_utils::RefLock;
use crate::util::redaction_utils::{self, HeadHolder, Redaction, RedactionsRead};
use crate::util::{config_utils, file_utils, object_utils, record_utils, scope_utils};

/// The record's file name in the shared forklift root, and its [`record_utils`] kind and version.
const FILE_NAME_APPLIED: &str = "redactions-applied";
const APPLIED_RECORD: &str = "redactions-applied";
const APPLIED_VERSION: u32 = 1;

fn applied_path() -> PathBuf {
    globals::forklift_root().join(FILE_NAME_APPLIED)
}

/// The hashes of every redaction this warehouse has applied.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AppliedSet {
    hashes: BTreeSet<String>,
}

impl AppliedSet {
    /// Whether the redaction of `hash` is applied.
    pub fn contains(&self, hash: &str) -> bool {
        self.hashes.contains(hash)
    }

    /// Whether no redaction is applied.
    pub fn is_empty(&self) -> bool {
        self.hashes.is_empty()
    }

    /// Every applied hash, sorted.
    pub fn hashes(&self) -> impl Iterator<Item = &String> {
        self.hashes.iter()
    }
}

/// Record `hashes` as applied, as `redact apply` would once nothing names them. For tests in other
/// modules, which delete the object with [`delete_applied`](crate::util::redaction_deleter::delete_applied) or remove it themselves.
#[cfg(test)]
pub(crate) fn apply_for_test(hashes: &[&str]) -> AppliedSet {
    let lock = RefLock::acquire().unwrap();
    add_locked(&lock, &hashes.iter().map(|hash| hash.to_string()).collect::<Vec<_>>()).unwrap()
}

/// Read the applied record. No record reads as nothing applied.
///
/// # Returns
/// * `Ok(AppliedSet)` - The applied hashes.
/// * `Err(String)`    - The record exists but cannot be read, is of a version this build does not
///                      know, or holds an entry that is not an object hash.
pub fn read_applied() -> Result<AppliedSet, String> {
    let path = applied_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(AppliedSet::default()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
    };

    let entries = record_utils::from_bytes(&bytes, APPLIED_RECORD, APPLIED_VERSION).map_err(|e| format!(
        "The record of applied redactions \"{}\" {} This version of Forklift never writes over it.",
        path.to_string_lossy(), e.describe()
    ))?;

    let mut set = AppliedSet::default();

    for entry in entries {
        if !object_utils::is_object_hash(&entry) {
            return Err(format!(
                "The record of applied redactions \"{}\" holds an entry that is not an object hash: \
                \"{}\". This version of Forklift never writes over it.",
                path.to_string_lossy(), entry.escape_debug()
            ));
        }

        set.hashes.insert(entry);
    }

    Ok(set)
}

/// The `content_redacted` refusal of a read that found `hash` held nowhere in the store, when it
/// is absent because this warehouse applied its redaction: the bytes are gone for good, so the read
/// is refused as redacted rather than reported as a missing object. `details` carries the hash.
///
/// # Returns
/// * `Ok(Some(String))` - The refusal: a redaction applied here names `hash`.
/// * `Ok(None)`         - None does; the absence is the caller's to report.
/// * `Err(String)`      - The applied record cannot be read (see [`read_applied`]).
pub(crate) fn absent_read_refusal(hash: &str) -> Result<Option<String>, String> {
    if !read_applied()?.contains(hash) {
        return Ok(None);
    }

    Ok(Some(crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentRedacted,
        format!(
            "Cannot read object {}: it is redacted in this warehouse, and its content has been \
            removed for good.",
            hash
        ),
        "Run \"forklift redact list\" to see the redaction.",
        serde_json::json!({ "hash": hash }),
    ).into()))
}

/// Add `hashes` to the applied record, for a caller that holds the ref lock. The record is re-read
/// under the lock and rewritten whole, atomically, holding what it held and `hashes`; nothing is
/// written when it already holds them all.
///
/// # Returns
/// * `Ok(AppliedSet)` - The applied hashes now.
/// * `Err(String)`    - A hash is not an object hash, the record cannot be read, or the write
///                      failed. A write that failed after its rename has still landed: read the
///                      record back to learn which.
fn add_locked(_lock: &RefLock, hashes: &[String]) -> Result<AppliedSet, String> {
    if let Some(hash) = hashes.iter().find(|hash| !object_utils::is_object_hash(hash)) {
        return Err(format!("\"{}\" is not an object hash, so it cannot be recorded as applied.", hash));
    }

    let mut set = read_applied()?;
    let before = set.hashes.len();
    set.hashes.extend(hashes.iter().cloned());

    if set.hashes.len() != before {
        let bytes = record_utils::to_bytes(APPLIED_RECORD, APPLIED_VERSION, set.hashes.iter());
        file_utils::replace_file_atomically(&applied_path(), &bytes)?;
    }

    Ok(set)
}

/// Whether this warehouse has a remote configured, in its own configuration or the global one.
/// One that is configured but broken still counts: what decides is whose report the warehouse
/// waits for, not whether it can reach it.
fn has_remote() -> Result<bool, String> {
    Ok(config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.is_some())
}

/// Where one redaction stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedactionState {
    /// Applied: when its scan ran, no pallet head and no meta pallet's history here named the
    /// content, and no head written since may name it anew.
    Applied,

    /// Pending: these pallet heads, and the history of these meta pallets (by name), still name
    /// it. A meta pallet's history is never rewritten, so a redaction it holds stays pending.
    Held { heads: Vec<HeadHolder>, meta_pallets: Vec<String> },

    /// Pending: nothing here names it, and this warehouse has a remote, whose report that it is
    /// applied there it waits for.
    AwaitingRemote,

    /// Pending: the scan could not tell whether anything names it, for this reason.
    Undecided(String),

    /// Pending, but nothing here names it and nothing else is waited for: [`apply`] applies it.
    /// Only [`statuses`], which applies nothing, reports it.
    Ready,
}

impl RedactionState {
    /// Whether the redaction is applied.
    pub fn is_applied(&self) -> bool {
        matches!(self, RedactionState::Applied)
    }
}

/// One redaction's hash and where it stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactionStatus {
    pub hash: String,
    pub state: RedactionState,
}

/// What still holds `redaction` pending here, by its own scan: the pallet heads that name its
/// content (by hash, or for a recipe, by chunk list) and the meta pallets whose history names its
/// hash. `None` when nothing does. A holder either walk found is reported ahead of the other
/// walk's error, as the more definite answer.
fn scan(redaction: &Redaction) -> Result<Option<RedactionState>, String> {
    let (heads, meta_pallets) = match (
        redaction_utils::head_holders(redaction),
        redaction_utils::meta_holders(&redaction.hash),
    ) {
        (Ok(heads), Ok(meta_pallets)) => (heads, meta_pallets),
        (Ok(heads), Err(_)) if !heads.is_empty() => (heads, Vec::new()),
        (Err(_), Ok(meta_pallets)) if !meta_pallets.is_empty() => (Vec::new(), meta_pallets),
        (Err(error), _) | (_, Err(error)) => return Err(error),
    };

    Ok(match heads.is_empty() && meta_pallets.is_empty() {
        true => None,
        false => Some(RedactionState::Held { heads, meta_pallets }),
    })
}

/// Who decides a clean scan: this warehouse alone, or its remote too.
#[derive(Clone, Copy)]
struct Judge {
    /// A remote is configured.
    remote: bool,

    /// The warehouse is sparse or a cache, so its scan skipped heads and trees it never fetched.
    partial: bool,
}

impl Judge {
    fn read() -> Result<Judge, String> {
        Ok(Judge { remote: has_remote()?, partial: scope_utils::content_may_be_absent()? })
    }

    /// Where a redaction stands that is not applied and whose scan found nothing holding it.
    fn clean(self) -> RedactionState {
        match (self.remote, self.partial) {
            (true, _) => RedactionState::AwaitingRemote,
            (false, true) => RedactionState::Undecided(
                "This warehouse holds only part of its pallets' content and has no remote, so a \
                head may name the content in a part it never fetched."
                    .to_string()
            ),
            (false, false) => RedactionState::Applied,
        }
    }
}

/// Where each of `redactions` stands, by the applied record and, for each not yet applied, a scan
/// of this warehouse. Applies none.
fn judge(redactions: &[&Redaction], applied: &AppliedSet, judge: Judge) -> Vec<RedactionStatus> {
    redactions.iter().map(|redaction| {
        let state = match applied.contains(&redaction.hash) {
            true => RedactionState::Applied,
            false => match scan(redaction) {
                Ok(Some(held)) => held,
                Ok(None) => judge.clean(),
                Err(why) => RedactionState::Undecided(why),
            },
        };

        RedactionStatus { hash: redaction.hash.clone(), state }
    }).collect()
}

/// The readable redactions of `read`, or with `only`, the one of that hash (none if `read` lacks
/// it).
fn chosen<'a>(read: &'a RedactionsRead, only: Option<&str>) -> Vec<&'a Redaction> {
    read.redactions.iter()
        .map(|attributed| &attributed.redaction)
        .filter(|redaction| only.is_none_or(|hash| redaction.hash == hash))
        .collect()
}

/// Where every redaction `read` lists stands, without applying any: an unapplied one whose scan
/// comes back clean in a full warehouse with no remote reads as [`RedactionState::Ready`]. A cell this build cannot read has no status here; it stays pending, since what it says
/// about a chunk list cannot be scanned for.
///
/// # Returns
/// * `Ok(Vec<RedactionStatus>)` - One per readable redaction, in the order `read` lists them.
/// * `Err(String)`              - The applied record, the configuration or the warehouse's mode
///                                cannot be read.
pub fn statuses(read: &RedactionsRead) -> Result<Vec<RedactionStatus>, String> {
    let applied = read_applied()?;
    let mut statuses = judge(&chosen(read, None), &applied, Judge::read()?);

    for status in statuses.iter_mut().filter(|status| status.state.is_applied() && !applied.contains(&status.hash)) {
        status.state = RedactionState::Ready;
    }

    Ok(statuses)
}

/// Apply every redaction `read` lists (with `only`, just the one of that hash) that a scan now
/// finds unheld, in a full warehouse with no remote, and report where each stands. `read` is the
/// local `@redactions` head, read after its cells committed, so nothing can come to name a clean
/// redaction's content (see the module doc). In a warehouse with a remote configured, served or
/// not, nothing is applied: each stays pending until the remote reports it applied. Whether a remote is
/// configured is read again under the ref lock before the record is written. The caller holds the
/// warehouse lock.
///
/// # Returns
/// * `Ok(Vec<RedactionStatus>)` - One per redaction judged, in the order `read` lists them; each
///                                that reads as applied is applied on disk.
/// * `Err(String)`              - The applied record, the configuration or the warehouse's mode
///                                cannot be read, or the record could not be written.
pub fn apply(read: &RedactionsRead, only: Option<&str>) -> Result<Vec<RedactionStatus>, String> {
    let applied = read_applied()?;
    let mut statuses = judge(&chosen(read, only), &applied, Judge::read()?);

    let newly: Vec<String> = statuses.iter()
        .filter(|status| status.state.is_applied() && !applied.contains(&status.hash))
        .map(|status| status.hash.clone())
        .collect();

    if newly.is_empty() {
        return Ok(statuses);
    }

    let lock = RefLock::acquire()?;
    let now = Judge::read()?;

    // A remote configured since the scan waits for its report instead.
    if !now.clean().is_applied() {
        for status in statuses.iter_mut().filter(|status| newly.contains(&status.hash)) {
            status.state = now.clean();
        }

        return Ok(statuses);
    }

    add_locked(&lock, &newly)?;

    Ok(statuses)
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
            let root = std::env::temp_dir().join(format!("forklift-applied-test-{}-{}", name, std::process::id()));
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

    fn hash(c: char) -> String {
        c.to_string().repeat(64)
    }

    #[test]
    fn the_record_only_grows_and_reads_back_sorted() {
        let _scratch = Scratch::new("applied-grows");
        assert!(read_applied().unwrap().is_empty(), "no record reads as nothing applied");

        let lock = RefLock::acquire().unwrap();
        add_locked(&lock, &[hash('b')]).unwrap();
        let set = add_locked(&lock, &[hash('a'), hash('b')]).unwrap();
        drop(lock);

        assert_eq!(set.hashes().cloned().collect::<Vec<_>>(), vec![hash('a'), hash('b')]);
        assert_eq!(read_applied().unwrap(), set);

        let bytes = std::fs::read(applied_path()).unwrap();
        assert_eq!(String::from_utf8(bytes).unwrap(),
                   format!("/forklift-redactions-applied 1\n{}\n{}\n", hash('a'), hash('b')));
    }

    #[test]
    fn a_record_this_build_cannot_read_fails_loudly_and_is_never_written_over() {
        let _scratch = Scratch::new("applied-unknown");

        for (bytes, expected) in [
            (b"/forklift-redactions-applied 2\n".to_vec(), "does not know"),
            (format!("/forklift-redactions-applied 1\n{}\n", "x".repeat(64)).into_bytes(), "not an object hash"),
            (b"aaaa\n".to_vec(), "no format header"),
        ] {
            std::fs::write(applied_path(), &bytes).unwrap();

            let error = read_applied().unwrap_err();
            assert!(error.contains(expected) && error.contains(FILE_NAME_APPLIED), "{}", error);

            let lock = RefLock::acquire().unwrap();
            assert!(add_locked(&lock, &[hash('a')]).is_err(), "never written over");
            assert_eq!(std::fs::read(applied_path()).unwrap(), bytes);
        }
    }

    /// Revert: let a partial warehouse with no remote decide by its own scan → it applies.
    #[test]
    fn only_a_full_warehouse_with_no_remote_applies_by_its_own_scan() {
        assert!(Judge { remote: false, partial: false }.clean().is_applied());
        assert_eq!(Judge { remote: true, partial: false }.clean(), RedactionState::AwaitingRemote);
        assert_eq!(Judge { remote: true, partial: true }.clean(), RedactionState::AwaitingRemote);
        assert!(matches!(Judge { remote: false, partial: true }.clean(), RedactionState::Undecided(_)));
    }

    #[test]
    fn only_an_object_hash_is_recorded_as_applied() {
        let _scratch = Scratch::new("applied-hash");
        let lock = RefLock::acquire().unwrap();

        assert!(add_locked(&lock, &["not-a-hash".to_string()]).is_err());
        assert!(!applied_path().exists(), "nothing was written");
    }
}
