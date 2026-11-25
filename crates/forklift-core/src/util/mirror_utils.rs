//! The git mirror's roots record: for each mirrored pallet, the parcel the mirror's last export of
//! it starts from next time (DESIGN.html §9.8).
//!
//! The mirror builds each export on the commits it wrote itself, so the Forklift history those
//! commits came from must outlive gc, even for a pallet this warehouse holds no ref for. The
//! record makes each named parcel a gc root. It is a root list, not a map: an entry that no longer
//! matches what the mirror exported only keeps more history, and nothing reads it to steer an
//! export. Everything it keeps is history the remote can serve again, so deleting the record
//! loses no data; it costs a later fetch and re-export.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use crate::globals::forklift_root;
use crate::util::{file_utils, object_utils, pallet_utils, record_utils};

/// The record's file name, at the shared `.forklift` root: every bay mirrors into one place.
pub const FILE_NAME_MIRROR_ROOTS: &str = "mirror-roots";

/// The record's kind and version, in its header. A later format gets a new version, which this
/// build refuses to read.
const MIRROR_ROOTS_RECORD: &str = "mirror-roots";
const MIRROR_ROOTS_VERSION: u32 = 2;

/// Where the record lives.
pub fn mirror_roots_path() -> PathBuf {
    forklift_root().join(FILE_NAME_MIRROR_ROOTS)
}

/// The mirror's git repository's folder name, at the shared `.forklift` root beside the record.
pub const FOLDER_NAME_MIRROR_REPOSITORY: &str = "mirror.git";

/// Where the mirror's git repository lives: a bare repository only Forklift writes.
pub fn mirror_repository_path() -> PathBuf {
    forklift_root().join(FOLDER_NAME_MIRROR_REPOSITORY)
}

/// The folder holding the shared `.forklift`: where the mirror reads a relative `mirror.url`
/// from, the same folder from every bay.
pub fn warehouse_root() -> PathBuf {
    let root = forklift_root();
    let root = std::path::absolute(&root).unwrap_or(root);

    root.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("."))
}

/// Read the record: each mirrored pallet's root parcel. No record is an empty one.
///
/// # Returns
/// * `Ok(BTreeMap)` - Pallet name to parcel hash.
/// * `Err(String)`  - If the record exists but cannot be read or parsed. The message names the
///                    record and the step that clears it (delete it), since deleting it is always
///                    safe.
pub fn read_mirror_roots() -> Result<BTreeMap<String, String>, String> {
    let path = mirror_roots_path();
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(unreadable(&e.to_string())),
    };

    parse(&bytes).map_err(|reason| unreadable(&reason))
}

/// Replace the record with `roots`, atomically.
pub fn write_mirror_roots(roots: &BTreeMap<String, String>) -> Result<(), String> {
    file_utils::write_file_atomically(&mirror_roots_path(), &roots_bytes(roots))
}

/// What a write of the record did, as reading it back showed. A write can land and still fail,
/// when its folder sync fails, so a failed write is read back, and each variant says only what
/// that read shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootsWrite {
    /// The record already read as wanted, so nothing was written.
    AlreadyRead,
    /// The write landed, with its folder synced.
    Landed,
    /// The write failed with this error, and the record reads as wanted: it may not be on disk
    /// durably yet.
    NotDurable(String),
    /// The write failed with this error, and the record reads otherwise.
    Failed(String),
    /// The write failed with `failed`, and the record could not be read back (`unread`).
    Unknown { failed: String, unread: String },
    /// The record could not be read, so what to write could not be worked out, and nothing was.
    /// Only a put-back of owed entries ([`put_back_owed_roots`]) ends this way: the takedown's
    /// put-back ([`put_back_mirror_roots`]) writes an unreadable record back whole.
    Unread(String),
}

