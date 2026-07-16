use std::ops::Add;
use std::path::Path;
use crate::enums::object_type::ObjectType;
use crate::util::{file_utils, object_utils};

/// A loose object.
/// Compress it before saving it to the object store.
pub struct LooseObject {
    pub content: Vec<u8>,
    pub object_type: ObjectType,
    pub hash: String
}

impl LooseObject {
    // TODO: handle in buffers instead of all at once. use zstd::stream::Encoder
    /// Compress the object.
    ///
    /// # Returns
    /// The compressed bytes of the object.
    pub fn compress(&mut self) -> Result<Vec<u8>, String> {
        zstd::encode_all(self.content.as_slice(), 0)
            .map_err(|e| format!("Error while compressing object: {}", e))
    }

    /// Compress and save the object to the object store.
    ///
    /// # Returns
    /// * `Ok(String, bool)`:
    ///    * `String`: The full path (relative to the root of the warehouse)
    /// where the object is stored.
    ///    * `bool`: True if a new object was stored, false if the object already existed.
    /// * `Err(String)`- The error message, if the operation failed.
    pub fn store(&mut self) -> Result<(String, bool), String> {
        // The whole-object ceiling, on the way in from local authorship (`stack`, `import-git`, a
        // meta write). Only a tree or a recipe can legitimately approach it; blobs and chunks are
        // bounded well below it by construction. Reads never re-store an object, so a grandfathered
        // giant authored before this policy stays readable — this gates new authorship only.
        object_utils::check_object_ceiling(&self.object_type, self.content.len())?;

        let does_exist = file_utils::does_object_exist(&self.hash)?;
        let (path, file_name) = file_utils::get_path_for_object(&self.hash)?;

        if !does_exist {
            let compressed = self.compress()?;
            file_utils::write_object_to_file(Path::new(&path), &file_name, compressed)?;
        }

        Ok((path.add(file_utils::PATH_SEPARATOR).add(&file_name), !does_exist))
    }

