//! Reading content a cache warehouse lacks: an object the sealed-object record holds by hash is
//! fetched from the origin when a read meets it absent, stored, and read again; and a mutation's
//! presence phase ([`ensure_present`]) fetches every recorded object it is about to read in one
//! batch, before it changes anything.
//!
//! The fetch is a hook the head registers ([`register`]); only the CLI does. Without one, or where
//! fetching cannot be done safely, a read of a recorded object fails as it always has, naming why
//! it was not fetched. The refill itself runs here, on the calling thread: see
//! [`sealed_utils::refill_all`] for the order its writes land in.
//!
//! The hook is off while this thread holds the warehouse's ref lock (the refill takes it to write
//! the record, and it does not nest), inside a bulk store session (whose writes are not durable
//! until it closes, while the record's removal would be), and inside a delta reconstruction (which
//! reads a base, not an object a caller asked for). A read that misses is one fetch of one object,
//! and each miss re-reads the record: a read of a whole old tree pays both once per object it
//! lacks, which is what a presence phase is for.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::OnceLock;
use crate::enums::dir_entry_type::DirEntryType;
use crate::error::{CoreError, RefusalCode};
use crate::util::{file_utils, object_utils, pack_utils, scope_utils, sealed_utils};
use crate::util::lock_utils::{RefLock, StoreAccessLock};

/// How a [`FetchHook`]'s fetch ended.
pub enum Fetched {
    /// Every object was asked for. Those the origin sent were delivered; it may not have sent all.
    Done,
    /// The origin could not be reached: the read refuses as a cache miss.
    Unreachable {
        /// The origin's URL, for the next step.
        origin: String,
        /// Why it could not be reached.
        why: String,
    },
    /// The fetch failed some other way, such as the origin refusing it, or the delivery of an
    /// object failing, which stops the fetch.
    Failed(String),
}

/// Hands one fetched object's hash and bytes, unverified, to the refill. An error stops the fetch.
pub type Deliver<'a> = &'a mut dyn FnMut(String, Vec<u8>) -> Result<(), String>;

/// Fetch the objects named, by hash, from the warehouse's origin, delivering each one as it
/// arrives. Called on the reading thread, with the storage root that read resolves.
pub type FetchHook = Box<dyn Fn(&[String], Deliver) -> Fetched + Send + Sync>;

static HOOK: OnceLock<FetchHook> = OnceLock::new();

/// Register the process's fetch hook. Only the first registration takes effect.
pub fn register(hook: FetchHook) {
    let _ = HOOK.set(hook);
}

/// The fetched bytes a refill gathers before it stores them, and how many objects: a batch is
/// stored in groups of at most this much (or one object, if larger), each with one synced append.
const GROUP_BYTES: usize = 32 * 1024 * 1024;
const GROUP_OBJECTS: usize = 512;

thread_local! {
    /// How many [`Unfetched`] scopes this thread is inside.
    static UNFETCHED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A scope whose reads never fetch: an absent recorded object reads as absent, as it does with no
/// hook. For a walk that reads to learn what the store holds, such as a closure check, which
/// treats a failed read as an absence and would otherwise fetch everything it meant to find
/// missing. Per thread: a walk that fans out enters one on each worker. A presence phase is an
/// explicit request to fetch, not a read, and fetches inside one.
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
    if UNFETCHED.with(|depth| depth.get() > 0) {
        return Ok(false);
    }

    refill_with(HOOK.get(), &[hash.to_string()])
}

/// A mutation's presence phase: in a cache warehouse, fetch every one of `hashes` that is absent
/// and recorded, in one batch, so the reads that follow find them held. Run before the mutation
/// changes anything, so an origin that cannot be reached refuses it with nothing changed. Anything
/// else absent is left for the read to report. Elsewhere, nothing.
///
/// # Returns
/// * `Ok(())`      - None of `hashes` is absent and recorded.
/// * `Err(String)` - Why some were not fetched: a [`RefusalCode::CacheMiss`] refusal, the whole
///                   message, when the origin could not be reached. Whatever arrived before the
///                   failure is stored.
pub fn ensure_present<'a>(hashes: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    ensure_with(HOOK.get(), hashes)
}