impl RootsWrite {
    /// The sentence for a run's last write of the record that failed, naming the record; `None`
    /// when it did not fail. It says nothing of what a later run will do: a taint that refused the
    /// write refuses the next one too, until it is healed. [`update_mirror_roots`] never ends as
    /// `Unread`, so that arm says only why the record could not be read.
    pub fn failure(&self) -> Option<String> {
        match self {
            RootsWrite::AlreadyRead | RootsWrite::Landed => None,
            RootsWrite::NotDurable(failed) => Some(format!(
                "The mirror record holds what this run wrote, but it may not be on disk durably yet ({}).", failed
            )),
            RootsWrite::Failed(failed) => Some(format!("The mirror record could not be written: {}", failed)),
            RootsWrite::Unknown { failed, unread } => Some(format!(
                "Whether the mirror record was written is not known: {} It could not be read back: {}", failed, unread
            )),
            RootsWrite::Unread(unread) => Some(unread.clone()),
        }
    }

    /// Whether, after a write that went this way, the record holds the put-backs a takedown
    /// refusal wanted, as far as reading it shows. After the run's last write, when it does not,
    /// or reading it failed, the owed put-backs are tried again ([`put_back_owed_roots`]). Whether
    /// the takedown's own put-back ([`put_back_mirror_roots`]) is owed is decided by an exhaustive
    /// match on its outcome in the mirror command (`mirror::put_back_on_takedown`), which owes the
    /// same variants.
    pub fn holds_what_was_wanted(&self) -> bool {
        match self {
            RootsWrite::AlreadyRead | RootsWrite::Landed | RootsWrite::NotDurable(_) => true,
            RootsWrite::Failed(_) | RootsWrite::Unknown { .. } | RootsWrite::Unread(_) => false,
        }
    }

    /// The sentence for a put-back of the record after a takedown refusal, said by the write that
    /// settled it: the takedown's own put-back ([`put_back_mirror_roots`]), the run's last write,
    /// or the retry of the owed entries ([`put_back_owed_roots`]). One vocabulary for all three.
    pub fn put_back(&self) -> String {
        match self {
            RootsWrite::AlreadyRead => "The mirror record reads as put back.".to_string(),
            RootsWrite::Landed => "The mirror record was put back.".to_string(),
            RootsWrite::NotDurable(failed) => format!(
                "The mirror record reads as put back, but it may not be on disk durably yet ({}).", failed
            ),
            RootsWrite::Failed(failed) => format!("The mirror record could not be put back: {}", failed),
            RootsWrite::Unknown { failed, unread } => format!(
                "Whether the mirror record was put back is not known: {} It could not be read back: {}", failed, unread
            ),
            RootsWrite::Unread(unread) => format!("Whether the mirror record was put back is not known: {}", unread),
        }
    }
}

/// Write the record as `roots` only when it does not already read as `roots`: a run whose steps
/// already wrote what it holds rewrites nothing, so a taint recorded meanwhile, which refuses the
/// ordinary write, does not fail the run.
pub fn update_mirror_roots(roots: &BTreeMap<String, String>) -> RootsWrite {
    if read_mirror_roots().ok().as_ref() == Some(roots) {
        return RootsWrite::AlreadyRead;
    }

    match write_mirror_roots(roots) {
        Ok(()) => RootsWrite::Landed,
        Err(failed) => read_back(failed, roots),
    }
}

/// What a write of `wanted` that failed with `failed` did, from reading the record back.
fn read_back(failed: String, wanted: &BTreeMap<String, String>) -> RootsWrite {
    match read_mirror_roots() {
        Ok(now) if now == *wanted => RootsWrite::NotDurable(failed),
        Ok(_) => RootsWrite::Failed(failed),
        Err(unread) => RootsWrite::Unknown { failed, unread },
    }
}

/// Put the record back to `before`, what it held before a mirror step that then failed, and say
/// what that did. A record that already reads as `before` is left alone (`AlreadyRead`): the
/// failed step did not change it. One that cannot be read is written back, as it may have changed.
/// The write is a compensating one ([`file_utils::write_file_compensating`]): it puts back what
/// stood before the step, so a taint standing for the storage root does not refuse it, whoever
/// recorded that taint.
pub fn put_back_mirror_roots(before: &BTreeMap<String, String>) -> RootsWrite {
    if read_mirror_roots().ok().as_ref() == Some(before) {
        return RootsWrite::AlreadyRead;
    }

    write_compensating(before)
}

/// Write the record as `wanted` through the compensating write, read back after a failure.
fn write_compensating(wanted: &BTreeMap<String, String>) -> RootsWrite {
    match file_utils::write_file_compensating(&mirror_roots_path(), &roots_bytes(wanted)) {
        Ok(()) => RootsWrite::Landed,
        Err(failed) => read_back(failed, wanted),
    }
}

