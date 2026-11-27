//! The durable taint-record primitive (DESIGN.html §3.1.1): after a write's final names are
//! visible but the directory entries recording them (or their own bytes) could not be proven
//! durable, this module lets the failing write record exactly which final object paths are
//! suspect, so a later heal knows precisely what to redo instead of re-verifying a whole storage
//! root.
//!
//! This module is the primitive only: recording a taint and reading one back, plus a
//! process-local in-memory gate that gives an *activated* process an immediate belt against
//! trusting existence under a root it just tainted. Nothing here decides *when* a taint should
//! fire (every post-rename directory-sync call site does, via `file_utils`'s
//! `sync_dir_or_taint`/`sync_result_or_taint`), and nothing here heals —
//! [`resolve_taints`] is the primitive
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted) clears a resolved
//! taint with, not a heal themselves; the restage logic and the entry chokepoint live there.
//! `resolve_taints` owns both halves of that clear — the durable taint files on disk and the
//! in-memory gate — deriving the gate decision from what is actually standing in the taint
//! directory at the moment of the call, never from a caller's own belief about what it just
//! resolved (DESIGN.html §3.1.1). The two functions that only ever touch one half
//! (`clear_gate`, `remove_taint_files`) are deliberately not exported even within the crate: a
//! caller that needs to clear a taint has exactly one door, this one.
//!
//! ## Activation
//!
//! [`activate`] flips a process-global switch. Every public function in this module is a
//! documented no-op until it is called: [`record_taint`] writes nothing, [`gate_check`] never
//! refuses, [`read_taints`] always reads back empty. A process that has not activated this
//! machinery has no way to *heal* a taint either, so it must not be able to see or set one —
//! taking half the mechanism (recording or gating without ever healing) is strictly worse than
//! today's baseline (a permanent wedge, or a disk record nobody ever consumes), so activation is
//! all-or-nothing, never per-call. Nothing in this module ever calls [`activate`] itself.
//!
//! ## Format
//!
//! One taint file per failed batch, under `<root>/taint/`, named
//! `taint-<pid>-<nonce_hex>-<counter>` and created with an exclusive create (never a rename-over —
//! see [`record_taint`]'s doc comment for why). `<pid>` is this process's OS pid; `<nonce_hex>` is
//! a random value drawn once, lazily, per process (never per file — see
//! [`taint_filename_nonce`]); `<counter>` is a single process-global counter that only ever
//! increases for as long as this process lives, however many taint files it creates or deletes in
//! between (see [`create_taint_file`]). That combination makes a filename this process has ever
//! used **unique-forever, even after the file itself is later deleted**: the counter never resets,
//! so this process can never reissue a name it already handed out, and the nonce means a
//! *different* process — even one that crashed and was replaced by a new one reusing the exact
//! same `pid` — draws its own counter from an independent, practically-never-colliding space. This
//! is load-bearing, not cosmetic: a heal that deletes taint files by the snapshot it read (see
//! [`TaintState::files`]) is only actually safe if a name, once observed, can never later name a
//! *different* file on disk — otherwise a healer holding a stale snapshot of `taint-P-0` could end
//! up deleting a freshly re-recorded `taint-P-0` that has nothing to do with the one it read (the
//! ABA hole a naive "restart the counter at 0 every call" scheme leaves open).
//!
//! Content is a [`record_utils`] record (see [`TAINT_RECORD`]): the batch's final object paths,
//! root-relative, one per line, then the record's digest line. A path
//! is escaped as a [`record_utils`] entry is, since an inventory shard's folder is named after a
//! working directory, whose name may hold a line break. Each path's components are joined with `/` on
//! every OS (see [`taint_record_line`]) — never the platform-native separator a bare
//! `Path::to_string_lossy()` would produce (`\` on Windows) — because a taint file written on one
//! platform must still parse into the exact same recorded paths on whatever platform later reads
//! it back (DESIGN.html §3.1.1); a record keyed by native separator could not make that promise.
//! A file whose digest matches its bytes is complete; anything else — truncated by a crash
//! mid-write, altered after it was written, written without this version's header by an older
//! Forklift, or simply absent — is **torn**: the parseable (fully newline-terminated, correctly
//! escaped) lines of a headed file still contribute recorded paths,
//! since a real write only ever appends, but the file can no longer prove it named every path the
//! failing batch touched. [`read_taints`] unions every file under the directory and reports `torn`
//! if any one of them is.
//!
//! ## What this module does not do
//!
//! No restage logic and no entry chokepoint (see [`heal_utils`](crate::util::heal_utils) for
//! both), and no closure walk or CLI-visible recovery verb for the cases the automatic entry-heal
//! cannot resolve on its own — that is
//! [`recovery_utils`](crate::util::recovery_utils), built on top of
//! [`heal_utils::heal_if_tainted`](crate::util::heal_utils::heal_if_tainted)'s refusal.

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};
use crate::globals::forklift_root;
use crate::util::{file_utils, record_utils};

/// The folder under a storage root that holds taint files.
const FOLDER_NAME_TAINT: &str = "taint";

/// The [`record_utils`] kind and version of a taint file's header line. A file without it is
/// torn, never read as raw unescaped paths.
pub(crate) const TAINT_RECORD: &str = "taint";
pub(crate) const TAINT_VERSION: u32 = 2;

/// The bytes of a complete taint file recording `paths` (root-relative, `/`-joined): a
/// [`record_utils`] record, one escaped path per entry, ending in its digest.
pub fn taint_file_bytes<I, S>(paths: I) -> Vec<u8>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    record_utils::to_bytes(TAINT_RECORD, TAINT_VERSION, paths)
}

/// An arbitrary bound on how many same-process, same-`pid` taint files [`record_taint`] will
/// probe for before giving up. Thousands of live taint files under one root means something else
/// is badly wrong (a stuck writer looping, a `pid` reused pathologically often) — this turns
/// that into a loud error instead of an unbounded scan.
const TAINT_FILENAME_SANITY_CAP: u32 = 10_000;

/// A substring every [`gate_check`] refusal contains, so a caller can recognize "the gate is
/// standing" without string-matching the whole message.
pub const GATE_TAINT_MARKER: &str = "a durability taint is standing";

/// The one message both [`gate_check`] (the in-memory belt) and
/// [`file_utils::taint_recheck`](crate::util::file_utils) (the durable re-check on a directory
/// sync's success path) refuse with: a standing taint means existence under `root` cannot be
/// trusted, whichever of the two mechanisms is the one that noticed. Shared so editing the
/// wording in one can never silently leave the other stale. Always contains
/// [`GATE_TAINT_MARKER`], per that constant's own doc comment.
pub(crate) fn gate_standing_message(root: &Path) -> String {
    format!(
        "{} under \"{}\"; existence cannot be trusted here until it is healed.",
        GATE_TAINT_MARKER, root.to_string_lossy()
    )
}

/// The process-global activation switch — see the module doc comment's activation section.
static ACTIVATED: AtomicBool = AtomicBool::new(false);

/// Activate the taint machinery for the rest of this process. See the module doc comment: this
/// is all-or-nothing, and nothing in this module calls it — a caller wires this in only once it
/// has also wired a heal, per the module doc comment's activation section.
pub fn activate() {
    ACTIVATED.store(true, Ordering::SeqCst);
}

/// Serializes every test — in this module or anywhere else in the crate — that touches
/// [`ACTIVATED`] or the gate map: both are process-global state (see the module doc comment's
/// activation section), so a test asserting the *unactivated* behavior would otherwise race a
/// concurrently running test that has already called [`activate`]. `pub(crate)` (rather than
/// nested inside this module's own `#[cfg(test)] mod tests`) so other modules' tests that also
/// need to observe the unactivated state (`file_utils`'s taint-wiring tests) serialize through
/// this exact same lock instead of a second, independent one that would not actually exclude
/// anything. Recovers from a poisoned lock (a prior test panicking while holding it) rather than
/// cascading a panic into every later test.
#[cfg(test)]
pub(crate) static ACTIVATION_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Reset the process-global activation switch for a test that needs to observe the unactivated
/// state. Test-only: production code has exactly one direction, [`activate`]. `pub(crate)` for
/// the same cross-module reason as [`ACTIVATION_TEST_LOCK`].
#[cfg(test)]
pub(crate) fn reset_activation_for_test() {
    ACTIVATED.store(false, Ordering::SeqCst);
}

fn is_activated() -> bool {
    ACTIVATED.load(Ordering::SeqCst)
}

/// Record that a batch of final object paths was left visible without a proven-durable
/// directory entry (or proven-durable bytes), so a later heal knows exactly what to redo.
///
/// A no-op — `Ok(())`, nothing written, no gate set — unless [`activate`] has been called in
/// this process (see the module doc comment).
///
/// Resolves the storage root that owns `final_paths` via [`forklift_root`]. If any given path is
/// not actually under that root — the shape a bare-path unit test's paths take, having never
/// entered a storage-root scope at all — there is no warehouse whose trust needs revoking, so
/// this skips silently rather than erroring: best-effort root resolution is this module's
/// documented scope tolerance, not a failure. An empty `final_paths` is the same case (nothing to
/// taint) and is likewise a silent no-op.
///
/// **Ordering, load-bearing:** the in-memory gate (see [`gate_check`]) is set *before* the
/// durable write below is even attempted. The gate is cheap and cannot itself fail; the disk
/// write can. A process that observes the disk write fail must still refuse to trust existence
/// under this root for the rest of its life (or until a future heal clears it) — gating first
/// means that refusal holds even when the durable half of this call never lands.
///
/// **No re-entry, ever.** The taint's own write below never calls [`file_utils::write_file_atomically`]
/// or anything that could itself fail a post-rename directory sync and recurse back into this
/// module — it is its own small, self-contained routine (exclusive-create, write, fsync file,
/// fsync directory, fsync the folders above). On a failure up to the directory's own fsync, this
/// returns the error and the routine removes whatever partial file it created (see
/// [`write_taint_file`]). A failure syncing the folders above returns the error with the file
/// kept, since that failure may be the very one being recorded: an `Err` does not mean nothing is
/// on disk. The one case that can still leave a torn file on disk
/// is a hard crash (power loss) *during* the write, which runs no cleanup code at all; that
/// residual — original failure, AND this write's own failure or a crash, AND a retry before any
/// heal — is exactly the double-failure window this design accepts and documents, never hidden
/// by pretending the write cannot fail.
///
/// # Returns
/// * `Ok(())`      - Not activated, nothing to resolve, or the taint was durably recorded.
/// * `Err(String)` - Activated, the root resolved, and the durable write itself failed. The gate
///                   for this root is set regardless (see the ordering note above).
///
/// An `Err` can also come with the record written, when the folders above it could not be synced
/// (see the no-re-entry note above).
pub fn record_taint(final_paths: &[&Path]) -> Result<(), String> {
    if !is_activated() || final_paths.is_empty() {
        return Ok(());
    }

    let Some(root) = resolve_root_for(final_paths) else {
        return Ok(());
    };
    let relative_paths = to_root_relative(&root, final_paths);

    // Cheap and infallible: set first, so this process gates itself even if the write below
    // fails outright (see this function's doc comment).
    set_gate(&root);

    write_taint_file(&root, &relative_paths)
}

/// The one resolution [`record_taint`] and
/// [`file_utils::taint_recheck`](crate::util::file_utils) both key their all-or-nothing scope
/// tolerance through, so the two sides can never drift apart into resolving "does this batch of
/// paths belong to a root" two different ways.
///
/// `None` when `final_paths` is empty, or when any single path is not actually under
/// [`forklift_root`] — the shape a bare-path unit test's own paths take, having never entered a
/// storage-root scope at all — in which case there is no root whose taint state a caller could
/// sensibly consult or record against. `Some(root)` — the resolved [`forklift_root`] — otherwise.
pub(crate) fn resolve_root_for(final_paths: &[&Path]) -> Option<PathBuf> {
    if final_paths.is_empty() {
        return None;
    }

    let root = forklift_root();
    final_paths.iter().all(|path| path.strip_prefix(&root).is_ok()).then_some(root)
}

/// What heal does with a vanished path of a declared [`TaintShape`]: the class
/// `recovery_utils::classify_vanished` hands its dispatch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VanishedDisposition {
    Loose,
    Signature,
    PackData,
    PackIndex,
    SignatureTable,
    Shard,
    Ref,
    MetaRef,
    MetaCopy,
    Keepsake,
    LoadMarker,
    Position,
    PositionCopy,
    Scope,
    ScopeCopy,
    MergeRecord,
    MergeRecordCopy,
    Journal,
    BayPath,
    FetchScope,
    FetchLedger,
    MirrorRoots,
}

/// One kind of root-relative path a taint record may name.
pub(crate) struct TaintShape {
    /// What the path is, as heal's remainder line names it.
    pub(crate) description: &'static str,
    /// `None` for a shape heal has no recovery for: a vanished path of it stays in the remainder.
    pub(crate) disposition: Option<VanishedDisposition>,
    pattern: &'static [Part],
}

