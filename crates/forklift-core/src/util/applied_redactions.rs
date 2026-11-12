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
    /// The set of `hashes`, for a remote that keeps its applied redactions somewhere other than
    /// this record: the serverless head keeps each in its redaction's item.
    pub fn of(hashes: impl IntoIterator<Item = String>) -> AppliedSet {
        AppliedSet { hashes: hashes.into_iter().collect() }
    }

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

/// The `content_redacted` refusal of a fetch that left out `hash`, the content of a file, because
/// the remote reports its redaction applied, when this warehouse could not apply it too: the remote
/// no longer serves the content, so the history cannot be taken here whole. `details` carries the
/// hash.
pub fn unapplied_skip_refusal(hash: &str) -> String {
    crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentRedacted,
        format!(
            "The remote no longer serves object {}, as it has applied its redaction, and this \
            warehouse could not apply that redaction too, so the history naming it cannot be taken.",
            hash
        ),
        "Run \"forklift redact list\" to see the redaction and why it is not applied here.",
        serde_json::json!({ "hash": hash }),
    ).into()
}

/// The [`RefusalCode::ContentRedacted`] refusal a server answers a request for `hash` with, read or
/// upload, once a redaction it applied names it: whether or not the bytes are deleted yet, they are
/// never served or taken again.
///
/// [`RefusalCode::ContentRedacted`]: crate::error::RefusalCode::ContentRedacted
pub fn served_refusal(hash: &str) -> String {
    crate::error::CoreError::refusal_with_details(
        crate::error::RefusalCode::ContentRedacted,
        format!(
            "Object {} is redacted in this warehouse: it is no longer served, and cannot be uploaded \
            again.",
            hash
        ),
        "Run \"forklift lower\" to receive the redaction, then \"forklift redact list\" to see it.",
        serde_json::json!({ "hash": hash }),
    ).into()
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

/// The remote this warehouse has configured, in its own configuration or the global one. One that
/// is configured but broken still counts: what decides is whose report the warehouse waits for,
/// not whether it can reach it.
pub(crate) fn configured_remote() -> Result<Option<String>, String> {
    Ok(config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.map(|(url, _)| url))
}

/// What a remote reported applied in its handshake, and which remote it is. A warehouse with a
/// remote applies a redaction only once its own scan is clean and this report, from the remote it
/// still has configured, lists it.
#[derive(Clone, Debug)]
pub struct RemoteReport {
    url: String,
    applied: AppliedSet,
}

impl RemoteReport {
    /// The report of the remote at `url`, whose handshake is `info`.
    pub fn of(url: &str, info: &crate::model::remote::WarehouseInfo) -> RemoteReport {
        RemoteReport { url: url.to_string(), applied: AppliedSet::of(info.applied_redactions().into_iter().map(str::to_string)) }
    }

    /// Whether this report lists the redaction of `hash` as applied, whichever remote it is from.
    pub fn lists(&self, hash: &str) -> bool {
        self.applied.contains(hash)
    }

    /// Whether this report is from `configured`, the remote the warehouse has now, and lists the
    /// redaction of `hash` as applied.
    fn applies(&self, configured: &str, hash: &str) -> bool {
        crate::util::origin_utils::same_remote(&self.url, configured) && self.lists(hash)
    }
}

/// Where one redaction stands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RedactionState {
    /// Applied: when its scan ran, no pallet head and no meta pallet's history here named the
    /// content, and no head written since may name it anew.
    Applied,

    /// Pending: these pallet heads, the heads these deleted pallets' tombstones keep, and the
    /// history of these meta pallets (by name), still name it. A meta pallet's history is never
    /// rewritten, so a redaction it holds stays pending.
    Held { heads: Vec<HeadHolder>, tombstones: Vec<HeadHolder>, meta_pallets: Vec<String> },

    /// Pending: nothing here names it, and this warehouse has a remote, whose report that it is
    /// applied there it waits for: no report was read, or the one read does not list it.
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

/// What still holds `redaction` pending here, by its own scan: the pallet heads and the tombstones'
/// heads that name its content (by hash, or for a recipe, by chunk list) and the meta pallets whose
/// history names its hash. `None` when nothing does. A holder any walk found is reported ahead of
/// another walk's error, as the more definite answer.
///
/// A hash this store holds as a tree, a parcel or a chunk is no file's content, so its redaction
/// never applies: deleting it would take away a tree or a chunk that other content may share, and
/// no walk holds such an absence. It is an error, which leaves the redaction undecided.
fn scan(redaction: &Redaction) -> Result<Option<RedactionState>, String> {
    redaction_utils::held_kind(&redaction.hash)?;

    let heads = redaction_utils::head_holders(redaction);
    let tombstones = redaction_utils::tombstone_holders(redaction);
    let meta_pallets = redaction_utils::meta_holders(&redaction.hash);

    let found = heads.as_ref().is_ok_and(|heads| !heads.is_empty())
        || tombstones.as_ref().is_ok_and(|tombstones| !tombstones.is_empty())
        || meta_pallets.as_ref().is_ok_and(|names| !names.is_empty());

    let (heads, tombstones, meta_pallets) = match (heads, tombstones, meta_pallets) {
        (Ok(heads), Ok(tombstones), Ok(meta_pallets)) => (heads, tombstones, meta_pallets),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) if !found => return Err(error),
        (heads, tombstones, meta_pallets) => (
            heads.unwrap_or_default(), tombstones.unwrap_or_default(), meta_pallets.unwrap_or_default(),
        ),
    };

    Ok(match heads.is_empty() && tombstones.is_empty() && meta_pallets.is_empty() {
        true => None,
        false => Some(RedactionState::Held { heads, tombstones, meta_pallets }),
    })
}