/// Put back the entries of the pallets in `owed`, whose takedown refusal could not put the record
/// back, to what `record` holds for each (or to no entry), and say what that did. It writes the
/// record as it reads now with only those entries changed, through the compensating write
/// ([`file_utils::write_file_compensating`]), which no standing taint refuses. That write's own
/// precondition, that what it writes stood before the command, holds for the entries it changes:
/// each goes back to what stood before its pallet's step. It does not hold for every other entry,
/// which a later step of the same run may have written, even with the failed folder sync that
/// recorded the taint: such an entry is written as it already reads, so nothing becomes visible
/// that was not, and a gc root kept longer only keeps more history.
pub fn put_back_owed_roots(record: &BTreeMap<String, String>, owed: &[String]) -> RootsWrite {
    let now = match read_mirror_roots() {
        Ok(now) => now,
        Err(unread) => return RootsWrite::Unread(unread),
    };
    let mut wanted = now.clone();
    for pallet in owed {
        match record.get(pallet) {
            Some(root) => wanted.insert(pallet.clone(), root.clone()),
            None => wanted.remove(pallet),
        };
    }
    if wanted == now {
        return RootsWrite::AlreadyRead;
    }

    write_compensating(&wanted)
}

fn roots_bytes(roots: &BTreeMap<String, String>) -> Vec<u8> {
    let lines = roots.iter().map(|(pallet, parcel)| format!("{} {}", pallet, parcel));

    record_utils::to_bytes(MIRROR_ROOTS_RECORD, MIRROR_ROOTS_VERSION, lines)
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let lines = record_utils::from_bytes(bytes, MIRROR_ROOTS_RECORD, MIRROR_ROOTS_VERSION)
        .map_err(|e| format!("it {}", e.describe().trim_end_matches('.')))?;

    let mut roots = BTreeMap::new();
    for line in &lines {
        let (pallet, parcel) = line.split_once(' ')
            .filter(|(pallet, parcel)| pallet_utils::validate_pallet_name(pallet).is_ok()
                && object_utils::is_object_hash(parcel))
            .ok_or(format!("the line \"{}\" is not a pallet name and a parcel hash", line.escape_debug()))?;

        if roots.insert(pallet.to_string(), parcel.to_string()).is_some() {
            return Err(format!("it names the pallet \"{}\" twice", pallet));
        }
    }

    Ok(roots)
}