fn ensure_with<'a>(hook: Option<&FetchHook>, hashes: impl IntoIterator<Item = &'a str>) -> Result<(), String> {
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(());
    }

    let mut seen: HashSet<&str> = HashSet::new();
    let mut absent: Vec<String> = Vec::new();

    for hash in hashes {
        if seen.insert(hash) && !file_utils::raw_object_present(hash)? {
            absent.push(hash.to_string());
        }
    }

    if absent.is_empty() {
        return Ok(());
    }

    refill_with(hook, &absent).map(|_| ())
}

/// A planner's presence phase: in a cache warehouse, fetch the trees it is about to walk, one
/// batch per level, so planning a mutation over trees the cache lacks costs one request per level
/// of the walk rather than one per tree.
///
/// The walk starts at `roots`, one per side the planner compares (an absent side is `None`), and
/// goes down level by level: at each directory, every subtree name any side holds is a child, with
/// each side's hash for it, and `descend(path, sides)` says whether the planner loads it, so pass
/// the planner's own rule. Outside a cache warehouse, nothing is walked.
///
/// A fetch only: it reports nothing. Where it fails (the origin cannot be reached, a tree is absent
/// and not recorded), the planner's own read of that tree fails as the fetch did, a cache miss
/// keeping its code; and where the planner refuses before it reaches the tree, as it does an
/// out-of-scope conflict, its refusal is the one the caller sees, as it was before any fetch.
pub fn prefetch_trees(roots: &[Option<&str>], descend: impl Fn(&str, &[Option<&str>]) -> bool) {
    let _ = walk_with(HOOK.get(), roots, descend);
}

fn walk_with(hook: Option<&FetchHook>,
             roots: &[Option<&str>],
             descend: impl Fn(&str, &[Option<&str>]) -> bool) -> Result<(), String> {
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(());
    }

    let mut level: Vec<(String, Vec<Option<String>>)> =
        vec![(String::new(), roots.iter().map(|hash| hash.map(str::to_string)).collect())];

    while !level.is_empty() {
        ensure_with(hook, level.iter().flat_map(|(_, sides)| sides.iter().flatten().map(String::as_str)))?;

        let mut next: Vec<(String, Vec<Option<String>>)> = Vec::new();

        for (key, sides) in &level {
            let trees = sides.iter()
                .map(|hash| hash.as_deref().map(object_utils::load_tree).transpose())
                .collect::<Result<Vec<_>, String>>()?;

            let subtrees: Vec<BTreeMap<&String, &str>> = trees.iter()
                .map(|tree| tree.as_ref()
                    .map(|tree| tree.get_subtrees().map(|(name, item)| (name, item.hash.as_str())).collect())
                    .unwrap_or_default())
                .collect();
            let names: BTreeSet<&String> = subtrees.iter().flat_map(|side| side.keys().copied()).collect();

            for name in names {
                let child: Vec<Option<&str>> = subtrees.iter().map(|side| side.get(name).copied()).collect();
                let path = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

                if descend(&path, &child) {
                    next.push((path, child.into_iter().map(|hash| hash.map(str::to_string)).collect()));
                }
            }
        }

        level = next;
    }

    Ok(())
}

/// [`ensure_present`] for the files a mutation is about to write, by entry hash and kind: their
/// blobs and recipes in one batch, then the chunks of every chunked one, which a refilled recipe
/// records as it lands.
pub fn ensure_files_present<'a>(files: impl IntoIterator<Item = (&'a str, DirEntryType)>) -> Result<(), String> {
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(());
    }

    let files: Vec<(&str, DirEntryType)> = files.into_iter().collect();

    ensure_present(files.iter().map(|(hash, _)| *hash))?;

    let mut chunks: Vec<String> = Vec::new();

    for (hash, item_type) in &files {
        if item_type.is_chunked() {
            chunks.extend(object_utils::recipe_chunk_hashes(hash)?);
        }
    }

    ensure_present(chunks.iter().map(String::as_str))
}