/// One component, or a run of components, of a [`TaintShape`]'s pattern.
enum Part {
    /// Exactly this name.
    Is(&'static str),
    /// Any one name.
    Name,
    /// One or more names.
    Names,
    /// A loose object's fan-out folder: `OBJECT_HASH_FOLDER_PATH_CHARACTERS` lowercase hex.
    FanOut,
    /// The rest of a loose object's hash: lowercase hex of any length.
    Hex,
    /// The rest of a loose object's hash, then the signature suffix.
    HexSignature,
    /// A non-empty stem with this extension.
    Extension(&'static str),
    /// One of the meta pallets' names.
    MetaPalletName,
}

use Part::{Extension, FanOut, Hex, HexSignature, Is, MetaPalletName, Name, Names};
use VanishedDisposition as Disposition;

const fn shape(description: &'static str, disposition: Option<VanishedDisposition>, pattern: &'static [Part]) -> TaintShape {
    TaintShape { description, disposition, pattern }
}

// Each shape is a named static so that [`WriterShape::entry`] can name the one its writer
// produces; [`TAINT_SHAPES`] lists them in match order.
static PALLET_REF: TaintShape = shape("a pallet ref", Some(Disposition::Ref), &[Is(crate::util::pallet_utils::FOLDER_NAME_PALLETS_ROOT), Names]);
static KEEPSAKE: TaintShape = shape("a removed ref's keepsake", Some(Disposition::Keepsake),
    &[Is(crate::util::ref_heal::FOLDER_NAME_REMOVED_REFS), Names]);
static META_REF: TaintShape = shape("a meta pallet ref", Some(Disposition::MetaRef), &[Is(crate::util::pallet_utils::FOLDER_NAME_META_ROOT), MetaPalletName]);
static META_FOLDER_STRAY: TaintShape = shape("a ref in the meta folder that is no meta pallet's", None,
    &[Is(crate::util::pallet_utils::FOLDER_NAME_META_ROOT), Name]);
static META_COPY: TaintShape = shape("a meta pallet ref's copy", Some(Disposition::MetaCopy),
    &[Is(crate::util::pallet_utils::FOLDER_NAME_META_COPY_ROOT), MetaPalletName]);
static META_COPY_STRAY: TaintShape = shape("a copy of a ref that is no meta pallet's", None,
    &[Is(crate::util::pallet_utils::FOLDER_NAME_META_COPY_ROOT), Name]);

static LOOSE_OBJECT: TaintShape = shape("a loose object", Some(Disposition::Loose),
    &[Is(crate::globals::FOLDER_NAME_OBJECTS_ROOT), FanOut, Hex]);
static LOOSE_SIGNATURE: TaintShape = shape("a loose object's signature", Some(Disposition::Signature),
    &[Is(crate::globals::FOLDER_NAME_OBJECTS_ROOT), FanOut, HexSignature]);
static PACK: TaintShape = shape("a pack", Some(Disposition::PackData), &[Is(crate::globals::FOLDER_NAME_OBJECTS_ROOT),
    Is(crate::util::pack_utils::FOLDER_NAME_PACK), Extension(crate::util::pack_utils::FILE_EXTENSION_PACK_DATA)]);
static PACK_INDEX: TaintShape = shape("a pack index", Some(Disposition::PackIndex), &[Is(crate::globals::FOLDER_NAME_OBJECTS_ROOT),
    Is(crate::util::pack_utils::FOLDER_NAME_PACK), Extension(crate::util::pack_utils::FILE_EXTENSION_PACK_INDEX)]);
static SIGNATURE_TABLE: TaintShape = shape("a signature table", Some(Disposition::SignatureTable),
    &[Is(crate::globals::FOLDER_NAME_OBJECTS_ROOT), Is(crate::util::pack_utils::FOLDER_NAME_PACK),
        Extension(crate::util::signature_table::FILE_EXTENSION_SIGNATURE_TABLE)]);
static SHARD: TaintShape = shape("a staged inventory shard", Some(Disposition::Shard), &[Is("inventory"), Names, Is("data")]);
static BAY_SHARD: TaintShape = shape("a bay's staged inventory shard", Some(Disposition::Shard), &[Is("bays"), Name, Is("inventory"), Names, Is("data")]);

static LOAD_MARKER: TaintShape = shape("the incomplete-load marker", Some(Disposition::LoadMarker), &[Is("inventory"), Is("incomplete-load")]);
static POSITION: TaintShape = shape("the checkout position", Some(Disposition::Position), &[Is("position")]);
static POSITION_COPY: TaintShape = shape("the checkout position's copy", Some(Disposition::PositionCopy), &[Is("position-copy")]);
static SCOPE: TaintShape = shape("the checkout scope", Some(Disposition::Scope), &[Is("scope")]);
static SCOPE_COPY: TaintShape = shape("the checkout scope's copy", Some(Disposition::ScopeCopy), &[Is("scope-copy")]);
static CONSOLIDATION: TaintShape = shape("the consolidation state", Some(Disposition::MergeRecord), &[Is("consolidation")]);
static CONSOLIDATION_COPY: TaintShape = shape("the consolidation state's copy", Some(Disposition::MergeRecordCopy), &[Is("consolidation-copy")]);
static SKELETON: TaintShape = shape("the consolidation skeleton", Some(Disposition::MergeRecord), &[Is("consolidation-skeleton")]);
static SKELETON_COPY: TaintShape = shape("the consolidation skeleton's copy", Some(Disposition::MergeRecordCopy), &[Is("consolidation-skeleton-copy")]);
static JOURNAL: TaintShape = shape("the journal", Some(Disposition::Journal), &[Is("journal.json")]);

static BAY_LOAD_MARKER: TaintShape = shape("a bay's incomplete-load marker", Some(Disposition::LoadMarker), &[Is("bays"), Name, Is("inventory"), Is("incomplete-load")]);
static BAY_POSITION: TaintShape = shape("a bay's checkout position", Some(Disposition::Position), &[Is("bays"), Name, Is("position")]);
static BAY_POSITION_COPY: TaintShape = shape("a bay's checkout position's copy", Some(Disposition::PositionCopy), &[Is("bays"), Name, Is("position-copy")]);
static BAY_SCOPE: TaintShape = shape("a bay's checkout scope", Some(Disposition::Scope), &[Is("bays"), Name, Is("scope")]);
static BAY_SCOPE_COPY: TaintShape = shape("a bay's checkout scope's copy", Some(Disposition::ScopeCopy), &[Is("bays"), Name, Is("scope-copy")]);
static BAY_CONSOLIDATION: TaintShape = shape("a bay's consolidation state", Some(Disposition::MergeRecord), &[Is("bays"), Name, Is("consolidation")]);
static BAY_CONSOLIDATION_COPY: TaintShape = shape("a bay's consolidation state's copy", Some(Disposition::MergeRecordCopy), &[Is("bays"), Name, Is("consolidation-copy")]);
static BAY_SKELETON: TaintShape = shape("a bay's consolidation skeleton", Some(Disposition::MergeRecord), &[Is("bays"), Name, Is("consolidation-skeleton")]);
static BAY_SKELETON_COPY: TaintShape = shape("a bay's consolidation skeleton's copy", Some(Disposition::MergeRecordCopy), &[Is("bays"), Name, Is("consolidation-skeleton-copy")]);
static BAY_JOURNAL: TaintShape = shape("a bay's journal", Some(Disposition::Journal), &[Is("bays"), Name, Is("journal.json")]);
static BAY_PATH: TaintShape = shape("a bay's path record", Some(Disposition::BayPath), &[Is("bays"), Name, Is("path")]);
static BAY_CHERRY_PICK: TaintShape = shape("a bay's cherry-pick state", Some(Disposition::MergeRecord), &[Is("bays"), Name, Is("cherry-pick")]);
static BAY_CHERRY_PICK_COPY: TaintShape = shape("a bay's cherry-pick state's copy", Some(Disposition::MergeRecordCopy), &[Is("bays"), Name, Is("cherry-pick-copy")]);

static CHERRY_PICK: TaintShape = shape("the cherry-pick state", Some(Disposition::MergeRecord), &[Is("cherry-pick")]);
static CHERRY_PICK_COPY: TaintShape = shape("the cherry-pick state's copy", Some(Disposition::MergeRecordCopy), &[Is("cherry-pick-copy")]);
static FETCH_SCOPE: TaintShape = shape("the fetch scope", Some(Disposition::FetchScope), &[Is("config"), Is("fetch-scope")]);
static FETCH_LEDGER: TaintShape = shape("the fetch ledger", Some(Disposition::FetchLedger), &[Is(crate::util::fetch_ledger::FILE_NAME_LEDGER)]);
static HELD_HEADS: TaintShape = shape("the held-heads record", None, &[Is(crate::util::held_heads::FILE_NAME)]);
static MIRROR_ROOTS: TaintShape = shape("the mirror roots record", Some(Disposition::MirrorRoots), &[Is(crate::util::mirror_utils::FILE_NAME_MIRROR_ROOTS)]);
static SERVED: TaintShape = shape("the served record", None, &[Is(crate::util::tombstone_utils::FILE_NAME_SERVED)]);
static TOMBSTONES: TaintShape = shape("the tombstones record", None, &[Is(crate::util::tombstone_utils::FILE_NAME_TOMBSTONES)]);
static UNVOUCHED_HEADS: TaintShape = shape("the unvouched-heads record", None, &[Is(crate::util::take_utils::FILE_NAME_UNVOUCHED)]);
static TRUST: TaintShape = shape("the trust anchor", None, &[Is(crate::util::office_utils::FILE_NAME_TRUST)]);
static TRUST_COPY: TaintShape = shape("the trust anchor's copy", None, &[Is(crate::util::office_utils::FILE_NAME_TRUST_COPY)]);

/// Every shape a taint record may name, in match order; the first match wins. The pallets/,
/// removed-refs/, meta/ and meta-copy/ entries come first, so a ref or keepsake whose name looks
/// like an object, a shard, a pack or a signature is matched as what it is. A shape missing here
/// fails [`assert_declared`] in a debug build the first time a test writes it, and a
/// [`DurablePath`] built for a [`WriterShape`] is checked against the entry the variant names.
static TAINT_SHAPES: &[&TaintShape] = &[
    &PALLET_REF, &KEEPSAKE, &META_REF, &META_FOLDER_STRAY, &META_COPY, &META_COPY_STRAY,
    &LOOSE_OBJECT, &LOOSE_SIGNATURE, &PACK, &PACK_INDEX, &SIGNATURE_TABLE, &SHARD, &BAY_SHARD,
    &LOAD_MARKER, &POSITION, &POSITION_COPY, &SCOPE, &SCOPE_COPY, &CONSOLIDATION, &CONSOLIDATION_COPY,
    &SKELETON, &SKELETON_COPY, &JOURNAL,
    &BAY_LOAD_MARKER, &BAY_POSITION, &BAY_POSITION_COPY, &BAY_SCOPE, &BAY_SCOPE_COPY, &BAY_CONSOLIDATION,
    &BAY_CONSOLIDATION_COPY, &BAY_SKELETON, &BAY_SKELETON_COPY, &BAY_JOURNAL, &BAY_PATH, &BAY_CHERRY_PICK,
    &BAY_CHERRY_PICK_COPY,
    &CHERRY_PICK, &CHERRY_PICK_COPY, &FETCH_SCOPE, &FETCH_LEDGER, &HELD_HEADS, &MIRROR_ROOTS, &SERVED, &TOMBSTONES,
    &UNVOUCHED_HEADS, &TRUST, &TRUST_COPY,
];

/// The declared shape of a root-relative path, or `None` for a path no entry of the registry
/// matches. A path with any component other than a plain name (a root, a `.`, a `..`) matches
/// none.
pub(crate) fn taint_shape(relative: &Path) -> Option<&'static TaintShape> {
    let mut names = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(name) => names.push(name),
            _ => return None,
        }
    }

    TAINT_SHAPES.iter().copied().find(|shape| pattern_matches(shape.pattern, &names))
}

fn pattern_matches(pattern: &[Part], names: &[&std::ffi::OsStr]) -> bool {
    let Some((part, rest)) = pattern.split_first() else {
        return names.is_empty();
    };

    if let Names = part {
        return (1..=names.len()).any(|taken| pattern_matches(rest, &names[taken..]));
    }

    let Some((name, names)) = names.split_first() else {
        return false;
    };
    let text = name.to_str();
    let fits = match part {
        Is(expected) => *name == std::ffi::OsStr::new(expected),
        Name => true,
        Names => unreachable!("handled above"),
        FanOut => text.is_some_and(|text| text.len() == file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS
            && file_utils::is_lowercase_hex(text)),
        Hex => text.is_some_and(file_utils::is_lowercase_hex),
        HexSignature => text.and_then(|text| text.strip_suffix(crate::util::sign_utils::FILE_SUFFIX_SIGNATURE))
            .is_some_and(file_utils::is_lowercase_hex),
        Extension(extension) => {
            let name = Path::new(name);
            name.extension() == Some(std::ffi::OsStr::new(extension))
                && name.file_stem().is_some_and(|stem| !stem.is_empty())
        }
        MetaPalletName => crate::util::meta_tree::META_PALLET_NAMES.iter().any(|known| *name == std::ffi::OsStr::new(known)),
    };

    fits && pattern_matches(rest, names)
}

/// In a debug build, refuse a batch of final paths that a taint could record and whose shape is
/// not declared in [`TAINT_SHAPES`], naming the first such path. Only a batch [`resolve_root_for`]
/// places under the root is checked, since [`record_taint`] records nothing for any other; and the
/// check runs whether or not this process is activated, so every write the suites drive is checked,
/// not only the ones whose sync fails. It compiles to nothing in a release build.
pub(crate) fn assert_declared(final_paths: &[&Path]) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(root) = resolve_root_for(final_paths) else {
        return;
    };

    for relative in to_root_relative(&root, final_paths) {
        debug_assert!(
            taint_shape(&relative).is_some(),
            "a write under \"{}\" names \"{}\", a path whose shape no entry of TAINT_SHAPES declares",
            root.to_string_lossy(), taint_record_line(&relative)
        );
    }
}

/// Which checkout a checkout-local record belongs to: the main working tree's, kept at the root
/// of [`forklift_root`], or a named bay's, kept under its `bays/<name>` folder there.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checkout {
    Main,
    Bay(String),
}

impl Checkout {
    /// The checkout this thread works in, read from the bay context as
    /// [`bay_root`](crate::globals::bay_root) reads it.
    pub fn active() -> Checkout {
        match crate::globals::active_bay() {
            None => Checkout::Main,
            Some(name) => Checkout::Bay(name),
        }
    }

    /// `file` in this checkout's folder, root-relative, with the variant of the two given that
    /// declares it there.
    fn place(&self, main: WriterShape, bay: WriterShape, file: &Path) -> (WriterShape, PathBuf) {
        match self {
            Checkout::Main => (main, file.to_path_buf()),
            Checkout::Bay(name) => (bay, Path::new(crate::globals::FOLDER_NAME_BAYS_ROOT).join(name).join(file)),
        }
    }
}

/// One kind of path a writer produces: each names the [`TaintShape`] its paths take. An entry of
/// [`TAINT_SHAPES`] no variant names is one only heal meets, such as a stray name in the meta
/// folder.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriterShape {
    PalletRef,
    Keepsake,
    MetaRef,
    MetaCopy,
    LooseObject,
    LooseSignature,
    Pack,
    PackIndex,
    SignatureTable,
    Shard,
    BayShard,
    LoadMarker,
    BayLoadMarker,
    Position,
    BayPosition,
    PositionCopy,
    BayPositionCopy,
    Scope,
    BayScope,
    ScopeCopy,
    BayScopeCopy,
    Consolidation,
    BayConsolidation,
    ConsolidationCopy,
    BayConsolidationCopy,
    Skeleton,
    BaySkeleton,
    SkeletonCopy,
    BaySkeletonCopy,
    Journal,
    BayJournal,
    CherryPick,
    BayCherryPick,
    CherryPickCopy,
    BayCherryPickCopy,
    BayPath,
    FetchScope,
    FetchLedger,
    HeldHeads,
    MirrorRoots,
    Served,
    Tombstones,
    UnvouchedHeads,
    Trust,
    TrustCopy,
}

impl WriterShape {
    /// The entry of [`TAINT_SHAPES`] this variant's paths take. No wildcard arm: a new variant
    /// does not compile until it names one.
    fn entry(self) -> &'static TaintShape {
        match self {
            WriterShape::PalletRef => &PALLET_REF,
            WriterShape::Keepsake => &KEEPSAKE,
            WriterShape::MetaRef => &META_REF,
            WriterShape::MetaCopy => &META_COPY,
            WriterShape::LooseObject => &LOOSE_OBJECT,
            WriterShape::LooseSignature => &LOOSE_SIGNATURE,
            WriterShape::Pack => &PACK,
            WriterShape::PackIndex => &PACK_INDEX,
            WriterShape::SignatureTable => &SIGNATURE_TABLE,
            WriterShape::Shard => &SHARD,
            WriterShape::BayShard => &BAY_SHARD,
            WriterShape::LoadMarker => &LOAD_MARKER,
            WriterShape::BayLoadMarker => &BAY_LOAD_MARKER,
            WriterShape::Position => &POSITION,
            WriterShape::BayPosition => &BAY_POSITION,
            WriterShape::PositionCopy => &POSITION_COPY,
            WriterShape::BayPositionCopy => &BAY_POSITION_COPY,
            WriterShape::Scope => &SCOPE,
            WriterShape::BayScope => &BAY_SCOPE,
            WriterShape::ScopeCopy => &SCOPE_COPY,
            WriterShape::BayScopeCopy => &BAY_SCOPE_COPY,
            WriterShape::Consolidation => &CONSOLIDATION,
            WriterShape::BayConsolidation => &BAY_CONSOLIDATION,
            WriterShape::ConsolidationCopy => &CONSOLIDATION_COPY,
            WriterShape::BayConsolidationCopy => &BAY_CONSOLIDATION_COPY,
            WriterShape::Skeleton => &SKELETON,
            WriterShape::BaySkeleton => &BAY_SKELETON,
            WriterShape::SkeletonCopy => &SKELETON_COPY,
            WriterShape::BaySkeletonCopy => &BAY_SKELETON_COPY,
            WriterShape::Journal => &JOURNAL,
            WriterShape::BayJournal => &BAY_JOURNAL,
            WriterShape::CherryPick => &CHERRY_PICK,
            WriterShape::BayCherryPick => &BAY_CHERRY_PICK,
            WriterShape::CherryPickCopy => &CHERRY_PICK_COPY,
            WriterShape::BayCherryPickCopy => &BAY_CHERRY_PICK_COPY,
            WriterShape::BayPath => &BAY_PATH,
            WriterShape::FetchScope => &FETCH_SCOPE,
            WriterShape::FetchLedger => &FETCH_LEDGER,
            WriterShape::HeldHeads => &HELD_HEADS,
            WriterShape::MirrorRoots => &MIRROR_ROOTS,
            WriterShape::Served => &SERVED,
            WriterShape::Tombstones => &TOMBSTONES,
            WriterShape::UnvouchedHeads => &UNVOUCHED_HEADS,
            WriterShape::Trust => &TRUST,
            WriterShape::TrustCopy => &TRUST_COPY,
        }
    }
}

/// The final path of a durable write: what the write entries of
/// [`file_utils`](crate::util::file_utils) hand a taint record. Its field is private, so outside
/// this module one is had only from its constructors: one per record, taking a [`Checkout`]
/// where the record is checkout-local, each building its path under [`forklift_root`] from the
/// record's parts, or [`DurablePath::outside_warehouse`] for a file no warehouse records. It
/// implements no `From`, `AsRef` or `Deref` to or from a path type; [`DurablePath::as_path`]
/// lends its path. A constructor reads
/// [`forklift_root`] when it is called, and that changes when a bay context is entered, so one
/// built before the switch names the old root.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DurablePath(PathBuf);

impl DurablePath {
    /// The path, for the write itself.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// `relative` under [`forklift_root`], for a writer of `shape`. In a debug build, refuses a
    /// path whose declared entry is not the one `shape` names, naming both, such as a meta copy
    /// built for an ordinary pallet's name, which classifies as a stray in the meta-copy folder.
    fn declared(shape: WriterShape, relative: PathBuf) -> DurablePath {
        if cfg!(debug_assertions) {
            let found = taint_shape(&relative);
            assert!(
                found.is_some_and(|found| std::ptr::eq(found, shape.entry())),
                "a {:?} writer built \"{}\", which is {}, not {}",
                shape, taint_record_line(&relative), found.map_or("no declared shape", |found| found.description),
                shape.entry().description
            );
        }
        DurablePath(forklift_root().join(relative))
    }

    /// A durable file no warehouse records: one outside [`forklift_root`], such as the key-owner
    /// manifest in the keys folder or a bay's redirect in its staging folder. In a debug build,
    /// refuses a path [`record_taint`] would place under the root.
    pub fn outside_warehouse(path: PathBuf) -> DurablePath {
        if cfg!(debug_assertions) {
            assert!(
                resolve_root_for(&[&path]).is_none(),
                "an out-of-warehouse write names \"{}\", which is under the warehouse folder \"{}\"",
                path.to_string_lossy(), forklift_root().to_string_lossy()
            );
        }
        DurablePath(path)
    }