fn unreadable(reason: &str) -> String {
    format!(
        "The git mirror's roots record \"{}\" cannot be read ({}). It only keeps history the remote \
        can serve again, so deleting it loses no data: delete it, then run this command again.",
        mirror_roots_path().display(), reason
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    struct Scratch {
        _scope: StorageRootScope,
        root: PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            let root = std::env::temp_dir().join(format!("forklift-mirror-test-{}-{}", name, std::process::id()));
            let _ = file_utils::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = file_utils::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn no_record_reads_as_empty_and_a_written_record_reads_back() {
        let _scratch = Scratch::new("round-trip");
        assert!(read_mirror_roots().unwrap().is_empty());

        let roots = BTreeMap::from([
            ("main".to_string(), "a1".repeat(32)),
            ("rel/x".to_string(), "b2".repeat(32)),
        ]);
        write_mirror_roots(&roots).unwrap();

        assert_eq!(
            std::fs::read(mirror_roots_path()).unwrap(),
            record_utils::with_digest(
                format!("/forklift-mirror-roots 2\nmain {}\nrel/x {}\n", "a1".repeat(32), "b2".repeat(32)).into_bytes()
            )
        );
        assert_eq!(read_mirror_roots().unwrap(), roots);
    }

    /// A mirror step refused after the record was written puts the record back even when a taint
    /// was recorded elsewhere since that write: the put-back stood before the step, so the taint
    /// does not refuse it, and the refusal says the record was put back. An ordinary write of the
    /// record beside it is still refused.
    /// Revert: put the record back through the ordinary write → the put-back is refused, the
    /// record keeps the step's entry, and the message says it could not be put back → red.
    #[test]
    fn a_refused_step_puts_the_record_back_under_a_taint_recorded_since_it_was_written() {
        let _serial = crate::util::taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::util::taint_utils::activate();
        let _scratch = Scratch::new("put-back-under-taint");

        let before = BTreeMap::from([("main".to_string(), "a1".repeat(32))]);
        write_mirror_roots(&before).unwrap();
        let mut stepped = before.clone();
        stepped.insert("rel/x".to_string(), "b2".repeat(32));
        write_mirror_roots(&stepped).unwrap();

        let taint_dir = crate::globals::forklift_root().join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(
            taint_dir.join("taint-99999-0"),
            crate::util::taint_utils::taint_file_bytes(["objects/zz/recorded-elsewhere"]),
        ).unwrap();

        let ordinary = write_mirror_roots(&stepped).expect_err("an ordinary write of the record is refused under the taint");
        assert!(ordinary.contains(crate::util::taint_utils::GATE_TAINT_MARKER), "got: {}", ordinary);

        assert_eq!(put_back_mirror_roots(&before), RootsWrite::Landed);
        assert_eq!(read_mirror_roots().unwrap(), before, "the record is put back");

        // A record that already reads as `before` is left alone.
        assert_eq!(put_back_mirror_roots(&before), RootsWrite::AlreadyRead);

        // An unreadable record is written back whole, under the taint too.
        std::fs::write(mirror_roots_path(), b"not a record").unwrap();
        assert_eq!(put_back_mirror_roots(&before), RootsWrite::Landed);
        assert_eq!(read_mirror_roots().unwrap(), before, "an unreadable record is put back");

        // The run's last write leaves a record that already holds what it would write alone, and
        // still refuses one that would change it.
        assert_eq!(update_mirror_roots(&before), RootsWrite::AlreadyRead, "an unchanged record is not rewritten");
        let changed = update_mirror_roots(&stepped).failure().expect("a changed record is still refused under the taint");
        assert!(changed.starts_with("The mirror record could not be written: "), "got: {}", changed);
        assert!(changed.contains(crate::util::taint_utils::GATE_TAINT_MARKER), "got: {}", changed);
        assert_eq!(read_mirror_roots().unwrap(), before);
    }

    /// An owed put-back lands under a standing taint that refuses the run's ordinary last write,
    /// and changes only the owed entries: one owed pallet is put back to its prior root, another
    /// with no prior root loses its entry, and an entry only the run's memory holds stays out.
    /// Revert: write the owed entries through the ordinary write → refused under the taint, the
    /// record keeps the stepped heads → red. Revert: write the whole record in memory → the entry
    /// only memory holds lands → red.
    #[test]
    fn an_owed_put_back_lands_under_a_taint_and_changes_only_the_owed_entries() {
        let _serial = crate::util::taint_utils::ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::util::taint_utils::activate();
        let _scratch = Scratch::new("owed-put-back");

        let on_disk = BTreeMap::from([
            ("main".to_string(), "b2".repeat(32)),
            ("feat".to_string(), "c3".repeat(32)),
            ("new".to_string(), "d4".repeat(32)),
        ]);
        write_mirror_roots(&on_disk).unwrap();
        // In memory: main put back to its prior root, new (no prior root) dropped, and an entry the
        // record on disk does not hold.
        let record = BTreeMap::from([
            ("main".to_string(), "a1".repeat(32)),
            ("feat".to_string(), "c3".repeat(32)),
            ("memory".to_string(), "e5".repeat(32)),
        ]);
        let owed = ["main".to_string(), "new".to_string()];

        let taint_dir = crate::globals::forklift_root().join("taint");
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(
            taint_dir.join("taint-99999-0"),
            crate::util::taint_utils::taint_file_bytes(["objects/zz/recorded-during-the-run"]),
        ).unwrap();

        assert_eq!(put_back_owed_roots(&record, &owed), RootsWrite::Landed);
        assert_eq!(read_mirror_roots().unwrap(), BTreeMap::from([
            ("main".to_string(), "a1".repeat(32)),
            ("feat".to_string(), "c3".repeat(32)),
        ]));

        // Already put back: nothing rewritten.
        assert_eq!(put_back_owed_roots(&record, &owed), RootsWrite::AlreadyRead);
        // The run's ordinary last write is still refused.
        assert!(matches!(update_mirror_roots(&record), RootsWrite::Failed(_)));
    }

    /// Every way a write of the record can go, with what each says as the run's last write, whether
    /// it leaves an owed put-back to try again, and what it says as the write that settled one:
    /// each read-back is said as it is, nothing is promised of a later run, and a write that
    /// failed is never said as having put the record back durably.
    /// Revert: say a landed-but-unsynced write as plainly put back, or skip the retry after an
    /// unreadable read-back → red.
    #[test]
    fn each_write_of_the_record_says_only_what_reading_it_back_shows() {
        let failed = || "Disk full.".to_string();
        let cases: [(RootsWrite, Option<&str>, bool, &str); 6] = [
            (RootsWrite::AlreadyRead, None, true, "The mirror record reads as put back."),
            (RootsWrite::Landed, None, true, "The mirror record was put back."),
            (
                RootsWrite::NotDurable(failed()),
                Some("The mirror record holds what this run wrote, but it may not be on disk durably yet (Disk full.)."),
                true,
                "The mirror record reads as put back, but it may not be on disk durably yet (Disk full.).",
            ),
            (
                RootsWrite::Failed(failed()),
                Some("The mirror record could not be written: Disk full."),
                false,
                "The mirror record could not be put back: Disk full.",
            ),
            (
                RootsWrite::Unknown { failed: failed(), unread: "Unreadable.".to_string() },
                Some("Whether the mirror record was written is not known: Disk full. It could not be read back: Unreadable."),
                false,
                "Whether the mirror record was put back is not known: Disk full. It could not be read back: Unreadable.",
            ),
            (
                RootsWrite::Unread("Unreadable.".to_string()),
                Some("Unreadable."),
                false,
                "Whether the mirror record was put back is not known: Unreadable.",
            ),
        ];

        for (write, failure, holds, put_back) in cases {
            assert_eq!(write.failure().as_deref(), failure, "{:?}", write);
            assert_eq!(write.holds_what_was_wanted(), holds, "{:?}", write);
            assert_eq!(write.put_back(), put_back, "{:?}", write);
        }
    }

    /// Anything this build does not understand is refused, naming the record and the way out,
    /// never read as fewer roots.
    #[test]
    fn a_record_this_build_cannot_read_is_refused_with_the_step_to_delete_it() {
        let _scratch = Scratch::new("refused");
        let hash = "c3".repeat(32);

        let digested = |text: String| record_utils::with_digest(text.into_bytes());
        for (content, reason) in [
            (Vec::new(), "no format header".to_string()),
            (format!("forklift mirror roots 1\nmain {}\n", hash).into_bytes(), "no format header".to_string()),
            (digested(format!("/forklift-mirror-roots 3\nmain {}\n", hash)), "/forklift-mirror-roots 3".to_string()),
            (format!("/forklift-mirror-roots 1\nmain {}\n", hash).into_bytes(), "/forklift-mirror-roots 1".to_string()),
            (format!("/forklift-mirror-roots 2\nmain {}\n", hash).into_bytes(), "damaged".to_string()),
            (digested(format!("/forklift-mirror-roots 2\nmain {}\nmain {}\n", hash, hash)), "twice".to_string()),
            (digested("/forklift-mirror-roots 2\nmain not-a-hash\n".to_string()), "main not-a-hash".to_string()),
            (digested(format!("/forklift-mirror-roots 2\nBad {}\n", hash)), format!("Bad {}", hash)),
            (digested(format!("/forklift-mirror-roots 2\nmain{}\n", hash)), format!("main{}", hash)),
        ] {
            std::fs::write(mirror_roots_path(), &content).unwrap();
            let error = read_mirror_roots().expect_err(&String::from_utf8_lossy(&content));
            assert!(error.contains(&reason), "{:?}: {}", String::from_utf8_lossy(&content), error);
            assert!(error.contains(&mirror_roots_path().display().to_string()), "{}", error);
            assert!(error.contains("delete it"), "{}", error);
        }

        std::fs::write(mirror_roots_path(), record_utils::with_digest(b"/forklift-mirror-roots 2\n\xff\n".to_vec())).unwrap();
        assert!(read_mirror_roots().unwrap_err().contains("not valid UTF-8"));
    }
}
