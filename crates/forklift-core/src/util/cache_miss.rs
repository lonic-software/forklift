//! Reading content a cache warehouse lacks: an object the sealed-object record holds by hash is
//! fetched from the origin when a read meets it absent, stored, and read again.
//!
//! The fetch is a hook the head registers ([`register`]); only the CLI does. Without one, or where
//! fetching cannot be done safely, a read of a recorded object fails as it always has, naming why
//! it was not fetched. The refill itself runs here, on the reading thread: see
//! [`sealed_utils::refill`] for the order its writes land in.
//!
//! The hook is off while this thread holds the warehouse's ref lock (the refill takes it to write
//! the record, and it does not nest), inside a bulk store session (whose writes are not durable
//! until it closes, while the record's removal would be), and inside a delta reconstruction (which
//! reads a base, not an object a caller asked for). Each fetch is one round trip, and each miss
//! re-reads the record: a read of a whole old tree pays both once per object it lacks.

use std::sync::OnceLock;
use crate::error::{CoreError, RefusalCode};
use crate::util::{file_utils, pack_utils, scope_utils, sealed_utils};
use crate::util::lock_utils::{RefLock, StoreAccessLock};

/// What a [`FetchHook`] found for one object.
pub enum Fetched {
    /// The object's bytes, as the origin sent them.
    Bytes(Vec<u8>),
    /// The origin could not be reached: the read refuses as a cache miss.
    Unreachable {
        /// The origin's URL, for the next step.
        origin: String,
        /// Why it could not be reached.
        why: String,
    },
    /// The fetch failed some other way, such as the origin refusing it.
    Failed(String),
}

/// Fetch one object, by hash, from the warehouse's origin. Called on the reading thread, with the
/// storage root that read resolves.
pub type FetchHook = Box<dyn Fn(&str) -> Fetched + Send + Sync>;

static HOOK: OnceLock<FetchHook> = OnceLock::new();

/// Register the process's fetch hook. Only the first registration takes effect.
pub fn register(hook: FetchHook) {
    let _ = HOOK.set(hook);
}