    /// User pallet `name`'s ref, each `/`-separated part of the name a folder, as
    /// [`get_pallet_ref_path`](crate::util::pallet_utils::get_pallet_ref_path) spells it.
    pub fn pallet_ref(name: &str) -> DurablePath {
        Self::declared(WriterShape::PalletRef, name_under(crate::util::pallet_utils::FOLDER_NAME_PALLETS_ROOT, name))
    }

    /// The keepsake of a removed ref, by its slot's file name in the removed-refs folder.
    pub fn keepsake(file_name: &str) -> DurablePath {
        Self::declared(WriterShape::Keepsake, Path::new(crate::util::ref_heal::FOLDER_NAME_REMOVED_REFS).join(file_name))
    }

    pub fn meta_ref(name: &str) -> DurablePath {
        Self::declared(WriterShape::MetaRef, name_under(crate::util::pallet_utils::FOLDER_NAME_META_ROOT, name))
    }

    pub fn meta_copy(name: &str) -> DurablePath {
        Self::declared(WriterShape::MetaCopy, name_under(crate::util::pallet_utils::FOLDER_NAME_META_COPY_ROOT, name))
    }

    /// A loose object, by its full hash, at its fan-out folder. `Err` for a hash too short to
    /// split or not all lowercase hex.
    pub fn loose_object(hash: &str) -> Result<DurablePath, String> {
        let (fan_out, rest) = split_object_hash(hash)?;
        Ok(Self::declared(WriterShape::LooseObject, Path::new(crate::globals::FOLDER_NAME_OBJECTS_ROOT).join(fan_out).join(rest)))
    }

    /// A loose object's signature, beside the object.
    pub fn loose_signature(hash: &str) -> Result<DurablePath, String> {
        let (fan_out, rest) = split_object_hash(hash)?;
        let file_name = format!("{}{}", rest, crate::util::sign_utils::FILE_SUFFIX_SIGNATURE);
        Ok(Self::declared(WriterShape::LooseSignature, Path::new(crate::globals::FOLDER_NAME_OBJECTS_ROOT).join(fan_out).join(file_name)))
    }

    pub fn pack(id: &str) -> DurablePath {
        Self::declared(WriterShape::Pack, pack_folder_file(id, crate::util::pack_utils::FILE_EXTENSION_PACK_DATA))
    }

    pub fn pack_index(id: &str) -> DurablePath {
        Self::declared(WriterShape::PackIndex, pack_folder_file(id, crate::util::pack_utils::FILE_EXTENSION_PACK_INDEX))
    }

    pub fn signature_table(id: &str) -> DurablePath {
        Self::declared(WriterShape::SignatureTable,
            pack_folder_file(id, crate::util::signature_table::FILE_EXTENSION_SIGNATURE_TABLE))
    }

    /// The staged inventory shard of the directory whose warehouse path key is `key`, at its
    /// shard address in the checkout's inventory. `Err` when that inventory's metadata names
    /// another format version, as [`file_utils::get_inventory_data_path_for_key`] refuses it.
    pub fn shard(checkout: &Checkout, key: &str) -> Result<DurablePath, String> {
        let (_, inventory) = checkout.place(WriterShape::Shard, WriterShape::BayShard,
            Path::new(crate::globals::FOLDER_NAME_INVENTORY_ROOT));
        file_utils::gate_inventory_layout(&forklift_root().join(&inventory))?;

        let file = Path::new(crate::globals::FOLDER_NAME_INVENTORY_ROOT).join(file_utils::shard_folder_relative(key))
            .join(file_utils::FILE_NAME_INVENTORY_DATA);
        let (shape, relative) = checkout.place(WriterShape::Shard, WriterShape::BayShard, &file);
        Ok(Self::declared(shape, relative))
    }

    pub fn load_marker(checkout: &Checkout) -> DurablePath {
        let file = Path::new(crate::globals::FOLDER_NAME_INVENTORY_ROOT).join("incomplete-load");
        let (shape, relative) = checkout.place(WriterShape::LoadMarker, WriterShape::BayLoadMarker, &file);
        Self::declared(shape, relative)
    }

    pub fn position(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::Position, WriterShape::BayPosition,
            Path::new(crate::util::pallet_utils::FILE_NAME_POSITION));
        Self::declared(shape, relative)
    }

    pub fn position_copy(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::PositionCopy, WriterShape::BayPositionCopy,
            Path::new(crate::util::pallet_utils::FILE_NAME_POSITION_COPY));
        Self::declared(shape, relative)
    }

    pub fn scope(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::Scope, WriterShape::BayScope,
            Path::new(crate::util::scope_utils::FILE_NAME_BAY_SCOPE));
        Self::declared(shape, relative)
    }

    pub fn scope_copy(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::ScopeCopy, WriterShape::BayScopeCopy,
            Path::new(crate::util::scope_utils::FILE_NAME_SCOPE_COPY));
        Self::declared(shape, relative)
    }

    pub fn consolidation(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::Consolidation, WriterShape::BayConsolidation,
            Path::new(crate::util::merge_utils::FILE_NAME_CONSOLIDATION));
        Self::declared(shape, relative)
    }

    pub fn consolidation_copy(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::ConsolidationCopy, WriterShape::BayConsolidationCopy,
            Path::new(crate::util::merge_utils::FILE_NAME_CONSOLIDATION_COPY));
        Self::declared(shape, relative)
    }

    pub fn skeleton(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::Skeleton, WriterShape::BaySkeleton,
            Path::new(crate::util::merge_utils::FILE_NAME_SKELETON));
        Self::declared(shape, relative)
    }

    pub fn skeleton_copy(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::SkeletonCopy, WriterShape::BaySkeletonCopy,
            Path::new(crate::util::merge_utils::FILE_NAME_SKELETON_COPY));
        Self::declared(shape, relative)
    }

    pub fn journal(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::Journal, WriterShape::BayJournal, Path::new("journal.json"));
        Self::declared(shape, relative)
    }

    pub fn cherry_pick(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::CherryPick, WriterShape::BayCherryPick,
            Path::new(crate::util::cherry_pick_utils::FILE_NAME_CHERRY_PICK));
        Self::declared(shape, relative)
    }

    pub fn cherry_pick_copy(checkout: &Checkout) -> DurablePath {
        let (shape, relative) = checkout.place(WriterShape::CherryPickCopy, WriterShape::BayCherryPickCopy,
            Path::new(crate::util::cherry_pick_utils::FILE_NAME_CHERRY_PICK_COPY));
        Self::declared(shape, relative)
    }

    /// A bay's path record, which names the bay's working directory.
    pub fn bay_path(name: &str) -> DurablePath {
        Self::declared(WriterShape::BayPath, Path::new(crate::globals::FOLDER_NAME_BAYS_ROOT).join(name).join("path"))
    }

    pub fn fetch_scope() -> DurablePath {
        Self::declared(WriterShape::FetchScope, Path::new(crate::util::config_utils::FOLDER_NAME_CONFIG_ROOT).join("fetch-scope"))
    }

    pub fn fetch_ledger() -> DurablePath {
        Self::declared(WriterShape::FetchLedger, PathBuf::from(crate::util::fetch_ledger::FILE_NAME_LEDGER))
    }

    pub fn held_heads() -> DurablePath {
        Self::declared(WriterShape::HeldHeads, PathBuf::from(crate::util::held_heads::FILE_NAME))
    }

    pub fn mirror_roots() -> DurablePath {
        Self::declared(WriterShape::MirrorRoots, PathBuf::from(crate::util::mirror_utils::FILE_NAME_MIRROR_ROOTS))
    }

    pub fn served() -> DurablePath {
        Self::declared(WriterShape::Served, PathBuf::from(crate::util::tombstone_utils::FILE_NAME_SERVED))
    }

    pub fn tombstones() -> DurablePath {
        Self::declared(WriterShape::Tombstones, PathBuf::from(crate::util::tombstone_utils::FILE_NAME_TOMBSTONES))
    }

    pub fn unvouched_heads() -> DurablePath {
        Self::declared(WriterShape::UnvouchedHeads, PathBuf::from(crate::util::take_utils::FILE_NAME_UNVOUCHED))
    }

    pub fn trust() -> DurablePath {
        Self::declared(WriterShape::Trust, PathBuf::from(crate::util::office_utils::FILE_NAME_TRUST))
    }

    pub fn trust_copy() -> DurablePath {
        Self::declared(WriterShape::TrustCopy, PathBuf::from(crate::util::office_utils::FILE_NAME_TRUST_COPY))
    }
}

/// A full object hash split at its fan-out folder, or `Err` for anything
/// [`file_utils::get_path_for_object`] refuses: too short to split, or not all lowercase hex.
fn split_object_hash(hash: &str) -> Result<(&str, &str), String> {
    if !file_utils::is_object_hash_or_prefix(hash) {
        return Err(format!("\"{}\" is not a valid object hash.", hash));
    }

    Ok(hash.split_at(file_utils::OBJECT_HASH_FOLDER_PATH_CHARACTERS))
}

/// Pallet `name` in `folder`, root-relative, each `/`-separated part of the name its own
/// component.
fn name_under(folder: &str, name: &str) -> PathBuf {
    let mut path = PathBuf::from(folder);
    for part in name.split('/') {
        path.push(part);
    }
    path
}

/// `<id>.<extension>` in the pack folder, root-relative.
fn pack_folder_file(id: &str, extension: &str) -> PathBuf {
    Path::new(crate::globals::FOLDER_NAME_OBJECTS_ROOT).join(crate::util::pack_utils::FOLDER_NAME_PACK)
        .join(format!("{}.{}", id, extension))
}

/// Strip `root` off every path in `final_paths`. Only ever called once [`resolve_root_for`] has
/// already confirmed every path in `final_paths` is under `root`, so every `strip_prefix` here is
/// infallible in practice; `filter_map` is used defensively rather than to signal a real partial
/// case.
fn to_root_relative(root: &Path, final_paths: &[&Path]) -> BTreeSet<PathBuf> {
    final_paths.iter()
        .filter_map(|path| path.strip_prefix(root).ok().map(Path::to_path_buf))
        .collect()
}

/// Render one root-relative path as a taint record line: its components joined with `/`,
/// regardless of the platform writing the record — see the module doc comment's format section
/// for why. Deliberately a per-component join, not `path.to_string_lossy()` followed by a
/// blanket `\`-to-`/` replace: a blanket replace would corrupt a Unix filename that legitimately
/// contains a literal backslash, which is a valid path component on that platform.
fn taint_record_line(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

/// The taint's own write routine — deliberately not [`file_utils::write_file_atomically`] (see
/// [`record_taint`]'s doc comment on no re-entry). Exclusive-creates a fresh file under
/// `<root>/taint/`, writes every recorded path plus the digest, fsyncs the file's own bytes,
/// then fsyncs the directory, then the folders above it. On any failure past file creation up to
/// the directory's own sync, best-effort removes the file it created before returning the error.
/// A failure syncing the folders above keeps the file and returns the error.
fn write_taint_file(root: &Path, relative_paths: &BTreeSet<PathBuf>) -> Result<(), String> {
    let taint_dir = root.join(FOLDER_NAME_TAINT);
    file_utils::create_folder_if_not_exists(&taint_dir)?;

    let (mut handle, candidate) = create_taint_file(&taint_dir)?;

    let content = taint_file_bytes(relative_paths.iter().map(|path| taint_record_line(path)));

    if let Err(e) = handle.write_all(&content) {
        let _ = file_utils::remove_file(&candidate);
        return Err(format!("Error while writing taint file \"{}\": {}", candidate.to_string_lossy(), e));
    }

    if let Err(e) = sync_taint_file(&handle, &candidate) {
        let _ = file_utils::remove_file(&candidate);
        return Err(e);
    }

    // The directory counterpart of the fsync above: the taint file's own bytes durable is not
    // enough if its directory entry (the name itself) is not. A plain fsync-of-a-directory-handle
    // helper, never itself a barrier that could fail a rename and recurse back into tainting.
    if let Err(e) = file_utils::sync_dir_alone(&taint_dir) {
        let _ = file_utils::remove_file(&candidate);
        return Err(e);
    }

    // The folders above: the failure being recorded may be one of theirs, so the record is kept
    // either way (one whose folder entry is not yet durable still gates every later command,
    // where a removed one gates nothing), and the failure is returned beside it.
    file_utils::sync_ancestors_to_storage_root(&taint_dir)
        .map_err(|e| format!("the taint is recorded, but its folder may not survive a power loss: {}", e))
}

/// The process-global, never-reset source of the `<counter>` suffix in every taint filename this
/// process creates — see the module doc comment's format section for why "never reset, even
/// across a file's own deletion" (not just "unique among files that currently exist") is the
/// property that makes a snapshot-scoped delete safe. `fetch_add` is the only operation ever
/// performed on this counter, from [`create_taint_file`] alone; ordering among concurrent draws
/// within this process does not matter, only that no two draws ever return the same value, which
/// `Ordering::Relaxed` already guarantees for a plain monotonic counter.
static TAINT_FILENAME_COUNTER: AtomicU64 = AtomicU64::new(0);

/// This process's own random nonce component of every taint filename it creates — drawn once,
/// lazily, on first use, and reused for the rest of the process's life (never redrawn per file).
/// Defends against the `<pid>` half of a filename repeating across processes — a crash survivor
/// whose replacement process happens to reuse the exact same OS `pid` — see the module doc
/// comment's format section.
fn taint_filename_nonce() -> u64 {
    static NONCE: OnceLock<u64> = OnceLock::new();
    *NONCE.get_or_init(|| {
        use std::hash::{BuildHasher, Hasher};
        // `RandomState::new()` is itself independently seeded per construction (the standard
        // library draws fresh OS entropy for it); folding in the pid and the current time on top
        // costs nothing and removes any dependence on exactly how that seeding is implemented.
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        if let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
            hasher.write_u128(now.as_nanos());
        }
        hasher.finish()
    })
}

/// Exclusive-create the next free `taint-<pid>-<nonce_hex>-<counter>` file under `taint_dir`:
/// `O_CREAT|O_EXCL` via [`std::fs::OpenOptions::create_new`]. `<counter>` is drawn fresh from
/// [`TAINT_FILENAME_COUNTER`] on every attempt — including every retry — never a locally-restarted
/// `0..CAP` index, so a name this process has ever handed out (even one whose file has since been
/// deleted) can never be handed out again: see the module doc comment's format section for why
/// that "unique-forever" property is what makes a snapshot-scoped delete
/// (`remove_taint_files`/`resolve_taints`) safe. `EEXIST` on a given candidate (a
/// crash survivor from an earlier process that reused this exact `pid` and independently drew a
/// colliding nonce, or a same-process race on the freshly-drawn counter value some other way)
/// retries with the next freshly-drawn counter value rather than ever renaming over it — see
/// [`record_taint`]'s doc comment: a crash survivor structurally cannot be overwritten by exclusive
/// create, whatever its name.
fn create_taint_file(taint_dir: &Path) -> Result<(std::fs::File, PathBuf), String> {
    let pid = std::process::id();
    let nonce = taint_filename_nonce();

    for _ in 0..TAINT_FILENAME_SANITY_CAP {
        let counter = TAINT_FILENAME_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = taint_dir.join(format!("taint-{}-{:016x}-{}", pid, nonce, counter));
        match OpenOptions::new().write(true).create_new(true).open(&candidate) {
            Ok(handle) => return Ok((handle, candidate)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!(
                "Error while creating taint file \"{}\": {}", candidate.to_string_lossy(), e
            )),
        }
    }

    Err(format!(
        "Could not find a free taint file name under \"{}\" after {} attempts.",
        taint_dir.to_string_lossy(), TAINT_FILENAME_SANITY_CAP
    ))
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection for [`sync_taint_file`], mirroring `file_utils`'s
    /// `DirSyncFaultGuard`: records every taint file path it is asked to sync, in call order,
    /// and can be armed to fail for paths containing a given substring instead of touching the
    /// filesystem. Thread-local for the same reason as its `file_utils` counterpart — a taint
    /// write always runs entirely on its caller's thread, with the same exception: a fetched
    /// bundle's import runs on a blocking-pool thread, out of reach of a rig armed by the caller.
    static TAINT_WRITE_FAULT: std::cell::RefCell<TaintFaultState> =
        const { std::cell::RefCell::new(TaintFaultState { attempted: Vec::new(), fail_needle: None }) };
}

#[cfg(test)]
struct TaintFaultState {
    attempted: Vec<PathBuf>,
    fail_needle: Option<String>,
}

/// RAII scope for [`TAINT_WRITE_FAULT`]: both construction and `Drop` reset this thread's state,
/// so neither a stale guard from a previous test on a reused thread, nor this guard's own
/// arming once the test that created it is done, can bleed into another test.
#[cfg(test)]
struct TaintFaultGuard {
    _syncs: file_utils::RealSyncs,
}

