use std::collections::{BTreeMap, BinaryHeap, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::remote::{HoldAnswer, MAX_HOLDS_CANDIDATES};
use crate::globals::bay_root;
use crate::util::scope_utils::{self, MaterializationScope, ScopeClass};
use crate::util::{fanout_utils, file_utils, graph_utils, lcs, object_utils, pallet_utils, record_utils};

/// The name of the consolidation-state file (inside the forklift root folder). While a
/// consolidation is in progress, it holds the head parcel hash of the pallet being
/// consolidated in (line 1), that pallet's name (line 2), the head the merge was built on
/// (line 3) and the pallet it was started on (line 4). The next `stack` records the first hash as
/// a second parent and removes the file.
const FILE_NAME_CONSOLIDATION: &str = "consolidation";

/// The maximum number of lines (per side) the line-level merge attempts. Larger files are
/// not merged line by line; they fall back to a whole-file conflict.
const MAX_MERGE_LINES: usize = 20_000;

/// The result of a three-way content merge.
pub struct MergeResult {
    /// The merged content (with conflict markers if `has_conflicts` is set).
    pub content: Vec<u8>,

    /// Whether the merge produced conflicts.
    pub has_conflicts: bool,
}

/// A consolidation in progress.
pub struct ConsolidationState {
    /// The head parcel hash of the pallet being consolidated in (the second parent of the
    /// upcoming merge parcel).
    pub their_head: String,

    /// The name of the pallet being consolidated in (informational).
    pub their_pallet: String,

    /// The current pallet's head the merge was built on: the merge parcel's first parent, and
    /// the head the completing `stack` expects the pallet still to hold.
    pub our_head: String,

    /// The pallet the merge was started on, and the only one `stack` completes it on.
    pub our_pallet: String,
}

/// The [`record_utils`] kind and version of the consolidation-state file: the other side's head,
/// then its label, which may be a haul's source and so may hold any character, then our head and
/// our pallet.
// The undo journal embeds this state (`journal_utils::ConsolidationInProgress`): bump
// `journal_utils::JOURNAL_VERSION` with it.
const CONSOLIDATION_RECORD: &str = "consolidation";
const CONSOLIDATION_VERSION: u32 = 2;

/// The [`record_utils`] kind and version of the out-of-scope skeleton file.
const SKELETON_RECORD: &str = "consolidation-skeleton";
// The undo journal embeds skeletons in this form (`journal_utils::ConsolidationInProgress`):
// bump `journal_utils::JOURNAL_VERSION` with it, so an old journal is named as another version.
const SKELETON_VERSION: u32 = 1;

/// How to abort the consolidation whose files live in the bay-local state dir `dir`: remove both
/// of them. A phrase such as `remove "<a>" and "<b>"`, naming that bay's own paths.
pub fn abort_hint_in(dir: &Path) -> String {
    format!(
        "remove \"{}\" and \"{}\"",
        dir.join(FILE_NAME_CONSOLIDATION).to_string_lossy(),
        dir.join(FILE_NAME_SKELETON).to_string_lossy()
    )
}

/// The way out of an unreadable consolidation in `dir`: both of its files go, and the merge
/// starts over.
fn abort_remedy_in(dir: &Path) -> String {
    format!("Abort the consolidation ({}) and consolidate again.", abort_hint_in(dir))
}

/// Get the path of the consolidation-state file (bay-local: a merge in progress belongs
/// to the bay resolving it).
fn get_consolidation_state_path() -> PathBuf {
    bay_root().join(FILE_NAME_CONSOLIDATION)
}

/// Read the consolidation state from a specific bay-local state dir, independent of the active
/// bay. For a caller enumerating every bay's state — see `bay_utils::all_bay_state_dirs` — never
/// just the active one; [`read_consolidation_state`] is this, scoped to the active bay.
///
/// # Arguments
/// * `dir` - The bay-local state dir to read: the main tree's own `forklift_root()`, or a named
///           bay's `bay_utils::bay_state_dir`.
///
/// # Returns
/// * `Ok(Some(ConsolidationState))` - The state of the consolidation in progress.
/// * `Ok(None)`                     - If no consolidation is in progress.
/// * `Err(String)`                  - If the state file exists but is malformed.
pub fn read_consolidation_state_in(dir: &Path) -> Result<Option<ConsolidationState>, String> {
    let path = dir.join(FILE_NAME_CONSOLIDATION);

    if !path.exists() {
        return Ok(None);
    }

    let bytes = std::fs::read(&path)
        .map_err(|e| format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))?;

    let malformed = |reason: String| format!(
        "The consolidation state file \"{}\" {} {}", path.to_string_lossy(), reason, abort_remedy_in(dir)
    );

    let lines = record_utils::from_bytes(&bytes, CONSOLIDATION_RECORD, CONSOLIDATION_VERSION)
        .map_err(|e| malformed(e.describe()))?;

    let [their_head, their_pallet, our_head, our_pallet] = <[String; 4]>::try_from(lines)
        .map_err(|_| malformed("is malformed.".to_string()))?;

    if !object_utils::is_object_hash(&their_head) || their_pallet.is_empty() || !object_utils::is_object_hash(&our_head)
        || pallet_utils::validate_pallet_name(&our_pallet).is_err() {
        return Err(malformed("is malformed.".to_string()));
    }

    Ok(Some(ConsolidationState { their_head, their_pallet, our_head, our_pallet }))
}

/// Read the consolidation state of the active bay, if a consolidation is in progress.
///
/// # Returns
/// * `Ok(Some(ConsolidationState))` - The state of the consolidation in progress.
/// * `Ok(None)`                     - If no consolidation is in progress.
/// * `Err(String)`                  - If the state file exists but is malformed.
pub fn read_consolidation_state() -> Result<Option<ConsolidationState>, String> {
    read_consolidation_state_in(&bay_root())
}

/// Write the consolidation state (atomically).
///
/// # Arguments
/// * `state` - The state to write.
///
/// # Returns
/// * `Ok(())`      - If the state was written.
/// * `Err(String)` - If the file could not be written.
pub fn write_consolidation_state(state: &ConsolidationState) -> Result<(), String> {
    file_utils::write_file_atomically(&get_consolidation_state_path(), &consolidation_state_bytes(state))
}

/// The bytes of the consolidation state file recording `state`.
pub(crate) fn consolidation_state_bytes(state: &ConsolidationState) -> Vec<u8> {
    record_utils::to_bytes(CONSOLIDATION_RECORD, CONSOLIDATION_VERSION, [&state.their_head, &state.their_pallet, &state.our_head, &state.our_pallet])
}

/// Remove the consolidation state file (a no-op when none exists).
///
/// # Returns
/// * `Ok(())`      - If the state file is gone.
/// * `Err(String)` - If the file exists but could not be removed.
pub fn clear_consolidation_state() -> Result<(), String> {
    let path = get_consolidation_state_path();

    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Error while removing \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// The name of the out-of-scope skeleton file (beside the consolidation-state file, bay-local).
const FILE_NAME_SKELETON: &str = "consolidation-skeleton";

/// The out-of-scope resolution skeleton a scoped-bay merge produces: every out-of-scope
/// subtree/file/symlink adopted from *theirs* by hash, keyed by warehouse path. It carries
/// no content — only the resolved `(hash, type)` (or a deletion) — and is consumed by the
/// *next* `stack`, whose overlay splices these entries into the merge parcel's root tree so
/// the committed tree is byte-identical to a full workspace merging the same two heads. A
/// one-sided out-of-scope change never touches the working directory or the inventory; a
/// two-sided one refuses (`out_of_scope_conflict`) before any skeleton or parcel exists.
///
/// Entries are keyed on the full warehouse path: `Some((hash, type))` sets the entry (a subtree,
/// file or symlink adopted from theirs), `None` deletes it (theirs removed it). A [`BTreeMap`]
/// gives a deterministic on-disk order regardless of the walk order that produced the entries.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct OutOfScopeSkeleton {
    entries: BTreeMap<String, Option<(String, DirEntryType)>>,
}

impl OutOfScopeSkeleton {
    /// Collect the out-of-scope resolutions out of a merge's action list.
    ///
    /// At a directory↔file type flip, the walk emits **two** `ResolveOutOfScope` actions for the
    /// same path: the file loop and the subtree loop each see the name in their own map, and
    /// exactly one of them matches theirs' actual (post-flip) entry — a `Set` — while the other
    /// sees no entry of its own type there — a `Delete`. Which loop produces the `Set` and which
    /// produces the `Delete` depends on the flip's direction (file loop runs before the subtree
    /// loop at each level), so plain last-write-wins insertion is **not** safe: it happens to keep
    /// the right answer when the `Set` is emitted second (file → directory), but silently drops it
    /// when the `Set` is emitted first (directory → file, clobbered by the `Delete` that follows).
    ///
    /// A tree level cannot hold both a file and a subtree of the same name, so at most one of the
    /// two actions for a given path is ever a `Set` — "keep whichever resolution is `Some`" is
    /// therefore unconditionally correct, independent of which one arrives first.
    pub fn from_actions(actions: &[MergeAction]) -> OutOfScopeSkeleton {
        let mut entries: BTreeMap<String, Option<(String, DirEntryType)>> = BTreeMap::new();

        for action in actions {
            if let MergeAction::ResolveOutOfScope { path, resolution } = action {
                entries.entry(path.clone())
                    .and_modify(|existing| {
                        // Never let a later `None` (delete) overwrite an already-recorded `Some`
                        // (set) — only a `Some` may ever replace what is there.
                        if resolution.is_some() {
                            *existing = resolution.clone();
                        }
                    })
                    .or_insert_with(|| resolution.clone());
            }
        }

        OutOfScopeSkeleton { entries }
    }