/// Who decides a clean scan: this warehouse alone, or its remote too.
#[derive(Clone, Copy)]
struct Judge<'a> {
    /// The remote configured, if one is.
    remote: Option<&'a str>,

    /// The warehouse is sparse or a cache, so its scan skipped heads and trees it never fetched.
    partial: bool,

    /// What a remote reported applied, if its handshake was read.
    report: Option<&'a RemoteReport>,
}

impl<'a> Judge<'a> {
    /// Where a redaction of `hash` stands that is not applied and whose scan found nothing holding
    /// it. With a remote configured, its report decides, even in a partial warehouse: an applied
    /// remote serves no head naming the content, and a head here naming it in a part never fetched
    /// holds no copy of it here.
    fn clean(self, hash: &str) -> RedactionState {
        match (self.remote, self.partial) {
            (Some(remote), _) => match self.report.is_some_and(|report| report.applies(remote, hash)) {
                true => RedactionState::Applied,
                false => RedactionState::AwaitingRemote,
            },
            (None, true) => RedactionState::Undecided(
                "This warehouse holds only part of its pallets' content and has no remote, so a \
                head may name the content in a part it never fetched."
                    .to_string()
            ),
            (None, false) => RedactionState::Applied,
        }
    }
}

/// The remote configured and the warehouse's mode, which a [`Judge`] reads.
fn read_judged() -> Result<(Option<String>, bool), String> {
    Ok((configured_remote()?, scope_utils::content_may_be_absent()?))
}