#[cfg(test)]
impl TaintFaultGuard {
    /// Record every taint file [`sync_taint_file`] is asked to sync; fail none of them.
    fn recording() -> Self {
        // Before arming: a panic here must leave nothing armed.
        let syncs = file_utils::real_syncs();
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState { attempted: Vec::new(), fail_needle: None });
        TaintFaultGuard { _syncs: syncs }
    }

    /// Record every taint file, and fail (with a distinctive error, no filesystem access) any
    /// whose path contains `needle`.
    fn failing(needle: &str) -> Self {
        // Before arming: a panic here must leave nothing armed.
        let syncs = file_utils::real_syncs();
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState {
            attempted: Vec::new(),
            fail_needle: Some(needle.to_string()),
        });
        TaintFaultGuard { _syncs: syncs }
    }
}

#[cfg(test)]
impl Drop for TaintFaultGuard {
    fn drop(&mut self) {
        TAINT_WRITE_FAULT.with(|f| *f.borrow_mut() = TaintFaultState { attempted: Vec::new(), fail_needle: None });
    }
}

/// The taint files [`sync_taint_file`] has been asked to sync on this thread since the current
/// [`TaintFaultGuard`] was armed, in call order.
#[cfg(test)]
fn taint_write_attempts() -> Vec<PathBuf> {
    TAINT_WRITE_FAULT.with(|f| f.borrow().attempted.clone())
}

/// Fsync the taint file's own bytes — `File::sync_all` is `F_FULLFSYNC` on macOS (a real
/// device-cache flush), so this alone is sufficient device durability for the taint file itself,
/// with no separate device-flush call needed (unlike the cheap `libc::fsync` used elsewhere in
/// this store's batched barrier, which is why *that* path needs one). Honors [`file_utils::fsync_enabled`]
/// like every other durability step in this store.
fn sync_taint_file(handle: &std::fs::File, path: &Path) -> Result<(), String> {
    #[cfg(test)]
    if let Some(injected) = TAINT_WRITE_FAULT.with(|f| {
        let mut f = f.borrow_mut();
        f.attempted.push(path.to_path_buf());
        f.fail_needle.as_deref()
            .filter(|needle| path.to_string_lossy().contains(needle))
            .map(|_| format!("injected taint-write failure for \"{}\"", path.to_string_lossy()))
    }) {
        return Err(injected);
    }

    if file_utils::fsync_enabled() {
        handle.sync_all()
            .map_err(|e| format!("Error while syncing taint file \"{}\": {}", path.to_string_lossy(), e))?;
    }

    Ok(())
}

/// The process-local per-root gate — see [`gate_check`]'s doc comment.
fn gate_state() -> &'static Mutex<BTreeSet<PathBuf>> {
    static GATE: OnceLock<Mutex<BTreeSet<PathBuf>>> = OnceLock::new();
    GATE.get_or_init(|| Mutex::new(BTreeSet::new()))
}

/// Lock-free mirror of [`gate_state`]'s cardinality — exactly the number of roots currently
/// gated. Exists only so [`gate_check`] can take a fast negative without taking the mutex on the
/// overwhelmingly common "no gate is standing anywhere" path (a bulk import/dedup calling this
/// from many threads per object checked would otherwise serialize entirely on that mutex for a
/// belt that is almost never tripped).
///
/// Kept exactly consistent with the set by construction, never independently: [`set_gate`] and
/// [`clear_gate`] adjust this *while still holding the same lock* as their `BTreeSet` mutation,
/// and only when that mutation actually changed membership (`insert`/`remove`'s `bool` return) —
/// never on every call, since both are idempotent (setting an already-set root, or clearing an
/// already-clear one, must not drift the count). No other code path touches this counter.
///
/// **Why this is safe as a fast negative, and only a negative:** the mutex over the `BTreeSet`
/// remains the sole source of truth for *membership* — this counter only ever answers "is the set
/// empty," and only the zero case is trusted without the lock. Zero is reached exactly when the
/// last `remove` that actually removed something ran (or before any `insert` ever has), which by
/// construction is exactly when the set itself is empty; so observing zero here means the
/// `BTreeSet` holds nothing for *any* root, and returning `Ok` without checking `root` in
/// particular is correct — there is nothing in the set it could possibly contain. Whenever the
/// count is nonzero, [`gate_check`] still takes the lock and runs the exact same per-root
/// membership test as before this optimization existed, so a standing gate for one root is never
/// missed and a check for an unrelated root is never falsely tripped — this counter never
/// participates in that decision, only in whether to skip straight to `Ok` first.
///
/// **Ordering:** `Relaxed` on every access. This establishes no happens-before relationship, but
/// none is needed beyond what already existed: `gate_check` already races a concurrent
/// `set_gate`/`clear_gate` under the mutex-only design (a taint set by another thread a moment
/// after this check reads may or may not be observed, exactly as today), so this fast path is not
/// asked to make a promise the code it replaces didn't already decline to make. What it must not
/// do — and does not — is report zero while the set is actually nonempty, and it cannot: the
/// increment/decrement happen inside the same critical section as the mutation they mirror, so
/// every observer of this counter sees a value consistent with *some* linearization point of the
/// set's history, never a stale "empty" left over from before an insert that already completed.
static GATE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Reset the process-global gate — both [`gate_state`]'s `BTreeSet` and its [`GATE_COUNT`] mirror
/// — to guaranteed-empty. Test-only: this module's tests share this same process-global state
/// (like [`ACTIVATED`]) across the whole test binary, and several existing tests intentionally
/// leave a gate standing for their own (uniquely-named) root when they finish, so a test that
/// needs to observe the true fast-path (`GATE_COUNT == 0`) cannot assume that starting from a
/// fresh process. Callers still take [`ACTIVATION_TEST_LOCK`] first, same as every other
/// gate-touching test.
#[cfg(test)]
fn reset_gate_for_test() {
    gate_state().lock().expect("taint gate lock poisoned").clear();
    GATE_COUNT.store(0, Ordering::Relaxed);
}

/// The one resolution [`record_taint`] (setting) and [`gate_check`] (checking) both key the gate
/// through, so the two sides can never drift apart into resolving "the same root" two different
/// ways. Identity today (both callers already hand in an already-resolved [`forklift_root`]
/// value), but named and shared so a future normalization need only change here.
fn gate_key(root: &Path) -> PathBuf {
    root.to_path_buf()
}

fn set_gate(root: &Path) {
    // Idempotent: setting an already-set root must not double-count. `insert`'s `bool` return
    // (true only when the key was actually new) is exactly the signal — increment inside the
    // same critical section as the mutation it mirrors, see [`GATE_COUNT`].
    let inserted = gate_state().lock().expect("taint gate lock poisoned").insert(gate_key(root));
    if inserted {
        GATE_COUNT.fetch_add(1, Ordering::Relaxed);
    }
}

/// Check whether `root` is gated: `Err` while a taint [`record_taint`] set for this root is
/// standing, `Ok(())` otherwise. A no-op — always `Ok(())` — unless [`activate`] has been called
/// in this process (see the module doc comment).
///
/// This is the process-local belt, not the durable record — it only sees taints this same
/// process set, never a sibling process's. The returned error always contains
/// [`GATE_TAINT_MARKER`], so a caller can recognize it without matching the whole message.
///
/// # Returns
/// * `Ok(())`      - Not activated, or no taint is standing for `root`.
/// * `Err(String)` - Activated and a taint is standing for `root`.
pub fn gate_check(root: &Path) -> Result<(), String> {
    if !is_activated() {
        return Ok(());
    }

    // Fast negative, lock-free: no gate is standing for *any* root, so none can be standing for
    // `root` in particular — skip the mutex entirely. See [`GATE_COUNT`] for why this is sound
    // as a negative-only shortcut and why `Relaxed` is the right ordering for it. When the count
    // is nonzero, fall through to the exact same locked membership test as before this
    // optimization existed — this branch never itself decides membership.
    if GATE_COUNT.load(Ordering::Relaxed) == 0 {
        return Ok(());
    }

    if gate_state().lock().expect("taint gate lock poisoned").contains(&gate_key(root)) {
        return Err(gate_standing_message(root));
    }

    Ok(())
}

/// Clear the gate for `root`, if one is standing. Module-private, on purpose, the same model
/// [`set_gate`] already uses: the only sound way to clear a gate is in the same breath as
/// checking what the taint directory actually holds, which is exactly what [`resolve_taints`]
/// does — a caller outside this module (or a future addition inside it that skipped
/// `resolve_taints`) has no way to reach this half of the clear on its own. See
/// [`test_clear_gate`] for the one sanctioned exception (test fixtures that isolate the gate from
/// the durable half on purpose).
fn clear_gate(root: &Path) {
    // Idempotent: clearing an already-clear root must not underflow. `remove`'s `bool` return
    // (true only when a key was actually removed) is exactly the signal — decrement inside the
    // same critical section as the mutation it mirrors, see [`GATE_COUNT`].
    let removed = gate_state().lock().expect("taint gate lock poisoned").remove(&gate_key(root));
    if removed {
        GATE_COUNT.fetch_sub(1, Ordering::Relaxed);
    }
}

/// The union of every taint file recorded under a storage root.
#[derive(Debug, Default)]
pub struct TaintState {
    /// Every root-relative final object path recorded across every taint file under the root,
    /// unioned. Populated even when [`torn`](Self::torn) is set — a torn file's parseable prefix
    /// still contributes (see the module doc comment's format section).
    pub recorded: BTreeSet<PathBuf>,

    /// Whether at least one taint file under the root did not match its digest — truncated by a
    /// crash mid-write, or otherwise unreadable as complete. A torn taint's scope is unknown:
    /// [`recorded`](Self::recorded) is a lower bound, never a proof of the full set the failing
    /// batch touched.
    pub torn: bool,

    /// The exact absolute paths of every taint file this read actually consumed, captured at read
    /// time — this is the snapshot a later deletion must be scoped to, never re-derived by
    /// scanning the taint directory again at deletion time. Taint files are born concurrently, from
    /// any process, at any time; a heal that deletes "whatever the directory holds right now" instead of
    /// "exactly what I read" can delete a taint a sibling process recorded after this read, losing
    /// a real durability gap forever. [`resolve_taints`] takes this set as its deletion scope for
    /// exactly that reason — see its own doc comment.
    pub files: Vec<PathBuf>,
}

impl TaintState {
    /// Whether this state stands: something is recorded, or a file is torn. A write outside heal
    /// is refused while it does ([`gate_standing_message`]): a single atomic write before its
    /// rename, a batched or bulk write after its renames. So a step that names a write asks this
    /// before it offers one.
    pub fn standing(&self) -> bool {
        !self.recorded.is_empty() || self.torn
    }
}

/// Read and union every taint file under `root`'s taint directory. A no-op — always the empty,
/// non-torn state — unless [`activate`] has been called in this process (see the module doc
/// comment): an unactivated process has no heal wired, so it must not act on taint state even to
/// the extent of reading it back.
///
/// An absent or empty taint directory reads as the empty, non-torn state — the overwhelmingly
/// common case.
///
/// # Returns
/// * `Ok(state)`   - Not activated (state is empty), or the directory was read successfully
///                   (possibly empty).
/// * `Err(String)` - Activated, and the taint directory exists but could not be read (a taint
///                   file could not be opened, or its bytes could not be read).
pub fn read_taints(root: &Path) -> Result<TaintState, String> {
    if !is_activated() {
        return Ok(TaintState::default());
    }

    let taint_dir = root.join(FOLDER_NAME_TAINT);
    if !taint_dir.exists() {
        return Ok(TaintState::default());
    }

    let mut state = TaintState::default();

    let entries = std::fs::read_dir(&taint_dir)
        .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let bytes = std::fs::read(&path)
            .map_err(|e| format!("Error while reading taint file \"{}\": {}", path.to_string_lossy(), e))?;

        let (file_recorded, file_torn) = parse_taint_content(&bytes);
        state.recorded.extend(file_recorded);
        state.torn |= file_torn;
        state.files.push(path);
    }

    Ok(state)
}

/// Parse one taint file's bytes into its recorded root-relative paths and whether it is torn —
/// see the module doc comment's format section.
///
/// Bytes that are not valid UTF-8 at all are treated as maximally torn: nothing about them can
/// be trusted as a path, so the recorded set contributes nothing and `torn` is set.
fn parse_taint_content(bytes: &[u8]) -> (BTreeSet<PathBuf>, bool) {
    let path = |entry: String| (!entry.is_empty()).then(|| PathBuf::from(entry));

    if let Ok(entries) = record_utils::from_bytes(bytes, TAINT_RECORD, TAINT_VERSION) {
        return (entries.into_iter().filter_map(path).collect(), false);
    }

    // Without this version's header the lines may be raw, unescaped paths an older Forklift
    // wrote, which would read back as different paths: nothing is trusted, and the file is torn.
    let header_line = format!("{}\n", record_utils::header(TAINT_RECORD, TAINT_VERSION));
    if !bytes.starts_with(header_line.as_bytes()) {
        return (BTreeSet::new(), true);
    }

    // A torn file's lines are trusted only when whole: a trailing fragment with no closing `\n`
    // is exactly the debris a crash mid-write-of-a-line would leave, and a line that is not UTF-8
    // or does not unescape cannot be trusted either. The digest line is not a path.
    let (entries, _) = record_utils::salvage(record_utils::without_digest_line(bytes), |line| Some(line.to_string()));

    (entries.into_iter().filter_map(path).collect(), true)
}

/// Delete every path in `files`, tolerating each one already being gone (`NotFound` — a sibling
/// healer racing against the same snapshot legitimately won, which is success here, not error),
/// then fsync `root`'s taint directory so the removals are themselves durable (tolerating the
/// directory itself being absent too — the sync is best-effort durability, not a correctness
/// requirement once every file is already confirmed gone). Never re-scans the directory: `files`
/// — always a caller's own [`TaintState::files`] snapshot — is the *entire* deletion scope, by
/// construction. Shared by [`remove_taint_files`] and [`resolve_taints`].
fn delete_snapshot_and_sync(root: &Path, files: &[PathBuf]) -> Result<(), String> {
    for path in files {
        match file_utils::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("Error while removing taint file \"{}\": {}", path.to_string_lossy(), e)),
        }
    }

    let taint_dir = root.join(FOLDER_NAME_TAINT);
    if !taint_dir.exists() {
        return Ok(());
    }

    // Only entries inside the taint folder changed; it is the folder alone that needs syncing.
    file_utils::sync_dir_alone(&taint_dir)
}

/// Delete exactly the taint files named in `files` — a snapshot a caller captured earlier via
/// [`read_taints`]'s [`TaintState::files`], never re-derived by scanning the taint directory again
/// at deletion time — then fsync the directory so the removals are themselves durable.
///
/// **Why snapshot-scoped, not a rescan-and-delete-everything:** taint files are born concurrently,
/// from any process, at any time — the entry-heal that eventually calls this runs lock-free, before the warehouse
/// lock. A version of this function that instead re-read the directory at deletion time would
/// delete every file it found there, including one recorded by a *different* process after this
/// heal's caller snapshotted its work — silently losing a real durability gap forever. Deleting
/// exactly `files` makes that impossible: a taint recorded after the snapshot was taken is not in
/// `files`, so it is never touched here, however concurrently it was born.
///
/// Module-private, on purpose — see [`clear_gate`]'s doc comment for why. [`resolve_taints`] is
/// the one production caller, once it has durably restaged (or otherwise resolved) every recorded
/// path; it also owns clearing the in-memory gate, which this function deliberately does not
/// touch. Not gated by activation: unlike recording or checking, deleting is only ever reached
/// through a heal an activated process itself drives, so gating here would be redundant, not
/// protective. See [`test_remove_taint_files`] for the one sanctioned test-only exception.
///
/// # Returns
/// * `Ok(())`      - Every path in `files` was removed (or already absent — see
///                   [`delete_snapshot_and_sync`]), and the directory sync succeeded.
/// * `Err(String)` - A file in `files` could not be removed for a reason other than already being
///                   gone, or the directory fsync failed.
fn remove_taint_files(root: &Path, files: &[PathBuf]) -> Result<(), String> {
    delete_snapshot_and_sync(root, files)
}

/// Test-only accessor for [`remove_taint_files`] — see that function's and [`clear_gate`]'s doc
/// comments for why the real functions are module-private. `#[cfg(test)]` rather than
/// `pub(crate)`: the wrapper does not exist in a production binary at all, so it cannot become a
/// second door production code could grow to reach either half of the clear through. Exists only
/// because `pack_utils`'s own test suite needs to isolate the durable-file half from the
/// in-memory gate on purpose (its next assertion pins the durable re-check path, not the gate) —
/// see that test for the full reasoning.
#[cfg(test)]
pub(crate) fn test_remove_taint_files(root: &Path, files: &[PathBuf]) -> Result<(), String> {
    remove_taint_files(root, files)
}

/// Test-only accessor for [`clear_gate`] — see [`test_remove_taint_files`] for why this shape
/// (`#[cfg(test)] pub(crate)`, not a widened production visibility) is the sanctioned exception.
/// Exists only because `file_utils`'s own test suite needs to isolate the gate from taint-file
/// state on purpose (an "unrelated" taint file is left standing on disk deliberately, to prove
/// `does_object_exist`'s gate consultation is independent of it) — see that test for the full
/// reasoning.
#[cfg(test)]
pub(crate) fn test_clear_gate(root: &Path) {
    clear_gate(root)
}