    /// Whether the skeleton has no entries (the common case: a merge whose out-of-scope siblings
    /// were all unchanged, or a full-bay merge, which produces none).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The resolved entries (warehouse path → `Some((hash, type))` to set, `None` to delete),
    /// consumed by the stack overlay (`tree_utils::build_scoped_root_tree`).
    pub fn entries(&self) -> &BTreeMap<String, Option<(String, DirEntryType)>> {
        &self.entries
    }

    /// The bay-local path of the skeleton file.
    pub(crate) fn path() -> PathBuf {
        bay_root().join(FILE_NAME_SKELETON)
    }

    /// Read the skeleton beside the in-progress consolidation, returning an empty skeleton when
    /// the file is absent (a merge with no out-of-scope resolutions). Used outside a completing
    /// stack (e.g. inspection); the completing stack itself uses [`OutOfScopeSkeleton::read_required`]
    /// instead, since *there* an absent file is not "no resolutions" but a broken invariant.
    pub fn read() -> Result<OutOfScopeSkeleton, String> {
        let path = OutOfScopeSkeleton::path();

        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(OutOfScopeSkeleton::default()),
            Err(e) => return Err(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)),
        };

        OutOfScopeSkeleton::from_bytes(&bytes, &path)
    }

    /// Read the skeleton beside an in-progress consolidation, **requiring the file to exist**.
    ///
    /// `consolidate` always writes the skeleton — even an empty one — strictly *before* it writes
    /// the consolidation state (see `merge_head_into_current`), so once a consolidation state file
    /// exists, its skeleton is guaranteed to exist too, unless the write between the two was
    /// interrupted (a crash, or a failed write). Defaulting to empty in that case would silently
    /// drop every adopted-by-hash out-of-scope entry from the committing merge tree — the exact
    /// wrong-but-complete-tree failure the skeleton exists to prevent, just relocated to a rarer
    /// window. Refusing instead makes the invariant checkable rather than merely assumed.
    ///
    /// # Returns
    /// * `Ok(OutOfScopeSkeleton)` - The skeleton (possibly empty).
    /// * `Err(String)` - The skeleton file is missing or unreadable; names the recovery.
    pub fn read_required() -> Result<OutOfScopeSkeleton, String> {
        let path = OutOfScopeSkeleton::path();

        let bytes = std::fs::read(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
format!(
                    "A consolidation is in progress but its out-of-scope skeleton (\"{}\") is \
                    missing — most likely an interrupted write (a crash mid-merge). The completing \
                    stack cannot tell what out-of-scope entries the merge resolved, so it refuses \
                    rather than silently drop them. {}",
                    path.to_string_lossy(), abort_remedy_in(&bay_root())
                )
            } else {
                format!("Error while reading \"{}\": {}", path.to_string_lossy(), e)
            }
        })?;

        OutOfScopeSkeleton::from_bytes(&bytes, &path)
    }

    /// Write the skeleton (atomically) beside the consolidation-state file.
    pub fn write(&self) -> Result<(), String> {
        file_utils::write_file_atomically(&OutOfScopeSkeleton::path(), &self.to_bytes())
    }

    /// Remove the skeleton file (a no-op when none exists), consumed alongside the consolidation.
    pub fn clear() -> Result<(), String> {
        let path = OutOfScopeSkeleton::path();

        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("Error while removing \"{}\": {}", path.to_string_lossy(), e)),
        }
    }

    /// Serialize to the on-disk form, a [`record_utils`] file of kind [`SKELETON_RECORD`]: one
    /// entry per line. A set is `S <type-code> <hash> <path>` (path last, so a path with spaces
    /// survives); a deletion is `D <path>`. The path may hold a line break, which the record
    /// escapes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let lines = self.entries.iter().map(|(path, resolution)| match resolution {
            Some((hash, item_type)) => format!("S {} {} {}", item_type.get_code(), hash, path),
            None => format!("D {}", path),
        });

        record_utils::to_bytes(SKELETON_RECORD, SKELETON_VERSION, lines)
    }

    /// Parse the on-disk form produced by [`OutOfScopeSkeleton::to_bytes`].
    ///
    /// `path` is where `bytes` were read from; an error names it, and the way out in its bay.
    pub fn from_bytes(bytes: &[u8], path: &Path) -> Result<OutOfScopeSkeleton, String> {
        let remedy = abort_remedy_in(path.parent().unwrap_or(Path::new("")));
        let lines = record_utils::from_bytes(bytes, SKELETON_RECORD, SKELETON_VERSION).map_err(|e| format!(
            "The consolidation skeleton file \"{}\" {} {}", path.to_string_lossy(), e.describe(), remedy
        ))?;

        let mut entries: BTreeMap<String, Option<(String, DirEntryType)>> = BTreeMap::new();

        for line in &lines {
            let malformed = || format!(
                "The consolidation skeleton file \"{}\" has a malformed line \"{}\". {}",
                path.to_string_lossy(), line.escape_debug(), remedy
            );

            match line.split_once(' ') {
                Some(("D", path)) if !path.is_empty() => {
                    entries.insert(path.to_string(), None);
                }
                Some(("S", rest)) => {
                    let mut parts = rest.splitn(3, ' ');
                    let code = parts.next().ok_or_else(malformed)?;
                    let hash = parts.next().ok_or_else(malformed)?;
                    let path = parts.next().filter(|path| !path.is_empty()).ok_or_else(malformed)?;

                    let code: u64 = code.parse().map_err(|_| malformed())?;
                    let item_type = DirEntryType::from_code(code)?;

                    if !object_utils::is_object_hash(hash) {
                        return Err(malformed());
                    }

                    entries.insert(path.to_string(), Some((hash.to_string(), item_type)));
                }
                _ => return Err(malformed()),
            }
        }

        Ok(OutOfScopeSkeleton { entries })
    }
}

/// Check whether `ancestor` is an ancestor of (or equal to) `descendant` in the parcel graph.
///
/// The fast path reads the commit-graph ([`graph_utils`]), not parcel objects, and prunes on
/// generation numbers: an ancestor never has a higher generation than its descendant, and each
/// step toward the parents strictly lowers it, so any parcel already at or below the target's
/// generation (and not the target itself) cannot lead to it — its whole sub-history is skipped.
/// On a deep history this turns an O(history) walk into one bounded by the generation gap.
///
/// The graph is an accelerator, never a source of truth. Computing a generation number needs a
/// parcel's *complete* ancestry present locally, which is not always so mid-sync (a diverged
/// remote head is fetched before its every deep ancestor is). When the graph cannot be
/// completed the query falls back to a plain object walk from the descendant — which only ever
/// touches the descendant's own reachable history and short-circuits at `ancestor`, so it needs
/// exactly the objects the answer depends on and no more. A stored generation is only ever
/// written from a fully-loaded ancestry, so the fast path, when it succeeds, is always exact.
///
/// # Arguments
/// * `ancestor`   - The parcel hash to look for.
/// * `descendant` - The parcel hash whose history is walked.
///
/// # Returns
/// * `Ok(bool)`    - Whether `ancestor` is reachable from `descendant`.
/// * `Err(String)` - If the answer's own history could not be read.
pub fn is_ancestor(ancestor: &str, descendant: &str) -> Result<bool, String> {
    match is_ancestor_via_graph(ancestor, descendant) {
        Ok(answer) => Ok(answer),
        // The graph could not be completed (an ancestor object is not present locally yet); the
        // plain walk needs only the descendant's own reachable history, so it answers regardless.
        Err(_) => is_ancestor_via_walk(ancestor, descendant),
    }
}

/// The generation-number-pruned ancestry check (see [`is_ancestor`]). Errors if the graph
/// cannot be built for either parcel (a missing object in its ancestry).
fn is_ancestor_via_graph(ancestor: &str, descendant: &str) -> Result<bool, String> {
    if ancestor == descendant {
        return Ok(true);
    }

    let target = graph_utils::generation(ancestor)?;
    if target > graph_utils::generation(descendant)? {
        // The candidate ancestor is newer than the parcel whose history we would walk, so it
        // cannot possibly be behind it.
        return Ok(false);
    }

    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back(descendant.to_string());

    while let Some(hash) = queue.pop_front() {
        if hash == ancestor {
            return Ok(true);
        }
        if !visited.insert(hash.clone()) {
            continue;
        }

        let node = graph_utils::node(&hash)?;
        // Only a parcel strictly above the target's generation can still reach it; at or below
        // it (and not the target, handled above) its ancestors are all older than the target.
        if node.generation <= target {
            continue;
        }
        for parent in node.parents {
            queue.push_back(parent);
        }
    }

    Ok(false)
}