    /// Stage this object's write into `batch` instead of writing (and fsyncing) it immediately.
    /// See [`file_utils::WriteBatch`] for why: `stack`'s tree build writes from parallel workers,
    /// where [`file_utils::BulkStoreSession`] cannot safely be shared (see its doc comment).
    ///
    /// Dedupes against writes staged earlier in *this same batch*, not just what is already
    /// durable on disk (DESIGN.html §5.0 D item 10, finding #2): [`file_utils::does_object_exist`]
    /// cannot see a staged-but-not-yet-renamed temp (it has no final name until `batch.finish()`
    /// runs), so without this every repeated occurrence of the same content hash in one batched
    /// walk (many copies of the same vendored asset, say) would independently compress and stage
    /// its own redundant temp — see [`file_utils::WriteBatch::reserve_final_path`]'s doc comment.
    ///
    /// The caller must call `batch.finish()` — and it must return `Ok` — before anything is
    /// allowed to depend on this object's durability or visibility (a ref pointing at it, or a
    /// reader expecting to find it): staging alone makes no promise about either.
    ///
    /// # Returns
    /// * `Ok((String, bool))`:
    ///    * `String`: The full path (relative to the root of the warehouse) where the object
    ///      will be stored once `batch.finish()` runs.
    ///    * `bool`: True if a write was staged, false if the object already existed (or was
    ///      already staged earlier in this batch) and nothing was staged here.
    /// * `Err(String)` - The error message, if the operation failed.
    pub fn store_deferred(&mut self, batch: &file_utils::WriteBatch) -> Result<(String, bool), String> {
        object_utils::check_object_ceiling(&self.object_type, self.content.len())?;

        let (path, file_name) = file_utils::get_path_for_object(&self.hash)?;

        if file_utils::does_object_exist(&self.hash)? {
            return Ok((path.add(file_utils::PATH_SEPARATOR).add(&file_name), false));
        }

        let mut final_path = std::path::PathBuf::from(&path);
        final_path.push(&file_name);

        if !batch.reserve_final_path(&final_path) {
            // Already staged (by an earlier occurrence in this same batch, or a concurrent one
            // that reserved it first) — the dedupe this function exists for, see the doc comment
            // above.
            return Ok((path.add(file_utils::PATH_SEPARATOR).add(&file_name), false));
        }

        let compressed = self.compress()?;
        file_utils::write_object_to_file_deferred(Path::new(&path), &file_name, compressed, batch)?;

        Ok((path.add(file_utils::PATH_SEPARATOR).add(&file_name), true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;

    /// A fresh warehouse root for one test, entered as the active storage-root scope for its
    /// lifetime.
    struct Scratch {
        _scope: StorageRootScope,
        root: std::path::PathBuf,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-loose-object-test-{}-{}-{}", name, std::process::id(), id
            ));
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

    /// Every loose file (of any kind — a real object or a staged `.tmp*` temp) currently sitting
    /// under the object store's fan-out folders, counted recursively.
    fn count_loose_files(objects_root: &Path) -> usize {
        fn walk(dir: &Path, count: &mut usize) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, count);
                } else {
                    *count += 1;
                }
            }
        }

        let mut count = 0;
        walk(objects_root, &mut count);
        count
    }

    /// DESIGN.html §5.0 D item 10, finding #2: `does_object_exist` alone cannot see a
    /// staged-but-not-yet-renamed write — a batch has no way to know, from the store alone,
    /// whether some *other* occurrence of the same content already staged a write earlier in
    /// this same batch. Without `WriteBatch::reserve_final_path`, N occurrences of the same
    /// content hash in one batched walk (many copies of an identical vendored file, say) each
    /// independently decide "not on disk yet" and stage their own full compressed temp — N
    /// redundant temps instead of one. This pins that `store_deferred` now dedupes within the
    /// batch: a burst of "the same content, staged repeatedly" produces exactly one staged
    /// write.
    #[test]
    fn store_deferred_dedupes_repeated_identical_content_within_one_batch() {
        let scratch = Scratch::new("dedupe");
        let objects_root = std::path::PathBuf::from(file_utils::get_path_objects_root());

        let content = vec![0x42u8; 5000];
        let batch = file_utils::WriteBatch::new();

        const OCCURRENCES: usize = 50;
        let mut staged_count = 0;
        let mut hash = String::new();

        for _ in 0..OCCURRENCES {
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.clone() });
            hash = object.hash.clone();
            let (_, staged) = object.store_deferred(&batch).unwrap();
            if staged {
                staged_count += 1;
            }
        }

        assert_eq!(staged_count, 1,
            "only the first of {} identical occurrences may actually stage a write", OCCURRENCES);

        // The proof that matters: only one temp file sits in the store before `finish` runs —
        // not one per occurrence. Pre-fix, this would be `OCCURRENCES` (50) full compressed
        // temps; the fix collapses it to 1.
        let staged_files = count_loose_files(&objects_root);
        assert_eq!(staged_files, 1,
            "exactly one staged temp file may exist before finish, found {}", staged_files);

        batch.finish().unwrap();

        // After finish, exactly one durable object exists, and its content is correct.
        assert!(file_utils::does_object_exist(&hash).unwrap());
        assert_eq!(object_utils::load_blob(&hash).unwrap().content, content);

        let final_files = count_loose_files(&objects_root);
        assert_eq!(final_files, 1, "exactly one durable object file must exist after finish");

        drop(scratch);
    }

    /// The companion case: distinct content hashes must each still stage their own write — the
    /// dedupe is keyed by final path (content hash), not a blanket "only ever stage once".
    #[test]
    fn store_deferred_still_stages_every_distinct_content_hash() {
        let scratch = Scratch::new("dedupe-distinct");
        let objects_root = std::path::PathBuf::from(file_utils::get_path_objects_root());

        let batch = file_utils::WriteBatch::new();
        const DISTINCT: usize = 10;
        let mut staged_count = 0;

        for i in 0..DISTINCT {
            let content = vec![i as u8; 5000];
            let mut object = LooseObjectBuilder::build_blob(&Blob { content });
            let (_, staged) = object.store_deferred(&batch).unwrap();
            if staged {
                staged_count += 1;
            }
        }

        assert_eq!(staged_count, DISTINCT, "every distinct content hash must stage its own write");

        let staged_files = count_loose_files(&objects_root);
        assert_eq!(staged_files, DISTINCT, "every distinct content hash must have its own staged temp");

        batch.finish().unwrap();
        drop(scratch);
    }

    /// Regression (post-merge review of PR #61's finding #2 fix, finding B): `store_deferred`
    /// reserves the final path *before* the two fallible steps that actually fulfil it
    /// (`compress()`, then `write_object_to_file_deferred`). If a caller wins the reservation and
    /// then hits a fallible step, the reservation stays "won" forever with nothing ever pushed
    /// into `pending` — reproduced here directly via `reserve_final_path` (the exact primitive
    /// `store_deferred` uses internally), which faithfully stands in for "task 1 reserved, then a
    /// fallible step failed" without needing to actually force `compress()` or the write itself
    /// to error.
    ///
    /// A second occurrence of the *same* content then loses the race (`reserve_final_path`
    /// returns `false`), reads that as "already staged," and returns `Ok` without staging
    /// anything of its own either — exactly what a concurrent or later task hitting the same hash
    /// does in the real walk. Pre-fix, `finish()` had no way to notice the mismatch: `pending` is
    /// empty for this hash, so it would return `Ok`, and a caller downstream could publish a
    /// shard naming a blob that was never actually written. `finish()` must now refuse instead.
    #[test]
    fn finish_fails_loudly_when_a_reservation_was_never_staged() {
        let scratch = Scratch::new("leaked-reservation");

        let content = vec![0x99u8; 4000];
        let batch = file_utils::WriteBatch::new();

        // "Task 1": wins the reservation for this content's hash, then (as if `compress()` or
        // the write itself had failed) never goes on to actually stage it.
        let (path, file_name) = {
            let object = LooseObjectBuilder::build_blob(&Blob { content: content.clone() });
            file_utils::get_path_for_object(&object.hash).unwrap()
        };
        let mut final_path = std::path::PathBuf::from(&path);
        final_path.push(&file_name);
        assert!(batch.reserve_final_path(&final_path),
            "the simulated first occurrence must win the reservation");

        // "Task 2": the same content arriving afterward, losing the race against the still-live
        // (never released, never fulfilled) reservation above.
        let mut second = LooseObjectBuilder::build_blob(&Blob { content });
        let (_, staged) = second.store_deferred(&batch).unwrap();
        assert!(!staged,
            "a second occurrence must lose the race exactly like the real concurrent interleaving");

        let error = match batch.finish() {
            Ok(()) => panic!(
                "finish() must refuse to publish a batch that leaked a reservation nobody ever \
                staged, not silently succeed"),
            Err(e) => e,
        };
        assert!(error.contains(&*final_path.to_string_lossy()),
            "the error should name the leaked reservation's path: {error}");

        // Nothing was ever durably published for this hash — the whole point of refusing.
        assert!(!file_utils::does_object_exist(&second.hash).unwrap(),
            "the object must not exist on disk after a refused finish()");

        drop(scratch);
    }
}