/// Refill those of `absent` that are recorded in a cache warehouse, fetching them with `hook`.
/// Returns whether any were.
fn refill_with(hook: Option<&FetchHook>, absent: &[String]) -> Result<bool, String> {
    // A warehouse whose mode cannot be read is not judged a cache here: the command that needs
    // the mode reports why it cannot be read.
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(false);
    }

    let named = describe(absent);
    let record = sealed_utils::read_sealed()
        .map_err(|why| format!("{} absent, and the sealed-object record could not be read: {}", named.is, why))?;

    let wanted: Vec<String> = absent.iter().filter(|hash| record.contains(hash)).cloned().collect();

    if wanted.is_empty() {
        return Ok(false);
    }

    let named = describe(&wanted);
    let unfetched = |why: &str| Err(format!(
        "{} not in this cache warehouse, and could not be fetched from its origin here: {}.",
        named.is, why
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
    // it could not store, and taken per group after it, so no deleter waits out the round trip.
    drop(StoreAccessLock::try_shared()?);

    let mut group = Group::default();
    let mut stored: Result<(), String> = Ok(());

    let fetched = hook(&wanted, &mut |hash, bytes| {
        let filled = group.add(hash, bytes);
        if let Err(why) = &filled {
            stored = Err(why.clone());
        }
        filled
    });

    // What arrived before a failure is kept: each stored object is one fewer to fetch next time.
    stored?;
    group.flush()?;

    match fetched {
        Fetched::Done => {}
        Fetched::Unreachable { origin, why } => return Err(CoreError::refusal(
            RefusalCode::CacheMiss,
            format!(
                "{} not in this cache warehouse, and its origin could not be reached to fetch {}: {}",
                describe(&still_absent(&wanted).unwrap_or(wanted.clone())).is, named.them, CoreError::from(why)
            ),
            format!("Run the command again once the origin ({}) can be reached.", origin),
        ).into()),
        Fetched::Failed(why) => return Err(format!(
            "{} not in this cache warehouse, and fetching {} from the origin failed: {}",
            describe(&still_absent(&wanted).unwrap_or(wanted.clone())).is, named.them, why
        )),
    }

    let missing = still_absent(&wanted)?;
    if !missing.is_empty() {
        return Err(format!(
            "{} not in this cache warehouse, and the origin did not send {}.",
            describe(&missing).is, describe(&missing).them
        ));
    }

    Ok(true)
}

/// Those of `hashes` the store still lacks.
fn still_absent(hashes: &[String]) -> Result<Vec<String>, String> {
    let mut absent: Vec<String> = Vec::new();

    for hash in hashes {
        if !file_utils::raw_object_present(hash)? {
            absent.push(hash.clone());
        }
    }

    Ok(absent)
}

/// How a message names a set of objects: as the subject ("Object X is"), and as the object ("it").
struct Named {
    is: String,
    them: &'static str,
}

fn describe(hashes: &[String]) -> Named {
    match hashes {
        [hash] => Named { is: format!("Object {} is", hash), them: "it" },
        [first, ..] => Named { is: format!("{} objects (the first {}) are", hashes.len(), first), them: "them" },
        [] => Named { is: "No object is".to_string(), them: "them" },
    }
}

/// Fetched objects not stored yet, each verified as it arrived.
#[derive(Default)]
struct Group {
    objects: Vec<(String, Vec<u8>)>,
    bytes: usize,
}

impl Group {
    /// Gather one object, storing the group once it is full. Bytes that are not the object are
    /// refused on arrival, after storing what was gathered before them, so one bad object loses
    /// none of the good ones.
    fn add(&mut self, hash: String, bytes: Vec<u8>) -> Result<(), String> {
        if let Err(why) = object_utils::verify_object_bytes(&hash, &bytes) {
            let corrupt = format!("The origin sent object {} corrupt: {}", hash, why);

            return Err(match self.flush() {
                Ok(()) => corrupt,
                Err(unstored) => format!("{}\n{}", corrupt, unstored),
            });
        }

        self.bytes += bytes.len();
        self.objects.push((hash, bytes));

        if self.bytes >= GROUP_BYTES || self.objects.len() >= GROUP_OBJECTS {
            self.flush()?;
        }

        Ok(())
    }

    /// Store what is gathered, under a store-access hold taken for it.
    fn flush(&mut self) -> Result<(), String> {
        if self.objects.is_empty() {
            return Ok(());
        }

        let objects = std::mem::take(&mut self.objects);
        self.bytes = 0;

        let hold = StoreAccessLock::try_shared()?;
        let refs: Vec<(&str, &[u8])> = objects.iter().map(|(hash, bytes)| (hash.as_str(), bytes.as_slice())).collect();

        sealed_utils::refill_all(&hold, &refs).map_err(|why| format!(
            "Could not store {}, fetched from the origin: {}", describe_held(&objects), why
        ))
    }
}

fn describe_held(objects: &[(String, Vec<u8>)]) -> String {
    match objects {
        [(hash, _)] => format!("object {}", hash),
        _ => format!("{} objects", objects.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use crate::builder::object::loose_object_builder::LooseObjectBuilder;
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
        web_bytes: Vec<u8>,
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
        let bytes = bytes_of(&root);
        let web_bytes = bytes_of(&web);

        sealed_utils::append(&[(SealOp::Record, &root)]).unwrap();
        remove(&root);
        remove(&web);

        Fixture { _scope: scope, root, bytes, web, web_bytes }
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

    /// A hook that runs `fetch` on each batch it is asked for, and how many times it was asked.
    fn hook(fetch: impl Fn(&[String], Deliver) -> Fetched + Send + Sync + 'static) -> (FetchHook, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        (Box::new(move |hashes, deliver| { counted.fetch_add(1, Ordering::SeqCst); fetch(hashes, deliver) }), calls)
    }

    /// A hook that delivers `objects`, whatever it is asked for, then ends with `end`.
    fn sending(objects: Vec<(String, Vec<u8>)>, end: impl Fn() -> Fetched + Send + Sync + 'static) -> (FetchHook, Arc<AtomicUsize>) {
        hook(move |_, deliver| {
            for (hash, bytes) in &objects {
                if let Err(why) = deliver(hash.clone(), bytes.clone()) {
                    return Fetched::Failed(why);
                }
            }
            end()
        })
    }

    fn refill(hook: &FetchHook, hash: &str) -> Result<bool, String> {
        refill_with(Some(hook), &[hash.to_string()])
    }

    fn unreachable() -> Fetched {
        Fetched::Unreachable { origin: "http://origin.example".to_string(), why: "connection refused".to_string() }
    }

    fn bytes_of(hash: &str) -> Vec<u8> {
        file_utils::retrieve_object_by_hash_uncached(hash).unwrap()
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
        let (hook, calls) = sending(vec![(fixture.root.clone(), fixture.bytes.clone())], || Fetched::Done);

        assert!(refill(&hook, &fixture.root).unwrap());
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
        let (hook, calls) = sending(vec![(fixture.root.clone(), fixture.bytes.clone())], || Fetched::Done);

        let lock = RefLock::acquire().unwrap();
        let refused = refill(&hook, &fixture.root).unwrap_err();
        assert!(refused.contains("ref lock"), "{}", refused);
        drop(lock);

        let unfetched = Unfetched::enter();
        assert!(!refill_recorded(&fixture.root).unwrap());
        drop(unfetched);

        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(!held(&fixture.root));
        assert_recorded(&[&fixture.root]);

        assert!(refill(&hook, &fixture.root).unwrap(), "out of both, it is fetched");
    }

    // Revert: refuse while any thread holds the ref lock → the read refuses, though the other
    // thread's hold lasts until the hook is asked.
    #[test]
    fn another_threads_ref_lock_does_not_stop_a_fetch() {
        let fixture = fixture("held-elsewhere");
        let bytes = fixture.bytes.clone();
        let (release, released) = std::sync::mpsc::channel::<()>();
        let (hook, calls) = hook(move |hashes, deliver| {
            let _ = release.send(());
            match deliver(hashes[0].clone(), bytes.clone()) {
                Ok(()) => Fetched::Done,
                Err(why) => Fetched::Failed(why),
            }
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
        let refilled = refill(&hook, &fixture.root);
        holder.join().unwrap();

        assert!(refilled.unwrap(), "another thread's hold does not stop the fetch");
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_recorded(&[&fixture.web]);
    }

    // Revert: skip the mode check → the full warehouse's absence is fetched; skip the record
    // check → the unrecorded `web` is.
    #[test]
    fn only_a_recorded_absence_in_a_cache_is_fetched() {
        let fixture = fixture("unrecorded");
        let (hook, calls) = sending(Vec::new(), || Fetched::Failed("asked".to_string()));

        assert!(!refill(&hook, &fixture.web).unwrap());

        warehouse_utils::mark_for_test(WarehouseMode::Full);
        assert!(!refill(&hook, &fixture.root).unwrap());

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    // Revert: build the unreachable arm as a plain error → no `cache_miss` code.
    #[test]
    fn an_unreachable_origin_is_a_cache_miss_naming_it() {
        let fixture = fixture("unreachable");
        let (hook, _) = sending(Vec::new(), unreachable);

        match CoreError::from(refill(&hook, &fixture.root).unwrap_err()) {
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
        let (hook, _) = sending(vec![(fixture.root.clone(), bytes)], || Fetched::Done);

        let refused = refill(&hook, &fixture.root).unwrap_err();
        assert_recorded(&[&fixture.root]);
        assert!(!held(&fixture.root));
        assert!(refused.contains("corrupt"), "{}", refused);
    }

    // Revert: store each object as it arrives, pushing its own children down → the record keeps
    // `web`, which the same batch stored.
    #[test]
    fn a_phase_fetches_a_tree_and_its_recorded_subtree_in_one_batch() {
        let fixture = fixture("batch");
        sealed_utils::append(&[(SealOp::Record, &fixture.web)]).unwrap();
        let (hook, calls) = sending(vec![
            (fixture.root.clone(), fixture.bytes.clone()),
            (fixture.web.clone(), fixture.web_bytes.clone()),
        ], || Fetched::Done);

        ensure_with(Some(&hook), [fixture.root.as_str(), fixture.web.as_str(), fixture.root.as_str()]).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert!(held(&fixture.root) && held(&fixture.web));
        assert_recorded(&[]);
    }

    // Revert: let a phase honour the scope → nothing is fetched inside it.
    #[test]
    fn a_phase_fetches_inside_an_unfetched_scope_and_skips_what_is_held() {
        let fixture = fixture("phase-scope");
        let (hook, calls) = sending(vec![(fixture.root.clone(), fixture.bytes.clone())], || Fetched::Done);
        let held_blob = blob("a");

        let _unfetched = Unfetched::enter();
        ensure_with(Some(&hook), [fixture.root.as_str(), held_blob.as_str()]).unwrap();
        ensure_with(Some(&hook), [fixture.root.as_str()]).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 1, "what is held is not asked for again");
        assert!(held(&fixture.root));
    }

    // Revert: build the phase's unreachable arm as a plain error → no `cache_miss` code; store
    // the group only at the end → what arrived before the failure is lost.
    #[test]
    fn an_unreachable_origin_refuses_a_phase_keeping_what_arrived() {
        let fixture = fixture("phase-unreachable");
        sealed_utils::append(&[(SealOp::Record, &fixture.web)]).unwrap();
        let (hook, _) = sending(vec![(fixture.root.clone(), fixture.bytes.clone())], unreachable);

        let refused = ensure_with(Some(&hook), [fixture.root.as_str(), fixture.web.as_str()]).unwrap_err();

        match CoreError::from(refused) {
            CoreError::Refusal { code, message, next_step, .. } => {
                assert_eq!(code, RefusalCode::CacheMiss);
                assert!(message.contains(&format!("Object {} is", fixture.web)), "{}", message);
                assert!(next_step.contains("http://origin.example"), "{}", next_step);
            }
            other => panic!("not a refusal: {:?}", other),
        }

        assert!(held(&fixture.root) && !held(&fixture.web));
        assert_recorded(&[&fixture.web]);
    }

    /// A hook that delivers whichever of `objects` it is asked for, and every batch it was asked.
    fn serving(objects: Vec<(String, Vec<u8>)>) -> (FetchHook, Arc<std::sync::Mutex<Vec<Vec<String>>>>) {
        let asked: Arc<std::sync::Mutex<Vec<Vec<String>>>> = Default::default();
        let saw = asked.clone();
        let objects: std::collections::HashMap<String, Vec<u8>> = objects.into_iter().collect();
        (Box::new(move |hashes, deliver| {
            saw.lock().unwrap().push(hashes.to_vec());
            for hash in hashes {
                if let Some(bytes) = objects.get(hash) {
                    if let Err(why) = deliver(hash.clone(), bytes.clone()) {
                        return Fetched::Failed(why);
                    }
                }
            }
            Fetched::Done
        }), asked)
    }

    // Revert: fetch each tree the walk reaches on its own → four batches, not three; ignore the
    // descent rule → `docs/deep` is fetched though the planner would not load it.
    #[test]
    fn a_tree_walk_fetches_one_batch_per_level_and_only_where_it_descends() {
        let fixture = fixture("tree-walk");
        let deep = tree(&[("c", &blob("c"), DirEntryType::Normal)]);
        let docs = tree(&[("d", &blob("d"), DirEntryType::Normal), ("deep", &deep, DirEntryType::Tree)]);
        let root = tree(&[("src", &fixture.root, DirEntryType::Tree), ("docs", &docs, DirEntryType::Tree)]);
        let mut objects: Vec<(String, Vec<u8>)> = [&root, &docs, &deep].iter()
            .map(|hash| (hash.to_string(), bytes_of(hash))).collect();
        objects.push((fixture.root.clone(), fixture.bytes.clone()));
        objects.push((fixture.web.clone(), fixture.web_bytes.clone()));
        sealed_utils::append(&[(SealOp::Record, &root)]).unwrap();
        for hash in [&root, &docs, &deep] {
            remove(hash);
        }
        let (hook, asked) = serving(objects);

        walk_with(Some(&hook), &[Some(&root)], |path, _| path != "docs/deep").unwrap();

        let asked = asked.lock().unwrap().clone();
        let mut levels: Vec<Vec<String>> = asked.into_iter().map(|mut batch| { batch.sort(); batch }).collect();
        let mut second = vec![fixture.root.clone(), docs.clone()];
        second.sort();
        assert_eq!(levels.remove(0), vec![root.clone()]);
        assert_eq!(levels.remove(0), second);
        assert_eq!(levels.remove(0), vec![fixture.web.clone()], "the third level is `src/web`, `src` being the fixture's root");
        assert!(levels.is_empty(), "nothing else is asked for: {:?}", levels);
        assert!(!held(&deep), "a subtree the planner does not descend into is not fetched");
        assert_recorded(&[&deep]);
    }

    // A planner's own refusal, raised before it reads the tree its prefetch could not fetch (no
    // hook here, so every fetch fails), is the one returned: the metadata-folder refusal of a
    // shift, and a scoped merge's out-of-scope conflict.
    //
    // Revert: return the prefetch's error from the planners → both refuse that `meta` and `in`
    // could not be fetched instead.
    #[test]
    fn a_planner_refusal_comes_before_a_failed_prefetch() {
        use crate::util::scope_utils::MaterializationScope;
        use crate::util::{merge_utils, shift_utils};

        let fixture = fixture("precedence");
        let meta = tree(&[("m", &blob("m"), DirEntryType::Normal)]);
        let root = tree(&[(crate::globals::FOLDER_NAME_FORKLIFT_ROOT, &meta, DirEntryType::Tree)]);
        sealed_utils::append(&[(SealOp::Record, &meta)]).unwrap();
        remove(&meta);

        let refused = shift_utils::diff_trees_in(None, &root, &MaterializationScope::full()).err().expect("the shift refuses");
        assert!(refused.contains("metadata folder"), "{}", refused);

        let held_in = tree(&[("a", &blob("a"), DirEntryType::Normal)]);
        let their_in = tree(&[("b", &blob("b"), DirEntryType::Normal)]);
        let side = |out: &str, inner: &str| tree(&[("in", inner, DirEntryType::Tree), ("out.txt", &blob(out), DirEntryType::Normal)]);
        let (base, ours, theirs) = (side("0", &held_in), side("1", &held_in), side("2", &their_in));
        sealed_utils::append(&[(SealOp::Record, &their_in)]).unwrap();
        remove(&their_in);

        let scope = MaterializationScope::from_prefixes(["in"]);
        match CoreError::from(merge_utils::compute_merge_actions(&base, &ours, &theirs, "ours", "theirs", &scope).err().expect("the merge refuses")) {
            CoreError::Refusal { code, .. } => assert_eq!(code, RefusalCode::OutOfScopeConflict),
            other => panic!("not the planner's refusal: {:?}", other),
        }

        assert!(!held(&their_in) && !held(&meta) && !held(&fixture.root), "nothing was fetched");
    }

    // Revert: drop the check after the fetch → the phase succeeds with `web` still absent.
    #[test]
    fn an_origin_that_sends_nothing_fails_the_phase() {
        let fixture = fixture("unsent");
        let (hook, _) = sending(Vec::new(), || Fetched::Done);

        let failed = ensure_with(Some(&hook), [fixture.root.as_str()]).unwrap_err();

        assert!(failed.contains("did not send"), "{}", failed);
        assert!(!matches!(CoreError::from(failed), CoreError::Refusal { .. }));
    }

    // Revert: skip the mode check in the phase → the full warehouse's absence is asked for.
    #[test]
    fn a_phase_outside_a_cache_does_nothing() {
        let fixture = fixture("phase-full");
        warehouse_utils::mark_for_test(WarehouseMode::Full);
        let (hook, calls) = sending(Vec::new(), || Fetched::Done);

        ensure_with(Some(&hook), [fixture.root.as_str()]).unwrap();

        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    // Revert: verify only when the group is stored → the good root is lost with the bad `web`.
    #[test]
    fn a_corrupt_object_in_a_batch_keeps_the_ones_before_it() {
        let fixture = fixture("corrupt-batch");
        sealed_utils::append(&[(SealOp::Record, &fixture.web)]).unwrap();
        let (hook, _) = sending(vec![
            (fixture.root.clone(), fixture.bytes.clone()),
            (fixture.web.clone(), fixture.bytes.clone()),
        ], || Fetched::Done);

        let failed = ensure_with(Some(&hook), [fixture.root.as_str(), fixture.web.as_str()]).unwrap_err();

        assert!(failed.contains("corrupt") && failed.contains(&fixture.web), "{}", failed);
        assert!(held(&fixture.root) && !held(&fixture.web));
        assert_recorded(&[&fixture.web]);
    }
}