/// The plain ancestry check: a breadth-first walk from `descendant` toward the roots, reading
/// parcel objects and short-circuiting at `ancestor`. The graph-free fallback for
/// [`is_ancestor`] — it touches only the descendant's reachable history.
fn is_ancestor_via_walk(ancestor: &str, descendant: &str) -> Result<bool, String> {
    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back(descendant.to_string());

    while let Some(hash) = queue.pop_front() {
        if hash == ancestor {
            return Ok(true);
        }
        if !visited.insert(hash.clone()) {
            continue;
        }
        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    Ok(false)
}

/// The most parcels one [`holds`] walk reads. Fixed rather than configurable: passing it answers
/// [`HoldAnswer::Unknown`], which only keeps more on the client, so a wrong value costs disk and
/// never data.
pub const MAX_HOLDS_WALK: usize = 10_000;

/// The longest one [`holds`] walk runs before answering, on every head: well inside the shortest
/// gateway timeout a hosted head sits behind, so a slow store answers [`HoldAnswer::Unknown`]
/// rather than no answer at all. A head whose invocation ends sooner passes its own deadline too.
pub const MAX_HOLDS_WALK_TIME: Duration = Duration::from_secs(10);

/// The most parcels one round of a [`holds_from`] walk asks its reader for at once. A store that
/// reads a round concurrently (the object-storage head) gains up to this much; the time bound is
/// checked between rounds, so it is also how far one round can carry a walk past its deadline.
pub const HOLDS_READ_ROUND: usize = 64;

/// Which of `candidates` a pallet whose head is `head` still holds, under the bounds every head
/// shares: the candidate cap ([`check_holds_candidates`]), [`MAX_HOLDS_WALK`] parcels, and the
/// earlier of [`MAX_HOLDS_WALK_TIME`] from now and `deadline`. The one entry point both heads
/// answer `POST /v1/pallets/{name}/holds` through.
///
/// # Arguments
/// * `head`       - The pallet's current head, or `None` when there is no such pallet.
/// * `candidates` - The parcel hashes to answer for.
/// * `deadline`   - A head's own outer deadline, when it has one.
/// * `parents_of` - Reads a round of parcels' parents (see [`holds_from`]).
///
/// # Returns
/// * `Ok(BTreeMap)` - One answer per distinct candidate.
/// * `Err(String)`  - If there are more candidates than one request may name.
pub fn holds(head: Option<&str>,
             candidates: &[String],
             deadline: Option<Instant>,
             parents_of: impl FnMut(&[String]) -> Vec<Result<Vec<String>, String>>)
             -> Result<BTreeMap<String, HoldAnswer>, String> {
    check_holds_candidates(candidates)?;

    let own = Instant::now() + MAX_HOLDS_WALK_TIME;
    let deadline = deadline.map_or(own, |deadline| deadline.min(own));

    Ok(holds_from(head, candidates, MAX_HOLDS_WALK, || Instant::now() >= deadline, parents_of))
}

/// Refuse a holds request naming more than [`MAX_HOLDS_CANDIDATES`] candidates.
pub fn check_holds_candidates(candidates: &[String]) -> Result<(), String> {
    if candidates.len() > MAX_HOLDS_CANDIDATES {
        return Err(format!("At most {} candidates per request.", MAX_HOLDS_CANDIDATES));
    }

    Ok(())
}

/// Which of `candidates` a pallet whose head is `head` still holds: a bounded ancestry check
/// with an answer for "could not tell", unlike [`is_ancestor`].
///
/// One breadth-first walk from `head` serves every candidate. A candidate the walk reaches is
/// [`HoldAnswer::Held`]. When the walk runs out of history without reaching one, it is
/// [`HoldAnswer::NotHeld`]; with no `head` (the pallet does not exist) every candidate is. A walk
/// that has read `max_steps` parcels, sees `out_of_time` answer true, or fails to read a parcel
/// stops, and every candidate it had not reached is [`HoldAnswer::Unknown`]: it cannot say
/// whether history it never read leads there.
///
/// The walk reads in rounds of up to [`HOLDS_READ_ROUND`] parcels, so a reader can fetch a round
/// concurrently; `out_of_time` is asked before each round. A round is the breadth of history at
/// that depth, so a straight chain of parcels reads one at a time: no parent can be asked for
/// before its child is read. A reader answering for fewer parcels than a round holds stops the
/// walk as an unreadable parcel does.
///
/// No commit-graph is consulted. Building a missing generation walks a parcel's whole ancestry
/// and writes the result, and neither is bounded.
///
/// # Arguments
/// * `head`        - The pallet's current head, or `None` when there is no such pallet.
/// * `candidates`  - The parcel hashes to answer for.
/// * `max_steps`   - The most parcels to read.
/// * `out_of_time` - Asked before each round; `true` stops the walk.
/// * `parents_of`  - Reads a round of parcels: one result per hash, in the order given.
///
/// # Returns
/// One answer per distinct candidate.
pub fn holds_from(head: Option<&str>,
                  candidates: &[String],
                  max_steps: usize,
                  out_of_time: impl Fn() -> bool,
                  mut parents_of: impl FnMut(&[String]) -> Vec<Result<Vec<String>, String>>)
                  -> BTreeMap<String, HoldAnswer> {
    let mut answers: BTreeMap<String, HoldAnswer> =
        candidates.iter().map(|candidate| (candidate.clone(), HoldAnswer::Unknown)).collect();
    let mut pending: HashSet<String> = answers.keys().cloned().collect();

    let Some(head) = head else {
        answers.values_mut().for_each(|answer| *answer = HoldAnswer::NotHeld);
        return answers;
    };

    let mut queue: VecDeque<String> = VecDeque::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut steps = 0;

    // A parcel is reached, and so settles a candidate, when it is first queued.
    let mut reach = |hash: String, queue: &mut VecDeque<String>, answers: &mut BTreeMap<String, HoldAnswer>| {
        if visited.insert(hash.clone()) {
            if pending.remove(&hash) {
                answers.insert(hash.clone(), HoldAnswer::Held);
            }

            queue.push_back(hash);
        }

        pending.is_empty()
    };

    if reach(head.to_string(), &mut queue, &mut answers) {
        return answers;
    }

    let exhausted = loop {
        if queue.is_empty() {
            break true;
        }

        if steps == max_steps || out_of_time() {
            break false;
        }

        let round: Vec<String> = queue.drain(..queue.len().min(HOLDS_READ_ROUND).min(max_steps - steps)).collect();
        steps += round.len();

        let read = parents_of(&round);

        // A reader answering for fewer parcels than it was asked leaves history unread, and a
        // walk that went on would take it for exhausted.
        let mut unreadable = read.len() != round.len();

        for parents in read {
            match parents {
                Ok(parents) => {
                    for parent in parents {
                        if reach(parent, &mut queue, &mut answers) {
                            return answers;
                        }
                    }
                }
                Err(_) => unreadable = true,
            }
        }

        if unreadable {
            break false;
        }
    };

    if exhausted {
        for candidate in pending {
            answers.insert(candidate, HoldAnswer::NotHeld);
        }
    }

    answers
}

/// Find a merge base of two parcels: a best (highest-generation) common ancestor.
///
/// The fast path paints the ancestors of `a` and `b`, exploring the highest generation first
/// via the commit-graph ([`graph_utils`]). Because a parcel's generation strictly exceeds every
/// one of its ancestors', by the time a parcel is popped every flag that can reach it already
/// has — so the first parcel popped carrying *both* flags is a highest-generation common
/// ancestor (a lowest common ancestor of the two). The generation ordering also lets the search
/// stop there instead of walking either parcel's history to the roots.
///
/// As with [`is_ancestor`], the graph is only an accelerator: if a generation cannot be
/// computed (an ancestor object is not present locally yet), the query falls back to a plain
/// object walk that finds the closest common ancestor from `b`'s side.
///
/// # Arguments
/// * `a` - The first parcel hash.
/// * `b` - The second parcel hash.
///
/// # Returns
/// * `Ok(Some(String))` - The hash of a common ancestor.
/// * `Ok(None)`         - If the two parcels share no history.
/// * `Err(String)`      - If the histories needed to answer could not be read.
pub fn find_merge_base(a: &str, b: &str) -> Result<Option<String>, String> {
    match find_merge_base_via_graph(a, b) {
        Ok(base) => Ok(base),
        Err(_) => find_merge_base_via_walk(a, b),
    }
}

/// The generation-ordered merge-base search (see [`find_merge_base`]). Errors if the graph
/// cannot be built for either parcel.
fn find_merge_base_via_graph(a: &str, b: &str) -> Result<Option<String>, String> {
    if a == b {
        return Ok(Some(a.to_string()));
    }

    const FLAG_A: u8 = 1;
    const FLAG_B: u8 = 2;
    const FLAG_BOTH: u8 = FLAG_A | FLAG_B;

    let mut flags: HashMap<String, u8> = HashMap::new();
    let mut heap: BinaryHeap<(u32, String)> = BinaryHeap::new();
    let mut descended: HashSet<String> = HashSet::new();

    flags.insert(a.to_string(), FLAG_A);
    heap.push((graph_utils::generation(a)?, a.to_string()));
    *flags.entry(b.to_string()).or_insert(0) |= FLAG_B;
    heap.push((graph_utils::generation(b)?, b.to_string()));

    while let Some((_, hash)) = heap.pop() {
        let flag = flags[&hash];
        if flag == FLAG_BOTH {
            return Ok(Some(hash));
        }
        // Descend each parcel once; its flags are already complete by the time it is popped.
        if !descended.insert(hash.clone()) {
            continue;
        }

        for parent in graph_utils::node(&hash)?.parents {
            let entry = flags.entry(parent.clone()).or_insert(0);
            let before = *entry;
            *entry |= flag;
            if *entry != before {
                heap.push((graph_utils::generation(&parent)?, parent));
            }
        }
    }

    Ok(None)
}

/// The plain merge-base search: collect `a`'s ancestors, then walk `b`'s toward the roots and
/// return the first parcel common to both. The graph-free fallback for [`find_merge_base`].
fn find_merge_base_via_walk(a: &str, b: &str) -> Result<Option<String>, String> {
    let mut ancestors_of_a: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<String> = VecDeque::new();

    queue.push_back(a.to_string());
    while let Some(hash) = queue.pop_front() {
        if !ancestors_of_a.insert(hash.clone()) {
            continue;
        }
        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    let mut visited: HashSet<String> = HashSet::new();
    queue.push_back(b.to_string());
    while let Some(hash) = queue.pop_front() {
        if ancestors_of_a.contains(&hash) {
            return Ok(Some(hash));
        }
        if !visited.insert(hash.clone()) {
            continue;
        }
        for parent in object_utils::load_parcel(&hash)?.parents {
            queue.push_back(parent);
        }
    }

    Ok(None)
}

/// Merge two derived versions of a text file against their common base, line by line
/// (a diff3-style three-way merge). Chunks changed on only one side merge cleanly; chunks
/// changed differently on both sides become conflicts, marked in the output:
///
/// ```text
/// <<<<<<< <ours_label>
/// (our lines)
/// ||||||| base
/// (base lines)
/// =======
/// (their lines)
/// >>>>>>> <theirs_label>
/// ```
///
/// The comparison is exact (whitespace matters — this is a merge, not a display diff).
///
/// # Arguments
/// * `base`         - The common ancestor's content.
/// * `ours`         - Our version's content.
/// * `theirs`       - Their version's content.
/// * `ours_label`   - The label for our side in conflict markers.
/// * `theirs_label` - The label for their side in conflict markers.
///
/// # Returns
/// * `MergeResult` - The merged content and whether it contains conflicts.
pub fn merge_file_contents(base: &[u8],
                           ours: &[u8],
                           theirs: &[u8],
                           ours_label: &str,
                           theirs_label: &str) -> MergeResult {
    let base_lines = split_lines(base);
    let our_lines = split_lines(ours);
    let their_lines = split_lines(theirs);

    let too_large = base_lines.len() > MAX_MERGE_LINES
        || our_lines.len() > MAX_MERGE_LINES
        || their_lines.len() > MAX_MERGE_LINES;

    if too_large {
        // Fall back to a whole-file conflict: correctness over cleverness.
        return conflict_chunk(&our_lines, &base_lines, &their_lines, ours_label, theirs_label);
    }

    // For each base line: the index of the matching line in ours / theirs (from the
    // longest common subsequence), or None where the side diverges.
    let matches_ours = lcs::lcs_matches(&base_lines, &our_lines);
    let matches_theirs = lcs::lcs_matches(&base_lines, &their_lines);

    let mut content: Vec<u8> = Vec::new();
    let mut has_conflicts = false;

    let mut b = 0usize;
    let mut o = 0usize;
    let mut t = 0usize;

    while b < base_lines.len() || o < our_lines.len() || t < their_lines.len() {
        // A stable point: the current base line is matched by both sides at exactly the
        // current cursors.
        let is_stable = b < base_lines.len()
            && matches_ours[b] == Some(o)
            && matches_theirs[b] == Some(t);

        if is_stable {
            content.extend_from_slice(base_lines[b]);
            b += 1;
            o += 1;
            t += 1;
            continue;
        }

        // An unstable chunk: scan forward to the next base line matched by both sides.
        let mut next_b = b;
        let (next_o, next_t) = loop {
            match (next_b < base_lines.len()).then(|| (matches_ours[next_b], matches_theirs[next_b])) {
                Some((Some(no), Some(nt))) => break (no, nt),
                Some(_) => next_b += 1,
                None => break (our_lines.len(), their_lines.len()),
            }
        };

        let base_chunk = &base_lines[b..next_b];
        let our_chunk = &our_lines[o..next_o];
        let their_chunk = &their_lines[t..next_t];

        if our_chunk == base_chunk {
            extend_lines(&mut content, their_chunk);
        } else if their_chunk == base_chunk || our_chunk == their_chunk {
            extend_lines(&mut content, our_chunk);
        } else {
            let conflict = conflict_chunk(our_chunk, base_chunk, their_chunk, ours_label, theirs_label);
            content.extend(conflict.content);
            has_conflicts = true;
        }

        b = next_b;
        o = next_o;
        t = next_t;
    }

    MergeResult { content, has_conflicts }
}

/// Check whether content looks like text that can be merged line by line.
/// A NUL byte anywhere marks the content as binary.
///
/// # Arguments
/// * `content` - The content to check.
///
/// # Returns
/// * `true`  - If the content can be merged line by line.
/// * `false` - If the content should be treated as binary.
pub fn is_mergeable_text(content: &[u8]) -> bool {
    !content.contains(&0)
}

/// One action a consolidation has to perform to merge "theirs" into "ours".
pub enum MergeAction {
    /// Take their version of the file (ours is unchanged since the base).
    TakeTheirs {
        path: String,
        hash: String,
        item_type: crate::enums::dir_entry_type::DirEntryType,
        /// Whether the file does not exist in our tree (whatever the path resolves to on disk,
        /// an untracked file or a case alias of another entry, is then a collision, and the
        /// write creates the file exclusively).
        is_new: bool,
    },

    /// Remove the file (they removed it; ours is unchanged since the base).
    Delete { path: String },

    /// Both sides changed the file and the line merge succeeded: write the merged content.
    Merged {
        path: String,
        content: Vec<u8>,
        item_type: crate::enums::dir_entry_type::DirEntryType,
    },

    /// The file is in conflict. When `content` is set, it is written to the working
    /// directory (line-merge conflicts with markers, or their content for a
    /// delete/modify conflict); when it is `None`, our file is kept as it is.
    Conflict {
        path: String,
        content: Option<Vec<u8>>,
        /// The hash recorded on the (stale, conflict-state) inventory entry.
        entry_hash: String,
        item_type: crate::enums::dir_entry_type::DirEntryType,
    },

    /// An out-of-scope entry (a subtree, file or symlink this bay never materialized) that
    /// theirs changed one-sided, resolved **by hash alone** — no content is loaded. It never
    /// touches the working directory or the inventory; it is recorded in the out-of-scope
    /// skeleton and spliced into the merge parcel's root tree by the next stack's overlay.
    /// `resolution` is the adopted `(hash, type)`, or `None` when theirs deleted the entry.
    /// Only ever produced in a scoped (sparse) bay.
    ResolveOutOfScope {
        path: String,
        resolution: Option<(String, crate::enums::dir_entry_type::DirEntryType)>,
    },
}

/// One entry of the merge walk before its heavy work is run. The walk is a cheap tree
/// traversal that decides on the spot every action it can; a file both sides changed needs
/// a three-way line merge — the expensive part — so it records a [`MergeJob`] to run later,
/// in parallel. Resolving the pending list preserves walk order, so the final actions are
/// exactly what a single-threaded merge produced.
enum PendingAction {
    /// An action already decided during the walk.
    Ready(MergeAction),

    /// A file both sides changed: load its three sides and line-merge them (deferred so the
    /// merges fan out across the cores).
    Merge(MergeJob),
}

/// A deferred three-way line merge of one file both sides changed (see [`PendingAction`]).
/// The (expensive) blob loads and line diff are all done when the job is resolved, so the
/// walk that records it stays cheap.
struct MergeJob {
    path: String,

    /// The base blob hash, or `None` if the file was added on both sides.
    base_hash: Option<String>,
    /// The base kind, or `None` if the file was added on both sides.
    base_type: Option<crate::enums::dir_entry_type::DirEntryType>,
    our_hash: String,
    their_hash: String,
    our_type: crate::enums::dir_entry_type::DirEntryType,
    their_type: crate::enums::dir_entry_type::DirEntryType,
}

/// Compute the actions that merge the `theirs` tree into the `ours` tree, given their
/// common `base` tree. Subtrees that are equal between ours and theirs — or unchanged on
/// their side — are skipped entirely.
///
/// The three-way line merges of files both sides changed (the expensive part) fan out
/// across the cores; the result is identical to a single-threaded merge, in the same order.
///
/// In a scoped (sparse) bay, out-of-scope entries (subtrees, files and symlinks the bay never
/// materialized) are resolved by hash alone: a one-sided change is adopted into a
/// [`MergeAction::ResolveOutOfScope`] without loading its content; a two-sided out-of-scope
/// conflict refuses with `out_of_scope_conflict` before any object is dereferenced. A full
/// (unscoped) `scope` classifies everything `InScope`, so the walk is exactly today's behavior.
///
/// # Arguments
/// * `base_hash`    - The root tree hash of the merge base.
/// * `ours_hash`    - The root tree hash of our head.
/// * `theirs_hash`  - The root tree hash of their head.
/// * `ours_label`   - The conflict-marker label for our side (the current pallet name).
/// * `theirs_label` - The conflict-marker label for their side.
/// * `scope`        - The bay's materialization scope (full = today's behavior everywhere).
///
/// # Returns
/// * `Ok(Vec<MergeAction>)` - The actions to perform.
/// * `Err(String)`          - A load failed, or a genuine out-of-scope conflict / type flip refuses.
pub fn compute_merge_actions(base_hash: &str,
                             ours_hash: &str,
                             theirs_hash: &str,
                             ours_label: &str,
                             theirs_label: &str,
                             scope: &MaterializationScope) -> Result<Vec<MergeAction>, String> {
    let base_tree = object_utils::load_tree(base_hash)?;
    let ours_tree = object_utils::load_tree(ours_hash)?;
    let theirs_tree = object_utils::load_tree(theirs_hash)?;

    // Phase 1 — walk the three trees and decide every cheap action; a file both sides
    // changed is recorded as a deferred merge (no blob load, no line diff yet).
    let mut pending: Vec<PendingAction> = Vec::new();

    merge_directory(
        Some(&base_tree),
        Some(&ours_tree),
        Some(&theirs_tree),
        "",
        scope,
        &mut pending
    )?;

    // Phase 2 — run the deferred three-way merges across the cores and reassemble the
    // actions in walk order.
    resolve_pending(pending, ours_label, theirs_label)
}

/// The number of deferred merges below which running them on the calling thread is cheaper
/// than the threads that would share them.
const PARALLEL_MERGE_THRESHOLD: usize = 8;

/// Resolve the pending merge walk into the final action list: cheap actions pass straight
/// through, and the deferred three-way merges — the expensive line diffs — run (fanned
/// across the cores once there are enough of them), their results reassembled in the
/// original walk order.
fn resolve_pending(pending: Vec<PendingAction>,
                   ours_label: &str,
                   theirs_label: &str) -> Result<Vec<MergeAction>, String> {
    let total = pending.len();

    let mut ready: Vec<(usize, MergeAction)> = Vec::new();
    let mut jobs: Vec<(usize, MergeJob)> = Vec::new();

    for (index, entry) in pending.into_iter().enumerate() {
        match entry {
            PendingAction::Ready(action) => ready.push((index, action)),
            PendingAction::Merge(job) => jobs.push((index, job)),
        }
    }

    let resolved: Vec<(usize, Result<MergeAction, String>)> =
        if jobs.len() < PARALLEL_MERGE_THRESHOLD {
            jobs.iter()
                .map(|(index, job)| (*index, resolve_merge_job(job, ours_label, theirs_label)))
                .collect()
        } else {
            resolve_merge_jobs_parallel(&jobs, ours_label, theirs_label)
        };

    // Slot every action back into its walk position (each index is filled exactly once).
    let mut slots: Vec<Option<MergeAction>> = std::iter::repeat_with(|| None).take(total).collect();

    for (index, action) in ready {
        slots[index] = Some(action);
    }
    for (index, action) in resolved {
        slots[index] = Some(action?);
    }

    Ok(slots.into_iter().map(|slot| slot.expect("every walk slot is filled exactly once")).collect())
}

/// Run the deferred three-way merges across the cores, returning `(walk index, result)` so
/// the caller can slot each back into walk order.
fn resolve_merge_jobs_parallel(jobs: &[(usize, MergeJob)],
                               ours_label: &str,
                               theirs_label: &str) -> Vec<(usize, Result<MergeAction, String>)> {
    // See `fanout_utils::fanout_map` for the fan-out idiom (chunking, worker count, and the
    // storage-scope re-entry every worker needs).
    fanout_utils::fanout_map(jobs, |(index, job)| {
        (*index, resolve_merge_job(job, ours_label, theirs_label))
    })
}

/// The kind a three-way merge of two plain files records. The executable bit merges like
/// content: a side that changed it from the base wins, so a `chmod -x` on one side survives an
/// edit on the other. With no base kind to compare (an add/add, or a base that was no plain
/// file), the bit wins from either side.
fn merged_plain_kind(base: Option<DirEntryType>, ours: DirEntryType, theirs: DirEntryType) -> DirEntryType {
    match base.map(|kind| kind.on_disk_kind()) {
        Some(base) if ours == base => theirs,
        Some(base) if theirs == base => ours,
        _ if ours == DirEntryType::Executable || theirs == DirEntryType::Executable => DirEntryType::Executable,
        _ => DirEntryType::Normal,
    }
}

/// Load one deferred file's three sides and three-way merge them — the body of the parallel
/// phase. It reads blobs through the shared, thread-safe object caches and the line merge is
/// pure CPU, so it is safe to run on many threads at once.
fn resolve_merge_job(job: &MergeJob,
                     ours_label: &str,
                     theirs_label: &str) -> Result<MergeAction, String> {
    let base_content = match &job.base_hash {
        Some(hash) => object_utils::load_blob(hash)?.content,
        None => Vec::new(),
    };
    let our_content = object_utils::load_blob(&job.our_hash)?.content;
    let their_content = object_utils::load_blob(&job.their_hash)?.content;

    let all_text = is_mergeable_text(&base_content)
        && is_mergeable_text(&our_content)
        && is_mergeable_text(&their_content);

    if !all_text {
        // Binary contents are not line-mergeable; keep ours, in conflict. Its kind too: nothing
        // is written, so the entry must record the file as it stands, bit included.
        return Ok(MergeAction::Conflict {
            path: job.path.clone(),
            content: None,
            entry_hash: job.our_hash.clone(),
            item_type: job.our_type,
        });
    }

    let merged_type = merged_plain_kind(job.base_type, job.our_type, job.their_type);

    let result = merge_file_contents(
        &base_content,
        &our_content,
        &their_content,
        ours_label,
        theirs_label
    );

    Ok(if result.has_conflicts {
        MergeAction::Conflict {
            path: job.path.clone(),
            content: Some(result.content),
            entry_hash: job.our_hash.clone(),
            item_type: merged_type,
        }
    } else {
        MergeAction::Merged {
            path: job.path.clone(),
            content: result.content,
            item_type: merged_type,
        }
    })
}

/// Merge one directory level of the three trees (recursively).
///
/// In a scoped bay the `scope` classifier gates every entry **before** any content is loaded:
/// an out-of-scope entry (a subtree, file or symlink) is resolved by hash into a
/// [`MergeAction::ResolveOutOfScope`] (one-sided) or refuses (`out_of_scope_conflict`, two-sided),
/// never dereferencing the absent object; a file where the scope needs a directory refuses
/// (`scope_path_type_changed`); everything in scope merges exactly as today. A full scope
/// classifies every path `InScope`, so a full-bay merge is unchanged.
fn merge_directory(base: Option<&crate::model::tree_item::TreeItem>,
                   ours: Option<&crate::model::tree_item::TreeItem>,
                   theirs: Option<&crate::model::tree_item::TreeItem>,
                   key: &str,
                   scope: &MaterializationScope,
                   pending: &mut Vec<PendingAction>) -> Result<(), String> {
    use std::collections::BTreeMap;
    use crate::enums::dir_entry_type::DirEntryType;
    use crate::model::tree_item::TreeItem;

    let collect_files = |tree: Option<&TreeItem>| -> BTreeMap<String, (String, DirEntryType)> {
        tree.map(|t| t.get_files()
                .map(|(name, item)| (name.clone(), (item.hash.clone(), item.item_type)))
                .collect())
            .unwrap_or_default()
    };

    let base_files = collect_files(base);
    let our_files = collect_files(ours);
    let their_files = collect_files(theirs);

    let mut names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    names.extend(base_files.keys());
    names.extend(our_files.keys());
    names.extend(their_files.keys());

    for name in names {
        let b = base_files.get(name);
        let o = our_files.get(name);
        let t = their_files.get(name);

        let path = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

        // A file where the scope needs a directory (an in-scope prefix itself, or a spine
        // ancestor of one) is a directory→file flip the sparse scope cannot reason about —
        // refuse rather than guess, before touching content. Never fires in a full bay
        // or a valid scoped bay (a spine path is always a directory there).
        if scope.requires_directory(&path) {
            return Err(scope_utils::type_changed_refusal(&path).into());
        }

        // Nothing to do when both sides agree, or their side is unchanged since the base.
        if o == t || t == b {
            continue;
        }

        let out_of_scope = scope.classify(&path) == ScopeClass::OutOfScope;

        // Our side is unchanged since the base: take their side.
        if o == b {
            match (out_of_scope, t) {
                // Out of scope: adopt theirs by hash into the skeleton — no blob load,
                // never the working directory. `None` = theirs deleted the entry.
                (true, Some((hash, item_type))) =>
                    pending.push(PendingAction::Ready(MergeAction::ResolveOutOfScope {
                        path,
                        resolution: Some((hash.clone(), *item_type)),
                    })),
                (true, None) =>
                    pending.push(PendingAction::Ready(MergeAction::ResolveOutOfScope {
                        path,
                        resolution: None,
                    })),
                // In scope: today's behavior — the blob is present and later applied to the
                // working directory by `apply_merge_action`.
                (false, Some((hash, item_type))) =>
                    pending.push(PendingAction::Ready(MergeAction::TakeTheirs {
                        path,
                        hash: hash.clone(),
                        item_type: *item_type,
                        is_new: o.is_none(),
                    })),
                (false, None) => pending.push(PendingAction::Ready(MergeAction::Delete { path })),
            }
            continue;
        }

        // One side changed only the kind (a chmod: the base's content), the other the content:
        // the content is the other side's, and the bit merges like content. Resolved by hash,
        // so a chunked, binary or out-of-scope file merges as cleanly as a text one.
        if let (Some((base_hash, base_type)), Some((our_hash, our_type)), Some((their_hash, their_type))) = (b, o, t) {
            let plain = |kind: &DirEntryType| matches!(kind.on_disk_kind(), DirEntryType::Normal | DirEntryType::Executable);

            if plain(base_type) && plain(our_type) && plain(their_type) && (our_hash == base_hash || their_hash == base_hash) {
                let (hash, storage) = if our_hash == base_hash { (their_hash, their_type) } else { (our_hash, our_type) };
                let kind = merged_plain_kind(Some(*base_type), our_type.on_disk_kind(), their_type.on_disk_kind());
                let item_type = if storage.is_chunked() { kind.to_chunked() } else { kind };

                if (hash, item_type) != (our_hash, *our_type) {
                    pending.push(PendingAction::Ready(if out_of_scope {
                        MergeAction::ResolveOutOfScope { path, resolution: Some((hash.clone(), item_type)) }
                    } else {
                        MergeAction::TakeTheirs { path, hash: hash.clone(), item_type, is_new: false }
                    }));
                }
                continue;
            }
        }

        // Both sides changed the file (relative to the base) in different ways. Out of scope,
        // that is a genuine conflict the bay has no content to reconcile — refuse before any
        // blob load, so no absent out-of-scope object is ever dereferenced.
        if out_of_scope {
            return Err(scope_utils::out_of_scope_conflict_refusal(&path).into());
        }

        // Both sides changed the file (relative to the base) in different ways.
        match (o, t) {
            // Delete/modify: they changed it, we deleted it. Their version is put back
            // in the working directory, in conflict. A chunked (binary) file is never loaded
            // whole into memory here — it carries no inline content; the apply path
            // re-materializes it from its recipe hash (which `entry_hash` holds).
            (None, Some((their_hash, their_type))) => {
                let content = if their_type.is_chunked() {
                    None
                } else {
                    Some(object_utils::load_blob(their_hash)?.content)
                };

                pending.push(PendingAction::Ready(MergeAction::Conflict {
                    path,
                    content,
                    entry_hash: their_hash.clone(),
                    item_type: *their_type,
                }));
            }

            // Modify/delete: we changed it, they deleted it. Our file is kept, in conflict.
            (Some((our_hash, our_type)), None) => {
                pending.push(PendingAction::Ready(MergeAction::Conflict {
                    path,
                    content: None,
                    entry_hash: our_hash.clone(),
                    item_type: *our_type,
                }));
            }

            (Some((our_hash, our_type)), Some((their_hash, their_type))) => {
                let both_plain_files = (*our_type == DirEntryType::Normal || *our_type == DirEntryType::Executable)
                    && (*their_type == DirEntryType::Normal || *their_type == DirEntryType::Executable);

                if !both_plain_files {
                    // Type conflicts (symlink vs file, …) are not line-mergeable; keep ours.
                    pending.push(PendingAction::Ready(MergeAction::Conflict {
                        path,
                        content: None,
                        entry_hash: our_hash.clone(),
                        item_type: *our_type,
                    }));
                    continue;
                }

                // Both sides changed a plain file: defer the three-way line merge (the blob
                // loads and the diff happen when the job is resolved, in parallel).
                pending.push(PendingAction::Merge(MergeJob {
                    path,
                    base_hash: b.map(|(base_hash, _)| base_hash.clone()),
                    base_type: b.map(|(_, base_type)| *base_type),
                    our_hash: our_hash.clone(),
                    their_hash: their_hash.clone(),
                    our_type: *our_type,
                    their_type: *their_type,
                }));
            }

            (None, None) => unreachable!("handled by the o == t case"),
        }
    }

    // Recurse into subtrees.
    let collect_subtrees = |tree: Option<&TreeItem>| -> BTreeMap<String, String> {
        tree.map(|t| t.get_subtrees()
                .map(|(name, item)| (name.clone(), item.hash.clone()))
                .collect())
            .unwrap_or_default()
    };

    let base_subtrees = collect_subtrees(base);
    let our_subtrees = collect_subtrees(ours);
    let their_subtrees = collect_subtrees(theirs);

    let mut subtree_names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    subtree_names.extend(base_subtrees.keys());
    subtree_names.extend(our_subtrees.keys());
    subtree_names.extend(their_subtrees.keys());

    for name in subtree_names {
        let b = base_subtrees.get(name);
        let o = our_subtrees.get(name);
        let t = their_subtrees.get(name);

        // Identical on both sides, or unchanged on their side: nothing to merge below.
        if o == t || t == b {
            continue;
        }

        let child_key = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

        // Out-of-scope subtree: resolve by hash without loading it. A one-sided change is
        // adopted from theirs into the skeleton; a two-sided one is a genuine conflict. A subtree
        // is a directory, so no `scope_path_type_changed` check applies here.
        if scope.classify(&child_key) == ScopeClass::OutOfScope {
            if o == b {
                // Our side is unchanged since the base: adopt theirs by hash. `None` = theirs
                // deleted the subtree.
                pending.push(PendingAction::Ready(MergeAction::ResolveOutOfScope {
                    path: child_key,
                    resolution: t.map(|hash| (hash.clone(), DirEntryType::Tree)),
                }));
            } else {
                // Both sides changed it (o != b, t != b, o != t) — refuse before any load.
                return Err(scope_utils::out_of_scope_conflict_refusal(&child_key).into());
            }

            continue;
        }

        // In scope, or on the spine to an in-scope leaf: descend exactly as today. The subtree
        // objects at a spine/in-scope path are always present (never sealed), so the loads are
        // safe even in a sparse store; the recursion re-classifies each child by its own path.
        let load = |hash: Option<&String>| -> Result<Option<TreeItem>, String> {
            match hash {
                Some(hash) => object_utils::load_tree(hash).map(Some),
                None => Ok(None),
            }
        };

        let base_loaded = load(b)?;
        let ours_loaded = load(o)?;
        let theirs_loaded = load(t)?;

        merge_directory(
            base_loaded.as_ref(),
            ours_loaded.as_ref(),
            theirs_loaded.as_ref(),
            &child_key,
            scope,
            pending
        )?;
    }

    Ok(())
}

/// Build a conflict chunk with markers.
fn conflict_chunk(ours: &[&[u8]],
                  base: &[&[u8]],
                  theirs: &[&[u8]],
                  ours_label: &str,
                  theirs_label: &str) -> MergeResult {
    let mut content: Vec<u8> = Vec::new();

    content.extend(format!("<<<<<<< {}\n", ours_label).as_bytes());
    extend_lines(&mut content, ours);
    content.extend(b"||||||| base\n");
    extend_lines(&mut content, base);
    content.extend(b"=======\n");
    extend_lines(&mut content, theirs);
    content.extend(format!(">>>>>>> {}\n", theirs_label).as_bytes());

    MergeResult { content, has_conflicts: true }
}

/// Append the given lines to the content.
fn extend_lines(content: &mut Vec<u8>, lines: &[&[u8]]) {
    for line in lines {
        content.extend_from_slice(line);
    }
}

/// Split content into lines, each including its trailing new line byte (the last line may
/// lack one). Exact bytes — no whitespace normalization.
fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;

    for (index, byte) in content.iter().enumerate() {
        if *byte == b'\n' {
            lines.push(&content[start..=index]);
            start = index + 1;
        }
    }

    if start < content.len() {
        lines.push(&content[start..]);
    }

    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_executable_bit_merges_like_content() {
        use DirEntryType::{Executable, Normal, NormalChunked, SymbolicLink};

        assert!(merged_plain_kind(Some(Executable), Normal, Executable) == Normal, "our chmod -x survives");
        assert!(merged_plain_kind(Some(Executable), Executable, Normal) == Normal, "their chmod -x survives");
        assert!(merged_plain_kind(Some(Normal), Normal, Executable) == Executable, "their chmod +x survives");
        assert!(merged_plain_kind(Some(Normal), Executable, Executable) == Executable);
        assert!(merged_plain_kind(Some(Normal), Normal, Normal) == Normal);
        assert!(merged_plain_kind(Some(NormalChunked), Normal, Executable) == Executable,
                "a chunked base compares by its on-disk kind");
        assert!(merged_plain_kind(None, Normal, Executable) == Executable, "an add/add keeps the bit");
        assert!(merged_plain_kind(Some(SymbolicLink), Normal, Normal) == Normal);
    }

    /// `holds_from` over an in-memory history: each entry is a parcel and its parents.
    fn holds_over(history: &[(&str, &[&str])],
                  head: Option<&str>,
                  candidates: &[&str],
                  max_steps: usize)
                  -> (BTreeMap<String, HoldAnswer>, usize) {
        let parents: std::collections::HashMap<&str, Vec<String>> = history.iter()
            .map(|(hash, parents)| (*hash, parents.iter().map(|parent| parent.to_string()).collect()))
            .collect();
        let candidates: Vec<String> = candidates.iter().map(|candidate| candidate.to_string()).collect();
        let mut reads = 0;
        let answers = holds_from(head, &candidates, max_steps, || false, |round| {
            reads += round.len();
            round.iter().map(|hash| parents.get(hash.as_str()).cloned().ok_or_else(|| format!("no parcel {}", hash))).collect()
        });

        (answers, reads)
    }

    #[test]
    fn holds_answers_held_for_the_head_and_its_ancestors_and_not_held_past_the_root() {
        // a <- b <- c (head); x is unrelated.
        let history: &[(&str, &[&str])] = &[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("x", &[])];
        let (answers, _) = holds_over(history, Some("c"), &["c", "a", "x"], MAX_HOLDS_WALK);

        assert_eq!(answers["c"], HoldAnswer::Held, "the head itself is held");
        assert_eq!(answers["a"], HoldAnswer::Held, "the root ancestor is held");
        assert_eq!(answers["x"], HoldAnswer::NotHeld, "the whole history was read without reaching x");
    }

    #[test]
    fn holds_answers_not_held_for_every_candidate_of_a_missing_pallet() {
        let (answers, reads) = holds_over(&[], None, &["a", "b"], MAX_HOLDS_WALK);

        assert!(answers.values().all(|answer| *answer == HoldAnswer::NotHeld), "{:?}", answers);
        assert_eq!(reads, 0);
    }

    #[test]
    fn holds_reads_a_shared_ancestor_once_and_stops_when_every_candidate_is_found() {
        // r <- l, r <- m, (l, m) <- h: the diamond's root is reachable twice.
        let history: &[(&str, &[&str])] = &[("r", &[]), ("l", &["r"]), ("m", &["r"]), ("h", &["l", "m"])];
        let (answers, reads) = holds_over(history, Some("h"), &["r"], MAX_HOLDS_WALK);

        assert_eq!(answers["r"], HoldAnswer::Held);
        assert_eq!(reads, 3, "h, l and m are read; r is found without reading its parents");
    }

    #[test]
    fn holds_answers_unknown_when_the_walk_bound_is_reached() {
        // a <- b <- c <- d (head): reaching a needs three reads.
        let history: &[(&str, &[&str])] = &[("a", &[]), ("b", &["a"]), ("c", &["b"]), ("d", &["c"])];

        let (answers, reads) = holds_over(history, Some("d"), &["a", "c", "x"], 2);
        assert_eq!(answers["c"], HoldAnswer::Held, "found inside the bound");
        assert_eq!(answers["a"], HoldAnswer::Unknown, "beyond the bound");
        assert_eq!(answers["x"], HoldAnswer::Unknown, "history was left unread, so x is not ruled out");
        assert_eq!(reads, 2);

        let (answers, _) = holds_over(history, Some("d"), &["a", "x"], 3);
        assert_eq!(answers["a"], HoldAnswer::Held, "three reads reach a");
        assert_eq!(answers["x"], HoldAnswer::Unknown, "a's own parents are still unread");

        let (answers, _) = holds_over(history, Some("d"), &["x"], 4);
        assert_eq!(answers["x"], HoldAnswer::NotHeld, "four reads exhaust the history");
    }

    #[test]
    fn holds_reads_a_wide_history_in_rounds_no_larger_than_the_round_cap() {
        // h merges 100 roots at once.
        let roots: Vec<String> = (0..100).map(|n| format!("p{}", n)).collect();
        let mut rounds: Vec<usize> = Vec::new();
        let answers = holds_from(Some("h"), &["x".to_string()], MAX_HOLDS_WALK, || false, |round| {
            rounds.push(round.len());
            round.iter().map(|hash| Ok(if hash == "h" { roots.clone() } else { Vec::new() })).collect()
        });

        assert_eq!(answers["x"], HoldAnswer::NotHeld);
        assert_eq!(rounds, vec![1, HOLDS_READ_ROUND, 100 - HOLDS_READ_ROUND]);
    }

    #[test]
    fn holds_answers_unknown_when_the_reader_answers_for_fewer_parcels_than_asked() {
        // h's parent p is "read" by a reader that drops it: its history is not exhausted.
        let answers = holds_from(Some("h"), &["x".to_string()], MAX_HOLDS_WALK, || false, |round| {
            if round == ["h"] { vec![Ok(vec!["p".to_string()])] } else { Vec::new() }
        });

        assert_eq!(answers["x"], HoldAnswer::Unknown);
    }

    #[test]
    fn holds_refuses_more_candidates_than_one_request_may_name() {
        let candidates: Vec<String> = (0..=MAX_HOLDS_CANDIDATES).map(|n| format!("{:064x}", n)).collect();
        let error = holds(Some("h"), &candidates, None, |_| panic!("no walk for a refused request")).unwrap_err();
        assert!(error.contains(&MAX_HOLDS_CANDIDATES.to_string()), "{}", error);

        assert!(holds(None, &candidates[..MAX_HOLDS_CANDIDATES], None, |_| Vec::new()).is_ok());
    }

    #[test]
    fn holds_answers_unknown_when_a_parcel_cannot_be_read_or_time_runs_out() {
        // b's parent a is missing from the store.
        let history: &[(&str, &[&str])] = &[("b", &["a"]), ("c", &["b"])];
        let (answers, _) = holds_over(history, Some("c"), &["b", "x"], MAX_HOLDS_WALK);
        assert_eq!(answers["b"], HoldAnswer::Held);
        assert_eq!(answers["x"], HoldAnswer::Unknown, "an unreadable parcel may lead to x");

        let candidates = vec!["c".to_string(), "x".to_string()];
        let answers = holds_from(Some("c"), &candidates, MAX_HOLDS_WALK, || true,
                                 |round| round.iter().map(|_| Ok(Vec::new())).collect());
        assert_eq!(answers["c"], HoldAnswer::Held, "the head needs no read");
        assert_eq!(answers["x"], HoldAnswer::Unknown);
    }

    /// A micro-benchmark (not run by default) contrasting the generation-number fast path with
    /// the plain object walk on a deep shared history with a recent fork — the realistic shape
    /// of a merge base or a divergence check. Run with:
    ///
    /// ```text
    /// cargo test -p forklift-core merge_base_bench -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore]
    fn merge_base_bench_generations_beat_the_plain_walk() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::globals::StorageRootScope;
        use crate::model::parcel::Parcel;
        use std::time::Instant;

        let temp = std::env::temp_dir().join(format!("forklift-mergebase-bench-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        // Ancestry never reads the tree, so a single dummy tree hash is enough for every parcel.
        // A per-parcel description keeps otherwise-identical parcels (the two tips) distinct —
        // content addressing would collapse them to one hash otherwise.
        let dummy_tree = "0".repeat(64);
        let store = |parents: Vec<String>, tag: &str| -> String {
            let parcel = Parcel {
                tree_hash: dummy_tree.clone(),
                parents,
                actions: vec![],
                description: Some(tag.to_string()),
            };
            let mut object = LooseObjectBuilder::build_parcel(&parcel);
            let hash = object.hash.clone();
            object.store().unwrap();
            hash
        };

        // A deep shared trunk, then two distinct tips forking off its newest parcel.
        const DEPTH: usize = 50_000;
        let mut prev: Vec<String> = vec![];
        let mut fork_point = String::new();
        for i in 0..DEPTH {
            let hash = store(prev.clone(), &format!("trunk {i}"));
            fork_point = hash.clone();
            prev = vec![hash];
        }
        let tip_a = store(vec![fork_point.clone()], "tip A");
        let tip_b = store(vec![fork_point.clone()], "tip B");

        let built = graph_utils::build_from_heads(&[tip_a.clone(), tip_b.clone()]).unwrap();
        assert!(built >= DEPTH, "the graph must cover the trunk");

        // find_merge_base: the plain walk collects the whole trunk; the graph stops at the fork.
        let t = Instant::now();
        let walk_base = find_merge_base_via_walk(&tip_a, &tip_b).unwrap();
        let walk_base_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let graph_base = find_merge_base_via_graph(&tip_a, &tip_b).unwrap();
        let graph_base_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(walk_base, Some(fork_point.clone()));
        assert_eq!(graph_base, Some(fork_point.clone()));

        // is_ancestor for a *divergence* check (the common consolidate/lift/haul case): tip_b is
        // not an ancestor of tip_a. The plain walk scans all of tip_a's trunk to say "no"; the
        // graph prunes on the first parcel (tip_a is not above tip_b's generation).
        let t = Instant::now();
        let walk_anc = is_ancestor_via_walk(&tip_b, &tip_a).unwrap();
        let walk_anc_ms = t.elapsed().as_secs_f64() * 1000.0;
        let t = Instant::now();
        let graph_anc = is_ancestor_via_graph(&tip_b, &tip_a).unwrap();
        let graph_anc_ms = t.elapsed().as_secs_f64() * 1000.0;
        assert!(!walk_anc && !graph_anc, "the tips diverged");

        println!("\n=== ancestry on a {DEPTH}-deep shared history (recent fork) ===");
        println!("find_merge_base : walk {walk_base_ms:8.2} ms  graph {graph_base_ms:8.3} ms  ({:.0}x)", walk_base_ms / graph_base_ms.max(0.0001));
        println!("is_ancestor(div): walk {walk_anc_ms:8.2} ms  graph {graph_anc_ms:8.3} ms  ({:.0}x)", walk_anc_ms / graph_anc_ms.max(0.0001));

        std::fs::remove_dir_all(&temp).ok();
    }

    fn merge(base: &str, ours: &str, theirs: &str) -> (String, bool) {
        let result = merge_file_contents(
            base.as_bytes(),
            ours.as_bytes(),
            theirs.as_bytes(),
            "ours",
            "theirs"
        );

        (String::from_utf8(result.content).unwrap(), result.has_conflicts)
    }

    #[test]
    fn non_overlapping_changes_merge_cleanly() {
        let base = "one\ntwo\nthree\nfour\nfive\n";
        let ours = "ONE\ntwo\nthree\nfour\nfive\n";
        let theirs = "one\ntwo\nthree\nfour\nFIVE\n";

        let (merged, conflicts) = merge(base, ours, theirs);
        assert!(!conflicts);
        assert_eq!(merged, "ONE\ntwo\nthree\nfour\nFIVE\n");
    }

    #[test]
    fn identical_changes_merge_cleanly() {
        let base = "one\ntwo\n";
        let both = "one\nTWO\n";

        let (merged, conflicts) = merge(base, both, both);
        assert!(!conflicts);
        assert_eq!(merged, both);
    }

    #[test]
    fn additions_on_one_side_are_taken() {
        let base = "one\ntwo\n";
        let ours = "one\ntwo\nthree\n";
        let theirs = "zero\none\ntwo\n";

        let (merged, conflicts) = merge(base, ours, theirs);
        assert!(!conflicts);
        assert_eq!(merged, "zero\none\ntwo\nthree\n");
    }

    #[test]
    fn competing_changes_conflict_with_markers() {
        let base = "one\ntwo\nthree\n";
        let ours = "one\nOURS\nthree\n";
        let theirs = "one\nTHEIRS\nthree\n";

        let (merged, conflicts) = merge(base, ours, theirs);
        assert!(conflicts);
        assert!(merged.contains("<<<<<<< ours\nOURS\n"));
        assert!(merged.contains("||||||| base\ntwo\n"));
        assert!(merged.contains("=======\nTHEIRS\n"));
        assert!(merged.contains(">>>>>>> theirs"));
        assert!(merged.starts_with("one\n"));
        assert!(merged.ends_with("three\n"));
    }

    #[test]
    fn whitespace_changes_are_real_changes() {
        // The display diff forgives whitespace-only line changes; the merge must not.
        let base = "line\n";
        let ours = "line \n";
        let theirs = "line\n";

        let (merged, conflicts) = merge(base, ours, theirs);
        assert!(!conflicts);
        assert_eq!(merged, "line \n");
    }

    #[test]
    fn files_without_trailing_newline_survive() {
        let base = "one\ntwo";
        let ours = "one\ntwo";
        let theirs = "one\ntwo\nthree";

        let (merged, conflicts) = merge(base, ours, theirs);
        assert!(!conflicts);
        assert_eq!(merged, "one\ntwo\nthree");
    }

    #[test]
    fn binary_content_is_not_mergeable() {
        assert!(is_mergeable_text(b"plain text\n"));
        assert!(!is_mergeable_text(b"bin\0ary"));
    }

    /// The review's BLOCKER, pinned directly: at an out-of-scope directory↔file flip, the merge
    /// walk emits **two** `ResolveOutOfScope` actions for the same path — a "set" from whichever
    /// loop (file or subtree) matches theirs' real, post-flip entry, and a "delete" from the
    /// other loop, which sees no entry of its own kind there. The file loop always runs before
    /// the subtree loop at a given level, so which action is emitted *second* — and would
    /// clobber the other under plain last-write-wins insertion — flips with the direction of the
    /// change. `OutOfScopeSkeleton::from_actions` must keep the "set" either way.
    ///
    /// This is a direct, CLI-independent proof of the exact defect and fix: it exercises
    /// `compute_merge_actions` + `from_actions` in isolation, with no materialization, no
    /// `apply_merge_action`, and no `stack` overlay involved.
    #[test]
    fn the_out_of_scope_skeleton_keeps_a_set_over_a_delete_regardless_of_flip_direction() {
        use crate::builder::object::loose_object_builder::LooseObjectBuilder;
        use crate::enums::dir_entry_type::DirEntryType::{Normal, Tree};
        use crate::globals::StorageRootScope;
        use crate::model::blob::Blob;
        use crate::model::tree_item::TreeItem;

        let temp = std::env::temp_dir()
            .join(format!("forklift-skeleton-flip-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(&temp).unwrap();
        let _scope_guard = StorageRootScope::enter(&temp);

        let store_blob = |content: &str| -> String {
            let mut object = LooseObjectBuilder::build_blob(&Blob { content: content.as_bytes().to_vec() });
            object.store().unwrap();
            object.hash
        };

        let store_tree = |entries: &[(&str, &str, crate::enums::dir_entry_type::DirEntryType)]| -> String {
            let mut tree = TreeItem::new(String::new(), String::new(), Tree);
            for (name, hash, item_type) in entries {
                tree.add_child(TreeItem::new(name.to_string(), hash.to_string(), *item_type));
            }
            let mut object = LooseObjectBuilder::build_tree(&tree);
            object.store().unwrap();
            object.hash
        };

        // "src/web" is out of scope throughout (only "src/api" is in scope); "src/api" never
        // needs to exist in these trees at all — the classifier is a pure path-prefix test.
        let scope = MaterializationScope::from_prefixes(["src/api"]);

        // Direction A: base has "src/web" as a DIRECTORY; theirs replaces it with a FILE (ours
        // unchanged). The file loop's "set" is emitted BEFORE the subtree loop's "delete" —
        // exactly the order a plain last-write-wins insert gets wrong.
        {
            let inner_content = store_blob("web v1");
            let web_dir = store_tree(&[("w.txt", &inner_content, Normal)]);
            let web_file = store_blob("web is a file now");

            let src_base = store_tree(&[("web", &web_dir, Tree)]);
            let root_base = store_tree(&[("src", &src_base, Tree)]);
            let root_ours = root_base.clone(); // ours == base: unchanged.

            let src_theirs = store_tree(&[("web", &web_file, Normal)]);
            let root_theirs = store_tree(&[("src", &src_theirs, Tree)]);

            let actions = compute_merge_actions(&root_base, &root_ours, &root_theirs, "ours", "theirs", &scope)
                .expect("a one-sided out-of-scope flip must resolve, not error");
            let skeleton = OutOfScopeSkeleton::from_actions(&actions);

            assert_eq!(
                skeleton.entries().get("src/web"),
                Some(&Some((web_file, Normal))),
                "directory-to-file: the skeleton must keep the 'set' (theirs' file), not the \
                'delete' the subtree loop emits right after it"
            );
        }

        // Direction B (the mirror): base has "src/web" as a FILE; theirs replaces it with a
        // DIRECTORY. The file loop's "delete" is emitted BEFORE the subtree loop's "set" — the
        // direction that happened to already work under plain last-write-wins, now guaranteed.
        {
            let file_content = store_blob("web is a file v1");
            let inner_content = store_blob("inner v1");
            let web_dir = store_tree(&[("inner.txt", &inner_content, Normal)]);

            let src_base = store_tree(&[("web", &file_content, Normal)]);
            let root_base = store_tree(&[("src", &src_base, Tree)]);
            let root_ours = root_base.clone();

            let src_theirs = store_tree(&[("web", &web_dir, Tree)]);
            let root_theirs = store_tree(&[("src", &src_theirs, Tree)]);

            let actions = compute_merge_actions(&root_base, &root_ours, &root_theirs, "ours", "theirs", &scope)
                .expect("a one-sided out-of-scope flip must resolve, not error");
            let skeleton = OutOfScopeSkeleton::from_actions(&actions);

            assert_eq!(
                skeleton.entries().get("src/web"),
                Some(&Some((web_dir, Tree))),
                "file-to-directory: the skeleton must keep the 'set' (theirs' directory)"
            );
        }

        std::fs::remove_dir_all(&temp).ok();
    }

    /// The names a tree may hold but a line-per-entry record cannot carry raw: a line feed, a
    /// carriage return (which `str::lines` also strips from a line's end), and the backslash the
    /// escape itself is built from.
    const HOSTILE_NAMES: [&str; 7] = ["a\nb", "c\rd", "e\r", "\nf", "g\\nh", "i\\", "with space"];

    #[test]
    fn the_skeleton_round_trips_names_holding_line_breaks_and_backslashes() {
        let hash = "0123456789abcdef".repeat(4);
        let mut entries = BTreeMap::new();
        for name in HOSTILE_NAMES {
            entries.insert(format!("src/{}", name), Some((hash.clone(), DirEntryType::Normal)));
            entries.insert(format!("gone/{}", name), None);
        }
        let skeleton = OutOfScopeSkeleton { entries: entries.clone() };

        let read_back = OutOfScopeSkeleton::from_bytes(&skeleton.to_bytes(), Path::new("consolidation-skeleton")).unwrap();

        assert_eq!(read_back.entries, entries);
    }

    #[test]
    fn a_skeleton_without_its_format_header_is_refused() {
        let refused = OutOfScopeSkeleton::from_bytes(b"D src/a\n", Path::new("consolidation-skeleton")).err().expect("a headerless skeleton must be refused");

        assert!(refused.contains("format"), "{}", refused);
    }

    #[test]
    fn the_consolidation_state_round_trips_a_label_holding_a_line_break() {
        use crate::globals::StorageRootScope;

        let temp = std::env::temp_dir()
            .join(format!("forklift-consolidation-state-label-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp);
        std::fs::create_dir_all(temp.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        let _scope = StorageRootScope::enter(&temp);

        for label in HOSTILE_NAMES {
            let state = ConsolidationState {
                their_head: "ab".repeat(32), their_pallet: label.to_string(), our_head: "cd".repeat(32), our_pallet: "main".to_string(),
            };
            write_consolidation_state(&state).unwrap();

            let read_back = read_consolidation_state().unwrap().unwrap();

            assert_eq!(read_back.their_pallet, label);
            assert_eq!(read_back.their_head, state.their_head);
            assert_eq!(read_back.our_head, state.our_head);
            assert_eq!(read_back.our_pallet, state.our_pallet);
        }

        // A version-1 state (no base) is refused by its version, never read without one.
        std::fs::write(get_consolidation_state_path(), record_utils::to_bytes(CONSOLIDATION_RECORD, 1, ["ab".repeat(32), "feature".to_string()])).unwrap();
        let error = read_consolidation_state().err().expect("a version-1 state file must be refused");
        assert!(error.contains("does not know") && error.contains("Abort the consolidation"), "{}", error);

        std::fs::write(get_consolidation_state_path(), format!("{}\nfeature\n", "ab".repeat(32))).unwrap();
        let error = read_consolidation_state().err().expect("a headerless state file must be refused");
        assert!(error.contains("has no format header") && error.contains("Abort the consolidation"), "{}", error);

        std::fs::remove_dir_all(&temp).ok();
    }
}
