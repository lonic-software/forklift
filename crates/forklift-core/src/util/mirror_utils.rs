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
use std::path::PathBuf;
use crate::globals::forklift_root;
use crate::util::{file_utils, object_utils, pallet_utils};

/// The record's file name, at the shared `.forklift` root: every bay mirrors into one place.
pub const FILE_NAME_MIRROR_ROOTS: &str = "mirror-roots";

/// The record's first line. A later format gets a new version, which this build refuses to read.
const HEADER: &str = "forklift mirror roots 1";

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
    let mut content = format!("{}\n", HEADER);
    for (pallet, parcel) in roots {
        content.push_str(&format!("{} {}\n", pallet, parcel));
    }

    file_utils::write_file_atomically(&mirror_roots_path(), content.as_bytes())
}

fn parse(bytes: &[u8]) -> Result<BTreeMap<String, String>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "it is not UTF-8".to_string())?;
    let mut lines = text.split_terminator('\n');

    match lines.next() {
        Some(HEADER) => {}
        Some(other) => return Err(format!("its first line is \"{}\", not \"{}\"", other, HEADER)),
        None => return Err("it is empty".to_string()),
    }

    let mut roots = BTreeMap::new();
    for line in lines {
        let (pallet, parcel) = line.split_once(' ')
            .filter(|(pallet, parcel)| pallet_utils::validate_pallet_name(pallet).is_ok()
                && object_utils::is_object_hash(parcel))
            .ok_or(format!("the line \"{}\" is not a pallet name and a parcel hash", line))?;

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
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = StorageRootScope::enter(&root);
            Scratch { _scope: scope, root }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
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
            std::fs::read_to_string(mirror_roots_path()).unwrap(),
            format!("forklift mirror roots 1\nmain {}\nrel/x {}\n", "a1".repeat(32), "b2".repeat(32))
        );
        assert_eq!(read_mirror_roots().unwrap(), roots);
    }

    /// Anything this build does not understand is refused, naming the record and the way out,
    /// never read as fewer roots.
    #[test]
    fn a_record_this_build_cannot_read_is_refused_with_the_step_to_delete_it() {
        let _scratch = Scratch::new("refused");
        let hash = "c3".repeat(32);

        for (content, reason) in [
            (String::new(), "it is empty".to_string()),
            (format!("forklift mirror roots 2\nmain {}\n", hash), "forklift mirror roots 2".to_string()),
            (format!("forklift mirror roots 1\nmain {}\nmain {}\n", hash, hash), "twice".to_string()),
            ("forklift mirror roots 1\nmain not-a-hash\n".to_string(), "main not-a-hash".to_string()),
            (format!("forklift mirror roots 1\nBad {}\n", hash), format!("Bad {}", hash)),
            (format!("forklift mirror roots 1\nmain{}\n", hash), format!("main{}", hash)),
        ] {
            std::fs::write(mirror_roots_path(), &content).unwrap();
            let error = read_mirror_roots().expect_err(&content);
            assert!(error.contains(&reason), "{:?}: {}", content, error);
            assert!(error.contains(&mirror_roots_path().display().to_string()), "{}", error);
            assert!(error.contains("delete it"), "{}", error);
        }

        std::fs::write(mirror_roots_path(), [0xff, 0xfe]).unwrap();
        assert!(read_mirror_roots().unwrap_err().contains("not UTF-8"));
    }
}
