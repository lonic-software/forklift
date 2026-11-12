use std::path::{Path, PathBuf};
use crate::globals::bay_root;
use crate::util::{file_utils, object_utils, record_utils};

/// The name of the parked-parcels file (inside the forklift root folder): a state record holding
/// one parcel hash per entry, the last one being the most recently parked. Parked parcels are
/// regular parcel objects (their parent is the head they were parked on); this file is the only
/// thing that references them.
const FILE_NAME_PARKED: &str = "parked";

/// The state record kind of the parked-parcels file.
const PARKED_RECORD: &str = "parked";

/// The version of the parked-parcels record.
const PARKED_VERSION: u32 = 1;

/// Get the path of the parked-parcels file (bay-local: parked work belongs to the bay).
fn get_parked_path() -> PathBuf {
    bay_root().join(FILE_NAME_PARKED)
}

/// Read the parked parcel hashes (oldest first) from a specific bay-local state dir, independent
/// of the active bay. For a caller enumerating every bay's state — see
/// `bay_utils::all_bay_state_dirs` — never just the active one; [`read_parked`] is this, scoped
/// to the active bay.
///
/// # Arguments
/// * `dir` - The bay-local state dir to read: the main tree's own `forklift_root()`, or a named
///           bay's `bay_utils::bay_state_dir`.
///
/// # Returns
/// * `Ok(Vec<String>)` - The parked parcel hashes.
/// * `Err(String)`     - If the file exists but could not be read or holds invalid data.
pub fn read_parked_in(dir: &Path) -> Result<Vec<String>, String> {
    let path = dir.join(FILE_NAME_PARKED);

    if !path.exists() {
        return Ok(Vec::new());
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    let hashes = record_utils::from_bytes(&bytes, PARKED_RECORD, PARKED_VERSION).map_err(|e| format!(
        "The parked-parcels file \"{}\" {} Fix it by hand.", path.to_string_lossy(), e.describe()
    ))?;

    if !hashes.iter().all(|hash| object_utils::is_object_hash(hash)) {
        return Err(format!(
            "The parked-parcels file \"{}\" is malformed; fix it by hand.",
            path.to_string_lossy()
        ));
    }

    Ok(hashes)
}

/// Read the parked parcel hashes (oldest first) of the active bay.
///
/// # Returns
/// * `Ok(Vec<String>)` - The parked parcel hashes.
/// * `Err(String)`     - If the file exists but could not be read or holds invalid data.
pub fn read_parked() -> Result<Vec<String>, String> {
    read_parked_in(&bay_root())
}

/// Write the parked parcel hashes (atomically).
///
/// # Arguments
/// * `hashes` - The parked parcel hashes (oldest first). An empty list removes the file.
///
/// # Returns
/// * `Ok(())`      - If the file was written (or removed).
/// * `Err(String)` - If the file could not be written.
pub fn write_parked(hashes: &[String]) -> Result<(), String> {
    let path = get_parked_path();

    if hashes.is_empty() {
        return match file_utils::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Error while removing \"{}\": {}", path.to_string_lossy(), e)),
        };
    }

    file_utils::write_file_atomically(&path, &parked_bytes(hashes))
}

/// The bytes [`write_parked`] writes for `hashes`.
pub(crate) fn parked_bytes<S: AsRef<str>>(hashes: &[S]) -> Vec<u8> {
    record_utils::to_bytes(PARKED_RECORD, PARKED_VERSION, hashes.iter().map(AsRef::as_ref))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-parked-{}-{}", tag, std::process::id()));
        let _ = file_utils::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn hash(seed: &str) -> String {
        object_utils::hash_object_bytes(seed.as_bytes())
    }

    /// The parked list reads back in order from the bytes the writer writes.
    #[test]
    fn the_parked_record_round_trips() {
        let dir = scratch("round-trip");
        let hashes = vec![hash("first"), hash("second")];
        std::fs::write(dir.join(FILE_NAME_PARKED), parked_bytes(&hashes)).unwrap();

        assert_eq!(read_parked_in(&dir).unwrap(), hashes);
        let _ = file_utils::remove_dir_all(&dir);
    }

    /// A parked file without a header, the shape written before it was a record, is refused, not
    /// read as hash lines.
    #[test]
    fn a_parked_file_without_a_header_is_refused() {
        let dir = scratch("unversioned");
        std::fs::write(dir.join(FILE_NAME_PARKED), format!("{}\n", hash("bare"))).unwrap();

        let error = read_parked_in(&dir).unwrap_err();
        assert!(error.contains("has no format header"), "{}", error);
        let _ = file_utils::remove_dir_all(&dir);
    }

    #[test]
    fn a_parked_entry_that_is_not_a_hash_is_refused() {
        let dir = scratch("not-a-hash");
        std::fs::write(dir.join(FILE_NAME_PARKED), parked_bytes(&[hash("good").as_str(), "not-a-hash"])).unwrap();

        let error = read_parked_in(&dir).unwrap_err();
        assert!(error.contains("is malformed"), "{}", error);
        let _ = file_utils::remove_dir_all(&dir);
    }
}