thread_local! {
    /// How many [`Unfetched`] scopes this thread is inside.
    static UNFETCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A scope whose reads never fetch: an absent recorded object reads as absent, as it does with no
/// hook. For a walk that reads to learn what the store holds, such as a closure check, which
/// treats a failed read as an absence and would otherwise fetch everything it meant to find
/// missing. Per thread: a walk that fans out enters one on each worker.
pub struct Unfetched(());

impl Unfetched {
    pub fn enter() -> Unfetched {
        UNFETCHED.with(|depth| depth.set(depth.get() + 1));
        Unfetched(())
    }
}

impl Drop for Unfetched {
    fn drop(&mut self) {
        UNFETCHED.with(|depth| depth.set(depth.get().saturating_sub(1)));
    }
}

/// Refill `hash`, which a read just found absent, when it is recorded in a cache warehouse.
///
/// # Returns
/// * `Ok(true)`    - It was fetched and stored: read it again.
/// * `Ok(false)`   - It is not this module's to fetch: the read is in an [`Unfetched`] scope, the
///                   warehouse is not a cache, or `hash` is not recorded. The read reports the
///                   absence as it always has.
/// * `Err(String)` - Why it was not fetched. A [`RefusalCode::CacheMiss`] refusal when the origin
///                   could not be reached.
pub(crate) fn refill_recorded(hash: &str) -> Result<bool, String> {
    refill_with(HOOK.get(), hash)
}

/// [`refill_recorded`] with the fetch hook `hook`.
fn refill_with(hook: Option<&FetchHook>, hash: &str) -> Result<bool, String> {
    if UNFETCHED.with(|depth| depth.get() > 0) {
        return Ok(false);
    }

    // A warehouse whose mode cannot be read is not judged a cache here: the command that needs
    // the mode reports why it cannot be read.
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(false);
    }

    let record = sealed_utils::read_sealed()
        .map_err(|why| format!("Object {} is absent, and the sealed-object record could not be read: {}", hash, why))?;

    if !record.contains(hash) {
        return Ok(false);
    }

    let unfetched = |why: &str| Err(format!(
        "Object {} is not in this cache warehouse, and could not be fetched from its origin here: {}.",
        hash, why
    ));

    let Some(hook) = hook else {
        return unfetched("this program does not fetch content a cache warehouse lacks");
    };

    if RefLock::held_by_this_thread() {
        return unfetched("the read ran while the warehouse's ref lock was held");
    }

    if file_utils::is_bulk_session_open() {
        return unfetched("the read ran inside a batch of store writes");
    }

    if pack_utils::in_reconstruction() {
        return unfetched("the read was of a delta's base");
    }

    // Probed before the fetch, so a deleter's hold refuses the read before it pays for a transfer
    // it could not store, and taken after it, so no deleter waits out the round trip.
    drop(StoreAccessLock::try_shared()?);

    match hook(hash) {
        Fetched::Bytes(bytes) => {
            let hold = StoreAccessLock::try_shared()?;
            sealed_utils::refill(&hold, hash, &bytes)
                .map_err(|why| format!("Could not store object {}, fetched from the origin: {}", hash, why))?;

            Ok(true)
        }
        Fetched::Unreachable { origin, why } => Err(CoreError::refusal(
            RefusalCode::CacheMiss,
            format!(
                "Object {} is not in this cache warehouse, and its origin could not be reached to fetch it: {}",
                hash, CoreError::from(why)
            ),
            format!("Run the command again once the origin ({}) can be reached.", origin),
        ).into()),
        Fetched::Failed(why) => Err(format!(
            "Object {} is not in this cache warehouse, and fetching it from the origin failed: {}",
            hash, why
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::globals::StorageRootScope;
    use crate::model::blob::Blob;
    use crate::model::tree_item::TreeItem;
    use crate::util::object_utils;
    use crate::util::sealed_utils::SealOp;
    use crate::util::warehouse_utils::{self, WarehouseMode};

    /// A cache warehouse whose root tree (`src/a`, `web/b`) is recorded and absent, as is its
    /// `web` subtree, which nothing records. Returns the root's hash and bytes and the `web` hash.
    struct Fixture {
        _scope: StorageRootScope,
        root: String,
        bytes: Vec<u8>,
        web: String,
    }

    fn fixture(name: &str) -> Fixture {
        let dir = std::env::temp_dir().join(format!("forklift-cache-miss-{}-{}", name, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let scope = StorageRootScope::enter(&dir);
        warehouse_utils::mark_for_test(WarehouseMode::Cache);

        let src = tree(&[("a", &blob("a"), DirEntryType::Normal)]);
        let web = tree(&[("b", &blob("b"), DirEntryType::Normal)]);
        let root = tree(&[("src", &src, DirEntryType::Tree), ("web", &web, DirEntryType::Tree)]);
        let bytes = file_utils::retrieve_object_by_hash_uncached(&root).unwrap();

        sealed_utils::append(&[(SealOp::Record, &root)]).unwrap();
        remove(&root);
        remove(&web);

        Fixture { _scope: scope, root, bytes, web }
    }

    fn blob(content: &str) -> String {
        let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
        object.store().unwrap();
        object.hash
    }

    fn tree(entries: &[(&str, &str, DirEntryType)]) -> String {
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for (name, hash, item_type) in entries {
            tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
        }
        let mut object = LooseObjectBuilder::build_tree(&tree);
        object.store().unwrap();
        object.hash
    }

    fn remove(hash: &str) {
        let (folder, file) = file_utils::get_path_for_object(hash).unwrap();
        std::fs::remove_file(std::path::Path::new(&folder).join(file)).unwrap();
    }

    fn held(hash: &str) -> bool {
        file_utils::raw_object_present(hash).unwrap()
    }

    /// A hook answering `fetched` for every hash, and how many times it was asked.
    fn hook(fetched: impl Fn() -> Fetched + Send + Sync + 'static) -> (FetchHook, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        (Box::new(move |_| { counted.fetch_add(1, Ordering::SeqCst); fetched() }), calls)
    }

    /// The record, compared against exactly `hashes`.
    fn assert_recorded(hashes: &[&str]) {
        let mut expected = sealed_utils::header_bytes();
        for hash in hashes {
            expected.extend(sealed_utils::encode_frame(SealOp::Record, hash).unwrap());
        }
        assert_eq!(sealed_utils::read_sealed().unwrap(), sealed_utils::parse(&expected).unwrap().set,
            "the record must name exactly {:?}", hashes);
    }

    // Revert: drop the push-down → the record misses the absent `web` subtree.
    #[test]
    fn a_recorded_absence_is_stored_and_its_record_pushed_down() {
        let fixture = fixture("refill");
        let bytes = fixture.bytes.clone();
        let (hook, calls) = hook(move || Fetched::Bytes(bytes.clone()));

        assert!(refill_with(Some(&hook), &fixture.root).unwrap());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(held(&fixture.root));
        assert_recorded(&[&fixture.web]);
        assert!(object_utils::load_tree(&fixture.root).is_ok());
    }

    // Revert: drop the ref-lock check → the refill waits on the lock this thread holds; drop the
    // scope check → the hook is asked inside it.
    #[test]
    fn nothing_is_fetched_under_a_ref_lock_or_in_an_unfetched_scope() {
        let fixture = fixture("gated");
        let bytes = fixture.bytes.clone();
        let (hook, calls) = hook(move || Fetched::Bytes(bytes.clone()));

        let lock = RefLock::acquire().unwrap();
        let refused = refill_with(Some(&hook), &fixture.root).unwrap_err();
        assert!(refused.contains("ref lock"), "{}", refused);
        drop(lock);

        let unfetched = Unfetched::enter();
        assert!(!refill_with(Some(&hook), &fixture.root).unwrap());
        drop(unfetched);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!held(&fixture.root));
        assert_recorded(&[&fixture.root]);

        assert!(refill_with(Some(&hook), &fixture.root).unwrap(), "out of both, it is fetched");
    }

    // Revert: refuse while any thread holds the ref lock → the read refuses instead of waiting out
    // another thread's hold, which lasts until the hook is asked.
    #[test]
    fn another_threads_ref_lock_is_waited_out() {
        let fixture = fixture("held-elsewhere");
        let bytes = fixture.bytes.clone();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let release = std::sync::Mutex::new(release);
        let (hook, calls) = hook(move || {
            let _ = release.lock().unwrap().send(());
            Fetched::Bytes(bytes.clone())
        });

        let root = crate::globals::current_scope_root().unwrap();
        let (locked, held) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _scope = StorageRootScope::enter(&root);
            let _lock = RefLock::acquire().unwrap();
            locked.send(()).unwrap();
            let _ = released.recv_timeout(std::time::Duration::from_secs(30));
        });

        held.recv().unwrap();
        let refilled = refill_with(Some(&hook), &fixture.root);
        holder.join().unwrap();

        assert!(refilled.unwrap(), "the refill waits for the other thread's hold to end");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_recorded(&[&fixture.web]);
    }

    // Revert: skip the mode check → the full warehouse's absence is fetched; skip the record
    // check → the unrecorded `web` is.
    #[test]
    fn only_a_recorded_absence_in_a_cache_is_fetched() {
        let fixture = fixture("unrecorded");
        let (hook, calls) = hook(|| Fetched::Failed("asked".to_string()));

        assert!(!refill_with(Some(&hook), &fixture.web).unwrap());

        warehouse_utils::mark_for_test(WarehouseMode::Full);
        assert!(!refill_with(Some(&hook), &fixture.root).unwrap());

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    // Revert: build the unreachable arm as a plain error → no `cache_miss` code.
    #[test]
    fn an_unreachable_origin_is_a_cache_miss_naming_it() {
        let fixture = fixture("unreachable");
        let (hook, _) = hook(|| Fetched::Unreachable {
            origin: "http://origin.example".to_string(),
            why: "connection refused".to_string(),
        });

        match CoreError::from(refill_with(Some(&hook), &fixture.root).unwrap_err()) {
            CoreError::Refusal { code, message, next_step, .. } => {
                assert_eq!(code, RefusalCode::CacheMiss);
                assert!(message.contains("connection refused"), "{}", message);
                assert!(next_step.contains("http://origin.example"), "{}", next_step);
            }
            other => panic!("not a refusal: {:?}", other),
        }

        assert!(!held(&fixture.root));
        assert_recorded(&[&fixture.root]);
    }

    // Revert: skip the verification → the push-down records the other tree's absent blob before
    // the store refuses the bytes.
    #[test]
    fn bytes_that_are_not_the_object_are_refused_and_nothing_written() {
        let fixture = fixture("corrupt");
        let lost = blob("z");
        let other = tree(&[("z", &lost, DirEntryType::Normal)]);
        let bytes = file_utils::retrieve_object_by_hash_uncached(&other).unwrap();
        remove(&other);
        remove(&lost);
        let (hook, _) = hook(move || Fetched::Bytes(bytes.clone()));

        let refused = refill_with(Some(&hook), &fixture.root).unwrap_err();
        assert_recorded(&[&fixture.root]);
        assert!(!held(&fixture.root));
        assert!(refused.contains("corrupt"), "{}", refused);
    }
}