/// Replace exactly the taint files named in `old_files` — a snapshot a caller captured earlier via
/// [`read_taints`]'s [`TaintState::files`] — with a single new one recording exactly `remainder`,
/// then bring the in-memory gate for `root` into agreement with whatever the taint directory
/// actually holds once that durable work is done. The one primitive every heal-shaped verb clears
/// (or partially clears) a taint through — see the module doc comment and DESIGN.html §3.1.1.
///
/// **The durable half** (unchanged from this function's own history): the taint must afterwards
/// record exactly the unresolved remainder, never the original full set (which would re-report
/// already-resolved paths forever) and never nothing (which would silently drop the paths still
/// genuinely in doubt). `remainder` empty is the full-clear case and is delegated to
/// [`remove_taint_files`] directly — no empty taint file is ever written for "nothing left to
/// record."
///
/// **Snapshot-scoped, same reason as [`remove_taint_files`]:** `old_files` is deleted exactly as
/// given, never by re-scanning the taint directory at deletion time — a taint recorded by a
/// concurrent process after `old_files` was snapshotted (this closure walk can run for minutes)
/// must never be swept just because it happened to be sitting in the directory when this function
/// got around to deleting things.
///
/// Crash-safe by construction, the same way [`record_taint`] itself is: the replacement is durably
/// written *first*, via the exact same exclusive-create + fsync + digest routine
/// [`record_taint`]'s own write uses ([`write_taint_file`] — reused directly, not reimplemented),
/// and only once that succeeds are `old_files` removed. There is therefore never a window where the
/// remainder is unrecorded on disk: a crash before the new file's write completes leaves the
/// original (larger, still-correct, if now stale-in-places) taint set standing; a crash after
/// leaves exactly the new, smaller set (plus, until the next heal's snapshot-scoped cleanup runs,
/// whatever of `old_files` survived that crash). The freshly-written remainder file can never
/// itself appear in `old_files` — the snapshot predates it (it was captured before this call even
/// started), and [`create_taint_file`]'s unique-forever names guarantee no future name can ever
/// collide with a past one — so writing before deleting is safe by construction, not by luck of
/// ordering.
///
/// **The gate half — what makes this different from the function it replaced.** A caller's own
/// `remainder` says only what *this run* believes it resolved; it says nothing about a taint
/// recorded by something else in this same run (a mid-heal store failure, born after `old_files`
/// was snapshotted) or about a gate left standing with nothing on disk (a durable write that
/// failed after `record_taint`'s own `set_gate` already succeeded). So once the file-level work
/// above lands, this lists the taint directory the same way [`read_taints`] does (`path.is_file()`
/// filter; an absent directory reads as empty) and syncs the gate to match: cleared iff nothing
/// remains, set iff anything does — in both directions, never clear-only, so a taint this process
/// has "seen recorded" (not just recorded itself) is still reflected, per [`gate_check`]'s own
/// documented semantics. **Fails closed on a directory-read error:** the gate is left exactly as
/// it was, and the error propagates — a listing failure is never read as "must be empty, clear
/// anyway."
///
/// # Returns
/// * `Ok(())`      - The taint directory now records exactly `remainder` (or is empty/absent, if
///                   `remainder` was empty), `old_files` are gone (or were already gone — see
///                   [`delete_snapshot_and_sync`]), and the in-memory gate for `root` matches what
///                   is now on disk.
/// * `Err(String)` - The new file could not be durably written (`old_files` are left completely
///                   untouched — the original, larger set still stands, never partially deleted
///                   before a replacement is durable), a file in `old_files` could not be removed
///                   for a reason other than already being gone, or the post-write directory
///                   listing that decides the gate failed (the gate is left unchanged in this
///                   case too — see above).
///
/// The new file can also be written with an `Err`, when the folders above it could not be synced.
/// It then stands beside the untouched `old_files`, and reads union them: a superset of
/// `remainder`.
pub(crate) fn resolve_taints(
    root: &Path,
    remainder: &BTreeSet<PathBuf>,
    old_files: &[PathBuf],
) -> Result<(), String> {
    if remainder.is_empty() {
        remove_taint_files(root, old_files)?;
    } else {
        write_taint_file(root, remainder)?;
        delete_snapshot_and_sync(root, old_files)?;
    }

    sync_gate_with_disk(root)
}

/// The gate half of [`resolve_taints`], split out for its own reasoning: lists `root`'s taint
/// directory the same way [`read_taints`] does (`path.is_file()` filter; an absent directory reads
/// as empty), then sets or clears the in-memory gate to match — see [`resolve_taints`]'s own doc
/// comment for the full contract, including the fail-closed behavior on a listing error.
fn sync_gate_with_disk(root: &Path) -> Result<(), String> {
    if any_taint_file_present(root)? {
        set_gate(root);
    } else {
        clear_gate(root);
    }

    Ok(())
}

/// Whether at least one taint file is standing under `root`'s taint directory right now — the
/// exact `path.is_file()` filter [`read_taints`] applies, with an absent directory reading as
/// `false`, but without paying for parsing or unioning any file's contents: [`sync_gate_with_disk`]
/// only needs presence, never what a taint file records.
fn any_taint_file_present(root: &Path) -> Result<bool, String> {
    let taint_dir = root.join(FOLDER_NAME_TAINT);
    if !taint_dir.exists() {
        return Ok(false);
    }

    let entries = std::fs::read_dir(&taint_dir)
        .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;

    for entry in entries {
        let entry = entry
            .map_err(|e| format!("Error while reading taint directory \"{}\": {}", taint_dir.to_string_lossy(), e))?;
        if entry.path().is_file() {
            return Ok(true);
        }
    }

    Ok(false)
}