/// Where each of `redactions` stands, by the applied record and, for each not yet applied, a scan
/// of this warehouse. Applies none.
fn judge(redactions: &[&Redaction], applied: &AppliedSet, judge: Judge<'_>) -> Vec<RedactionStatus> {
    redactions.iter().map(|redaction| {
        let state = match applied.contains(&redaction.hash) {
            true => RedactionState::Applied,
            false => match scan(redaction) {
                Ok(Some(held)) => held,
                Ok(None) => judge.clean(&redaction.hash),
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

/// Where every redaction `read` lists stands, without applying any: an unapplied one that
/// [`apply`] would apply with no remote's report reads as [`RedactionState::Ready`]: this asks no
/// remote, so in a warehouse with a remote each clean one reads as awaiting it. A cell
/// this build cannot read has no status here; it stays pending, since what it says about a chunk
/// list cannot be scanned for.
///
/// # Returns
/// * `Ok(Vec<RedactionStatus>)` - One per readable redaction, in the order `read` lists them.
/// * `Err(String)`              - The applied record, the configuration or the warehouse's mode
///                                cannot be read.
pub fn statuses(read: &RedactionsRead) -> Result<Vec<RedactionStatus>, String> {
    let applied = read_applied()?;
    let (remote, partial) = read_judged()?;
    let mut statuses = judge(&chosen(read, None), &applied, Judge { remote: remote.as_deref(), partial, report: None });

    for status in statuses.iter_mut().filter(|status| status.state.is_applied() && !applied.contains(&status.hash)) {
        status.state = RedactionState::Ready;
    }

    Ok(statuses)
}

/// Apply every redaction `read` lists (with `only`, just the one of that hash) that a scan now
/// finds unheld, and report where each stands. `read` is the local `@redactions` head, read after
/// its cells committed, so nothing can come to name a clean redaction's content (see the module
/// doc). A full warehouse with no remote applies by its scan alone. One with a remote configured,
/// served or not, applies a clean one only when `report`, read from that same remote, lists it
/// applied; without a report, each stays pending. The remote configured is read again under the
/// ref lock before the record is written, so a report from a remote replaced meanwhile applies
/// nothing. A CLI caller holds the warehouse lock; a server applies after each ref write it takes.
///
/// Under that same lock, before the record is written, the warehouse's bundle is withdrawn: it
/// may hold the content, and a build publishes only under the lock and only when no redaction was
/// applied since it began ([`bundle_utils::build_bundle`]). So no bundle holding applied content
/// is published once the record is.
///
/// [`bundle_utils::build_bundle`]: crate::util::bundle_utils::build_bundle
///
/// # Returns
/// * `Ok(Vec<RedactionStatus>)` - One per redaction judged, in the order `read` lists them; each
///                                that reads as applied is applied on disk.
/// * `Err(String)`              - The applied record, the configuration or the warehouse's mode
///                                cannot be read, or the bundle could not be withdrawn or the
///                                record written.
pub fn apply(read: &RedactionsRead,
             only: Option<&str>,
             report: Option<&RemoteReport>) -> Result<Vec<RedactionStatus>, String> {
    let applied = read_applied()?;
    let (remote, partial) = read_judged()?;
    let mut statuses = judge(&chosen(read, only), &applied, Judge { remote: remote.as_deref(), partial, report });

    let newly = |statuses: &[RedactionStatus]| -> Vec<String> {
        statuses.iter()
            .filter(|status| status.state.is_applied() && !applied.contains(&status.hash))
            .map(|status| status.hash.clone())
            .collect()
    };

    if newly(&statuses).is_empty() {
        return Ok(statuses);
    }

    let lock = RefLock::acquire()?;
    let (remote, partial) = read_judged()?;
    let now = Judge { remote: remote.as_deref(), partial, report };

    // A remote configured or replaced since the scan decides by its own report instead.
    for status in statuses.iter_mut().filter(|status| status.state.is_applied() && !applied.contains(&status.hash)) {
        status.state = now.clean(&status.hash);
    }

    let newly = newly(&statuses);

    if newly.is_empty() {
        return Ok(statuses);
    }

    file_utils::remove_unfollowed(&crate::util::bundle_utils::get_latest_bundle_path()).map_err(|e| format!(
        "Cannot apply the redaction: the bundle, which may hold its content, could not be withdrawn. {}", e
    ))?;

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
        let judge = |remote, partial| Judge { remote, partial, report: None };

        assert!(judge(None, false).clean(&hash('a')).is_applied());
        assert_eq!(judge(Some("http://r"), false).clean(&hash('a')), RedactionState::AwaitingRemote);
        assert_eq!(judge(Some("http://r"), true).clean(&hash('a')), RedactionState::AwaitingRemote);
        assert!(matches!(judge(None, true).clean(&hash('a')), RedactionState::Undecided(_)));
    }

    /// A warehouse with a remote applies a clean redaction only on a report from the remote it has
    /// configured that lists that very hash, in a full warehouse or a partial one.
    ///
    /// Revert: ignore the report's URL → the other remote's report applies; ignore its hashes →
    /// a hash it does not list applies.
    #[test]
    fn a_remote_s_report_applies_only_what_it_lists_and_only_from_that_remote() {
        let report = RemoteReport { url: "http://r/".to_string(), applied: AppliedSet::of([hash('a')]) };
        let judge = |remote, partial| Judge { remote: Some(remote), partial, report: Some(&report) };

        for partial in [false, true] {
            assert!(judge("http://r", partial).clean(&hash('a')).is_applied(), "the same remote, spelt with no slash");
            assert_eq!(judge("http://r", partial).clean(&hash('b')), RedactionState::AwaitingRemote, "not listed");
            assert_eq!(judge("http://other", partial).clean(&hash('a')), RedactionState::AwaitingRemote, "another remote");
        }

        // With no remote configured the report is not this warehouse's to wait for.
        let alone = Judge { remote: None, partial: true, report: Some(&report) };
        assert!(matches!(alone.clean(&hash('a')), RedactionState::Undecided(_)));
    }

    /// A redaction naming an object this store holds as a chunk or a tree is never applied: its
    /// scan is an error, which leaves it undecided.
    ///
    /// Revert: drop the kind check from the scan → both scans come back clean.
    #[test]
    fn a_redaction_naming_a_chunk_or_a_tree_is_never_scanned_clean() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType;
        use crate::model::tree_item::TreeItem;

        let _scratch = Scratch::new("applied-kind");
        let mut chunk = LooseObjectBuilder::build_chunk(&crate::model::chunk::Chunk { content: b"bytes".to_vec() });
        chunk.store().unwrap();
        let mut tree = LooseObjectBuilder::build_tree(&TreeItem::new(String::new(), String::new(), DirEntryType::Tree));
        tree.store().unwrap();

        for hash in [&chunk.hash, &tree.hash] {
            let redaction = Redaction { hash: hash.clone(), reason: "r".to_string(), redacted_at: 0, chunks: None };
            let error = scan(&redaction).unwrap_err();
            assert!(error.contains("not a file's content"), "{}", error);
        }
    }

    #[test]
    fn only_an_object_hash_is_recorded_as_applied() {
        let _scratch = Scratch::new("applied-hash");
        let lock = RefLock::acquire().unwrap();

        assert!(add_locked(&lock, &["not-a-hash".to_string()]).is_err());
        assert!(!applied_path().exists(), "nothing was written");
    }
}
