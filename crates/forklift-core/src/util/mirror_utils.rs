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

/// Write the record as `roots` only when it does not already read as `roots`: a run whose steps
/// already wrote what it holds rewrites nothing, so a taint recorded meanwhile, which refuses the
/// ordinary write, does not fail the run. A write can land and still return an error, when its
/// folder sync fails, so a failed write is read back, and the error is a sentence naming the
/// record that says only what that read shows: the record holds `roots` but may not be durable
/// yet, it could not be written, or whether it was is not known ([`failed_write`]).
pub fn update_mirror_roots(roots: &BTreeMap<String, String>) -> Result<(), String> {
    if read_mirror_roots().ok().as_ref() == Some(roots) {
        return Ok(());
    }

    let Err(failed) = write_mirror_roots(roots) else { return Ok(()) };
    Err(failed_write(&failed, read_mirror_roots().map(|now| now == *roots)))
}

/// The sentence for a write of the record that failed with `failed`, from what reading it back
/// showed: whether it holds what was written, or why it could not be read. It says only that, and
/// nothing of what a later run will do: a taint that refused the write refuses the next one too,
/// until it is healed.
fn failed_write(failed: &str, read_back: Result<bool, String>) -> String {
    match read_back {
        Ok(true) => format!("The mirror record holds what this run wrote, but it may not be on disk durably yet ({}).", failed),
        Ok(false) => format!("The mirror record could not be written: {}", failed),
        Err(unread) => format!("Whether the mirror record was written is not known: {} It could not be read back: {}", failed, unread),
    }
}

/// Put the record back to `before`, what it held before a mirror step that then failed with
/// `error`, and return `error` with what happened to the record appended
/// ([`file_utils::put_back_as_read`]). The write is a compensating one
/// ([`file_utils::write_file_compensating`]): it puts back what stood before the step, so a taint
/// standing for the storage root does not refuse it, whoever recorded that taint.
pub fn put_back_mirror_roots(error: String, before: &BTreeMap<String, String>) -> String {
    file_utils::put_back_as_read(error, "mirror record", before, read_mirror_roots, |before| {
        file_utils::write_file_compensating(&mirror_roots_path(), &roots_bytes(before))
    })
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

        let reported = put_back_mirror_roots("The step was refused.".to_string(), &before);

        assert_eq!(read_mirror_roots().unwrap(), before, "the record is put back");
        assert_eq!(reported, "The step was refused. The mirror record was put back.");

        // A record that already reads as `before` is left alone and goes unmentioned.
        assert_eq!(put_back_mirror_roots("Refused again.".to_string(), &before), "Refused again.");

        // The run's last write leaves a record that already holds what it would write alone, and
        // still refuses one that would change it.
        update_mirror_roots(&before).expect("an unchanged record is not rewritten");
        let changed = update_mirror_roots(&stepped).expect_err("a changed record is still refused under the taint");
        assert!(changed.starts_with("The mirror record could not be written: "), "got: {}", changed);
        assert!(changed.contains(crate::util::taint_utils::GATE_TAINT_MARKER), "got: {}", changed);
        assert_eq!(read_mirror_roots().unwrap(), before);
    }

    /// Each read-back is said as it is, and nothing is promised of a later run.
    #[test]
    fn a_failed_write_says_only_what_reading_the_record_back_shows() {
        assert_eq!(
            failed_write("Disk full.", Ok(true)),
            "The mirror record holds what this run wrote, but it may not be on disk durably yet (Disk full.)."
        );
        assert_eq!(failed_write("Disk full.", Ok(false)), "The mirror record could not be written: Disk full.");
        assert_eq!(
            failed_write("Disk full.", Err("Unreadable.".to_string())),
            "Whether the mirror record was written is not known: Disk full. It could not be read back: Unreadable."
        );
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