/// The path of the taint directory under a given warehouse root, independent of any active
/// process-wide storage-root scope — for a caller that already knows the target warehouse's root
/// directly rather than through the ambient scope [`record_taint`]/[`read_taints`] resolve
/// against (a test driving `forklift` as a spawned subprocess, whose own working directory has
/// nothing to do with the warehouse being exercised — the same need `load_guard_utils::
/// marker_path_under` serves for the incomplete-load marker). Assumes a plain, non-bay warehouse
/// layout (`<root>/.forklift/taint/...`), which is all such a caller ever drives.
pub fn taint_dir_path_under(warehouse_root: &Path) -> PathBuf {
    warehouse_root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT).join(FOLDER_NAME_TAINT)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::globals::StorageRootScope;

    /// This module's own tests share [`super::ACTIVATION_TEST_LOCK`] (via the glob import above)
    /// with every other module's activation-sensitive tests — see its doc comment.
    fn lock_activation() -> std::sync::MutexGuard<'static, ()> {
        ACTIVATION_TEST_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A record the format keeps per bay is declared for a bay exactly when it is declared for
    /// the main tree: every rewritable `{bay}` row of the warehouse format's path table, read from
    /// the spec itself, with its placeholders filled in.
    /// Revert: drop the `bays/<name>/cherry-pick` entry → the pair disagrees → red.
    #[test]
    fn every_bay_record_is_declared_for_a_bay_as_for_the_main_tree() {
        let spec = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/format/WAREHOUSE_FORMAT.md");
        let spec = std::fs::read_to_string(&spec).unwrap_or_else(|e| panic!("read {}: {}", spec.display(), e));

        let mut declared_for_both = 0;
        let mut mismatched = Vec::new();
        for row in spec.lines().filter(|line| line.starts_with("| `{bay}")) {
            let columns: Vec<&str> = row.split('|').map(str::trim).collect();
            if columns[2] != "rewritable" {
                continue;
            }
            for path in columns[1].split(", ").map(|path| path.trim_matches('`')) {
                if path.ends_with('/') {
                    continue;
                }
                let rest = path.strip_prefix("{bay}").unwrap().replace("<xx>", "ab").replace("<hash>", "cd");
                let main_tree = taint_shape(Path::new(&rest)).is_some();
                let bay = taint_shape(&Path::new("bays").join("side").join(&rest)).is_some();
                if main_tree != bay {
                    mismatched.push((rest.clone(), main_tree, bay));
                } else if main_tree {
                    declared_for_both += 1;
                }
            }
        }

        assert_eq!(mismatched, Vec::<(String, bool, bool)>::new(), "(path, declared for the main tree, for a bay)");
        assert!(declared_for_both >= 10, "sanity: the spec's bay rows were read: {}", declared_for_both);
    }

    /// An absolute storage root, entered on this thread; nothing is created under it.
    fn absolute_root(name: &str) -> (StorageRootScope, PathBuf) {
        let root = std::env::temp_dir().join(format!("forklift-taint-utils-{}-{}", name, std::process::id()));
        assert!(root.is_absolute(), "sanity: the root is absolute: {}", root.display());
        (StorageRootScope::enter(&root), root)
    }

    /// One example of `shape`, built through its constructor. No wildcard arm: a new variant
    /// does not compile without one.
    fn writer_shape_example(shape: WriterShape) -> DurablePath {
        let bay = Checkout::Bay("side".to_string());
        let shard = "src/lib";
        match shape {
            WriterShape::PalletRef => DurablePath::pallet_ref("team/main"),
            WriterShape::Keepsake => DurablePath::keepsake("team%2Fmain+1"),
            WriterShape::MetaRef => DurablePath::meta_ref("haul"),
            WriterShape::MetaCopy => DurablePath::meta_copy("haul"),
            WriterShape::LooseObject => DurablePath::loose_object("ab12cd").unwrap(),
            WriterShape::LooseSignature => DurablePath::loose_signature("ab12cd").unwrap(),
            WriterShape::Pack => DurablePath::pack("p1"),
            WriterShape::PackIndex => DurablePath::pack_index("p1"),
            WriterShape::SignatureTable => DurablePath::signature_table("p1"),
            WriterShape::Shard => DurablePath::shard(&Checkout::Main, shard).unwrap(),
            WriterShape::BayShard => DurablePath::shard(&bay, shard).unwrap(),
            WriterShape::LoadMarker => DurablePath::load_marker(&Checkout::Main),
            WriterShape::BayLoadMarker => DurablePath::load_marker(&bay),
            WriterShape::Position => DurablePath::position(&Checkout::Main),
            WriterShape::BayPosition => DurablePath::position(&bay),
            WriterShape::PositionCopy => DurablePath::position_copy(&Checkout::Main),
            WriterShape::BayPositionCopy => DurablePath::position_copy(&bay),
            WriterShape::Scope => DurablePath::scope(&Checkout::Main),
            WriterShape::BayScope => DurablePath::scope(&bay),
            WriterShape::ScopeCopy => DurablePath::scope_copy(&Checkout::Main),
            WriterShape::BayScopeCopy => DurablePath::scope_copy(&bay),
            WriterShape::Consolidation => DurablePath::consolidation(&Checkout::Main),
            WriterShape::BayConsolidation => DurablePath::consolidation(&bay),
            WriterShape::ConsolidationCopy => DurablePath::consolidation_copy(&Checkout::Main),
            WriterShape::BayConsolidationCopy => DurablePath::consolidation_copy(&bay),
            WriterShape::Skeleton => DurablePath::skeleton(&Checkout::Main),
            WriterShape::BaySkeleton => DurablePath::skeleton(&bay),
            WriterShape::SkeletonCopy => DurablePath::skeleton_copy(&Checkout::Main),
            WriterShape::BaySkeletonCopy => DurablePath::skeleton_copy(&bay),
            WriterShape::Journal => DurablePath::journal(&Checkout::Main),
            WriterShape::BayJournal => DurablePath::journal(&bay),
            WriterShape::CherryPick => DurablePath::cherry_pick(&Checkout::Main),
            WriterShape::BayCherryPick => DurablePath::cherry_pick(&bay),
            WriterShape::CherryPickCopy => DurablePath::cherry_pick_copy(&Checkout::Main),
            WriterShape::BayCherryPickCopy => DurablePath::cherry_pick_copy(&bay),
            WriterShape::BayPath => DurablePath::bay_path("side"),
            WriterShape::FetchScope => DurablePath::fetch_scope(),
            WriterShape::FetchLedger => DurablePath::fetch_ledger(),
            WriterShape::HeldHeads => DurablePath::held_heads(),
            WriterShape::MirrorRoots => DurablePath::mirror_roots(),
            WriterShape::Served => DurablePath::served(),
            WriterShape::Tombstones => DurablePath::tombstones(),
            WriterShape::UnvouchedHeads => DurablePath::unvouched_heads(),
            WriterShape::Trust => DurablePath::trust(),
            WriterShape::TrustCopy => DurablePath::trust_copy(),
        }
    }

    /// Every writer shape's constructor renders its example into the entry the variant names, at
    /// the path written here, and a checkout-local record's main and bay variants are separate
    /// rows. Every entry of the registry is named by exactly one row, or is one of the heal-only
    /// entries listed here. The escape keeps a path outside the warehouse folder as given.
    /// Revert: point `WriterShape::BayPosition` at the main tree's entry → its row's description
    /// differs → red; build a bay's record under the root → its row's path differs → red.
    #[test]
    fn every_writer_shape_renders_its_example_into_its_entry() {
        let (_scope, root) = absolute_root("writer-shapes");
        let forklift = root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT);

        let rows: &[(WriterShape, &str, &str)] = &[
            (WriterShape::PalletRef, "pallets/team/main", "a pallet ref"),
            (WriterShape::Keepsake, "removed-refs/team%2Fmain+1", "a removed ref's keepsake"),
            (WriterShape::MetaRef, "meta/haul", "a meta pallet ref"),
            (WriterShape::MetaCopy, "meta-copy/haul", "a meta pallet ref's copy"),
            (WriterShape::LooseObject, "objects/ab/12cd", "a loose object"),
            (WriterShape::LooseSignature, "objects/ab/12cd.sig", "a loose object's signature"),
            (WriterShape::Pack, "objects/pack/p1.pack", "a pack"),
            (WriterShape::PackIndex, "objects/pack/p1.idx", "a pack index"),
            (WriterShape::SignatureTable, "objects/pack/p1.sigs", "a signature table"),
            (WriterShape::Shard, "inventory/inv_/32/ae56798cd16a1f7d10d50387731aaceed83ae94f48bcf50fc30b7444c43191/data", "a staged inventory shard"),
            (WriterShape::BayShard, "bays/side/inventory/inv_/32/ae56798cd16a1f7d10d50387731aaceed83ae94f48bcf50fc30b7444c43191/data", "a bay's staged inventory shard"),
            (WriterShape::LoadMarker, "inventory/incomplete-load", "the incomplete-load marker"),
            (WriterShape::BayLoadMarker, "bays/side/inventory/incomplete-load", "a bay's incomplete-load marker"),
            (WriterShape::Position, "position", "the checkout position"),
            (WriterShape::BayPosition, "bays/side/position", "a bay's checkout position"),
            (WriterShape::PositionCopy, "position-copy", "the checkout position's copy"),
            (WriterShape::BayPositionCopy, "bays/side/position-copy", "a bay's checkout position's copy"),
            (WriterShape::Scope, "scope", "the checkout scope"),
            (WriterShape::BayScope, "bays/side/scope", "a bay's checkout scope"),
            (WriterShape::ScopeCopy, "scope-copy", "the checkout scope's copy"),
            (WriterShape::BayScopeCopy, "bays/side/scope-copy", "a bay's checkout scope's copy"),
            (WriterShape::Consolidation, "consolidation", "the consolidation state"),
            (WriterShape::BayConsolidation, "bays/side/consolidation", "a bay's consolidation state"),
            (WriterShape::ConsolidationCopy, "consolidation-copy", "the consolidation state's copy"),
            (WriterShape::BayConsolidationCopy, "bays/side/consolidation-copy", "a bay's consolidation state's copy"),
            (WriterShape::Skeleton, "consolidation-skeleton", "the consolidation skeleton"),
            (WriterShape::BaySkeleton, "bays/side/consolidation-skeleton", "a bay's consolidation skeleton"),
            (WriterShape::SkeletonCopy, "consolidation-skeleton-copy", "the consolidation skeleton's copy"),
            (WriterShape::BaySkeletonCopy, "bays/side/consolidation-skeleton-copy", "a bay's consolidation skeleton's copy"),
            (WriterShape::Journal, "journal.json", "the journal"),
            (WriterShape::BayJournal, "bays/side/journal.json", "a bay's journal"),
            (WriterShape::CherryPick, "cherry-pick", "the cherry-pick state"),
            (WriterShape::BayCherryPick, "bays/side/cherry-pick", "a bay's cherry-pick state"),
            (WriterShape::CherryPickCopy, "cherry-pick-copy", "the cherry-pick state's copy"),
            (WriterShape::BayCherryPickCopy, "bays/side/cherry-pick-copy", "a bay's cherry-pick state's copy"),
            (WriterShape::BayPath, "bays/side/path", "a bay's path record"),
            (WriterShape::FetchScope, "config/fetch-scope", "the fetch scope"),
            (WriterShape::FetchLedger, "fetched-heads", "the fetch ledger"),
            (WriterShape::HeldHeads, "held-heads", "the held-heads record"),
            (WriterShape::MirrorRoots, "mirror-roots", "the mirror roots record"),
            (WriterShape::Served, "served", "the served record"),
            (WriterShape::Tombstones, "tombstones", "the tombstones record"),
            (WriterShape::UnvouchedHeads, "unvouched-heads", "the unvouched-heads record"),
            (WriterShape::Trust, "trust", "the trust anchor"),
            (WriterShape::TrustCopy, "trust-copy", "the trust anchor's copy"),
        ];
        let heal_only = ["a ref in the meta folder that is no meta pallet's", "a copy of a ref that is no meta pallet's"];

        let mut named = BTreeSet::new();
        for &(shape, relative, description) in rows {
            let expected = relative.split('/').fold(forklift.clone(), |path, name| path.join(name));
            let built = writer_shape_example(shape);
            // The spelling, not only the components: a name's `/` is a separator of its own, as
            // the readers' path builders push it, never one joined inside a component.
            assert_eq!(built.as_path().as_os_str(), expected.as_os_str(), "{:?}", shape);
            let found = taint_shape(Path::new(relative)).map(|found| found.description);
            assert_eq!((found, shape.entry().description), (Some(description), description), "{:?}", shape);
            assert!(named.insert(description), "{:?} names an entry another row names", shape);
        }

        let unnamed: Vec<&str> = TAINT_SHAPES.iter().map(|entry| entry.description)
            .filter(|description| !named.contains(description)).collect();
        assert_eq!(unnamed, heal_only);
        assert_eq!(named.len() + heal_only.len(), TAINT_SHAPES.len(), "sanity: every entry was read");

        let outside = root.join("elsewhere").join("record");
        assert_eq!(DurablePath::outside_warehouse(outside.clone()).as_path(), outside);

        // The main tree's shard is the path the reader of that key opens.
        let read = file_utils::get_inventory_data_path_for_key("src/lib").unwrap();
        assert_eq!(DurablePath::shard(&Checkout::Main, "src/lib").unwrap().as_path(), Path::new(&read));
    }

    /// A constructor refuses what its writer's own path builder refuses today: a hash that is not
    /// lowercase hex, whatever its length, and a shard of an inventory whose metadata names
    /// another format version, in the main tree and in a bay. A well-formed hash and a shard of
    /// an inventory this build knows are built (the table test above).
    /// Revert: drop the hex check in `split_object_hash`, or the gate in `shard` → `Ok` → red.
    #[test]
    fn a_constructor_refuses_what_its_writers_path_builder_refuses() {
        let (_scope, root) = absolute_root("writer-refusals");
        let _ = file_utils::remove_dir_all(&root);

        for hash in ["ZZ1234", "../../etc/x", "AB12CD"] {
            assert!(file_utils::get_path_for_object(hash).is_err(), "sanity: the writer refuses {}", hash);
            assert_eq!(DurablePath::loose_object(hash), Err(format!("\"{}\" is not a valid object hash.", hash)));
            assert_eq!(DurablePath::loose_signature(hash), Err(format!("\"{}\" is not a valid object hash.", hash)));
        }

        let bay = Checkout::Bay("side".to_string());
        for (checkout, inventory) in [(&Checkout::Main, Path::new("inventory")), (&bay, Path::new("bays/side/inventory"))] {
            // Each inventory is a root the gate has not passed in this process.
            let inventory = forklift_root().join(inventory);
            std::fs::create_dir_all(&inventory).unwrap();
            let newer = record_utils::to_bytes(file_utils::INVENTORY_METADATA_RECORD,
                file_utils::INVENTORY_METADATA_VERSION + 1, ["./"]);
            std::fs::write(inventory.join(file_utils::FILE_NAME_INVENTORY_METADATA), newer).unwrap();

            let refused = DurablePath::shard(checkout, "src/lib");
            assert!(refused.as_ref().is_err_and(|e| e.contains("does not know")), "{:?}: {:?}", checkout, refused);
        }
        file_utils::remove_dir_all(&root).ok();
    }

    /// A constructor refuses a path that classifies as another entry: a meta copy built for an
    /// ordinary pallet's name classifies as the heal-only meta-copy entry, the first entry it
    /// matches.
    /// Revert: drop the check in `declared` → no panic → red.
    #[test]
    #[should_panic(expected = "a MetaCopy writer built \"meta-copy/main\", which is a copy of a ref that is no meta pallet's, not a meta pallet ref's copy")]
    fn a_constructor_refuses_a_path_of_another_entry_in_a_debug_build() {
        let (_scope, _root) = absolute_root("writer-mismatch");

        let _ = DurablePath::meta_copy("main");
    }

    /// The escape refuses a path under the warehouse folder, spelled absolutely.
    /// Revert: drop the assertion in `outside_warehouse` → no panic → red.
    #[test]
    #[should_panic(expected = "an out-of-warehouse write names")]
    fn the_escape_refuses_a_path_under_the_warehouse_in_a_debug_build() {
        let (_scope, _root) = absolute_root("escape-in-root");

        let _ = DurablePath::outside_warehouse(forklift_root().join("position"));
    }

    /// Whether `ty` names `DurablePath`, or `Self` where `Self` is it, anywhere inside.
    fn mentions_durable_path(ty: &syn::ReturnType, self_is_durable: bool) -> bool {
        struct Mentions(bool, bool);
        impl<'ast> syn::visit::Visit<'ast> for Mentions {
            fn visit_ident(&mut self, ident: &'ast syn::Ident) {
                self.0 |= ident == "DurablePath" || (self.1 && ident == "Self");
            }
        }
        let mut mentions = Mentions(false, self_is_durable);
        syn::visit::Visit::visit_return_type(&mut mentions, ty);
        mentions.0
    }

    /// Where this file's non-test source builds a `DurablePath`, and which functions return one.
    #[derive(Default)]
    struct Constructions {
        within: Vec<String>,
        durable_impl: Vec<bool>,
        built_in: BTreeSet<String>,
        returned_by: BTreeSet<String>,
    }

    impl Constructions {
        fn in_durable_impl(&self) -> bool {
            self.durable_impl.last().copied().unwrap_or(false)
        }

        /// Whether `path`, as an expression, is the type's constructor.
        fn is_constructor(&self, path: &syn::Path) -> bool {
            let single_self = path.segments.len() == 1 && path.segments[0].ident == "Self";
            path.segments.last().is_some_and(|last| last.ident == "DurablePath")
                || (single_self && self.in_durable_impl())
        }

        fn note_built(&mut self) {
            let within = self.within.last().cloned().unwrap_or_else(|| "<item>".to_string());
            self.built_in.insert(within);
        }

        /// A function returning a `DurablePath`, by its name inside `impl DurablePath`, and
        /// marked as outside it anywhere else, so no allowed name passes there.
        fn note_fn(&mut self, sig: &syn::Signature) {
            if mentions_durable_path(&sig.output, self.in_durable_impl()) {
                let name = sig.ident.to_string();
                self.returned_by.insert(if self.in_durable_impl() { name } else { format!("{} (outside impl DurablePath)", name) });
            }
        }
    }

    use crate::util::file_utils::tests::is_test_only as is_cfg_test;

    impl<'ast> syn::visit::Visit<'ast> for Constructions {
        fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_mod(self, item);
            }
        }

        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if is_cfg_test(&item.attrs) {
                return;
            }
            let durable = matches!(&*item.self_ty, syn::Type::Path(ty)
                if ty.path.segments.last().is_some_and(|last| last.ident == "DurablePath"));
            self.durable_impl.push(durable);
            syn::visit::visit_item_impl(self, item);
            self.durable_impl.pop();
        }

        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if !is_cfg_test(&item.attrs) {
                // A free function, even one nested in a method's body, is outside the impl.
                self.durable_impl.push(false);
                self.note_fn(&item.sig);
                self.within.push(item.sig.ident.to_string());
                syn::visit::visit_item_fn(self, item);
                self.within.pop();
                self.durable_impl.pop();
            }
        }

        fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
            if !is_cfg_test(&item.attrs) {
                self.note_fn(&item.sig);
                self.within.push(item.sig.ident.to_string());
                syn::visit::visit_impl_item_fn(self, item);
                self.within.pop();
            }
        }

        fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
            if !is_cfg_test(&item.attrs) {
                syn::visit::visit_item_trait(self, item);
            }
        }

        fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
            if !is_cfg_test(&item.attrs) {
                // A trait's `Self` is its implementor, so a trait method is outside the impl.
                self.durable_impl.push(false);
                self.note_fn(&item.sig);
                self.within.push(item.sig.ident.to_string());
                syn::visit::visit_trait_item_fn(self, item);
                self.within.pop();
                self.durable_impl.pop();
            }
        }

        fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
            if self.is_constructor(&expr.path) {
                self.note_built();
            }
            syn::visit::visit_expr_path(self, expr);
        }

        fn visit_expr_struct(&mut self, expr: &'ast syn::ExprStruct) {
            if self.is_constructor(&expr.path) {
                self.note_built();
            }
            syn::visit::visit_expr_struct(self, expr);
        }
    }

    /// The constructor census over this file's non-test source: a `DurablePath` is built, by a
    /// call, a braced literal or the constructor taken as a value, spelled `DurablePath` or
    /// `Self` in any impl for it, only in `declared` and `outside_warehouse`; and every function
    /// returning one, bare or inside another type, is a function of `impl DurablePath` the
    /// writer-shape example calls, or one of those two. The type derives only the traits listed
    /// here, so no derive (`Default`, `Deserialize`) builds one either.
    /// Revert: add `fn sneaky(p: &Path) -> Result<DurablePath, String> { Ok(Self(p.to_path_buf())) }`
    /// to `impl DurablePath` → red.
    #[test]
    fn a_durable_path_is_built_only_by_its_constructors() {
        let file = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/util/taint_utils.rs");
        let source = std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("read {}: {}", file.display(), e));
        let parsed = syn::parse_file(&source).unwrap();

        let definition = parsed.items.iter().find_map(|item| match item {
            syn::Item::Struct(definition) if definition.ident == "DurablePath" => Some(definition),
            _ => None,
        }).expect("the type is defined in this file");
        let mut derived = Vec::new();
        for attribute in definition.attrs.iter().filter(|attribute| !attribute.path().is_ident("doc")) {
            assert!(attribute.path().is_ident("derive"), "an attribute other than a derive: {:?}", attribute.path().get_ident());
            attribute.parse_nested_meta(|meta| {
                derived.push(meta.path.get_ident().map(|ident| ident.to_string()).unwrap_or_default());
                Ok(())
            }).unwrap();
        }
        assert_eq!(derived, ["Clone", "Debug", "PartialEq", "Eq"]);

        let mut constructions = Constructions::default();
        syn::visit::Visit::visit_file(&mut constructions, &parsed);
        assert_eq!(constructions.built_in, BTreeSet::from(["declared".to_string(), "outside_warehouse".to_string()]));

        struct Called(BTreeSet<String>);
        impl<'ast> syn::visit::Visit<'ast> for Called {
            fn visit_expr_path(&mut self, expr: &'ast syn::ExprPath) {
                let segments: Vec<String> = expr.path.segments.iter().map(|segment| segment.ident.to_string()).collect();
                if let [ty, function] = segments.as_slice() {
                    if ty == "DurablePath" {
                        self.0.insert(function.clone());
                    }
                }
            }
        }
        let example = parsed.items.iter().find_map(|item| match item {
            syn::Item::Mod(module) if module.ident == "tests" => module.content.as_ref(),
            _ => None,
        }).and_then(|(_, items)| items.iter().find_map(|item| match item {
            syn::Item::Fn(function) if function.sig.ident == "writer_shape_example" => Some(function),
            _ => None,
        })).expect("the writer-shape example is in the tests module");
        let mut called = Called(BTreeSet::new());
        syn::visit::Visit::visit_item_fn(&mut called, example);
        assert!(called.0.len() >= 30, "sanity: the example's constructors were read: {:?}", called.0);

        let allowed: BTreeSet<String> = called.0.iter().cloned()
            .chain(["declared".to_string(), "outside_warehouse".to_string()]).collect();
        let unlisted: Vec<&String> = constructions.returned_by.iter().filter(|name| !allowed.contains(*name)).collect();
        assert_eq!(unlisted, Vec::<&String>::new());
        assert!(constructions.returned_by.contains("position") && constructions.returned_by.contains("loose_object"),
            "sanity: returns were read, bare and inside a Result: {:?}", constructions.returned_by);
    }

    /// What the constructor census reads, over literal sources: a trait's method that returns or
    /// builds the type counts, as outside its impl, even where the trait sits in a method of the
    /// impl; a `#[cfg(test)]` method, in the impl or a trait, or a `#[cfg(test)]` trait, counts for
    /// nothing, while a non-test sibling does.
    /// Revert: drop `visit_trait_item_fn` → the trait's method is not named → red; drop its
    /// outside-the-impl push → the nested trait's `Self(..)` counts as the constructor → red; drop
    /// the `cfg(test)` skip of an impl method, a trait method or a trait → the test-only item is
    /// counted → red. (The free-function, module and impl skips are not pinned here.)
    #[test]
    fn the_constructor_census_reads_what_it_claims() {
        let read = |source: &str| {
            let mut constructions = Constructions::default();
            syn::visit::Visit::visit_file(&mut constructions, &syn::parse_file(source).unwrap());
            (constructions.built_in, constructions.returned_by)
        };
        let names = |names: &[&str]| names.iter().map(|name| name.to_string()).collect::<BTreeSet<String>>();

        assert_eq!(read("trait T { fn f() -> DurablePath { DurablePath(p) } }"),
            (names(&["f"]), names(&["f (outside impl DurablePath)"])));
        assert_eq!(read("impl DurablePath { #[cfg(test)] fn t() -> Self { Self(p) } fn u() -> Self { Self(p) } }"),
            (names(&["u"]), names(&["u"])));
        assert_eq!(read("impl DurablePath { fn m() { trait T { fn f() -> Self { Self(p) } } } }"),
            (names(&[]), names(&[])));
        assert_eq!(read("trait T { #[cfg(test)] fn t() -> DurablePath { DurablePath(p) } }"),
            (names(&[]), names(&[])));
        assert_eq!(read("#[cfg(test)] trait T { fn t() -> DurablePath { DurablePath(p) } }"),
            (names(&[]), names(&[])));
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("forklift-taint-utils-{}-{}", name, std::process::id()));
        let _ = file_utils::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    #[cfg(windows)]
    fn taint_record_line_never_carries_the_native_backslash_separator() {
        // Pins the writer side of DESIGN.html §3.1.1's cross-platform-parse promise on the one
        // platform where a bare `Path::to_string_lossy()` would actually differ from a
        // component-join: Windows. Without `taint_record_line`'s per-component join, this line
        // would contain `\` (the native separator `to_string_lossy` would have produced), which a
        // record written elsewhere and read here would never match.
        let root = PathBuf::from(r"C:\forklift-root");
        let path = root.join("objects").join("ab").join("cdef0123456789abcdef0123456789abcdef01");
        let relative = path.strip_prefix(&root).unwrap();

        let line = taint_record_line(relative);

        assert!(!line.contains('\\'), "taint record line must never contain a backslash, got {:?}", line);
        assert_eq!(line, "objects/ab/cdef0123456789abcdef0123456789abcdef01");
    }

    #[test]
    #[cfg(unix)]
    fn taint_record_line_preserves_a_literal_backslash_in_a_unix_component() {
        // The cross-platform-meaningful pin: on Unix, `\` is just an ordinary filename byte, not
        // a separator. A blanket string replace (`to_string_lossy().replace('\\', "/")`) would
        // corrupt a component containing one; the per-component join in `taint_record_line` must
        // not. This test goes red the moment anyone "simplifies" the join into a blanket replace.
        let root = PathBuf::from("/forklift-root");
        let path = root.join("objects").join(r"weird\name");
        let relative = path.strip_prefix(&root).unwrap();

        let line = taint_record_line(relative);

        assert_eq!(line, r"objects/weird\name",
            "a literal backslash inside a Unix path component must survive verbatim");
    }

    #[test]
    fn record_then_read_round_trips_the_exact_root_relative_set() {
        // Pins the round trip end to end: what is recorded is exactly what comes back, and a
        // freshly (and completely) written taint never reads as torn.
        let _serial = lock_activation();
        activate();

        let root = scratch("round-trip");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let a = forklift.join("objects").join("ab").join("cdef1234567890");
        let b = forklift.join("objects").join("cd").join("00112233445566");
        record_taint(&[a.as_path(), b.as_path()]).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn, "a freshly recorded taint must not read as torn");

        let expected: BTreeSet<PathBuf> = [
            PathBuf::from("objects/ab/cdef1234567890"),
            PathBuf::from("objects/cd/00112233445566"),
        ].into_iter().collect();
        assert_eq!(state.recorded, expected);
    }

    /// The exact taint filename `read_taints`/`record_taint` most recently created under
    /// `taint_dir` — used by tests that need to reason about the concrete `taint-<pid>-<nonce_hex>-
    /// <counter>` shape without hardcoding a counter value, since [`TAINT_FILENAME_COUNTER`] is a
    /// single process-global counter shared (and already advanced) by every other test in this
    /// binary that has recorded a taint before this one ran.
    fn only_taint_filename(taint_dir: &Path) -> String {
        let names: Vec<String> = std::fs::read_dir(taint_dir).unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), 1, "expected exactly one taint file under {:?}, found {:?}", taint_dir, names);
        names.into_iter().next().unwrap()
    }

    #[test]
    fn a_preexisting_next_candidate_filename_forces_a_retry_and_both_survive() {
        // Pins the O_CREAT|O_EXCL crash-survivor guarantee (DESIGN.html §3.1.1): a taint file
        // already occupying the exact name a fresh write would generate must never be
        // overwritten — the write takes the next counter suffix instead, and reading unions both.
        let _serial = lock_activation();
        activate();

        let root = scratch("o-excl-survivor");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);

        // Learn this process's actual next filename shape by recording for real first — the
        // process-global counter's absolute value depends on how many taint files earlier tests in
        // this same test binary have already created, so this test cannot assume it starts at 0.
        let first_object = forklift.join("objects").join("11").join("22334455");
        record_taint(&[first_object.as_path()]).unwrap();
        let first_name = only_taint_filename(&taint_dir);

        // "taint-<pid>-<nonce_hex>-<counter>": split off the counter to learn the exact value this
        // process just drew, so the very next draw (a single global, monotonic counter) can be
        // preempted deterministically.
        let (prefix, counter_str) = first_name.rsplit_once('-').expect("well-formed taint filename");
        let counter: u64 = counter_str.parse().expect("counter suffix must be numeric");

        let survivor_path = taint_dir.join(format!("{}-{}", prefix, counter + 1));
        let survivor_content = String::from_utf8(taint_file_bytes(["objects/aa/survivor"])).unwrap();
        std::fs::write(&survivor_path, &survivor_content).unwrap();

        let second_object = forklift.join("objects").join("22").join("33445566");
        record_taint(&[second_object.as_path()]).unwrap();

        // The pre-existing file must be untouched, byte for byte.
        assert_eq!(std::fs::read_to_string(&survivor_path).unwrap(), survivor_content);

        // The fresh write must have skipped the occupied counter and landed at the next one.
        let next_path = taint_dir.join(format!("{}-{}", prefix, counter + 2));
        assert!(next_path.exists(), "the next counter value must be used when the immediate next candidate exists");

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn);
        assert!(state.recorded.contains(&PathBuf::from("objects/aa/survivor")),
            "the survivor's own recorded path must still be read back");
        assert!(state.recorded.contains(&PathBuf::from("objects/22/33445566")),
            "the fresh write's recorded path must be read back alongside the survivor");
    }

    #[test]
    fn a_deleted_taint_filename_is_never_reissued_by_the_same_process() {
        // Pins Part 1's load-bearing property: a filename this process has already used must never
        // be handed out again, even after the file is deleted — the ABA hole a naive "restart the
        // counter at 0 every call" scheme leaves open. Reverting the process-global monotonic
        // counter back to a per-call `0..CAP` local index makes this go red: with the taint
        // directory emptied by the deletion below, the very next call would again probe counter 0
        // first (and succeed immediately, since nothing occupies it anymore), reproducing the
        // exact first filename.
        let _serial = lock_activation();
        activate();

        let root = scratch("unique-forever");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);

        let object_a = forklift.join("objects").join("aa").join("bb001122");
        record_taint(&[object_a.as_path()]).unwrap();
        let first_name = only_taint_filename(&taint_dir);
        file_utils::remove_file(taint_dir.join(&first_name)).unwrap();

        let object_b = forklift.join("objects").join("cc").join("dd003344");
        record_taint(&[object_b.as_path()]).unwrap();
        let second_name = only_taint_filename(&taint_dir);

        assert_ne!(first_name, second_name,
            "a filename this process has already used must never be reissued, even after deletion");
    }

    #[test]
    fn a_taint_file_missing_its_digest_reads_as_torn_but_keeps_its_parseable_prefix() {
        // Pins the torn-file contract: a file a crash cut off before its digest reads as
        // torn, but every complete line before the cut point still counts as recorded.
        let _serial = lock_activation();
        activate();

        let root = scratch("torn");
        let forklift = root.join(".forklift");
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        std::fs::create_dir_all(&taint_dir).unwrap();
        std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 2\nobjects/ab/cdef\nobjects/c").unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(state.torn, "a taint file without its digest must read as torn");
        assert_eq!(state.recorded, BTreeSet::from([PathBuf::from("objects/ab/cdef")]),
            "the parseable prefix must still be recorded even though the file is torn");
    }

    #[test]
    fn a_taint_path_holding_a_line_break_reads_back_as_one_path() {
        let _serial = lock_activation();
        activate();

        let root = scratch("line-break");
        let forklift = root.join(".forklift");
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        std::fs::create_dir_all(&taint_dir).unwrap();
        let paths = ["dir/a\nb/file", "dir/c\r/file", "objects/ab/cd\\n"];
        std::fs::write(taint_dir.join("taint-99999-0"), taint_file_bytes(paths)).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.torn);
        assert_eq!(state.recorded, paths.iter().map(PathBuf::from).collect::<BTreeSet<_>>());
    }

    #[test]
    fn a_taint_file_without_its_header_or_with_a_bad_escape_is_torn_and_trusts_nothing_unreadable() {
        let _serial = lock_activation();
        activate();

        let root = scratch("headerless");
        let forklift = root.join(".forklift");
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        std::fs::create_dir_all(&taint_dir).unwrap();

        // An older, unescaped file: torn, and none of its lines read as paths — this raw path,
        // holding a literal backslash before "n", would otherwise unescape into a different one.
        std::fs::write(taint_dir.join("taint-99999-0"), b"dir/a\\nb/file\nEND\n").unwrap();
        let state = read_taints(&forklift).unwrap();
        assert!(state.torn, "a headerless taint file must read as torn");
        assert!(state.recorded.is_empty(), "{:?}", state.recorded);

        // A file of the version before the digest, whole as it was written: torn, trusting nothing.
        std::fs::write(taint_dir.join("taint-99999-0"), b"/forklift-taint 1\nobjects/cd\nEND\n").unwrap();
        let state = read_taints(&forklift).unwrap();
        assert!(state.torn, "an older version's taint file must read as torn");
        assert!(state.recorded.is_empty(), "{:?}", state.recorded);

        // A line no writer produces makes a file torn, even under a digest that matches it.
        let bad = record_utils::with_digest(b"/forklift-taint 2\nobjects/ab\\x\nobjects/cd\n".to_vec());
        std::fs::write(taint_dir.join("taint-99999-0"), bad).unwrap();
        let state = read_taints(&forklift).unwrap();
        assert!(state.torn, "a badly escaped line must make the file torn");
        assert_eq!(state.recorded, BTreeSet::from([PathBuf::from("objects/cd")]));
    }

    /// Damage that still parses, a hex digit flipped to another or a cut on a line boundary,
    /// makes the file torn, so the recovery checks everything rather than trusting the paths it
    /// names.
    ///
    /// Revert: read a file whose digest fails as complete → neither reads as torn.
    #[test]
    fn a_taint_file_whose_digest_fails_reads_as_torn() {
        let _serial = lock_activation();
        activate();

        let root = scratch("digest");
        let forklift = root.join(".forklift");
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        std::fs::create_dir_all(&taint_dir).unwrap();
        let whole = taint_file_bytes(["objects/ab/cdef", "objects/12/3456"]);
        let at = b"/forklift-taint 2\nobjects/ab/".len();
        let mut hex_flip = whole.clone();
        hex_flip[at] = b'd';
        let line_cut = [&whole[..at + 5], &whole[at + 5 + b"objects/12/3456\n".len()..]].concat();

        for (shape, bytes) in [("a hex flip", hex_flip), ("a cut on a line boundary", line_cut)] {
            std::fs::write(taint_dir.join("taint-99999-0"), bytes).unwrap();
            let state = read_taints(&forklift).unwrap();
            assert!(state.torn, "{} must make the taint file torn", shape);
            assert!(!state.recorded.iter().any(|path| path.to_string_lossy().starts_with("digest")),
                "{}: the digest line is not a path: {:?}", shape, state.recorded);
        }
    }

    #[test]
    fn activation_gates_every_public_entry_point_until_it_is_called() {
        // Pins the all-or-nothing activation contract: before `activate()`, every public entry
        // point is a true no-op (no file written, no gate ever trips, reads see nothing); after
        // it, the exact same calls behave for real.
        let _serial = lock_activation();
        reset_activation_for_test();

        let root = scratch("activation");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("22").join("33445566");

        record_taint(&[object_path.as_path()]).unwrap();
        assert!(!forklift.join(FOLDER_NAME_TAINT).exists(),
            "an unactivated process must create no taint directory at all");
        assert!(gate_check(&forklift).is_ok(), "the gate must never trip in an unactivated process");
        assert!(read_taints(&forklift).unwrap().recorded.is_empty(),
            "an unactivated read must see nothing recorded");

        activate();

        record_taint(&[object_path.as_path()]).unwrap();
        let gate_error = gate_check(&forklift).expect_err("once activated, a recorded taint must trip the gate");
        assert!(gate_error.contains(GATE_TAINT_MARKER),
            "a gate refusal must contain the machine-recognizable marker, got {:?}", gate_error);
        assert!(!read_taints(&forklift).unwrap().recorded.is_empty(),
            "once activated, the write must actually be readable");

        clear_gate(&forklift);
        assert!(gate_check(&forklift).is_ok(), "clearing the gate must restore a passing check");
    }

    #[test]
    fn gate_key_resolves_identically_for_the_setter_and_the_checker() {
        // Pins gate-key pinning: the same root, however independently it is built by the
        // checking side, must resolve to the same key the setting side used — and a disjoint
        // root must never be affected by an unrelated one's taint.
        let _serial = lock_activation();
        activate();

        let root_a = scratch("gate-key-a");
        let root_b = scratch("gate-key-b");

        let _scope_a = StorageRootScope::enter(&root_a);
        let forklift_a = forklift_root();
        let object_path = forklift_a.join("objects").join("44").join("55667788");
        record_taint(&[object_path.as_path()]).unwrap();

        // Built independently of the scope machinery, but denoting the exact same root.
        let same_root_independently_built = root_a.join(".forklift");
        assert!(gate_check(&same_root_independently_built).is_err(),
            "the gate must trip when checked against the same resolved root, however it was built");

        {
            let _scope_b = StorageRootScope::enter(&root_b);
            let forklift_b = forklift_root();
            assert!(gate_check(&forklift_b).is_ok(), "a disjoint root's gate must be unaffected");
        }

        clear_gate(&forklift_a);
        assert!(gate_check(&forklift_a).is_ok(), "clearing must restore the exact root it was asked to clear");
    }

    #[test]
    fn a_fault_during_the_taints_own_write_leaves_no_taint_file_but_still_gates() {
        // Pins the no-re-entry / double-failure-residual contract: a *returned* failure from the
        // taint's own write leaves no taint file behind (never a rename-over, never partial
        // debris from a controlled failure — only a real crash can do that, see the torn test),
        // fires exactly once (no recursion back into tainting), and the gate — set before the
        // write was even attempted — still stands afterward.
        let _serial = lock_activation();
        activate();

        let root = scratch("fault");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("66").join("77889900");

        let fault = TaintFaultGuard::failing("taint-");
        let result = record_taint(&[object_path.as_path()]);
        assert!(result.is_err(), "an injected failure during the taint's own write must surface as an error");
        assert_eq!(taint_write_attempts().len(), 1,
            "the fault must have fired exactly once — no recursion back into tainting");
        drop(fault);

        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        let leftovers: Vec<_> = std::fs::read_dir(&taint_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert!(leftovers.is_empty(), "a failed taint write must leave no taint file behind, found {:?}",
            leftovers.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        assert!(gate_check(&forklift).is_err(),
            "the process must gate itself even though the durable write failed (ordering: gate before write)");
    }

    #[test]
    fn record_taint_skips_silently_when_a_path_is_not_under_the_resolved_root() {
        // Pins the scope-tolerance clause: a path with no relation to the resolved storage root
        // (the shape a bare-path unit test's own paths take) is tolerated, not errored, and
        // nothing is written for it.
        let _serial = lock_activation();
        activate();

        let root = scratch("unresolvable");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let unrelated = std::env::temp_dir()
            .join(format!("forklift-taint-unrelated-{}", std::process::id()));

        let result = record_taint(&[unrelated.as_path()]);
        assert!(result.is_ok(), "an unresolvable root must be tolerated, not errored");
        assert!(!forklift.join(FOLDER_NAME_TAINT).exists(),
            "nothing may be written when a path cannot be resolved against the storage root");
    }

    #[test]
    fn remove_taint_files_clears_every_snapshotted_file_so_a_fresh_read_sees_nothing() {
        // Pins the future-heal primitive in isolation: after removal, a read sees the fully
        // empty, non-torn state again — the same state an untainted root reads as.
        let _serial = lock_activation();
        activate();

        let root = scratch("remove-taints");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("88").join("99001122");

        record_taint(&[object_path.as_path()]).unwrap();
        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.is_empty());

        remove_taint_files(&forklift, &state.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(state.recorded.is_empty() && !state.torn,
            "every snapshotted taint file must be gone after a clean removal");
    }

    #[test]
    fn remove_taint_files_deletes_only_the_snapshot_sparing_a_concurrently_recorded_taint() {
        // Pins Part 3: the core bug this whole fix exists for. `remove_taint_files` must delete
        // exactly the snapshot a caller read earlier, never whatever the directory happens to hold
        // at deletion time. Interleaving: activate -> record T1 -> read_taints (snapshot) ->
        // record F2 (a *different* process's taint, born after the snapshot) -> remove using the
        // stale snapshot. Reverting Part 3 (rescanning the directory at deletion time instead of
        // deleting exactly `files`) deletes F2 too, going red.
        let _serial = lock_activation();
        activate();

        let root = scratch("snapshot-scoped-remove");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let taint_dir = forklift.join(FOLDER_NAME_TAINT);

        let object_t1 = forklift.join("objects").join("11").join("aa112233");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1, "sanity: exactly T1's file was read");

        // A second taint recorded *after* the snapshot was taken — standing in for a sibling
        // process's heal-triggering write racing this one.
        let object_f2 = forklift.join("objects").join("22").join("bb223344");
        record_taint(&[object_f2.as_path()]).unwrap();

        remove_taint_files(&forklift, &snapshot.files).unwrap();

        let remaining: Vec<_> = std::fs::read_dir(&taint_dir).unwrap().filter_map(|e| e.ok()).collect();
        assert_eq!(remaining.len(), 1, "exactly the concurrently-recorded taint must survive, found {:?}",
            remaining.iter().map(|e| e.file_name()).collect::<Vec<_>>());

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/11/aa112233")),
            "T1, named in the snapshot, must be gone after the snapshot-scoped removal");
        assert!(state.recorded.contains(&PathBuf::from("objects/22/bb223344")),
            "F2, recorded after the snapshot, must survive the removal completely untouched");
    }

    #[test]
    fn remove_taint_files_tolerates_a_snapshotted_file_already_gone() {
        // Pins the ENOENT-tolerance half of Part 3: a sibling healer racing the exact same
        // snapshot may already have removed a file by the time this call gets to it — that is a
        // legitimate race won, not an error. Reverting the `NotFound` tolerance (making any
        // `remove_file` error propagate) turns this red.
        let _serial = lock_activation();
        activate();

        let root = scratch("enoent-tolerant-remove");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_path = forklift.join("objects").join("33").join("cc334455");
        record_taint(&[object_path.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        // Out-of-band removal, standing in for a sibling healer that already won the race on this
        // exact file.
        file_utils::remove_file(&snapshot.files[0]).unwrap();

        let _guard = file_utils::SyncDirFaultGuard::recording();
        let result = remove_taint_files(&forklift, &snapshot.files);
        assert!(result.is_ok(), "a file already gone from the snapshot must not be an error, got {:?}", result);
        // `sync_dir` is a documented no-op on non-Unix (`file_utils::sync_dir`), so the fault-
        // recording rig behind `sync_dir_attempts` never records there — this assertion only
        // holds on Unix.
        #[cfg(unix)]
        assert!(file_utils::sync_dir_attempts().contains(&forklift.join(FOLDER_NAME_TAINT)),
            "the taint directory must still be synced even when every snapshotted file was already gone");
    }

    #[test]
    fn resolve_taints_is_snapshot_scoped_and_spares_a_concurrent_record() {
        // Pins Part 4: the same snapshot-scoped-delete discipline as `remove_taint_files`, but for
        // the partial-clear primitive, whose real caller (the closure walk in `recovery_utils`) can
        // run for minutes between snapshotting and deleting. Reverting Part 4 (rescanning the
        // directory for `old_files` instead of using exactly the given snapshot) deletes the
        // concurrently-recorded F2 too, going red.
        let _serial = lock_activation();
        activate();

        let root = scratch("snapshot-scoped-replace");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("44").join("dd445566");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        let object_f2 = forklift.join("objects").join("55").join("ee556677");
        record_taint(&[object_f2.as_path()]).unwrap();

        let remainder: BTreeSet<PathBuf> = [PathBuf::from("objects/99/still-dangling")].into_iter().collect();
        resolve_taints(&forklift, &remainder, &snapshot.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/44/dd445566")),
            "T1, named in the snapshot, must be superseded by the replacement");
        assert!(state.recorded.contains(&PathBuf::from("objects/99/still-dangling")),
            "the new remainder must be recorded");
        assert!(state.recorded.contains(&PathBuf::from("objects/55/ee556677")),
            "F2, recorded after the snapshot, must survive the replacement completely untouched");
    }

    #[test]
    fn resolve_taints_empty_remainder_delegates_to_snapshot_scoped_removal() {
        // The empty-`remainder` branch must delegate to the same snapshot-scoped
        // `remove_taint_files`, not a rescan-and-delete-everything shortcut — otherwise a
        // concurrently recorded taint would be lost via this branch even after Part 3/4 fixed the
        // other two call shapes.
        let _serial = lock_activation();
        activate();

        let root = scratch("replace-empty-remainder");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("66").join("ff667788");
        record_taint(&[object_t1.as_path()]).unwrap();

        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1);

        let object_f2 = forklift.join("objects").join("77").join("00778899");
        record_taint(&[object_f2.as_path()]).unwrap();

        resolve_taints(&forklift, &BTreeSet::new(), &snapshot.files).unwrap();

        let state = read_taints(&forklift).unwrap();
        assert!(!state.recorded.contains(&PathBuf::from("objects/66/ff667788")), "T1 must be gone");
        assert!(state.recorded.contains(&PathBuf::from("objects/77/00778899")),
            "F2, recorded after the snapshot, must survive an empty-remainder replacement untouched");
    }

    #[test]
    fn taint_fault_guard_recording_mode_observes_without_failing() {
        // Pins the fault guard's own "recording" mode (used by stage-2 tests that want to assert
        // a taint write happened without injecting a failure): the write still succeeds, and the
        // attempted path is still observable.
        let _serial = lock_activation();
        activate();

        let root = scratch("fault-recording");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("99").join("00112233");

        let _fault = TaintFaultGuard::recording();
        assert!(file_utils::fsync_enabled(), "the armed rig runs real syncs");
        record_taint(&[object_path.as_path()]).unwrap();
        assert_eq!(taint_write_attempts().len(), 1, "a successful write must still be observed");
    }

    // --- `resolve_taints`'s gate-owning contract (DESIGN.html §3.1.1) ----------------------
    //
    // The gate-clear-on-empty-remainder shape used to be split across a snapshot-scoped durable
    // half (this module) and a root-wide, unconditional gate clear (each call site). A taint
    // recorded mid-run, after the caller's own snapshot, had its file correctly spared but its
    // gate wrongly wiped anyway. `resolve_taints` collapses both halves into one call that derives
    // the gate decision from the taint directory itself, at the moment of the call — the tests
    // below pin that directly, plus the companion fix for a gate left standing with nothing on
    // disk.

    #[test]
    fn resolve_taints_leaves_the_gate_standing_when_a_fresh_taint_lands_after_the_snapshot() {
        // TEST A: pins the false-clear direction — the defect this whole fix exists for. A run
        // that believes it resolved everything (`remainder` empty) must not clear the gate over a
        // taint file that landed *after* its own snapshot was taken but *before* this call —
        // standing in for a mid-heal store failure elsewhere in the same run. Reverting to the old
        // unconditional-clear-on-empty-remainder shape (delete the snapshot, then unconditionally
        // clear the gate, without ever consulting the directory) turns the gate assertion red; T2
        // surviving on disk is already covered by
        // `resolve_taints_is_snapshot_scoped_and_spares_a_concurrent_record`.
        let _serial = lock_activation();
        activate();

        let root = scratch("resolve-taints-false-clear");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();

        let object_t1 = forklift.join("objects").join("aa").join("11223344");
        record_taint(&[object_t1.as_path()]).unwrap();
        let snapshot = read_taints(&forklift).unwrap();
        assert_eq!(snapshot.files.len(), 1, "sanity: exactly T1's file was read");

        // Standing in for a mid-heal store failure landing after the snapshot but before the
        // resolve call — the exact interleaving `resolve_the_rest` can hit (§1 of the fix design).
        let object_t2 = forklift.join("objects").join("bb").join("55667788");
        record_taint(&[object_t2.as_path()]).unwrap();

        resolve_taints(&forklift, &BTreeSet::new(), &snapshot.files).unwrap();

        assert!(gate_check(&forklift).is_err(),
            "T2's taint file is still standing on disk, so the gate must still be standing too");
        let state = read_taints(&forklift).unwrap();
        assert!(state.recorded.contains(&PathBuf::from("objects/bb/55667788")),
            "T2 must survive the snapshot-scoped removal completely untouched");
    }

    #[test]
    fn resolve_taints_clears_a_gate_left_standing_with_nothing_on_disk() {
        // TEST C: the two-way sync and the companion fix's target state. A gate can be left
        // standing with nothing on disk when `record_taint`'s own write fails *after* `set_gate`
        // already succeeded (its own doc comment: that ordering is load-bearing). Reproduced here
        // via `TaintFaultGuard::failing` — an existing, already-shipped `#[cfg(test)]` fault guard
        // (see its own doc comment above), not a new backdoor: it fails `sync_taint_file`, so
        // `record_taint`'s `set_gate` succeeds and its subsequent write fails, leaving exactly the
        // gate-standing-with-nothing-on-disk state this test targets. Calling `resolve_taints`
        // with an empty remainder *and* an empty snapshot models the companion fix's own
        // early-return call shape (§2.1; Tests D1/D2 below pin the actual call sites).
        //
        // A clear-only primitive (never syncing the "set" direction) would already pass this
        // exact assertion, so this test's real job is pinning that the gate step is never
        // skipped outright — an easy short-circuit to introduce by accident once `remainder` and
        // `old_files` are both empty (there is nothing to delete, so it can look safe to return
        // early before ever consulting the directory for the gate decision). Reverting
        // `resolve_taints` to skip `sync_gate_with_disk` on that empty/empty shape turns this red.
        let _serial = lock_activation();
        activate();

        let root = scratch("resolve-taints-stray-gate");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("cc").join("99001122");

        let fault = TaintFaultGuard::failing("taint-");
        record_taint(&[object_path.as_path()]).unwrap_err();
        drop(fault);

        assert!(gate_check(&forklift).is_err(), "sanity: the gate is standing with nothing on disk");
        assert!(read_taints(&forklift).unwrap().recorded.is_empty(),
            "sanity: the failed write left no taint file behind");

        resolve_taints(&forklift, &BTreeSet::new(), &[]).unwrap();

        assert!(gate_check(&forklift).is_ok(),
            "the gate must clear despite never having been set by a successful record");
    }

    #[test]
    fn run_clears_a_stray_gate_on_its_own_nothing_recorded_early_return() {
        // TEST D1: pins the site-level wiring for `recovery_utils::run`'s "nothing recorded" early
        // return (§2.1), not just a `resolve_taints` call shaped like it (Test C covers the
        // primitive alone). Same stray-gate fixture as Test C, then drives the real `run` entry
        // point via a `current_thread` runtime — mirroring `recovery_utils::tests::run_heal`.
        // Reverting §2.1's routing back to a bare `return Ok(HealOutcome::nothing())` at
        // `recovery_utils.rs`'s "nothing recorded" branch turns the gate assertion red: the gate
        // stays standing after `run` reports "nothing to heal."
        let _serial = lock_activation();
        activate();

        let root = scratch("run-clears-stray-gate");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("dd").join("11223344");

        let fault = TaintFaultGuard::failing("taint-");
        record_taint(&[object_path.as_path()]).unwrap_err();
        drop(fault);

        assert!(gate_check(&forklift).is_err(), "sanity: the gate is standing with nothing on disk");
        assert!(read_taints(&forklift).unwrap().recorded.is_empty(),
            "sanity: `run` must see an empty recorded set, the same empty-state shape \
            `heal_if_tainted_is_a_cheap_ok_when_nothing_is_recorded` fixtures directly");

        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let outcome = runtime.block_on(crate::util::recovery_utils::run(None));
        assert!(outcome.is_ok(), "an empty-but-stray-gated root must still report a clean 'nothing to heal'");

        assert!(gate_check(&forklift).is_ok(), "the stray gate must be cleared by run's early return");
    }

    #[test]
    fn heal_if_tainted_clears_a_stray_gate_on_its_own_nothing_recorded_early_return() {
        // TEST D2: the same pin as D1, for `heal_utils::heal_if_tainted`'s own early return
        // (`heal_utils.rs:325-327`) instead of `run`'s. Reverting that routing back to a bare
        // `return Ok(())` turns the gate assertion red the same way.
        let _serial = lock_activation();
        activate();

        let root = scratch("heal-if-tainted-clears-stray-gate");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("ee").join("55667788");

        let fault = TaintFaultGuard::failing("taint-");
        record_taint(&[object_path.as_path()]).unwrap_err();
        drop(fault);

        assert!(gate_check(&forklift).is_err(), "sanity: the gate is standing with nothing on disk");
        assert!(read_taints(&forklift).unwrap().recorded.is_empty());

        crate::util::heal_utils::heal_if_tainted()
            .expect("an empty-but-stray-gated root must still report ok");

        assert!(gate_check(&forklift).is_ok(),
            "the stray gate must be cleared by heal_if_tainted's own early return");
    }

    #[test]
    fn resolve_taints_fails_closed_and_leaves_the_gate_standing_on_a_directory_read_error() {
        // TEST E: §2 item 1's fail-closed clause — on a directory-read error, the gate is left
        // exactly as it was, never guessed. A permission-based fixture cannot isolate this: this
        // function's own inherited durable-file work already calls `file_utils::sync_dir` on the
        // taint directory unconditionally (whenever it exists), and `sync_dir` opens the
        // directory with `std::fs::File::open` — the same read bit `std::fs::read_dir` needs — so
        // a `chmod`'d-unreadable directory trips that earlier step and this function never reaches
        // the new gate-decision code at all (verified empirically on this machine: `chmod 300`
        // makes both `ls` and a plain `open(O_RDONLY)` fail identically).
        //
        // The fixture that actually isolates it: replace the taint directory with a plain regular
        // file at the identical path. Opening a regular file `O_RDONLY` and `fsync`-ing it
        // succeeds (so `sync_dir`'s call during the durable-file-work phase succeeds trivially,
        // same as on a real, empty directory), while `read_dir` on a regular file fails
        // deterministically with "not a directory" — cleanly separating the two steps, with no
        // race and no production fault hook. Not unix-specific: `std::fs::read_dir` returns `Err`
        // on a non-directory path on every target, and `sync_dir` is already a no-op on non-unix
        // targets, so the durable-file-work phase trivially succeeds on Windows regardless of what
        // sits at that path.
        let _serial = lock_activation();
        activate();

        let root = scratch("resolve-taints-fail-closed");
        let _scope = StorageRootScope::enter(&root);
        let forklift = forklift_root();
        let object_path = forklift.join("objects").join("ff").join("00998877");

        record_taint(&[object_path.as_path()]).unwrap();
        assert!(gate_check(&forklift).is_err(), "sanity: the gate is standing before the swap");

        let taint_dir = forklift.join(FOLDER_NAME_TAINT);
        file_utils::remove_dir_all(&taint_dir).unwrap();
        std::fs::write(&taint_dir, b"not a directory").unwrap();

        // Empty remainder *and* empty `old_files`, deliberately: a non-empty `old_files` would
        // make the durable-file-work phase's own `remove_file` calls fail with their own
        // "not a directory" error — a different, already-covered failure mode, not the one this
        // test targets.
        let result = resolve_taints(&forklift, &BTreeSet::new(), &[]);
        assert!(result.is_err(), "a directory-read error must surface as an error, not be swallowed");

        assert!(gate_check(&forklift).is_err(),
            "the gate must be left exactly as it was — still standing — never guessed clear");

        file_utils::remove_file(&taint_dir).unwrap();
    }

    // --- GATE_COUNT fast-path tests --------------------------------------------------------
    //
    // These pin the `gate_check` fast negative added on top of the mutex-only gate: a lock-free
    // `GATE_COUNT` that must stay in exact lockstep with `gate_state`'s `BTreeSet` cardinality.
    // Each test uses `reset_gate_for_test` first since the gate is process-global state shared
    // with every other test in this file (several of which deliberately leave a gate standing),
    // then asserts `GATE_COUNT` directly (not just `gate_check`'s return) where that is the only
    // way to redden a miscount — an over-count is invisible to `gate_check`'s return value,
    // because a nonzero count just falls through to the same exact locked membership test as
    // before this optimization, so it can never itself produce a wrong answer, only waste a lock
    // acquisition. Only an *under*-count (reaching zero while a gate genuinely still stands) can
    // corrupt `gate_check`'s answer, so several of these tests assert the count itself to catch
    // over-count bugs that a same-root `gate_check` call alone would never reveal.

    #[test]
    fn gate_check_takes_the_fast_path_when_no_gate_is_standing() {
        // Item 1: nothing ever gated -> the count is zero and the fast path alone answers Ok.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-none");
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0);
        assert!(gate_check(&root).is_ok(), "an unset root must pass with no gate ever standing");
    }

    #[test]
    fn gate_check_finds_a_standing_gate_for_its_own_root() {
        // Item 2: a nonzero count still takes the lock and finds the exact root that was set.
        // Reddens if the fast-path condition is inverted (e.g. `== 0` flipped to `!= 0`) or if
        // `set_gate` fails to increment on a genuine insert.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-self");
        set_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1, "a fresh set_gate must increment the count exactly once");
        assert!(gate_check(&root).is_err(), "a standing gate for this exact root must still trip");
    }

    #[test]
    fn gate_check_is_unaffected_by_a_standing_gate_on_a_different_root() {
        // Item 3: a nonzero count does not make the fast path (or a sloppy slow path) over-trigger
        // for a root that was never gated — the locked membership test is still exact per-root.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root_a = scratch("gate-count-diff-a");
        let root_b = scratch("gate-count-diff-b");
        set_gate(&root_a);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1);

        assert!(gate_check(&root_b).is_ok(), "a disjoint root must pass even while another root is gated");
        assert!(gate_check(&root_a).is_err(), "the gated root itself must still trip");
    }

    #[test]
    fn clearing_the_only_standing_gate_restores_the_fast_path() {
        // Item 4: set then clear -> the count returns to zero and the fast path answers Ok again.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-clear");
        set_gate(&root);
        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0, "clearing the only standing gate must bring the count back to zero");
        assert!(gate_check(&root).is_ok(), "clearing the only standing gate must restore a passing check");
    }

    #[test]
    fn a_duplicate_set_gate_does_not_inflate_the_count() {
        // Item 5 (double-set half): set_gate is idempotent, so setting an already-set root a
        // second time must not increment again. Reddens if someone implements the increment as
        // "on every set_gate call" rather than gated on `BTreeSet::insert`'s `true` return — that
        // bug leaves the count stuck at 1 after the single real `clear_gate` below, which this
        // test catches by asserting the count directly (gate_check(root) alone would still read
        // Ok in that buggy case, via the slow path finding the root genuinely absent — it would
        // not redden).
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-double-set");
        set_gate(&root);
        set_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1,
            "a duplicate set_gate on an already-set root must not inflate the count");

        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0,
            "the single real clear must bring the count back to zero, proving the duplicate set never incremented it");
        assert!(gate_check(&root).is_ok());
    }

    #[test]
    fn a_duplicate_clear_gate_does_not_underflow_the_count() {
        // Item 5 (double-clear mirror): clear_gate is idempotent, so clearing an already-clear (or
        // never-set) root must never decrement. Reddens if the decrement is not gated on
        // `BTreeSet::remove`'s `true` return — that bug underflows the `AtomicUsize` (wraps to a
        // huge nonzero value under `fetch_sub`, since atomics wrap rather than panic even in
        // debug builds), which this test catches directly via the count assertion.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root = scratch("gate-count-double-clear");
        clear_gate(&root);
        clear_gate(&root);
        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 0, "clearing an unset root must never decrement the count");
        assert!(gate_check(&root).is_ok());
    }

    #[test]
    fn clearing_an_unset_root_does_not_erode_a_different_standing_gate() {
        // The dangerous shape of an ungated decrement: clearing a root that was *never* set must
        // not erode the count contributed by a *different*, genuinely standing gate down to zero
        // — that would send `gate_check` for the still-tainted root down the fast path to a false
        // `Ok`. This is the one miscount class that a same-root `gate_check` call cannot catch on
        // its own (see this section's header comment), so it is asserted on both the count and
        // the still-tainted root's `gate_check` result.
        let _serial = lock_activation();
        activate();
        reset_gate_for_test();

        let root_a = scratch("gate-count-erosion-a");
        let root_b_never_set = scratch("gate-count-erosion-b");
        set_gate(&root_a);
        clear_gate(&root_b_never_set);

        assert_eq!(GATE_COUNT.load(Ordering::Relaxed), 1,
            "clearing a root that was never gated must not erode a different root's standing gate");
        assert!(gate_check(&root_a).is_err(), "the genuinely standing gate must still trip, not be skipped via the fast path");
    }
}
