use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;
use crate::builder::inventory::InventoryBuilder;
use crate::enums::inventory_item_state::InventoryItemState;
use crate::enums::dir_entry_type::DirEntryType;
use crate::model::inventory::{Inventory, InventoryItem, UNPROVEN_MTIME};
use crate::model::object::loose_object::LooseObject;
use crate::model::task::inventory_builder::inventory_builder_context::InventoryBuilderContext;
use crate::model::task::inventory_builder::inventory_builder_task::InventoryBuilderTask;
use crate::model::task::TaskExecutor;
use crate::parser;
use crate::traits::task_context::TaskContext;
use crate::util::{alias_utils, chunk_utils, fanout_utils, file_utils, load_guard_utils, object_utils, office_utils, path_utils, record_utils, tree_utils, warehouse_utils};
use crate::util::object_utils::IngestMode;
use crate::util::path_utils::WarehousePath;
use crate::util::scope_utils::{MaterializationScope, ScopeClass};

mod shard_write;

use shard_write::{publish_shards, write_shard_mutation, ShardPublish};
pub use shard_write::{cleanup_after_stack_with, remove_inventories_under, replace_all_inventories};

/// The metadata entry used for the warehouse root (its key is the empty string,
/// which would be confusing as a line in the metadata file).
const METADATA_ENTRY_ROOT: &str = "./";

/// The warehouse path keys of every strict ancestor of `key`, from the root (`""`) down to
/// (but not including) `key` itself.
fn ancestor_keys_root_first(key: &str) -> Vec<String> {
    let mut chain: Vec<String> = Vec::new();
    let mut current = key;

    while !current.is_empty() {
        current = current.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
        chain.push(current.to_string());
    }

    chain.reverse();
    chain
}

/// Process-wide count of rollup-based skips actually applied (DESIGN.html §5.0 D item 8, stage
/// 2) — incremented once per skipped subtree root, by both consumers (the staged-changes walk,
/// `stack`'s tree build). Not a performance feature: a cheap, always-on observability hook the
/// tests use to prove a skip *actually fired* (not just that its output happens to be correct,
/// which the equivalence tests already cover) — see `rollup_skip_count`.
static ROLLUP_SKIP_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Record that a rollup-based skip was just applied to one subtree root. Call exactly once per
/// skip decision (never once per key it covers — a skip's whole point is never visiting its
/// descendants to begin with).
pub fn record_rollup_skip() {
    ROLLUP_SKIP_COUNT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// The current value of the process-wide rollup-skip counter — see [`record_rollup_skip`].
pub fn rollup_skip_count() -> u64 {
    ROLLUP_SKIP_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Whether the rollup-based skip (DESIGN.html §5.0 D item 8, stage 2) is currently allowed.
/// Test-only kill switch: set `FORKLIFT_DISABLE_ROLLUP_SKIP=1` to force every consumer (the
/// staged-changes walk in `stocktake_utils`, `stack`'s tree build in `tree_utils`) to behave
/// exactly as if no shard ever carried a rollup — a full walk/build every time. Both consumers
/// call this (never re-implement their own env check) so the equivalence tests can flip one
/// switch and diff the result against the optimized path. Undocumented; not a supported setting.
pub fn rollup_skip_enabled() -> bool {
    std::env::var("FORKLIFT_DISABLE_ROLLUP_SKIP").is_err()
}

/// Peek a shard's rollup hash without parsing its entries. Missing shard, absent rollup, or an
/// old-version shard (which never carries one) all read as `None` — the caller decides what
/// that means (usually: no skip, fall back to the ordinary walk/build).
///
/// # Arguments
/// * `key` - The warehouse path key of the directory.
///
/// # Returns
/// * `Ok(Some(String))` - The shard exists and carries a rollup.
/// * `Ok(None)`         - The shard is missing or carries no rollup.
/// * `Err(String)`      - If the shard exists but its header could not be parsed.
pub fn peek_shard_rollup(key: &str) -> Result<Option<String>, String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(None);
    };

    parser::inventory::inventory_parser::peek_rollup_hash(&bytes)
}

/// Whether two inventories carry the same *effective* content — the same set of entries, each
/// with the same name, type, hash and state. Stat fields (timestamps, device/inode, size,
/// ownership) are deliberately excluded: they can differ (e.g. after a stat-only refresh)
/// without the tree `stack` would build from either inventory differing at all. Used to decide
/// whether a shard rewrite may carry its existing rollup forward instead of invalidating it.
fn inventory_content_matches(a: &Inventory, b: &Inventory) -> bool {
    a.get_items_count() == b.get_items_count()
        && a.get_items().all(|(name, item)| {
            b.get_item_by_name(name).is_some_and(|other| {
                other.hash == item.hash && other.item_type == item.item_type && other.state == item.state
            })
        })
}

/// Add a file or directory to its corresponding inventory.
/// If no inventory exists for the given directory, a new inventory file will be created.
///
/// This is `load`'s tree-walk entry point, so it is where the incomplete-load guard
/// (`load_guard_utils`) brackets the walk: `create_inventory_for_directory` and
/// `add_file_to_inventory` each durably record a marker naming `path` *before* anything they do
/// mutates the staging area (folded into their own first durable barrier — see each function's
/// doc comment), cleared only once the walk below returns `Ok` (best-effort, and only if `path`
/// covers whatever the marker still names — see `load_guard_utils::mark_load_completed`). A load
/// that returns `Err`, or one that is killed outright before it ever gets the chance to, both
/// leave the marker in place; `stack` and `park` refuse while any is.
///
/// # Arguments
/// * `path`  - The path of the file or directory to add to the inventory.
/// * `check` - The load's alias check: listings it has already read are not read again.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub async fn add_changes_to_inventory(path: &WarehousePath, check: &mut alias_utils::AliasCheck<'_>) -> Result<(), String> {
    // The walk skips the metadata folder by its exact name, but a named path is loaded whatever
    // the rules say, so refuse it here. Every spelling some file system opens as a metadata
    // folder (`.Forklift`, `.forklift.`, `FORKLI~1`) is refused, in any component, and so is a
    // name this volume resolves to one.
    match office_utils::metadata_folder_component(path.as_key())? {
        Some((0, component)) => return Err(format!(
            "Refusing to load \"{}\": {:?} is, on this or some other file system, the warehouse's \
            metadata folder, and it is never tracked.",
            path.as_key(), component
        )),
        Some((_, component)) => return Err(format!(
            "Refusing to load \"{}\": {:?} is, on this or some other file system, a warehouse's \
            metadata folder, and checking it out would plant another working tree inside this one.",
            path.as_key(), component
        )),
        None => {}
    }

    // The path the volume opens, so a directory this warehouse registers is found under the
    // spelling it is registered by, whatever spelling was typed.
    let opened = check.resolve(path.as_key())?;

    // A named path never meets the walk's nested-working-tree boundary as an entry of its parent
    // (`load sub` walks from `sub` itself, and `load sub/x` does not walk), so test every
    // directory down to it here.
    if !path.is_root() {
        let full = opened.as_deref().unwrap_or(path.as_key());
        let ends = full.match_indices('/').map(|(end, _)| end).chain(std::iter::once(full.len()));

        for end in ends {
            let key = &full[..end];

            if warehouse_utils::is_working_tree_root(Path::new(key))? {
                // As the walk judges it (see `rule_verdict`): tracked until every file below it
                // is staged for removal.
                let registered = registered_keys()?;
                if registered.contains(key) && subtree_has_live_entry(key, &registered)? {
                    return Err(tracked_nested_working_tree_error(key));
                }

                return Err(format!(
                    "Refusing to load \"{}\": the folder \"{}\" holds a \".forklift\", so it is the \
                    root of another working tree, and its content is that working tree's to load.",
                    path.as_key(), key
                ));
            }
        }
    }

    // After the nested-working-tree test: a load it refuses under any spelling is never offered.
    refuse_unlisted_spelling(path.as_key(), opened, check)?;

    let is_directory = file_utils::is_directory(&path.to_fs_path())?;

    if is_directory {
        create_inventory_for_directory(path).await?;
    } else {
        add_file_to_inventory(path)?;
    }

    load_guard_utils::mark_load_completed(path.as_key());

    Ok(())
}

/// Refuse a named path that some directory along it does not list under exactly that spelling,
/// as the walk judges every name it loads. `lstat` alone would open a case or normalization
/// alias of a listed entry, and loading under that spelling would stage a second live entry for
/// one file. The path is refused as a case-sensitive volume would refuse it, naming the path this
/// volume opens instead and the one way to stage it: if that path or a directory above it is one
/// entry with something the inventory holds live, the pair's own remedy (loading the listed path
/// alone would leave the pair), otherwise loading the listed path. A path that opens nothing is
/// left to the load's own error. `opened` is what `check` resolved `key` to.
fn refuse_unlisted_spelling(key: &str, opened: Option<String>, check: &mut alias_utils::AliasCheck) -> Result<(), String> {
    let Some(listed) = opened else {
        return Ok(());
    };

    if listed == key {
        return Ok(());
    }

    let nodes = alias_utils::Nodes::from_live_files(live_file_paths()?);

    let remedy = match check.pair_along(&listed, &nodes)? {
        Some((prefix, node)) => format!(
            "The inventory still holds \"{}\", which this file system opens as \"{}\" too, so loading \
            \"{}\" alone would leave two entries for one file. {}",
            node, prefix, listed, check.remedy(&prefix, &node, &nodes)?.instructions()
        ),
        None => alias_utils::Remedy::Load(listed.clone()).instructions(),
    };

    Err(format!(
        "Nothing is listed as \"{}\": this file system opens \"{}\" under that name. {}",
        key, listed, remedy
    ))
}

/// The path of every live (not `Deleted`) file entry the inventory holds.
fn live_file_paths() -> Result<BTreeSet<String>, String> {
    let mut paths = BTreeSet::new();

    for key in registered_keys()? {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(&key)?;
        let Some(bytes) = bytes else { continue };
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        paths.extend(inventory.get_items()
            .filter(|(_, item)| item.state != InventoryItemState::Deleted)
            .map(|(name, _)| if key.is_empty() { name.to_string() } else { format!("{}/{}", key, name) }));
    }

    Ok(paths)
}

/// Stage a file or directory for removal: its inventory entries are marked as
/// `Deleted` instead of being erased, so the staged removal is remembered until the next
/// parcel is stacked (and can be reported by status-like commands). The working directory
/// is never touched.
///
/// # Arguments
/// * `path` - The path of the file or directory to stage for removal.
///
/// # Returns
/// * `Ok(())`      - If the operation was successful.
/// * `Err(String)` - If there was an error.
pub fn stage_removal(path: &WarehousePath) -> Result<(), String> {
    let mut ledger = ShardLedger::read()?;

    if path.is_root() {
        return stage_removal_for_directory(&mut ledger, path);
    }

    match tracked_kind(&ledger, path)? {
        TrackedKind::Directory => stage_removal_for_directory(&mut ledger, path),
        TrackedKind::File => stage_removal_for_file(&mut ledger, path),
        TrackedKind::UntrackedDirectory => Err(format!("No inventory found for folder \"{}\".", path.as_key())),
    }
}

/// What `remove` or `restore` should treat a path as. See [`tracked_kind`].
pub enum TrackedKind {
    /// A tracked directory: its key is registered.
    Directory,
    /// A file, tracked or not; the file branch reports a path that is not in the inventory.
    File,
    /// A directory on disk whose key is not registered.
    UntrackedDirectory,
}

/// Decide whether a (non-root) path is a file or a directory for `remove` and `restore`.
///
/// Registration says a directory is tracked, not that the path is a directory now: after a
/// flip, a registered key can name a file on disk, and a live file entry can name a directory.
/// So the inventory entry matching the type on disk wins, then the other one, and a path that is
/// gone takes the live file entry first:
/// * a directory on disk: registered, then a live file entry, else untracked;
/// * a file on disk, or nothing: a live file entry, then registered, else the file branch (which
///   reports the path as not in the inventory).
///
/// # Arguments
/// * `ledger` - The registered set.
/// * `path`   - The warehouse path.
///
/// # Returns
/// * `Ok(TrackedKind)` - What to treat the path as.
/// * `Err(String)`     - If the path or the parent's shard could not be read.
pub fn tracked_kind(ledger: &ShardLedger, path: &WarehousePath) -> Result<TrackedKind, String> {
    let is_directory_on_disk = match std::fs::symlink_metadata(path.to_fs_path()) {
        Ok(metadata) => metadata.is_dir(),
        // A file where a component of the path should be: nothing is at the path itself, and
        // whatever writes it refuses on the file.
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => false,
        Err(e) => return Err(format!("Error while getting metadata for path \"{}\": {}", path.as_key(), e)),
    };

    let (parent, file_name) = path.split_parent()?;
    // Read through the ledger, as the writer this picks does: an unregistered parent lists nothing.
    let parent_inventory = ledger.inventory(parent.as_key())?.unwrap_or_else(Inventory::new);
    let has_live_file_entry = has_live_entry(&parent_inventory, &file_name);
    let registered = ledger.holds(path.as_key());

    Ok(match (is_directory_on_disk, registered, has_live_file_entry) {
        (true, true, _) => TrackedKind::Directory,
        (true, false, true) => TrackedKind::File,
        (true, false, false) => TrackedKind::UntrackedDirectory,
        (false, _, true) => TrackedKind::File,
        (false, true, false) => TrackedKind::Directory,
        (false, false, false) => TrackedKind::File,
    })
}

/// Whether a shard holds a live (not `Deleted`) entry with the given name.
fn has_live_entry(inventory: &Inventory, name: &str) -> bool {
    matches!(inventory.get_item_by_name(name), Some(item) if item.state != InventoryItemState::Deleted)
}

/// What the ignore rules make of one entry of a directory a walk reads. See [`rule_verdict`].
pub enum RuleVerdict {
    /// No rule hides the entry: classify it like any other.
    Walk,
    /// The rules hide the entry, and nothing under its name is tracked.
    Ignored,
    /// The rules hide what is on disk, but its name is tracked as the other type (a tracked
    /// file replaced by a folder, or a tracked folder by a file). The entry on disk is skipped
    /// like ignored content, and the tracked one is reported as gone, never hidden.
    CoveredOtherType,
    /// No rule hides the entry, but it is an untracked directory holding a `.forklift` entry: the
    /// root of another working tree (see [`warehouse_utils::is_working_tree_root`]), whose content
    /// belongs to that one. Each walk decides what that means: load skips it, stocktake reports it
    /// as one untracked directory.
    NestedBoundary,
}

/// The refusal for a directory this warehouse tracks that has become the root of another working
/// tree: walking it would report the other tree's metadata as content, and skipping it would report
/// this warehouse's tracked content there as gone.
///
/// # Arguments
/// * `key` - The directory's warehouse path key.
pub fn tracked_nested_working_tree_error(key: &str) -> String {
    let remedy = tree_utils::remedy_path(key);

    format!(
        "The folder \"{}\" holds a \".forklift\", so it is the root of another working tree, but \
        this warehouse tracks it. To leave it to the other working tree, run \"forklift remove {}\"{}, \
        which keeps its files on disk, then \"forklift stack\". Otherwise move its \".forklift\" \
        out of it.",
        key,
        remedy.as_deref().unwrap_or("-- <folder>"),
        if remedy.is_some() { "" } else { " with that path quoted for your shell" }
    )
}

/// The ignore rules' verdict on an entry of a directory. A rule covers only untracked paths,
/// and an entry is tracked as what it is on disk now: a file by a live entry in the
/// directory's shard, a directory by its registered key. So a rule hides the entry on disk
/// whenever that one is untracked, but a tracked entry of the other type under the same name
/// must still be reported as gone, which [`RuleVerdict::CoveredOtherType`] tells apart.
///
/// The inventory is consulted first, and the entry's type (`DirEntry::file_type`, no extra
/// stat on the common platforms) only when a rule covers a name that is tracked, or the rules
/// leave a directory to be walked: then one more lstat asks whether it is the root of another
/// working tree. That comes after the rules, so an ignored nested working tree stays
/// [`RuleVerdict::Ignored`]; whatever deletes a directory cannot rely on this verdict.
///
/// # Arguments
/// * `rules`      - The ignore rules.
/// * `parent`     - The directory's coverage (see [`file_utils::IgnoreRules::coverage_of`]).
/// * `shard`      - The directory's shard (`None` when it has none, or it could not be read).
/// * `entry`      - The entry.
/// * `name`       - The entry's name.
/// * `key`        - The entry's warehouse path key.
/// * `registered` - The registered directory keys.
///
/// # Returns
/// * `Ok(RuleVerdict)` - The verdict.
/// * `Err(String)`     - If the entry's type could not be read, or the entry is a tracked
///                       directory that has become another working tree's root while some file
///                       below it is still tracked (see [`tracked_nested_working_tree_error`]).
pub fn rule_verdict(rules: &file_utils::IgnoreRules,
                    parent: file_utils::Coverage,
                    shard: Option<&Inventory>,
                    entry: &std::fs::DirEntry,
                    name: &str,
                    key: &str,
                    registered: &BTreeSet<String>) -> Result<RuleVerdict, String> {
    let is_directory = || entry.file_type()
        .map(|file_type| file_type.is_dir())
        .map_err(|e| format!("Error while getting the type of \"{}\": {}", key, e));

    if rules.excludes(key, false, parent) {
        let has_file_entry = shard.is_some_and(|inventory| has_live_entry(inventory, name));
        let is_registered = registered.contains(key);

        if rules.excludes(key, true, parent) || (!has_file_entry && !is_registered) {
            return Ok(RuleVerdict::Ignored);
        }

        let is_directory = is_directory()?;

        if !((is_directory && is_registered) || (!is_directory && has_file_entry)) {
            return Ok(RuleVerdict::CoveredOtherType);
        }
    }

    // `DirEntry::file_type` does not follow a symlink, so a link to a working tree is a link.
    if is_directory()? && warehouse_utils::is_working_tree_root(&entry.path())? {
        // Once every tracked file below it is staged for removal (the way out the error names),
        // nothing of it is left to report as gone or stale, and it is a boundary like any other.
        if registered.contains(key) && subtree_has_live_entry(key, registered)? {
            return Err(tracked_nested_working_tree_error(key));
        }

        return Ok(RuleVerdict::NestedBoundary);
    }

    Ok(RuleVerdict::Walk)
}

/// Whether the registered directory `key`, or any registered directory below it, has an entry not
/// staged for removal.
fn subtree_has_live_entry(key: &str, registered: &BTreeSet<String>) -> Result<bool, String> {
    let prefix = format!("{}/", key);

    // `sub-x` sorts between `sub` and `sub/`, so the descendants are a range of their own.
    let below = registered.range(prefix.clone()..).take_while(|tracked| tracked.starts_with(&prefix));

    for tracked in std::iter::once(key).chain(below.map(String::as_str)) {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(tracked)?;
        let Some(bytes) = bytes else { continue };
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", tracked, e))?;

        if inventory.get_items().any(|(_, item)| item.state != InventoryItemState::Deleted) {
            return Ok(true);
        }
    }

    Ok(false)
}

/// Every key registered in the inventory metadata (an empty set when there is no metadata file).
///
/// # Returns
/// * `Ok(BTreeSet<String>)` - The registered warehouse path keys.
/// * `Err(String)`          - If the metadata file could not be read.
fn registered_keys() -> Result<BTreeSet<String>, String> {
    Ok(ShardLedger::read()?.keys().map(str::to_string).collect())
}

/// Create an inventory for the specified directory (and all subdirectories).
///
/// A failure while deciding one directory's content does not cost the whole walk by itself:
/// whatever every other task decided is still published (and registered in the metadata file),
/// so previously loaded, unrelated inventories are never destroyed. But every blob this walk
/// stages shares one batch, and a producer that fails between winning a blob's reservation and
/// finishing its write leaks that reservation — `WriteBatch::finish` then refuses to publish the
/// *whole* batch, so nothing this walk decided is published this round (see the phase-0 comment
/// in the function body for the full reasoning). A producer that fails before ever winning a
/// reservation leaks nothing and only costs its own shard's decision, same as any other per-shard
/// failure. Either way, re-running the load after fixing the problem completes the inventory —
/// nothing already published on disk is ever destroyed by a later, failed pass.
///
/// The incomplete-load marker for `path` rides this walk's own first durable barrier (Phase 0
/// below) instead of paying a second, standalone one — see `load_guard_utils`'s doc comment
/// (folding section) for the general argument. Staging it here, before the walk even begins,
/// costs nothing durability-wise: `WriteBatch::stage` only creates an as-yet-invisible temp file
/// (not itself a durable mutation), and nothing below is durable until `blob_batch.finish()`
/// (Phase 0) returns `Ok`. Two outcomes, both consistent: something between here and Phase 0
/// fails first (`get_ignore_rules`, `populate_dirty_inventory_paths`, or the walk itself) —
/// `context` (and the marker's still-unpublished temp with it) is simply dropped, exactly as
/// invisible as before this call, and nothing else from this load became durable either; or
/// Phase 0 is reached and runs — which it always does once the walk has been attempted,
/// regardless of whether the walk itself succeeded (see Phase 0's own comment below) — so the
/// marker is never left staged-but-unpublished by a code path that also durably published
/// something else from this load.
///
/// # Arguments
/// * `path` - The path to the directory.
///
/// # Returns
/// * `Ok(())`      - If the inventory was successfully created.
/// * `Err(String)` - If an error occurred while creating the inventory.
pub async fn create_inventory_for_directory(path: &WarehousePath) -> Result<(), String> {
    let context = Arc::new(InventoryBuilderContext::new());

    load_guard_utils::stage_start_marker(&context.blob_batch, path.as_key())?;

    let executor = TaskExecutor::new(Arc::clone(&context));
    let ignore_rules = file_utils::get_ignore_rules()?;

    // Every previously inventoried directory inside the loaded subtree starts out "dirty";
    // the walk removes each directory it visits. Whatever is left afterwards no longer
    // exists in the working directory, so its entries are staged as removals. The same set,
    // taken before the walk changes it, is what the walk asks whether a directory is tracked.
    let registered = populate_dirty_inventory_paths(&context, path).await?;

    let parent_coverage = match path.split_parent() {
        Ok((parent, _)) if !path.is_root() => ignore_rules.coverage_of(parent.as_key()),
        _ => ignore_rules.coverage_of(""),
    };

    let root_task: InventoryBuilderTask = Box::pin(build_inventory(
        Arc::clone(&context),
        Arc::new(path.clone()),
        Arc::new(ignore_rules),
        Arc::new(registered),
        parent_coverage,
    ));

    // `result` accumulates the *first* failure from anywhere in this function — the walk itself,
    // or any fallible step of the join point below. Every join-point step still runs its
    // best-effort work even once `result` already holds an error (exactly as the walk's own
    // failure already tolerated: whatever the tasks that did complete decided is still published
    // — see the join point's own doc comment), but never lets a later, possibly less useful,
    // error overwrite the first one a caller would want to see.
    let mut result: Result<(), Option<String>> = executor.execute(root_task).await;

    // Phase 0: every changed or brand-new small file's blob the walk staged becomes durable now,
    // in its own barrier — strictly before any shard content is even staged below (DESIGN.html
    // §5.0 D item 10, findings #1/#7). A shard phase B publishes can name one of these blobs'
    // hashes, so the blob must already be durable — not merely staged in a batch that has not yet
    // been through its own `finish()` — before that shard's rename is allowed to land. See
    // `shard_write::publish_shards`'s doc comment for why blobs and shard content cannot share one
    // batch and still give that ordering.
    //
    // A failure here is different from every other failure this join point tolerates: this batch
    // covers *every* blob the whole walk decided, and `WriteBatch::finish` can fail more than one
    // way (see its own `# Returns` for the full breakdown) — no failure mode leaves a per-hash way
    // to tell which blobs actually landed, and some (a rename partway, or the barrier's own
    // trailing directory sync — see `run_write_barrier` for why that is not all-or-nothing) leave
    // some of this batch's blobs durable and others not. Either way, publishing any shard content
    // below could then durably name a blob that never actually landed. So a failure here — unlike
    // the walk's own failure, which still publishes whatever it *did* decide — skips phase A/B
    // entirely (`blob_batch_published` below): nothing this walk decided is published this round,
    // and a retry starts over. Still strictly better than the pre-batching baseline, where a torn
    // single-object write could only ever cost that one file, never an entire load, and losing a
    // whole walk's *decisions* (not their eventual content — nothing here is destructive) to a
    // retry is a fair trade for closing finding #7.
    //
    // The blast radius is deliberate, but only for the producer failure that actually causes it: a
    // producer that fails *between* winning a `reserve_final_path` reservation (see
    // `LooseObject::store_deferred`) and finishing its write leaves that reservation leaked, and
    // `WriteBatch::finish` refuses the whole batch before anything is renamed (see its own doc
    // comment) — every other occurrence of that same content already saw the reservation lost and
    // staged nothing itself, so the missing blob could be named by any shard's decision, not just
    // the one whose producer failed. A producer that fails *before* ever winning a reservation (the
    // object-ceiling check, `does_object_exist`, or the file read itself) leaks nothing, so it does
    // not by itself force that outcome — that shard's own decision simply fails, exactly like any
    // other per-shard failure this join point already tolerates.
    let mut blob_batch_published = true;

    if let Err(e) = context.blob_batch.finish() {
        if result.is_ok() { result = Err(Some(e)); }
        blob_batch_published = false;
    }

    // The join point (DESIGN.html §5.0 D item 8, stage 2b): every per-directory task only ever
    // *decided* what to write (collected in `context.writes`) — nothing was written to disk by any
    // task. Publishing happens here, once, single-threaded, after every task that managed to run
    // has reported in (whether or not the walk as a whole succeeded — see below), through
    // `shard_write::publish_shards`, which decides every rollup.
    let mut writes: BTreeMap<String, Inventory> = std::mem::take(&mut *context.writes.lock().await);
    let mut stale_keys: BTreeSet<String> = BTreeSet::new();

    // Directories that are gone from the working directory keep their
    // inventory shard, with every entry marked as a staged removal. Folded into the same join
    // point as every other mutation this walk makes, rather than its own separate atomic write
    // per directory. Only ever considered after a fully successful walk — on failure, the walk
    // may not have reached every directory, so a shard still marked "dirty" here might simply be
    // one it hasn't visited yet, not one that is actually gone (the original contract, preserved
    // below: dirty inventories are never removed from metadata unless this pass actually proved
    // them stale).
    //
    // A failure partway through this pass (an unreadable or corrupt leftover shard) stops the
    // pass right there — exactly like a walk failure, whatever was classified before the failure
    // is kept, and the loop's own error is folded into `result` instead of aborting the function:
    // the failure-resilience branch below must still run so the shards this walk *did* manage to
    // publish stay registered, and the user sees the same "entries loaded so far were kept"
    // message a walk failure gives, not a raw, unwrapped parse error.
    if result.is_ok() {
        let dirty_paths = context.dirty_inventory_paths.lock().await;

        for dirty_key in dirty_paths.iter() {
            let bytes_opt = match file_utils::retrieve_inventory_or_none_by_key(dirty_key) {
                Ok((_, bytes_opt)) => bytes_opt,
                Err(e) => { result = Err(Some(e)); break; }
            };

            let Some(bytes) = bytes_opt else {
                // The shard itself is gone — every ancestor's rollup that named a tree including
                // this directory is now wrong (it would still match head, but head no longer
                // reflects a subtree that no longer exists), so the publish below invalidates
                // them exactly like any other real content change, not just drops it from
                // metadata.
                stale_keys.insert(dirty_key.clone());
                continue;
            };

            let inventory_result = parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", dirty_key, e));

            let mut inventory = match inventory_result {
                Ok(inventory) => inventory,
                Err(e) => { result = Err(Some(e)); break; }
            };

            if inventory.mark_all_items_deleted() {
                writes.insert(dirty_key.clone(), inventory);
            }
        }
    }

    // Phase A ancestor clears, then registration, then phase B shard content — all in
    // `shard_write::publish_shards`. Every directory whose task finished is registered in one
    // durable metadata write before any shard of this walk is published: a shard only ever lands at
    // a key already registered (see `ShardLedger`). A directory that was not registered before this
    // walk always has an outcome here (its baseline was empty), and one that was already registered
    // is skipped by the registration, so this registers exactly the directories about to be
    // published.
    //
    // Only attempted when phase 0 (the blob barrier above) fully succeeded — see its own comment
    // for why a blob-batch failure must not let any shard content publish at all. Any failure is
    // folded into `result`. A failed registration publishes nothing: it may have cleared and
    // registered some of the keys, which then read as empty directories until a later load
    // publishes them, never as a shard nobody registered; a key registered whose shard did not land
    // reads as empty too.
    if blob_batch_published {
        let new_inventory_paths = context.new_inventory_paths.lock().await.clone();

        // A stale key's shard is already gone. It is unregistered in the same metadata write, once
        // every rollup above it is cleared. A failure before that leaves it registered with no
        // shard, which reads as an empty directory until the next load finds it stale again. A
        // blob-batch failure needs no such write: it sets `result` before the dirty-path pass,
        // which then never runs, so there are no stale keys to unregister.
        let published = ShardLedger::read().and_then(|mut ledger| publish_shards(&mut ledger, ShardPublish {
            writes,
            removed: stale_keys,
            register: new_inventory_paths,
            expected: None,
            extra: None,
        }));

        if let Err(e) = published {
            if result.is_ok() { result = Err(Some(e)); }
        }
    }

    if let Err(e) = result {
        let message = e.unwrap_or("An unknown error occurred while building the inventory.".to_string());

        return Err(format!(
            "{}\nThe load did not complete; entries loaded so far were kept. \
            Re-run the load once the problem is fixed.",
            message
        ));
    }

    Ok(())
}

/// Mark every previously inventoried directory inside the given subtree as dirty.
/// Directories visited by the inventory build remove themselves from this set, so the
/// directories remaining after the walk are the ones deleted from the working directory.
///
/// # Arguments
/// * `context` - The inventory builder context.
/// * `path`    - The root of the subtree being loaded.
///
/// # Returns
/// * `Ok(BTreeSet<String>)` - The registered keys at or under `path`: the dirty set as populated.
/// * `Err(String)`          - If the inventory metadata could not be read.
async fn populate_dirty_inventory_paths(context: &InventoryBuilderContext,
                                        path: &WarehousePath) -> Result<BTreeSet<String>, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(BTreeSet::new());
    };

    let mut registered: BTreeSet<String> = BTreeSet::new();

    for entry in metadata {
        let key = metadata_entry_to_key(&entry);

        if path_utils::covers(path.as_key(), key) {
            registered.insert(key.to_string());
        }
    }

    context.dirty_inventory_paths.lock().await.extend(registered.iter().cloned());

    Ok(registered)
}

/// Convert an inventory metadata file entry to a warehouse path key.
/// The warehouse root is stored as `./` in the metadata file, but its key is the empty string.
pub fn metadata_entry_to_key(entry: &str) -> &str {
    if entry == METADATA_ENTRY_ROOT { "" } else { entry }
}

/// Convert a warehouse path key to its inventory metadata file entry.
fn key_to_metadata_entry(key: &str) -> String {
    if key.is_empty() { String::from(METADATA_ENTRY_ROOT) } else { key.to_string() }
}

/// Stage the removal of a directory: mark every entry in its inventory (and in the
/// inventories of all of its subdirectories) as `Deleted`. The inventory shards and their
/// metadata entries are kept — they are the record of the staged removals.
///
/// # Arguments
/// * `ledger` - The registered set.
/// * `path`   - The path of the folder in the working dir.
///
/// # Returns
/// * `Ok(())`      - If the removals were staged successfully.
/// * `Err(String)` - If no inventory exists for the folder, or there was an error.
fn stage_removal_for_directory(ledger: &mut ShardLedger, path: &WarehousePath) -> Result<(), String> {
    // Only registered keys hold staged content: an unregistered address is nobody's shard.
    let keys: BTreeSet<String> = ledger.keys()
        .filter(|key| path_utils::covers(path.as_key(), key))
        .map(str::to_string)
        .collect();

    let mut found_any_shard = false;
    let mut writes: BTreeMap<String, Inventory> = BTreeMap::new();

    for key in keys {
        let Some(mut inventory) = ledger.inventory(&key)? else {
            continue;
        };

        found_any_shard = true;

        // A shard is only rewritten when an entry actually changed state.
        if inventory.mark_all_items_deleted() {
            writes.insert(key, inventory);
        }
    }

    if !found_any_shard {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            if path.is_root() { METADATA_ENTRY_ROOT } else { path.as_key() }
        ));
    }

    publish_shards(ledger, ShardPublish::of(writes))
}

/// Split a warehouse path into the key of the shard that lists it and its name in that shard.
pub fn split_path_key(path: &str) -> (&str, &str) {
    match path.rsplit_once(file_utils::PATH_SEPARATOR_CHAR) {
        Some((parent, name)) => (parent, name),
        None => ("", path),
    }
}

/// One shard a [`ShardMutationBatch`] has touched: the bytes it read on first touch (`None` when
/// the shard did not exist yet), and the in-memory copy every change applies to.
struct TouchedShard {
    original_bytes: Option<Vec<u8>>,
    inventory: Inventory,
}

/// Many shard edits from one single-threaded operation, published together as one
/// [`shard_write::publish_shards`] call — beside `load`'s walk and `park`'s refresh — instead of
/// one [`ShardLedger::update_shard`] (and so one ancestor-clear barrier, one shard write and one
/// metadata rewrite) per edited entry.
///
/// [`update`](Self::update) reads a shard on its first touch and applies every later change for
/// the same key to the same in-memory copy; nothing reaches disk before [`publish`](Self::publish).
/// Each shard then clears every rollup above it if its effective content (any entry's
/// name/hash/type/state) differs from what was read, or the shard is new, and keeps its own rollup
/// otherwise (see [`shard_write::publish_shards`]) — so a pure stat refresh, such as `restore`
/// writing a file back to the blob it already has staged, keeps every rollup above it.
///
/// An entry the batch records from a stat proves itself (see [`file_utils::StatAnchor`]): the
/// staging methods take their anchor after the caller's write and before the stat, so an edit
/// after the anchor is recorded as unproven. A caller must stat a file right after writing it:
/// writing many files first and staging them later would record an edit made in between against
/// the hash the caller wrote, and an edit older than the anchor would be trusted.
///
/// Every blob a change makes an entry name must be durable before its shard is published, as
/// [`shard_write::publish_shards`] requires: either stored before the change, or staged through
/// [`stage_file_storing_blob`](Self::stage_file_storing_blob), whose blobs every publish makes
/// durable first, in one barrier.
pub struct ShardMutationBatch<'l> {
    shards: BTreeMap<String, TouchedShard>,
    blobs: file_utils::WriteBatch,
    /// The registered set every first touch reads its baseline through.
    ledger: LazyLedger<'l>,
}

impl ShardMutationBatch<'static> {
    /// A batch that reads the registered set on its first touch.
    pub fn new() -> ShardMutationBatch<'static> {
        ShardMutationBatch::over_ledger(LazyLedger::Unread)
    }
}

impl<'l> ShardMutationBatch<'l> {
    /// A batch whose first touches read through `ledger`, for a caller that already holds the
    /// registered set and writes no metadata before [`publish`](Self::publish).
    pub fn over(ledger: &'l ShardLedger) -> ShardMutationBatch<'l> {
        ShardMutationBatch::over_ledger(LazyLedger::Lent(ledger))
    }

    fn over_ledger(ledger: LazyLedger<'l>) -> ShardMutationBatch<'l> {
        ShardMutationBatch {
            shards: BTreeMap::new(),
            blobs: file_utils::WriteBatch::new(),
            ledger,
        }
    }

    /// Apply `change` to the shard at `key`, reading it first if this batch has not touched it
    /// yet. `change` cannot fail: anything that can, such as statting the file it records,
    /// belongs before this call.
    ///
    /// # Arguments
    /// * `key`    - The warehouse path key of the shard.
    /// * `change` - The change to apply to the inventory.
    ///
    /// # Returns
    /// * `Ok(())`      - If the change was applied in memory.
    /// * `Err(String)` - If the shard could not be read or parsed.
    pub fn update(&mut self,
                  key: &str,
                  change: impl FnOnce(&mut Inventory)) -> Result<(), String> {
        if !self.shards.contains_key(key) {
            // Through the ledger: an unregistered key starts empty, whatever its address holds.
            let original_bytes = self.ledger.get()?.retrieve(key)?;

            let inventory = match &original_bytes {
                Some(bytes) => parser::inventory::inventory_parser::parse_inventory(bytes)
                    .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
                None => Inventory::new(),
            };

            self.shards.insert(key.to_string(), TouchedShard { original_bytes, inventory });
        }

        let shard = self.shards.get_mut(key).unwrap();

        change(&mut shard.inventory);

        Ok(())
    }

    /// Stat a file just written for a blob or recipe that is already stored (e.g. one just
    /// written from a tree or merge), and record it in its parent's shard. Call it right after
    /// the write: the anchor is taken here, before the stat — see the type's doc comment.
    ///
    /// # Arguments
    /// * `path`      - The warehouse path of the file.
    /// * `hash`      - The blob or recipe hash of the file's content.
    /// * `item_type` - The authoritative entry type (from the tree / merge action), carried through
    ///   so a chunked entry keeps its `*Chunked` type in the inventory rather than being demoted to
    ///   a plain type a `stat` would report.
    ///
    /// # Returns
    /// * `Ok(())`      - If the entry was recorded in memory.
    /// * `Err(String)` - If the clock probe could not be stamped, the file's metadata gathered, or
    ///                   its shard read.
    pub fn stage_file_from_stat(&mut self,
                                path: &str,
                                hash: String,
                                item_type: DirEntryType) -> Result<(), String> {
        let (parent_key, name) = split_path_key(path);

        let anchor = file_utils::take_stat_anchor()?;
        let entry = build_inventory_item_from_stat(Path::new(path), name, hash, item_type, anchor)?;

        self.update(parent_key, |inventory| inventory.add_item(entry))
    }

    /// Stat a file just written with content not stored yet (e.g. a merge result), record it in
    /// its parent's shard, and stage its blob into this batch's own write batch — made durable
    /// before any shard is published, so no published entry names a blob that is not. Call it
    /// right after the write: the anchor and the stat come first, before the blob is compressed.
    ///
    /// # Arguments
    /// * `path`      - The warehouse path of the file.
    /// * `object`    - The blob of the file's content.
    /// * `item_type` - The entry type to record.
    ///
    /// # Returns
    /// * `Ok(())`      - If the entry was recorded in memory and the blob staged.
    /// * `Err(String)` - If the clock probe could not be stamped, the file's metadata gathered,
    ///                   the blob staged, or its shard read.
    pub fn stage_file_storing_blob(&mut self,
                                   path: &str,
                                   object: &mut LooseObject,
                                   item_type: DirEntryType) -> Result<(), String> {
        let (parent_key, name) = split_path_key(path);

        let anchor = file_utils::take_stat_anchor()?;
        let entry = build_inventory_item_from_stat(Path::new(path), name, object.hash.clone(), item_type, anchor)?;

        object.store_deferred(&self.blobs)?;

        self.update(parent_key, |inventory| inventory.add_item(entry))
    }

    /// Publish every touched shard, registering any touched key the metadata does not list yet.
    ///
    /// Each shard is re-read first, under the shard-write lock: if any changed since this batch
    /// read it, nothing is registered or published and an error is returned — the batch would
    /// otherwise overwrite that change, or carry a rollup decided against bytes no longer there.
    /// An unregistered key reads as no shard both times, whatever its address holds, so a shard
    /// nobody registered never blocks the batch; a key registered and written since this batch
    /// read it does. Within a command (which holds the warehouse lock) that can only be a caller
    /// writing a touched shard some other way before publishing.
    ///
    /// # Returns
    /// * `Ok(())`      - Every touched shard is durably published and registered.
    /// * `Err(String)` - If a shard changed underneath the batch, or the publish failed; no shard
    ///                   of this batch may then be treated as published.
    pub fn publish(self) -> Result<(), String> {
        let shards = self.shards;

        // Every blob a shard below names, durable before any shard is.
        self.blobs.finish()?;

        if shards.is_empty() {
            return Ok(());
        }

        let mut ledger = ShardLedger::read()?;

        let mut expected: BTreeMap<String, Option<Vec<u8>>> = BTreeMap::new();
        let mut writes: BTreeMap<String, Inventory> = BTreeMap::new();

        for (key, shard) in shards {
            expected.insert(key.clone(), shard.original_bytes);
            writes.insert(key, shard.inventory);
        }

        let register = writes.keys().cloned().collect();

        publish_shards(&mut ledger, ShardPublish {
            writes,
            removed: BTreeSet::new(),
            register,
            expected: Some(expected),
            extra: None,
        })
    }
}

/// A task for building an inventory file for a given directory.
/// When encountering a subdirectory, a new task is created to build the inventory for that directory.
///
/// Never writes *shard content* to disk itself: it only *decides* this directory's new content
/// (or that nothing changed at all, in which case it decides nothing) and records that decision in
/// the shared [`InventoryBuilderContext`], for `create_inventory_for_directory`'s single-threaded
/// join point to publish once every task has run, through [`shard_write::publish_shards`] —
/// which also decides, from the shard on disk, whether the rollups above survive. This is
/// deliberate: this walker is fire-and-forget concurrent (a directory's task fires off its
/// subdirectories' tasks and returns without waiting for them), so writing — and, worse,
/// invalidating ancestors — directly from here would need cross-task synchronization around every
/// write, one durability barrier at a time. Deferring the decision to
/// a join point instead turns the whole walk into at most three barriers total (DESIGN.html §5.0
/// D item 10, finding #7 — one more than the two of the original stage 2b design, because the
/// blob barrier below must complete, on its own, before shard content publishes), and needs no
/// lock at all: nothing concurrent ever touches another task's key.
///
/// A changed or brand-new *small* file's blob is the one exception (DESIGN.html §5.0 D item 10,
/// finding #1): its content is content-addressed and its write is staged (not fsynced) via
/// [`file_utils::WriteBatch::stage`] into `context.blob_batch` — a batch the join point finishes,
/// on its own, strictly *before* it stages any shard content (see `shard_write::publish_shards`'s
/// doc comment for why the two must not share one batch) — instead of paying its own atomic-write
/// barrier per file. This is safe to do straight from a concurrent task (unlike a shard's
/// content): `WriteBatch::stage` is documented safe for concurrent callers. Two tasks staging the
/// same hash never both pay the compress-and-write cost either (DESIGN.html §5.0 D item 10,
/// finding #2): `LooseObject::store_deferred` reserves the
/// object's final path in the batch before compressing, so only the first occurrence actually
/// stages anything — see [`file_utils::WriteBatch::reserve_final_path`]'s doc comment. Every blob
/// staged here is durable — `blob_batch.finish()` has already returned `Ok` — strictly before any
/// shard content that might reference it is even staged, never merely "in the same barrier".
///
/// A large (chunked) file's recipe and chunks are a separate, still-unbatched exception this
/// finding does not touch: `object_utils::ingest_file`'s `IngestMode::Store` path stores each of
/// them immediately (`flush_chunk_batch`/the recipe's own `store()`), still from within this same
/// concurrent task. Out of scope here — chunking has its own design (§9.4b) and its own
/// parallelism (`fanout_utils::fanout_map`, not this walk's per-directory `TaskExecutor`) — and
/// `build_item_and_object_for_file` only ever returns a blob (the `Option<LooseObject>` above) for
/// a small file to begin with; a chunked file's `object` is always `None` here.
///
/// # Arguments
/// * `context`         - The task context.
/// * `path`            - The warehouse path of the directory.
/// * `ignore_rules`    - The ignore rules. A user rule skips only untracked entries, so a tracked
/// entry a rule matches is refreshed like any other, never staged for removal.
/// * `registered`      - The registered directory keys under the load's root, taken before the
/// walk started: what decides whether a directory is tracked.
///
/// # Returns
/// * `Ok(())`      - If the directory was classified successfully.
/// * `Err(String)` - If there was an error during the operation.
fn build_inventory(context: Arc<InventoryBuilderContext>,
                   path: Arc<WarehousePath>,
                   ignore_rules: Arc<file_utils::IgnoreRules>,
                   registered: Arc<BTreeSet<String>>,
                   parent_coverage: file_utils::Coverage) -> impl Future<Output = Result<(), String>> + Send {
    async move {
        // Taken before this task stats anything below, so every entry it records from a stat
        // proves itself against it (see `file_utils::StatAnchor`).
        let anchor = file_utils::take_stat_anchor()?;

        let directory = file_utils::read_directory(&path.to_fs_path())?;

        // Only the walk's root can be ignored here: every other directory was already checked
        // by its parent's per-entry filter below. A folder load never force-tracks, so an
        // untracked root the rules cover adds nothing. A tracked one is walked: its tracked
        // content is refreshed, and its untracked content stays skipped by the filter below.
        // The root is asked as the directory it is, by registration: a stale file entry under its
        // name (a file replaced by this folder) is the parent's business, and the parent's walk
        // stages its removal. Walking the folder for it would only register an empty directory.
        if !path.is_root() && ignore_rules.excludes(path.as_key(), registered.contains(path.as_key()), parent_coverage) {
            return Ok(());
        }

        let coverage = ignore_rules.coverage_below(parent_coverage, path.as_key());

        // The existing inventory of this directory (if any) is the stat cache: entries whose
        // file metadata is unchanged are reused without reading or hashing the file.
        // It is also what says which files are tracked, which the ignore rules depend on, so an
        // unreadable or unparsable shard fails the load instead of being rebuilt from scratch:
        // a rebuild would silently drop every tracked file a rule covers.
        // Its rollup (if any) is captured here too, at task start — the tentative "carry
        // forward" value if this directory's content turns out unchanged.
        // Only a registered directory has one: whatever an unregistered address holds is no
        // baseline (see `ShardLedger`), so the directory is built fresh and published as new.
        let existing_bytes = match registered.contains(path.as_key()) {
            true => file_utils::retrieve_inventory_or_none_by_key(path.as_key())?.1,
            false => None,
        };

        let existing_inventory = match existing_bytes {
            Some(bytes) => {
                let folder = if path.is_root() { "." } else { path.as_key() };
                // Quoted for the shell the user is likely in: POSIX single quotes, or double quotes
                // on Windows (where a path cannot contain one, and cmd.exe ignores single quotes).
                let quoted = if cfg!(windows) {
                    format!("\"{}\"", folder)
                } else {
                    format!("'{}'", folder.replace('\'', "'\\''"))
                };
                let unreadable = |e: String| format!(
                    "The inventory of folder \"{}\" cannot be read: {}. \"forklift restore --staged {}\" \
                    resets it, and every folder below it, to the pallet head, unstaging everything staged \
                    there. Then load the folder again, naming any file an ignore rule covers that you had \
                    loaded by name.",
                    folder, e.trim_end_matches('.'), quoted
                );
                Some(parser::inventory::inventory_parser::parse_inventory(&bytes).map_err(unreadable)?)
            }
            None => None,
        };

        let mut inventory = Inventory::new();
        // Whether any entry needed more than the pure stat-cache fast path (read, rehashed, or
        // newly built) — see the case (a)/(b) split below.
        let mut any_entry_rebuilt = false;

        context.dirty_inventory_paths.lock().await.remove(path.as_key());

        for entry_result in directory {
            let entry = entry_result.map_err(|e|
                format!("Error while reading directory entry: {}", e)
            )?;

            let name = file_utils::get_name_for_file_or_directory(&entry)?;
            let entry_path = path.child(&name);

            // An entry the rules hide is left out of the rebuilt shard either way: a tracked
            // entry of the other type under its name is then staged as removed. So is another
            // working tree's root, whose content is that tree's to track.
            let verdict = rule_verdict(
                &ignore_rules, coverage, existing_inventory.as_ref(),
                &entry, &name, entry_path.as_key(), &registered,
            )?;

            if !matches!(verdict, RuleVerdict::Walk) {
                continue;
            }

            let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
            let item_type = file_utils::get_type_of_dir_entry(&metadata)?;

            if item_type.is_file() {
                let existing_entry = existing_inventory.as_ref()
                    .and_then(|inv| inv.get_item_by_name(&name));

                let index_item = match existing_entry {
                    Some(item) => {
                        let verdict = classify_file_against_entry(
                            &item, &metadata, item_type, &entry.path(), &name, IngestMode::Store, anchor,
                        )?;

                        match verdict {
                            FileVerdict::UnchangedByStat => {
                                // Loading stages the *current* state: a file that is present
                                // on disk is staged as Normal even if it was staged for
                                // removal before (the same way "git add" re-stages a file
                                // after "git rm --cached"). A state flip here (Deleted ->
                                // Normal) is a real content change, caught below by
                                // `inventory_content_matches` comparing `.state` too — no
                                // special-casing needed here.
                                let mut item = (*item).clone();
                                item.state = InventoryItemState::Normal;
                                item
                            }
                            // Storing on the unchanged-by-hash path too keeps load
                            // self-healing: a blob that went missing from the object
                            // store comes back on the next re-load. A chunked file's objects
                            // were already stored during ingest (`object` is `None`); a large
                            // file its entry holds as one plain blob is re-proved by streaming,
                            // so that blob is not healed here.
                            //
                            // Staged, not stored immediately (DESIGN.html §5.0 D item 10, finding
                            // #1): see `context.blob_batch`'s and this task's own doc comments for
                            // why staging straight from a concurrent task is safe here.
                            FileVerdict::UnchangedByHash(fresh, object)
                                | FileVerdict::Modified(fresh, object) => {
                                any_entry_rebuilt = true;
                                if let Some(mut object) = object {
                                    object.store_deferred(&context.blob_batch)?;
                                }
                                fresh
                            }
                        }
                    }
                    None => {
                        any_entry_rebuilt = true;
                        build_inventory_item_from_file_deferred(&entry.path(), name.as_str(), item_type, &context.blob_batch, anchor)?
                    }
                };

                inventory.add_item(index_item);
            } else {
                let new_task = Box::pin(build_inventory(
                    context.clone(),
                    Arc::new(entry_path),
                    Arc::clone(&ignore_rules),
                    Arc::clone(&registered),
                    coverage,
                ));

                context.send_task(new_task)?;
            }
        }

        // Entries of the old inventory whose file is no longer in the directory (deleted,
        // renamed, or replaced by a directory) are carried over as staged
        // removals — this is the "present only in the shard → Deleted" half of the
        // per-directory merge-join. A real content change (a state flip to `Deleted`), caught
        // the same way as any other by `inventory_content_matches` below.
        if let Some(old_inventory) = existing_inventory.as_ref() {
            carry_over_missing_entries_as_deleted(old_inventory, &mut inventory);
        }

        let key = path.as_key().to_string();

        let content_unchanged = existing_inventory.as_ref()
            .is_some_and(|old_inventory| inventory_content_matches(&inventory, old_inventory));

        // Every entry hit the pure stat-cache fast path and the entry set is unchanged: literally
        // nothing here differs from what is already on disk (not even stat data), so nothing is
        // written. Otherwise the shard is rewritten — with its rollup kept when only stat data
        // drifted or an entry was re-verified to the same content, and every rollup above it
        // cleared when its content changed, as the join point's publish decides.
        if !content_unchanged || any_entry_rebuilt {
            context.writes.lock().await.insert(key, inventory);
        }

        // Registered only once this directory's content is decided: registration is what makes
        // a directory tracked, so a task that failed partway must not leave its key registered
        // with nothing decided for it. (A descendant whose task did finish still registers it,
        // through the ancestor closure: it is then the ancestor of tracked content.)
        context.new_inventory_paths.lock().await.insert(path.as_key().to_string());

        Ok(())
    }
}

/// Build an inventory item for a file whose blob is already stored (its hash is known,
/// e.g. from a tree object): only the file's metadata is gathered, nothing is read or
/// hashed. Used when repopulating the inventory after materializing a tree.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `hash`      - The (already known) blob or recipe hash of the file's content.
/// * `item_type` - The entry type from the authoritative source (the tree / merge action). It is
///   **not** re-derived from `stat`: a `stat` cannot tell a chunked file from a plain one
///   (chunking is a storage choice), so the tree's `NormalChunked`/`ExecutableChunked` must be
///   carried through here or the next stack would emit a wrong (plain) tree entry over a recipe
///   hash. For a plain file this equals what `stat` reports; for a symlink it is `SymbolicLink`.
/// * `anchor`    - Taken before this stat, and after whatever made `hash` true of the file (see
///   [`file_utils::StatAnchor`]). An mtime it does not prove is recorded as unproven.
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item.
/// * `Err(String)`       - If the file's metadata could not be gathered.
pub fn build_inventory_item_from_stat(path: &Path,
                                      name: &str,
                                      hash: String,
                                      item_type: DirEntryType,
                                      anchor: file_utils::StatAnchor) -> Result<InventoryItem, String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;

    let (device_id, inode) = file_utils::get_device_and_inode(path, &metadata)?;

    let mtime = anchor.record(device_id, file_utils::get_content_modification_timestamp_for_file(&metadata)?);
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);

    Ok(
        InventoryItem {
            metadata_change_timestamp: ctime,
            content_change_timestamp: mtime,
            device: device_id,
            inode,
            item_type,
            user_id,
            group_id,
            file_size: metadata.len(),
            hash,
            file_name_length: name.len() as u64,
            state: InventoryItemState::Normal,
            name: String::from(name),
        }
    )
}

/// Whether the directory at `path` may be safely replaced (by a file, or cleared to make
/// way for one) without losing data: it is tracked — its key is registered, see
/// [`ShardLedger::holds`] — and every entry beneath it, recursively, is tracked too, so
/// removing the tracked files leaves it empty. Called for a path a merge or shift
/// wants to write a new file to that already exists as a directory on disk (a tracked
/// dir→file flip); the caller still refuses when this returns `false`, exactly as it does
/// for a plain untracked file at the path.
///
/// # Arguments
/// * `ledger` - The registered directory keys, read by the caller.
/// * `path`   - The warehouse path of the directory (assumed to exist on disk).
///
/// # Returns
/// * `Ok(true)`    - The directory is tracked and has no untracked content beneath it.
/// * `Ok(false)`   - The directory is untracked, or has untracked content beneath it.
/// * `Err(String)` - If a directory entry or a shard could not be read or parsed.
pub fn directory_is_safe_to_replace(ledger: &ShardLedger, path: &str) -> Result<bool, String> {
    if !ledger.holds(path) {
        return Ok(false);
    }

    let registered: BTreeSet<String> = ledger.keys().map(str::to_string).collect();

    let ignore_rules = file_utils::get_ignore_rules()?;

    directory_has_no_untracked_content(path, &ignore_rules, &registered)
}

/// Recursively check a tracked directory for untracked content (the body of
/// `directory_is_safe_to_replace`). Every untracked entry blocks, ignored or not: applying
/// removes only tracked files, so anything else would keep the directory from being replaced,
/// and an ignore rule can hide another working tree's root, which must never be deleted. A
/// tracked entry a rule matches is checked like any other.
///
/// # Arguments
/// * `key`          - The warehouse path key of the directory.
/// * `ignore_rules` - The ignore rules, read once by the caller.
/// * `registered`   - The registered directory keys, read once by the caller.
fn directory_has_no_untracked_content(key: &str,
                                      ignore_rules: &file_utils::IgnoreRules,
                                      registered: &BTreeSet<String>) -> Result<bool, String> {
    let fs_path = if key.is_empty() { std::path::PathBuf::from(".") } else { std::path::PathBuf::from(key) };

    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;
    let inventory = match bytes_opt {
        Some(bytes) => parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?,
        None => Inventory::new(),
    };

    let coverage = ignore_rules.coverage_of(key);

    for entry_result in file_utils::read_directory(&fs_path)? {
        let entry = entry_result.map_err(|e| format!("Error while reading directory entry: {}", e))?;
        let name = file_utils::get_name_for_file_or_directory(&entry)?;
        let entry_key = if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) };

        // Ignored content, an entry that took over a tracked name as the other type, and another
        // working tree's root are all untracked.
        if !matches!(rule_verdict(ignore_rules, coverage, Some(&inventory), &entry, &name, &entry_key, registered)?, RuleVerdict::Walk) {
            return Ok(false);
        }

        let metadata = file_utils::get_symlink_metadata_for_path(&entry.path())?;
        let item_type = file_utils::get_type_of_dir_entry(&metadata)?;

        // Tracked as what it is on disk now: a file by its live entry, a directory by its
        // registration and, recursively, its content.
        let is_tracked = if item_type.is_file() {
            has_live_entry(&inventory, &name)
        } else {
            registered.contains(&entry_key) && directory_has_no_untracked_content(&entry_key, ignore_rules, registered)?
        };

        if !is_tracked {
            return Ok(false);
        }
    }

    Ok(true)
}

/// Refresh every *tracked* entry of the inventory from the working directory: modified
/// files are re-hashed (their blobs stored) and re-staged, files gone from disk become
/// staged removals. Untracked files are deliberately left alone — this is `park`'s way of
/// staging the whole work in progress without swallowing untracked content.
///
/// Decides every shard's new content first, without writing any of it (DESIGN.html §5.0 D item
/// 10, finding #3) — the new content of every shard that needs rewriting, as `load`'s parallel
/// walk decides per directory — then publishes through [`shard_write::publish_shards`], the
/// funnel shared with `load`'s own `create_inventory_for_directory`. It decides every rollup from
/// all of this pass's shards at once, so a shard whose stat data alone drifted never keeps a
/// rollup over the ancestor clear a sibling's real content change requires, whichever of the two
/// comes first in metadata order (finding #1).
///
/// Each decision takes its stat anchor at its start, before it stats anything, so every entry it
/// rebuilds proves itself (see [`file_utils::StatAnchor`]); when the shard is later published
/// makes no difference to what its entries are trusted for. A file whose stat missed is
/// classified the way `load`'s walk does (see [`classify_file_against_entry`]), in its entry's
/// representation, so an unchanged file keeps its storage and is never restaged as a change of
/// storage alone.
///
/// Every rebuilt file's blob is staged into its own batch, finished on its own — durable —
/// strictly before `shard_write::publish_shards` stages any shard content (DESIGN.html §5.0 D
/// item 10, finding #7; see its doc comment for why the two must not share one batch).
///
/// Shards are decided in parallel, and taken up in metadata order. If deciding one fails (an
/// unreadable file, a corrupt shard), the first such shard in that order ends the pass there:
/// neither it nor any shard after it is published, though those were decided too, and any blob they
/// rebuilt was staged and becomes durable with the batch, unreferenced. That failure alone does not
/// cost the whole pass: every shard *before* it in metadata order is still handed to the publish
/// step below, the same keep-whatever-was-decided resilience `create_inventory_for_directory` gives
/// `load` (see its own doc comment). But every rebuilt file's blob this pass stages shares one
/// batch, and if finishing that batch fails — including the leaked-reservation case where a
/// producer won a reservation but never finished staging it — `WriteBatch::finish` refuses to
/// publish the whole batch (see the blob-batch comment below), so nothing this pass decided is
/// published, not just the shards after that failure. Because every shard is decided, a shard
/// *after* the first failure can be the one that leaks, and then the shards before the failure
/// are not published either. A retry after a decide-loop failure only has to redo the shards from
/// the failure onward when the batch finished; otherwise it has to redo the whole tracked set.
///
/// # Returns
/// * `Ok(())`      - If the refresh completed.
/// * `Err(String)` - If a shard or file could not be processed, or the blob batch failed to
///                   publish. Every shard before the first failure in metadata order is handed
///                   to the publish step when the blob batch finishes; a blob-batch failure
///                   publishes nothing from this pass — see the doc comment above.
pub fn refresh_tracked_entries() -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    // Every key this pass decides is one it read from the metadata.
    let mut ledger = ShardLedger { entries: metadata.clone() };

    // Pass 1: decide every shard's new content without writing any of it yet — see the function's
    // own doc comment for why this mirrors `load`'s per-directory decisions rather than the old
    // immediate-write loop. Every rebuilt file's blob is staged into one shared batch instead of
    // paying its own atomic-write barrier, finished below before any decision is published.
    let blob_batch = file_utils::WriteBatch::new();
    let mut writes: BTreeMap<String, Inventory> = BTreeMap::new();

    // The first failure in metadata order, if any — recorded instead of propagated immediately
    // (see the function's own doc comment on why: every shard before it in metadata order must
    // still reach the publish step below).
    let mut result: Result<(), String> = Ok(());

    // Every shard is decided across every core; each decision is independent of the others (its
    // own anchor, its own shard, its own files), and they are taken up below in metadata order.
    let keys: Vec<&str> = metadata.iter().map(|entry| metadata_entry_to_key(entry)).collect();
    let decisions = fanout_utils::fanout_map(&keys, |key| decide_tracked_refresh(key, &blob_batch));

    for (key, decision) in keys.iter().zip(decisions) {
        match decision {
            // Whether only stat data went stale (the rollup kept) or content changed (every
            // rollup above cleared) is the publish's to decide, against the shard on disk.
            Ok(Some(inventory)) => { writes.insert(key.to_string(), inventory); }
            Ok(None) => {}
            // The first failure in metadata order ends the pass there: no shard after it is
            // published, though the fan-out decided them too (see the function's doc comment).
            Err(e) => { result = Err(e); break; }
        }
    }

    // Every rebuilt file's blob decided above becomes durable now, in its own barrier — strictly
    // before `publish_shards` below even stages any shard content (DESIGN.html §5.0 D
    // item 10, finding #7): a published shard's entries may carry one of these blobs' hashes, so
    // the blob must already be durable — not merely staged — before that shard's rename can land.
    //
    // A failure here skips `publish_shards` entirely, rather than the "keep whatever was
    // decided" resilience the decide loop above gives a per-shard failure. `WriteBatch::finish` can
    // fail more than one way (see its own `# Returns`; `run_write_barrier` for why a rename-partway
    // or trailing-sync failure is not all-or-nothing) — no failure mode leaves a per-hash way to
    // tell which of this pass's blobs actually landed, and some leave a subset durable and the rest
    // not, so publishing any shard content below could durably name a blob that never actually
    // landed — see `create_inventory_for_directory`'s identical phase-0 gating for the same
    // reasoning (and the leaked-reservation blast radius) in more detail. The decisions run
    // concurrently, so a shard decided cleanly can name a blob whose reservation another decision
    // won and then failed to stage; that leak fails `finish`, which keeps this pass from
    // publishing the clean shard over a blob that never landed.
    let blob_batch_published = match blob_batch.finish() {
        Ok(()) => true,
        Err(e) => { if result.is_ok() { result = Err(e); } false }
    };

    // Publish every shard this pass decided to rewrite — see `shard_write::publish_shards`'s doc
    // comment for the exact ordering (and how it closes finding #1, per this function's own doc
    // comment above).
    if blob_batch_published {
        if let Err(e) = publish_shards(&mut ledger, ShardPublish::of(writes)) {
            if result.is_ok() { result = Err(e); }
        }
    }

    result
}

/// Decide one tracked shard's refreshed content for [`refresh_tracked_entries`], writing nothing
/// but staging every rebuilt file's blob into `blob_batch`.
///
/// # Returns
/// * `Ok(Some(Inventory))` - The shard's new content, when any entry changed (stat data included).
/// * `Ok(None)`            - When nothing about the shard changed, or it has no shard file.
/// * `Err(String)`         - If the shard could not be read or parsed, or a file not processed.
fn decide_tracked_refresh(key: &str, blob_batch: &file_utils::WriteBatch) -> Result<Option<Inventory>, String> {
    // Before this decision stats anything below — see `refresh_tracked_entries`'s doc comment.
    let anchor = file_utils::take_stat_anchor()?;

    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else {
        return Ok(None);
    };

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    let names: Vec<String> = inventory.get_items().map(|(name, _)| name.clone()).collect();
    let mut changed = false;

    for name in names {
        let item = inventory.get_item_by_name(&name).unwrap();

        if item.state == InventoryItemState::Deleted {
            continue;
        }

        let file_path = if key.is_empty() {
            std::path::PathBuf::from(&name)
        } else {
            std::path::PathBuf::from(format!("{}/{}", key, name))
        };

        let Ok(metadata) = file_utils::get_symlink_metadata_for_path(&file_path) else {
            // The file is gone from disk: its removal becomes staged.
            inventory.mark_item_deleted(&name);
            changed = true;
            continue;
        };

        let item_type = file_utils::get_type_of_dir_entry(&metadata)?;

        // The entry is rebuilt even when only the stat data went stale (same content), so the
        // refreshed shard keeps the fast path warm. Blobs are staged, not stored immediately — see
        // `refresh_tracked_entries`'s batch comment. Staging an unchanged file's blob too heals one
        // that went missing from the store, as `load` does.
        let rebuilt = match classify_file_against_entry(
            &item, &metadata, item_type, &file_path, &name, IngestMode::Store, anchor,
        )? {
            FileVerdict::UnchangedByStat => continue,
            FileVerdict::UnchangedByHash(rebuilt, object) | FileVerdict::Modified(rebuilt, object) => {
                if let Some(mut object) = object {
                    object.store_deferred(blob_batch)?;
                }
                rebuilt
            }
        };
        changed = true;

        inventory.add_item(rebuilt);
    }

    Ok(changed.then_some(inventory))
}

/// One read-and-parse pass over the inventory shards a tree build can need.
///
/// Built once per `stack` (`prepare_stack_inventory`) and shared by the three steps that used to
/// each read the whole shard set on their own: the conflict check, the tree build and
/// [`cleanup_after_stack_with`]. `park` push (`park::park_changes`) builds its own, single-use one,
/// because `build_tree_from_inventory_deferred` reads shard content from it. Both check
/// [`has_conflict_entries_in`] on it before anything consumes `shards`.
///
/// A shard strictly beneath a skip root of `skip` is never read (see [`prepare_stack_inventory`]),
/// so `shards` covers only the part of the tree that can differ from head.
pub struct PreparedInventory {
    /// `None` when there is no inventory metadata file at all (nothing was ever loaded).
    /// `Some` (possibly empty) mirrors the metadata file's own registered directory keys —
    /// kept distinct from an empty set purely so callers can reproduce the exact behavior of
    /// the original functions where "no file" and "empty file" diverge (e.g.
    /// `cleanup_after_stack_with`'s early return when there was never a file to rewrite).
    pub metadata: Option<BTreeSet<String>>,

    /// Every parsed shard, keyed by its warehouse path key. A registered key outside
    /// `skip.pruned` and absent here has no shard file on disk (a stale metadata entry), and every
    /// consumer treats it as empty. A key in `skip.pruned` other than a skip root is absent
    /// because it was never read.
    ///
    /// Incomplete when `has_conflict` is `true` (see [`prepare_stack_inventory`]). Harmless: both
    /// callers refuse on a conflict before this is consulted again.
    pub shards: BTreeMap<String, Inventory>,

    /// Whether a conflict entry (an unresolved consolidation) was found while parsing. Set at the
    /// *first* read shard, in sorted key order, found to have one.
    pub has_conflict: bool,

    /// Which subtrees the rollup skip proved unchanged from head. Empty without a head, with the
    /// kill switch set, or when no rollup matched.
    pub skip: RollupSkipPlan,

    /// The directories the tree build visits, derived once here from the metadata so the read and
    /// the build walk one relation.
    pub layout: DirectoryLayout,
}

/// Every directory a tree build over the metadata visits: each registered key and all of its
/// ancestors (an ancestor may have no shard of its own), the root `""` included, with the parent
/// → children relation over them in key order. The keys `skip` pruned are left out of both, so a
/// skipped subtree is built as if it were never inventoried. Empty without metadata entries.
#[derive(Default)]
pub struct DirectoryLayout {
    pub(crate) keys: BTreeSet<String>,

    /// Shared with every task of the tree build.
    pub(crate) children: Arc<BTreeMap<String, Vec<String>>>,
}

/// The subtrees a [`PreparedInventory`] proved unchanged from head, because each one's shard
/// carries a rollup equal to the head subtree hash at its key. The tree build never reads, hashes
/// or stores them, and `stack`'s cleanup leaves their shards as they sit.
#[derive(Default)]
pub struct RollupSkipPlan {
    /// Set only when the *entire* staged tree is provably unchanged from head (a fully scoped
    /// bay's root rollup matches the head root tree hash): every registered key is in `pruned`.
    pub whole_tree_hash: Option<String>,

    /// Every skip root and every key beneath one, ancestors-only keys included.
    pub pruned: BTreeSet<String>,

    /// For each *unpruned* directory key, the `(name, head_hash)` pairs of its immediate children
    /// that are skip roots, added to that directory's tree by hash.
    pub injections: BTreeMap<String, Vec<(String, String)>>,
}

/// Read the inventory metadata file and parse every shard a tree build against `skip`'s head can
/// need, each exactly once.
///
/// **What is read.** With `skip` `Some((head_root_tree_hash, scope))` and the rollup skip enabled,
/// the read walks the tree top-down, one depth at a time (each depth fanned out across every core
/// with [`fanout_utils::fanout_map`]). A shard whose rollup equals the head subtree hash at its key
/// becomes a skip root, and nothing beneath it is read: every shard write goes through a funnel
/// of [`shard_write`], which clears the rollup of a shard and of every ancestor before it writes
/// any change of content (an entry's name, type, hash or state), so a matching rollup proves that
/// no shard beneath it holds a staged change, a staged removal or a conflict entry. A shard
/// beneath a skip root that would not parse is therefore not reported either: its content is not
/// needed, and nothing reads it. A skip root is only taken where its parent is fully in scope —
/// see the scoped-bay caveat below. Without `skip`, or with the kill switch set
/// (`FORKLIFT_DISABLE_ROLLUP_SKIP`), every registered shard is read.
///
/// **Scoped-bay caveat.** The outermost in-scope boundary directory (whose parent is `Spine`) is
/// never a skip root: a scoped stack's spine splice (`tree_utils::build_scoped_root_tree`) later
/// re-visits exactly that directory as an opaque `TreeItem` and prunes it if it reads as empty,
/// which a hash-only stand-in would, silently dropping a real subtree from the signed tree. A
/// scoped bay's root is `Spine` itself, so the whole-tree skip applies to a full bay only.
///
/// **Priority.** The first conflict or parse error, in sorted key order over the shards read,
/// wins: a real conflict in an earlier shard is never masked by an unrelated corrupt shard later
/// in the set. Every shard is read before that fold runs, so an early conflict does not stop the
/// reads.
///
/// # Arguments
/// * `skip` - `Some((head_root_tree_hash, scope))` to skip the subtrees whose rollup matches that
///            head; `None` (no head yet) to read every registered shard.
///
/// # Returns
/// * `Ok(PreparedInventory)` - The snapshot (empty when there is nothing staged; incomplete, with
///                             `has_conflict` set, when a conflict was found).
/// * `Err(String)`           - If the metadata file, a head tree on the walk, or a read shard at
///                             or before the first conflict in sorted key order could not be
///                             read or parsed.
pub fn prepare_stack_inventory(skip: Option<(&str, &MaterializationScope)>) -> Result<PreparedInventory, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(PreparedInventory {
            metadata: None, shards: BTreeMap::new(), has_conflict: false, skip: RollupSkipPlan::default(),
            layout: DirectoryLayout::default(),
        });
    };

    let registered: BTreeSet<&str> = metadata.iter().map(|entry| metadata_entry_to_key(entry)).collect();
    let (keys, children) = directory_relation(&registered);

    let mut parsed: BTreeMap<String, Result<Option<(Inventory, bool)>, String>> = BTreeMap::new();
    let mut plan = RollupSkipPlan::default();

    match skip.filter(|_| rollup_skip_enabled()) {
        None => {
            let keys: Vec<&str> = registered.iter().copied().collect();
            for (key, result) in keys.iter().zip(fanout_utils::fanout_map(&keys, |key| read_stack_shard(key))) {
                parsed.insert(key.to_string(), result);
            }
        }
        Some((head_root_tree_hash, scope)) => {
            read_unskipped_shards(&registered, &children, head_root_tree_hash, scope, &mut parsed, &mut plan)?;
        }
    }

    let layout = if plan.pruned.is_empty() {
        DirectoryLayout { keys, children: Arc::new(children) }
    } else {
        let children = children.into_iter()
            .filter(|(key, _)| !plan.pruned.contains(key))
            .map(|(key, child_keys)| (key, child_keys.into_iter().filter(|c| !plan.pruned.contains(c)).collect()))
            .collect();

        DirectoryLayout { keys: keys.difference(&plan.pruned).cloned().collect(), children: Arc::new(children) }
    };

    // The deterministic fold: `parsed` is a `BTreeMap`, so walking it stops at the first `Err` or
    // conflict in sorted key order, whatever order the parallel workers finished in.
    let mut shards: BTreeMap<String, Inventory> = BTreeMap::new();

    for (key, result) in parsed {
        match result {
            Ok(None) => {}
            Ok(Some((inventory, has_conflict))) => {
                shards.insert(key, inventory);

                if has_conflict {
                    return Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: true, skip: plan, layout });
                }
            }
            Err(e) => return Err(e),
        }
    }

    Ok(PreparedInventory { metadata: Some(metadata), shards, has_conflict: false, skip: plan, layout })
}

/// Every registered key plus all of its ancestors and the root `""` (none when nothing is
/// registered), and the parent → children relation over them, each child list in key order.
fn directory_relation(registered: &BTreeSet<&str>) -> (BTreeSet<String>, BTreeMap<String, Vec<String>>) {
    let mut keys: BTreeSet<String> = BTreeSet::new();

    for &registered_key in registered {
        let mut key = registered_key;

        while keys.insert(key.to_string()) && !key.is_empty() {
            key = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).map(|(parent, _)| parent).unwrap_or("");
        }
    }

    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();

    for key in keys.iter().filter(|key| !key.is_empty()) {
        let parent = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).map(|(parent, _)| parent).unwrap_or("");
        children.entry(parent.to_string()).or_default().push(key.clone());
    }

    (keys, children)
}

/// The first path below the top level that `stack` would record in a tree and that names a metadata
/// folder in some component (see [`office_utils::opens_metadata_folder`]): checking it out would
/// plant another working tree. Only the shards `prepared` read are asked, each one holding a live
/// entry: a subtree the rollup skip proved unchanged was checked by the stack that recorded it, and
/// a directory with no live entry beneath it is in no tree.
///
/// # Arguments
/// * `prepared` - The stack's inventory snapshot.
///
/// # Returns
/// * `Ok(Some(path))` - The first such path, in key order.
/// * `Ok(None)`       - If there is none.
/// * `Err(String)`    - If the volume could not say for some name.
pub fn nested_metadata_entry(prepared: &PreparedInventory) -> Result<Option<String>, String> {
    for (key, shard) in &prepared.shards {
        let live: Vec<&str> = shard.get_items()
            .filter(|(_, item)| item.state != InventoryItemState::Deleted)
            .map(|(name, _)| name.as_str())
            .collect();

        if live.is_empty() || key.is_empty() {
            continue;
        }

        // The top-level component is `stack`'s own root-entry check.
        for (slash, _) in key.match_indices('/') {
            let parent = &key[..slash];
            let end = key[slash + 1..].find('/').map_or(key.len(), |next| slash + 1 + next);

            if office_utils::opens_metadata_folder(Path::new(parent), &key[slash + 1..end])? {
                return Ok(Some(key[..end].to_string()));
            }
        }

        for name in live {
            if office_utils::opens_metadata_folder(Path::new(key), name)? {
                return Ok(Some(format!("{}/{}", key, name)));
            }
        }
    }

    Ok(None)
}

/// Read and parse one registered shard for [`prepare_stack_inventory`] and
/// [`has_conflict_entries`], noting whether it holds a conflict entry. `Ok(None)` when the shard
/// file does not exist.
fn read_stack_shard(key: &str) -> Result<Option<(Inventory, bool)>, String> {
    let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

    let Some(bytes) = bytes_opt else { return Ok(None); };

    let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    let has_conflict = inventory.get_items().any(|(_, item)| matches!(
        item.state,
        InventoryItemState::FirstParentConflict
            | InventoryItemState::SecondParentConflict
            | InventoryItemState::ThirdParentConflict
    ));

    Ok(Some((inventory, has_conflict)))
}

/// The top-down walk behind [`prepare_stack_inventory`]'s skip: read one depth of the tree at a
/// time, decide which of its keys are skip roots, and descend only into the rest.
fn read_unskipped_shards(registered: &BTreeSet<&str>,
                         children: &BTreeMap<String, Vec<String>>,
                         head_root_tree_hash: &str,
                         scope: &MaterializationScope,
                         parsed: &mut BTreeMap<String, Result<Option<(Inventory, bool)>, String>>,
                         plan: &mut RollupSkipPlan) -> Result<(), String> {
    let scope_is_full = scope.is_full();
    let in_scope = |key: &str| scope_is_full || scope.classify(key) == ScopeClass::InScope;

    // (key, the head subtree hash at key if head has a subtree there, whether key's parent is
    // fully in scope). The root has no parent: a whole-tree skip needs a full bay instead.
    let mut frontier: Vec<(&str, Option<String>, bool)> =
        vec![("", Some(head_root_tree_hash.to_string()), scope_is_full)];

    while !frontier.is_empty() {
        let to_read: Vec<&str> = frontier.iter().map(|(key, _, _)| *key).filter(|key| registered.contains(key)).collect();

        for (key, result) in to_read.iter().zip(fanout_utils::fanout_map(&to_read, |key| read_stack_shard(key))) {
            parsed.insert(key.to_string(), result);
        }

        // Keys the walk descends below, with their head subtree hash.
        let mut descend: Vec<(&str, Option<String>)> = Vec::new();

        for (key, head_hash, parent_in_scope) in frontier {
            let rollup = match parsed.get(key) {
                Some(Ok(Some((inventory, _)))) => inventory.get_rollup_hash().cloned(),
                _ => None,
            };

            if let (true, Some(head_hash), Some(rollup)) = (parent_in_scope, &head_hash, &rollup) {
                if rollup == head_hash {
                    prune_keys_under(key, children, &mut plan.pruned);
                    record_rollup_skip();

                    if key.is_empty() {
                        plan.whole_tree_hash = Some(rollup.clone());
                        return Ok(());
                    }

                    let (parent, name) = key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).unwrap_or(("", key));
                    plan.injections.entry(parent.to_string()).or_default().push((name.to_string(), rollup.clone()));
                    continue;
                }
            }

            if children.contains_key(key) {
                descend.push((key, head_hash));
            }
        }

        // The head trees of one level load in parallel, so a stack that prunes nothing (every
        // rollup cleared) costs a parallel pass per level rather than one serial load per key.
        let head_trees = fanout_utils::fanout_map(&descend, |(_, head_hash)| match head_hash {
            Some(hash) => object_utils::load_tree_shared(hash).map(Some),
            None => Ok(None),
        });
        let mut next = Vec::new();

        for ((key, _), head_tree) in descend.iter().zip(head_trees) {
            let head_tree = head_tree?;
            let head_subtrees: BTreeMap<&str, &str> = match &head_tree {
                Some(tree) => tree.get_subtrees().map(|(name, item)| (name.as_str(), item.hash.as_str())).collect(),
                None => BTreeMap::new(),
            };
            let key_in_scope = in_scope(key);

            for child_key in children[*key].iter().map(String::as_str) {
                let child_name = child_key.rsplit_once(file_utils::PATH_SEPARATOR_CHAR).map(|(_, name)| name).unwrap_or(child_key);
                next.push((child_key, head_subtrees.get(child_name).map(|hash| hash.to_string()), key_in_scope));
            }
        }

        frontier = next;
    }

    Ok(())
}

/// Add `key` and every key beneath it (through `children`) to `pruned`.
fn prune_keys_under(key: &str, children: &BTreeMap<String, Vec<String>>, pruned: &mut BTreeSet<String>) {
    pruned.insert(key.to_string());

    if let Some(child_keys) = children.get(key) {
        for child_key in child_keys {
            prune_keys_under(child_key, children, pruned);
        }
    }
}

/// Check whether any inventory entry is in a conflict state (an unresolved consolidation).
///
/// Every shard is read and parsed across every core, then judged in key order, so the answer is a
/// serial scan's: a conflict in a shard before the first unreadable one in key order is reported,
/// and an unreadable shard before any conflict is an error.
///
/// # Returns
/// * `Ok(bool)`    - Whether at least one entry is in conflict.
/// * `Err(String)` - If a shard could not be read or parsed.
pub fn has_conflict_entries() -> Result<bool, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(false);
    };

    let keys: Vec<&str> = metadata.iter().map(|entry| metadata_entry_to_key(entry)).collect();

    // Each worker keeps only its answer, never the parsed shard.
    let answers = fanout_utils::fanout_map(&keys, |key| {
        read_stack_shard(key).map(|shard| shard.is_some_and(|(_, has_conflict)| has_conflict))
    });

    for has_conflict in answers {
        if has_conflict? {
            return Ok(true);
        }
    }

    Ok(false)
}


/// Like [`has_conflict_entries`], but against an already-parsed [`PreparedInventory`] snapshot
/// instead of reading and parsing the shards again. Used by `stack` (`stack_utils::stack_parcel`),
/// which must still abort before any warehouse mutation when this is true.
///
/// A thin accessor: [`prepare_stack_inventory`] already determines this — and stops scanning as
/// soon as it does, so a real conflict is never masked by a later shard's parse error — so there
/// is nothing left to scan here.
///
/// # Returns
/// * `true`  - At least one entry is in conflict.
/// * `false` - No entry is in conflict.
pub fn has_conflict_entries_in(prepared: &PreparedInventory) -> bool {
    prepared.has_conflict
}

/// List the warehouse paths of every inventory entry in a conflict state (an
/// unresolved consolidation), sorted. The counterpart of [`has_conflict_entries`] for
/// callers that need the paths themselves (the `conflicts` command).
///
/// # Returns
/// * `Ok(Vec<String>)` - The conflicted paths (empty when there are none).
/// * `Err(String)`     - If a shard could not be read or parsed.
pub fn list_conflict_paths() -> Result<Vec<String>, String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(Vec::new());
    };

    let mut paths = Vec::new();

    for entry in &metadata {
        let key = metadata_entry_to_key(entry);
        let (_, bytes_opt) = file_utils::retrieve_inventory_or_none_by_key(key)?;

        let Some(bytes) = bytes_opt else {
            continue;
        };

        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

        for (name, item) in inventory.get_items() {
            let is_conflict = matches!(
                item.state,
                InventoryItemState::FirstParentConflict
                    | InventoryItemState::SecondParentConflict
                    | InventoryItemState::ThirdParentConflict
            );

            if is_conflict {
                paths.push(if key.is_empty() { name.clone() } else { format!("{}/{}", key, name) });
            }
        }
    }

    paths.sort();

    Ok(paths)
}

/// Build a "stale" inventory item: the hash and type are known (e.g. from a head tree),
/// but the stat fields are zeroed and its mtime is [`UNPROVEN_MTIME`] on purpose, so the stat
/// cache can never trust the entry
/// — the next comparison against the working directory always rehashes the file. Used by
/// `restore --staged`, where the file on disk may not match the recorded hash.
///
/// # Arguments
/// * `name`      - The name of the file.
/// * `hash`      - The blob hash of the entry's content.
/// * `item_type` - The type of the entry.
///
/// # Returns
/// * `InventoryItem` - The stale inventory item.
pub fn build_stale_inventory_item(name: &str, hash: String, item_type: DirEntryType) -> InventoryItem {
    InventoryItem {
        metadata_change_timestamp: 0,
        content_change_timestamp: UNPROVEN_MTIME,
        device: 0,
        inode: 0,
        item_type,
        user_id: 0,
        group_id: 0,
        file_size: 0,
        hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    }
}

impl ShardLedger {
    /// Load the inventory shard for the given key (or an empty one), apply the given change,
    /// and save it back, registering the key first if it is not yet.
    ///
    /// # Arguments
    /// * `key`    - The warehouse path key of the directory.
    /// * `change` - The change to apply to the inventory.
    ///
    /// # Returns
    /// * `Ok(())`      - If the shard was updated.
    /// * `Err(String)` - If the shard could not be read, parsed or written.
    pub fn update_shard(&mut self,
                        key: &str,
                        change: impl FnOnce(&mut Inventory) -> Result<(), String>) -> Result<(), String> {
        let mut inventory = self.inventory(key)?.unwrap_or_else(Inventory::new);

        change(&mut inventory)?;

        publish_shards(self, ShardPublish {
            register: BTreeSet::from([key.to_string()]),
            ..ShardPublish::one(key, inventory)
        })
    }
}

/// Remove one entry of the shard store: a folder with everything in it, or a file or link in a
/// folder's place. A link is removed itself, never what it points at. An entry already gone is not
/// an error.
fn remove_store_entry(path: &Path) -> std::io::Result<()> {
    let removed = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        // Windows removes a link to a folder (or a junction) only as a folder.
        Ok(metadata) if metadata.is_symlink() => std::fs::remove_file(path)
            .or_else(|e| std::fs::remove_dir(path).map_err(|_| e)),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };

    match removed {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// Delete every shard folder in the store whose address is not a registered key's, after a
/// subtree removal has written its metadata. No writer of this build leaves such a shard: every
/// shard is published only at a key already registered (see [`ShardLedger`]). One can still be
/// there — from damage done outside Forklift, from a store written before keys were registered
/// first, from a power loss while fsync was off, or from a power loss that undid an unsynced
/// unlink of a shard whose key was then unregistered — so not every such shard is damage this
/// build caused, and none is read: a writer reads only through the ledger, and registering a key
/// clears its address first. Sweeping keeps the store from holding them indefinitely.
///
/// Safe because every shard-writing command holds the warehouse lock, and a caller sweeps only
/// after its own metadata write: no shard published by a command still running is unregistered
/// by then. Nothing staged is lost: an unregistered shard holds nothing staged.
///
/// Best-effort, and always its caller's last step: by the time it runs the caller's own change
/// has landed, so failing the command would leave the state cleared after it (a recorded merge, a
/// load marker, a narrowed subtree's files) behind for nothing. An entry it cannot read or remove
/// stays where it is, as every ghost does until a sweep reaches it, and the next subtree removal
/// tries again.
fn sweep_unregistered_shards() {
    let Ok((_, metadata_opt)) = file_utils::retrieve_inventory_metadata_or_none() else {
        return;
    };

    let registered: BTreeSet<(String, String)> = metadata_opt.unwrap_or_default().iter()
        .map(|entry| file_utils::shard_address(metadata_entry_to_key(entry)))
        .collect();

    let Ok(store_root) = file_utils::get_inventory_store_root() else {
        return;
    };

    // A link in place of the store is damage that only a wholesale reset repairs; listing through
    // it would sweep whatever it points at.
    if !std::fs::symlink_metadata(&store_root).is_ok_and(|meta| meta.is_dir()) {
        return;
    }

    let read = |folder: &Path| -> Vec<std::fs::DirEntry> {
        std::fs::read_dir(folder).map(|entries| entries.filter_map(Result::ok).collect()).unwrap_or_default()
    };

    for fan_out in read(&store_root) {
        // Only real folders belong at the fan-out level; anything else would block every shard
        // whose address starts with its name. A link is removed itself and never entered, so
        // nothing it points at is swept.
        if !fan_out.file_type().is_ok_and(|kind| kind.is_dir()) {
            let _ = remove_store_entry(&fan_out.path());
            continue;
        }

        let fan_out_name = fan_out.file_name().to_string_lossy().into_owned();

        for shard in read(&fan_out.path()) {
            let address = (fan_out_name.clone(), shard.file_name().to_string_lossy().into_owned());

            if !registered.contains(&address) {
                let _ = remove_store_entry(&shard.path());
            }
        }
    }
}

/// Carry over the entries of the previous inventory that were not re-added by the directory
/// walk (their file was deleted, renamed, or replaced by a directory),
/// marking them as staged removals.
///
/// # Arguments
/// * `old_inventory` - The inventory of the previous load.
/// * `new_inventory` - The inventory being rebuilt from the working directory.
fn carry_over_missing_entries_as_deleted(old_inventory: &Inventory, new_inventory: &mut Inventory) {
    let missing_items: Vec<InventoryItem> = old_inventory.get_items()
        .filter(|(name, _)| new_inventory.get_item_by_name(name).is_none())
        .map(|(_, item)| (**item).clone())
        .collect();

    for mut item in missing_items {
        item.state = InventoryItemState::Deleted;
        new_inventory.add_item(item);
    }
}

/// Drop keys from the inventory metadata file (a text file that contains the paths of all
/// inventoried directories, sorted alphabetically) in a single write. Registering goes through
/// [`ShardLedger`] instead, which clears a new key's address first.
///
/// # Arguments
/// * `keys_to_remove` - Warehouse path keys of directories to unregister.
///
/// # Returns
/// * `Ok(())`      - If the metadata was successfully updated.
/// * `Err(String)` - If an error occurred while updating the metadata.
fn unregister_inventory_keys(keys_to_remove: &BTreeSet<String>) -> Result<(), String> {
    let (metadata_path, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;
    let mut metadata = metadata_opt.unwrap_or(BTreeSet::new());

    for key in keys_to_remove {
        metadata.remove(&key_to_metadata_entry(key));
    }

    write_metadata_to_file(&metadata_path, &metadata)
}

/// Save the inventory metadata to file.
///
/// The set is closed under ancestors before it is written: every ancestor of a registered key is
/// registered too, so every directory between the root and a tracked file is tracked (see
/// [`ShardLedger::holds`]). Every metadata write passes through here, so no writer can break
/// that, whether it registers only the directory it wrote (single-file load,
/// [`ShardLedger::update_shard`]) or drops a key a registered descendant still needs (the removal
/// paths).
///
/// # Arguments
/// * `path`     - The path of the file where the metadata should be saved.
/// * `metadata` - Inventory metadata. A `BTreeSet` consisting of paths of directories where
/// inventories are stored.
///
/// # Returns
/// * `Ok(())`      - If the metadata file was saved successfully.
/// * `Err(String)` - If there was an error while saving the metadata file.
fn write_metadata_to_file(path: &Path, metadata: &BTreeSet<String>) -> Result<(), String> {
    #[cfg(test)]
    if METADATA_WRITE_FAULT.with(|armed| armed.get()) {
        return Err(format!("injected metadata-write failure for \"{}\"", path.to_string_lossy()));
    }

    let closed = close_under_ancestors(metadata);

    let metadata_bytes = record_utils::to_bytes(
        file_utils::INVENTORY_METADATA_RECORD, file_utils::INVENTORY_METADATA_VERSION, &closed
    );

    // Replaced atomically, so a crash mid-write leaves the old metadata and never a torn file
    // the reader refuses. Not `write_file_atomically`, which only stages a write inside a bulk
    // store session, where a later read in the same command would still see the old metadata.
    file_utils::replace_file_atomically(path, &metadata_bytes)
}

#[cfg(test)]
thread_local! {
    /// Test-only fault injection for [`write_metadata_to_file`]: while armed, every metadata write
    /// on this thread fails before it touches the file, so the metadata keeps what it held.
    static METADATA_WRITE_FAULT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// RAII scope for [`METADATA_WRITE_FAULT`]: armed on construction, disarmed on `Drop`.
#[cfg(test)]
struct MetadataWriteFaultGuard;

#[cfg(test)]
impl MetadataWriteFaultGuard {
    fn failing() -> Self {
        METADATA_WRITE_FAULT.with(|armed| armed.set(true));
        MetadataWriteFaultGuard
    }
}

#[cfg(test)]
impl Drop for MetadataWriteFaultGuard {
    fn drop(&mut self) {
        METADATA_WRITE_FAULT.with(|armed| armed.set(false));
    }
}

/// Close a set of metadata entries under ancestors: add the entry of every ancestor of every
/// entry's key, the root included.
///
/// # Arguments
/// * `metadata` - Metadata entries (see [`key_to_metadata_entry`]).
///
/// # Returns
/// * `BTreeSet<String>` - The entries plus the entries of all their ancestors.
fn close_under_ancestors(metadata: &BTreeSet<String>) -> BTreeSet<String> {
    let mut closed = metadata.clone();

    for entry in metadata {
        for ancestor in ancestor_keys_root_first(metadata_entry_to_key(entry)) {
            closed.insert(key_to_metadata_entry(&ancestor));
        }
    }

    closed
}

/// The registered set of directory keys — the inventory metadata, closed under ancestors — and the
/// only way to write a shard's content. Every shard-write funnel ([`shard_write`]) takes one and
/// refuses a key it does not hold, so a writer must register a key before it publishes the
/// key's shard. A shard on disk whose key is not registered is therefore never one this build
/// published, and is never read as anything: [`retrieve`](Self::retrieve) answers `None` for an
/// unregistered key without opening its address, so no writer adopts what an unregistered address
/// holds as its baseline.
///
/// Registering a key first clears its address and syncs every folder on it (see
/// [`register`](Self::register)), so a shard left there by anything — damage, a store written
/// before registration came first, an unlink a power loss undid — is gone, durably, before the key
/// becomes registered. A failure after the registration and before the shard lands leaves a
/// registered key with no shard, which reads as an empty directory: nothing staged is lost or
/// invented, and nothing needs rolling back.
///
/// Every shard-writing command holds the warehouse lock, so a ledger read once stays the registered
/// set for as long as its holder is the only writer. A holder that writes the metadata some other
/// way, or hands control to code that does, reads a fresh one afterwards.
pub struct ShardLedger {
    entries: BTreeSet<String>,
}

thread_local! {
    /// How many times this thread has read the ledger: a cheap, always-on observability hook the
    /// tests use to prove a command reads it once, not once per path — see [`ledger_read_count`].
    static LEDGER_READS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// How many times this thread has read the ledger ([`ShardLedger::read`]). Per thread, so a test
/// counts only its own command's reads while other tests run beside it.
pub fn ledger_read_count() -> u64 {
    LEDGER_READS.with(std::cell::Cell::get)
}

/// A [`ShardLedger`] for a holder that may never need one: read on first use, or lent by a caller
/// that already read it, so one command reads the metadata once however many holders it passes
/// the registered set through.
pub(crate) enum LazyLedger<'l> {
    /// Not read yet.
    Unread,
    /// Read by this holder.
    Read(ShardLedger),
    /// Lent by the caller.
    Lent(&'l ShardLedger),
}

impl LazyLedger<'_> {
    /// The registered set, read now if it has not been.
    ///
    /// # Returns
    /// * `Ok(&ShardLedger)` - The registered set.
    /// * `Err(String)`      - If the metadata could not be read or parsed.
    pub(crate) fn get(&mut self) -> Result<&ShardLedger, String> {
        if let LazyLedger::Unread = self {
            *self = LazyLedger::Read(ShardLedger::read()?);
        }

        Ok(match self {
            LazyLedger::Read(ledger) => ledger,
            LazyLedger::Lent(ledger) => ledger,
            LazyLedger::Unread => unreachable!("read above"),
        })
    }
}

impl ShardLedger {
    /// Read the registered set from the inventory metadata. A missing metadata file is an empty
    /// set.
    ///
    /// # Returns
    /// * `Ok(ShardLedger)` - The registered set.
    /// * `Err(String)`     - If the metadata could not be read or parsed.
    pub fn read() -> Result<ShardLedger, String> {
        LEDGER_READS.with(|reads| reads.set(reads.get() + 1));

        let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

        Ok(ShardLedger { entries: metadata_opt.unwrap_or_default() })
    }

    /// Every registered key, sorted.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|entry| metadata_entry_to_key(entry))
    }

    /// Whether `key` is registered, which is the one definition of a tracked directory. Every
    /// ancestor of a registered key is registered too (see [`write_metadata_to_file`]), so every
    /// directory between the root and a tracked file is tracked. A registered key need not have a
    /// shard file: a directory that holds only subdirectories has none, and reads as an empty
    /// inventory.
    pub fn holds(&self, key: &str) -> bool {
        self.entries.contains(&key_to_metadata_entry(key))
    }

    /// The bytes of `key`'s shard, or `None` when `key` is not registered (its address is not
    /// opened) or is registered with no shard.
    ///
    /// # Returns
    /// * `Ok(Option<Vec<u8>>)` - The shard's bytes, if `key` is registered and has a shard.
    /// * `Err(String)`         - If the shard of a registered key could not be read.
    pub fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        if !self.holds(key) {
            return Ok(None);
        }

        Ok(file_utils::retrieve_inventory_or_none_by_key(key)?.1)
    }

    /// The parsed shard of `key`, as [`retrieve`](Self::retrieve) finds it.
    ///
    /// # Returns
    /// * `Ok(Option<Inventory>)` - The shard, if `key` is registered and has one.
    /// * `Err(String)`           - If the shard of a registered key could not be read or parsed.
    pub fn inventory(&self, key: &str) -> Result<Option<Inventory>, String> {
        self.retrieve(key)?
            .map(|bytes| parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e)))
            .transpose()
    }

    /// Register `keys`, and every ancestor of each, before any of their shards is published.
    ///
    /// Each key not registered yet first has its address cleared and the folders on it synced
    /// (see [`clear_new_addresses`]), and only then is the metadata written, durably, as the union.
    /// When every key is already registered, nothing is cleared, synced or written.
    ///
    /// # Arguments
    /// * `keys` - Warehouse path keys of the directories about to be published.
    ///
    /// # Returns
    /// * `Ok(())`      - Every key is registered, durably.
    /// * `Err(String)` - If an address could not be cleared or synced, or the metadata written;
    ///                   no key of `keys` this call would have added may then be published.
    pub fn register(&mut self, keys: &BTreeSet<String>) -> Result<(), String> {
        self.update(&BTreeSet::new(), keys)
    }

    /// Unregister `removed` and register `keys` (and their ancestors) in one durable metadata
    /// write: for a caller that has removed `removed`'s shards and is about to publish `keys`'.
    ///
    /// A key counts as new, and has its address cleared and synced first, when it is not
    /// registered once `removed` is gone. So a removed key published again is cleared like any
    /// other, which makes the caller's own unsynced removal durable before the key is registered
    /// again, while a key still registered and not removed is never cleared. When no key is new and
    /// the registered set would not change, nothing is cleared, synced or written.
    ///
    /// # Arguments
    /// * `removed` - Warehouse path keys to unregister (whose shards are gone).
    /// * `keys`    - Warehouse path keys of the directories about to be published.
    ///
    /// # Returns
    /// * `Ok(())`      - The metadata now registers the old set minus `removed`, plus `keys` and
    ///                   their ancestors.
    /// * `Err(String)` - If an address could not be cleared or synced, or the metadata written;
    ///                   no key of `keys` this call would have added may then be published.
    pub fn update(&mut self, removed: &BTreeSet<String>, keys: &BTreeSet<String>) -> Result<(), String> {
        let mut kept = self.entries.clone();

        for key in removed {
            kept.remove(&key_to_metadata_entry(key));
        }

        let wanted = close_under_ancestors(&keys.iter().map(|key| key_to_metadata_entry(key)).collect());
        let new_entries: BTreeSet<String> = wanted.difference(&kept).cloned().collect();

        kept.extend(wanted);
        let entries = close_under_ancestors(&kept);

        if new_entries.is_empty() && entries == self.entries {
            return Ok(());
        }

        self.publish(&new_entries, entries)
    }

    /// Reset the registered set to exactly `keys` and their ancestors, for a caller about to
    /// publish a whole new staging area. The old metadata is never read, so a damaged one cannot
    /// block the reset. The whole store is removed first and the inventory root synced, which is
    /// every address's clear and sync at once: with the store gone, the inventory root is the only
    /// folder left on any address.
    ///
    /// # Arguments
    /// * `keys` - Warehouse path keys of every directory about to be published.
    ///
    /// # Returns
    /// * `Ok(ShardLedger)` - The new registered set, durably written.
    /// * `Err(String)`     - If the store could not be removed, the inventory root synced, or the
    ///                       metadata written.
    pub fn replace(keys: &BTreeSet<String>) -> Result<ShardLedger, String> {
        let store_root = file_utils::get_inventory_store_root()?;

        remove_store_entry(&store_root).map_err(|e| format!("Error while clearing the staging area: {}", e))?;

        let inventory_root = std::path::PathBuf::from(file_utils::get_path_inventory_root());
        file_utils::create_folder_if_not_exists(&inventory_root)?;
        file_utils::sync_folders_before_flushed_write(&BTreeSet::from([inventory_root]))?;

        let entries = close_under_ancestors(&keys.iter().map(|key| key_to_metadata_entry(key)).collect());
        write_metadata_to_file(&file_utils::get_inventory_metadata_path(), &entries)?;

        Ok(ShardLedger { entries })
    }

    /// Clear and sync `new_entries`' addresses, then write `entries` as the metadata.
    fn publish(&mut self, new_entries: &BTreeSet<String>, entries: BTreeSet<String>) -> Result<(), String> {
        if !new_entries.is_empty() {
            clear_new_addresses(new_entries)?;
        }

        write_metadata_to_file(&file_utils::get_inventory_metadata_path(), &entries)?;

        self.entries = close_under_ancestors(&entries);

        Ok(())
    }
}

/// Clear the address of every newly registered key and sync every folder on it, before the
/// metadata write that registers them. Each level of an address is judged without following a
/// link:
///
/// * a store root that exists and is not a real folder (a link included) fails the registration —
///   it is damage only a reset of the whole staging area repairs, and entering it would clear
///   whatever it points at;
/// * a non-folder or link at the fan-out level is removed itself, never entered;
/// * the shard slot is removed with [`remove_store_entry`], which judges only its last component —
///   the reason the two levels above it are judged here.
///
/// Then, whether or not anything was removed, every folder that exists on a new key's address —
/// its fan-out folder, the store root and the inventory root — is synced, each once. An empty slot
/// may be empty only because something unlinked its shard without a sync (a remover, the sweep, a
/// command run with fsync off), and a folder above it may have been removed and recreated since:
/// syncing every level makes every such unlink durable before the key is registered, so no
/// unlinker has to sync its own. The device flush is borrowed from the metadata write that
/// follows.
///
/// # Arguments
/// * `new_entries` - Metadata entries (see [`key_to_metadata_entry`]) of the keys being registered.
///
/// # Returns
/// * `Ok(())`      - Every new address is empty and every folder on it synced.
/// * `Err(String)` - If the store root is not a folder, or an entry could not be judged, removed
///                   or synced.
fn clear_new_addresses(new_entries: &BTreeSet<String>) -> Result<(), String> {
    let store_root = file_utils::get_inventory_store_root()?;
    let inventory_root = std::path::PathBuf::from(file_utils::get_path_inventory_root());

    file_utils::create_folder_if_not_exists(&inventory_root)?;

    let mut folders: BTreeSet<std::path::PathBuf> = BTreeSet::new();
    folders.insert(inventory_root);

    let judge_error = |path: &Path, e: std::io::Error| format!(
        "Error while reading \"{}\" in the staging area: {}", path.to_string_lossy(), e
    );

    let store_exists = match std::fs::symlink_metadata(&store_root) {
        Ok(metadata) if metadata.is_dir() => true,
        Ok(_) => return Err(format!(
            "The staging area's store \"{}\" is not a folder, so nothing can be staged. Remove it, then \
            run \"forklift restore --staged .\" to reset the staging area to the pallet head.",
            store_root.to_string_lossy()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(judge_error(&store_root, e)),
    };

    if store_exists {
        for entry in new_entries {
            let key = metadata_entry_to_key(entry);
            let (fan_out_name, shard_name) = file_utils::shard_address(key);
            let fan_out = store_root.join(&fan_out_name);

            let clear_error = |e: std::io::Error| format!(
                "Error while clearing the inventory of folder \"{}\": {}", load_guard_utils::display_path(key), e
            );

            match std::fs::symlink_metadata(&fan_out) {
                Ok(metadata) if metadata.is_dir() => {
                    remove_store_entry(&fan_out.join(&shard_name)).map_err(clear_error)?;
                    folders.insert(fan_out);
                }
                Ok(_) => remove_store_entry(&fan_out).map_err(clear_error)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(judge_error(&fan_out, e)),
            }
        }

        folders.insert(store_root);
    }

    file_utils::sync_folders_before_flushed_write(&folders)
}

/// The kind a file has against its entry, given the kind a fresh `stat` reports (see
/// [`file_utils::effective_kind`]). The bit belongs to the path until a removal is stacked: an
/// entry staged for removal lends it too, so `remove` then `load` of the same file round-trips.
pub fn effective_kind(existing: &InventoryItem,
                      metadata: &std::fs::Metadata,
                      stat_kind: DirEntryType) -> Result<DirEntryType, String> {
    file_utils::effective_kind(metadata, stat_kind, existing.item_type)
}

/// Check whether a file is unchanged compared to its existing inventory entry, based purely
/// on file metadata (no file content is read). This is the stat-cache fast path: matching
/// ctime, mtime, device, inode, type (and size, for non-symlinks) means the stored hash is
/// still valid, exactly like git's index stat cache.
///
/// Any error while gathering metadata simply reports "changed", falling back to the full
/// read-and-hash path.
///
/// Note that editors which save via write-new-then-rename replace the inode on every save;
/// an inode mismatch therefore just means "changed" (rehash), never "different file".
///
/// "Racily clean" protection: timestamps have second granularity, so a file modified in the
/// same second it was statted could keep identical mtime/ctime/size and slip past the stat
/// check. Each entry carries its own proof: its stat was taken against an anchor, and one the
/// anchor could not prove was recorded as [`UNPROVEN_MTIME`], which no file's mtime equals (see
/// [`file_utils::StatAnchor`]). Nothing about the shard the entry is read from is consulted, so
/// no later write of that shard can make an unproven entry trusted.
///
/// # Arguments
/// * `existing`  - The inventory entry from the previous load.
/// * `metadata`  - The current (symlink) metadata of the file.
/// * `item_type` - The current type of the directory entry.
/// * `path`      - The path of the file.
///
/// # Returns
/// * `true`  - If the file is unchanged and the existing entry can be reused.
/// * `false` - If the file changed (or freshness could not be determined).
pub fn is_entry_unchanged(existing: &InventoryItem,
                      metadata: &std::fs::Metadata,
                      item_type: DirEntryType,
                      path: &Path) -> bool {
    // Compare the on-disk kind, not the chunked storage decision: `item_type` is derived from a
    // fresh `stat` and can only ever be `Normal`/`Executable`/`SymbolicLink`, while the inventory
    // entry may hold a `*Chunked` variant for the same file. Chunking is a storage choice a stat
    // cannot see, so an unchanged giant (stat says `Normal`, inventory says `NormalChunked`) must
    // still hit this fast path and never be re-chunked. A genuine normal↔executable flip is still
    // caught (their on-disk kinds differ), except where a stat cannot see the bit at all (see
    // `effective_kind`).
    let Ok(item_type) = effective_kind(existing, metadata, item_type) else {
        return false;
    };
    if existing.item_type.on_disk_kind() != item_type.on_disk_kind() {
        return false;
    }

    let Ok(mtime) = file_utils::get_content_modification_timestamp_for_file(metadata) else {
        return false;
    };
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(metadata);

    let Ok((device, inode)) = file_utils::get_device_and_inode(path, metadata) else {
        return false;
    };

    // For symlinks the stored size is the length of the target path, which is not comparable
    // to the metadata size on every platform; the other fields are sufficient for them.
    let size_matches = item_type == DirEntryType::SymbolicLink
        || existing.file_size == metadata.len();

    existing.content_change_timestamp != UNPROVEN_MTIME
        && existing.content_change_timestamp == mtime
        && existing.metadata_change_timestamp == ctime
        && existing.device == device
        && existing.inode == inode
        && size_matches
}

/// Create an inventory item for a file.
/// If the given file does not exist the object store, a new blob is created and stored.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
/// * `anchor`    - Taken before the file's stat (see [`file_utils::StatAnchor`]).
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item for the file.
/// * `Err(String)`       - The error message if the inventory item could not be created.
pub fn build_inventory_item_from_file(path: &Path,
                                      name: &str,
                                      item_type: DirEntryType,
                                      anchor: file_utils::StatAnchor) -> Result<InventoryItem, String> {
    // A first-time `load` of a file: persist its objects. A small file returns its blob for us to
    // store; a chunked file already stored its chunks and recipe during ingest (`None`).
    let (item, object) = build_item_and_object_for_file(path, name, item_type, IngestMode::Store, anchor)?;

    if let Some(mut object) = object {
        object.store()?;
    }

    Ok(item)
}

/// Like [`build_inventory_item_from_file`], but stages the file's blob into `batch` (see
/// [`file_utils::WriteBatch`]) instead of writing and fsyncing it immediately (DESIGN.html §5.0 D
/// item 10, finding #1). Used by every caller that rebuilds several files in one operation
/// (`load`'s parallel walk, `park`'s working-directory refresh), so what used to be one
/// atomic-write barrier per file collapses into the caller's own shared batch.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry.
/// * `batch`     - Where the file's blob (if any) is staged.
/// * `anchor`    - Taken before the file's stat (see [`file_utils::StatAnchor`]).
///
/// # Returns
/// * `Ok(InventoryItem)` - The inventory item for the file. Its blob, if any, is staged but not
///                         yet durable — the caller must call `batch.finish()` (and it must
///                         return `Ok`) before anything may depend on it.
/// * `Err(String)`       - The error message if the inventory item could not be created or its
///                         blob could not be staged.
pub fn build_inventory_item_from_file_deferred(path: &Path,
                                               name: &str,
                                               item_type: DirEntryType,
                                               batch: &file_utils::WriteBatch,
                                               anchor: file_utils::StatAnchor) -> Result<InventoryItem, String> {
    let (item, object) = build_item_and_object_for_file(path, name, item_type, IngestMode::Store, anchor)?;

    if let Some(mut object) = object {
        object.store_deferred(batch)?;
    }

    Ok(item)
}

/// Create an inventory item for a file, together with the built-but-unstored blob object for a
/// small file. A file at or above the chunk threshold is ingested as a recipe plus chunks
/// instead (its entry type becomes a `*Chunked` variant, its hash is the recipe hash) — those
/// objects are handled per `mode`, so the returned object is `None` for a chunked file.
///
/// The read-only stocktake/diff caller passes `IngestMode::ComputeOnly` (nothing is written to
/// the store, not even a chunked giant's chunks); `load`/`park` pass `IngestMode::Store`. Either
/// way the returned blob (small files only) is unstored — writers store it, read-only callers
/// drop it, exactly as before.
///
/// # Arguments
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `item_type` - The type of the directory entry (a `*Chunked` upgrade is decided here).
/// * `mode`      - Whether to persist a chunked file's objects or only compute their hashes.
/// * `anchor`    - Taken before this stat (see [`file_utils::StatAnchor`]). The stat precedes the
///   read, so the hash is of content at least as new as the stat; an mtime the anchor does not
///   prove is recorded as unproven.
///
/// # Returns
/// * `Ok((InventoryItem, Option<LooseObject>))` - The item, and the unstored blob for a small
///   file (`None` for a chunked file, whose objects were handled per `mode`).
/// * `Err(String)`                              - If the file could not be read or stat'ed.
fn build_item_and_object_for_file(path: &Path,
                                  name: &str,
                                  item_type: DirEntryType,
                                  mode: IngestMode,
                                  anchor: file_utils::StatAnchor)
                                  -> Result<(InventoryItem, Option<LooseObject>), String> {
    let metadata = file_utils::get_symlink_metadata_for_path(path)?;

    let (device_id, inode) = file_utils::get_device_and_inode(path, &metadata)?;

    let mtime = anchor.record(device_id, file_utils::get_content_modification_timestamp_for_file(&metadata)?);
    let ctime = file_utils::get_metadata_modification_timestamp_for_file(&metadata);

    let (user_id, group_id) = file_utils::get_owners_for_file(&metadata);
    let ingested = object_utils::ingest_file(name, path, item_type, mode)?;

    let item = InventoryItem {
        metadata_change_timestamp: ctime,
        content_change_timestamp: mtime,
        device: device_id,
        inode,
        item_type: ingested.item_type,
        user_id,
        group_id,
        file_size: ingested.file_size,
        hash: ingested.hash,
        file_name_length: name.len() as u64,
        state: InventoryItemState::Normal,
        name: String::from(name),
    };

    Ok((item, ingested.deferred))
}

/// The verdict of classifying one on-disk file against its existing inventory entry.
/// This is the shared per-file core of the per-directory merge-join (§3.2.1): `load` and
/// the unstaged stocktake walk both classify with it, so their verdicts can never drift
/// apart. The verdict carries facts, not policy — what an untracked file or a staged
/// removal *means* stays with the caller.
pub enum FileVerdict {
    /// The stat cache proves the entry still matches the file — nothing was read or hashed.
    UnchangedByStat,

    /// The stat cache missed, but the content hash and the on-disk kind match the entry: the
    /// file is unchanged. Carries the rebuilt item (same hash, fresh stat data) and, for a small
    /// file, the unstored blob object — writers store it anyway (a cheap no-op when present),
    /// which is what makes a re-load heal a blob that went missing from the object store. A
    /// chunked file carries `None` (its chunks/recipe were handled per the ingest mode).
    UnchangedByHash(InventoryItem, Option<LooseObject>),

    /// The content or the kind changed (a chmod alone keeps the hash). Carries the rebuilt
    /// item (current hash and kind, fresh stat data) and, for a
    /// small file, the unstored blob object, so a writer can store it without reading the file
    /// again — and a read-only caller simply drops it. A chunked file carries `None`.
    Modified(InventoryItem, Option<LooseObject>),
}

/// Classify one on-disk file against its existing inventory entry: the stat-cache fast
/// path first (see `is_entry_unchanged`, including the racily-clean protection), then a
/// read-and-hash comparison in the entry's own representation.
///
/// The file is read once when ingesting it would record it in the entry's representation (a
/// symlink as a symlink, a file below the chunk threshold as a plain blob, one at or above it as
/// a recipe): the ingested item then either carries the entry's hash or is the modified item.
/// Only when the two differ — a large file the entry holds as one plain blob, or a chunked entry
/// whose file shrank — is the file first re-proved in the entry's representation (see
/// [`reprove_file`]) and then, on a miss, ingested. Nothing is written to the inventory; under
/// `IngestMode::Store` a chunked file's chunks and recipe are stored as it is ingested, which
/// heals any that went missing.
///
/// # Arguments
/// * `existing`  - The inventory entry the file is compared against.
/// * `metadata`  - The current (symlink) metadata of the file.
/// * `item_type` - The current type of the directory entry (the filesystem-visible kind).
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `mode`      - Whether a re-chunked giant's objects are stored (`load`) or only hashed
///   (read-only stocktake/diff).
/// * `anchor`    - Taken before `metadata`'s stat, for the rebuilt item (see
///   [`file_utils::StatAnchor`]). A read-only caller whose items are never persisted passes
///   `StatAnchor::Unproven`.
///
/// # Returns
/// * `Ok(FileVerdict)` - The verdict.
/// * `Err(String)`     - If the file could not be read or stat'ed.
pub fn classify_file_against_entry(existing: &InventoryItem,
                                   metadata: &std::fs::Metadata,
                                   item_type: DirEntryType,
                                   path: &Path,
                                   name: &str,
                                   mode: IngestMode,
                                   anchor: file_utils::StatAnchor) -> Result<FileVerdict, String> {
    let item_type = effective_kind(existing, metadata, item_type)?;

    if is_entry_unchanged(existing, metadata, item_type, path) {
        return Ok(FileVerdict::UnchangedByStat);
    }

    // The content alone does not decide it: a file whose bytes match but whose kind flipped (a
    // chmod) is a change the next stack records, so it is reported as one.
    let kind_matches = existing.item_type.on_disk_kind() == item_type.on_disk_kind();

    if ingest_keeps_representation(existing, metadata, item_type) {
        let (item, object) = build_item_and_object_for_file(path, name, item_type, mode, anchor)?;

        return Ok(if item.hash == existing.hash && kind_matches {
            FileVerdict::UnchangedByHash(item, object)
        } else {
            FileVerdict::Modified(item, object)
        });
    }

    if let Some((item, object)) = reprove_file(existing, item_type, path, name, anchor)? {
        return Ok(if kind_matches {
            FileVerdict::UnchangedByHash(item, object)
        } else {
            FileVerdict::Modified(item, object)
        });
    }

    let (item, object) = build_item_and_object_for_file(path, name, item_type, mode, anchor)?;

    Ok(FileVerdict::Modified(item, object))
}

/// Whether ingesting the file would record it in the existing entry's representation, so the
/// ingest's own hash decides whether the file still holds the entry's content (see
/// [`object_utils::hash_file_as_entry`], which this must agree with). A symlink↔file flip never
/// does: a link target and a file's bytes can hash alike without being the same content.
fn ingest_keeps_representation(existing: &InventoryItem,
                               metadata: &std::fs::Metadata,
                               item_type: DirEntryType) -> bool {
    let entry_is_link = existing.item_type == DirEntryType::SymbolicLink;
    if entry_is_link != (item_type == DirEntryType::SymbolicLink) {
        return false;
    }

    let ingest_chunks = metadata.len() >= chunk_utils::CHUNK_THRESHOLD_BYTES as u64;

    entry_is_link || existing.item_type.is_chunked() == ingest_chunks
}

/// Re-prove a file whose stat missed against its existing entry, by hashing it in the entry's
/// representation (see [`object_utils::hash_file_as_entry`]). Every caller that decides by hash
/// whether a file still holds an entry's content goes through here.
///
/// On a match the rebuilt item keeps the entry's hash and storage (chunked or not), takes the
/// file's current kind, so an executable-bit flip is still recorded, and records a fresh stat
/// under `anchor`. That stat is taken after the hash; any edit since the anchor leaves an mtime
/// the anchor does not prove, so the entry is then recorded as unproven.
///
/// Nothing is stored: a chunked entry is re-chunked only to compute its recipe hash. A writer
/// that should heal a chunked entry's objects goes through [`classify_file_against_entry`],
/// which ingests a file still at chunk size in the store-writing mode instead.
///
/// # Arguments
/// * `existing`  - The inventory entry the file is compared against.
/// * `item_type` - The file's current on-disk kind.
/// * `path`      - The path of the file.
/// * `name`      - The name of the file.
/// * `anchor`    - Taken before the caller's stat of the file.
///
/// # Returns
/// * `Ok(Some((InventoryItem, Option<LooseObject>)))` - The file still holds the entry's content:
///   the rebuilt item, and for a small plain file its blob, so a writer heals a blob that went
///   missing from the store.
/// * `Ok(None)`    - It does not (or it changed between a symlink and a file).
/// * `Err(String)` - If the file could not be read or stat'ed.
pub fn reprove_file(existing: &InventoryItem,
                    item_type: DirEntryType,
                    path: &Path,
                    name: &str,
                    anchor: file_utils::StatAnchor)
                    -> Result<Option<(InventoryItem, Option<LooseObject>)>, String> {
    let Some(hashed) = object_utils::hash_file_as_entry(name, path, existing.item_type, item_type)? else {
        return Ok(None);
    };

    if hashed.hash != existing.hash {
        return Ok(None);
    }

    let stored_type = if existing.item_type.is_chunked() { item_type.to_chunked() } else { item_type };
    let mut item = build_inventory_item_from_stat(path, name, hashed.hash, stored_type, anchor)?;
    item.file_size = hashed.file_size;
    let object = hashed.blob;

    Ok(Some((item, object)))
}

/// Add a single file to its corresponding inventory file.
/// If the file is already in the inventory, it is classified against its entry the way the
/// directory walk does (see [`classify_file_against_entry`]), so a file that still holds the
/// entry's content keeps the entry's hash and storage, and a large file stored as one plain blob
/// is not re-chunked into a staged change of storage alone.
///
/// # Arguments
/// * `path` - The path of the file.
///
/// # Returns
/// * `Ok(())`      - If the file was successfully added to the inventory.
/// * `Err(String)` - If there was an error during the operation.
fn add_file_to_inventory(path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let mut ledger = ShardLedger::read()?;

    let mut inventory = ledger.inventory(parent.as_key())?.unwrap_or_else(Inventory::new);

    let fs_path = path.to_fs_path();
    let anchor = file_utils::take_stat_anchor()?;
    let file_metadata = file_utils::get_symlink_metadata_for_path(&fs_path)?;

    let item_type = file_utils::get_type_of_dir_entry(&file_metadata)?;
    let existing = inventory.get_item_by_name(&file_name).map(|item| item.as_ref().clone());

    let item = match existing {
        Some(existing) => {
            let verdict = classify_file_against_entry(
                &existing, &file_metadata, item_type, &fs_path, &file_name, IngestMode::Store, anchor,
            )?;

            match verdict {
                // Loading stages the current state, so a file staged for removal comes back.
                FileVerdict::UnchangedByStat => InventoryItem { state: InventoryItemState::Normal, ..existing },
                // Storing on the unchanged-by-hash path too heals a blob missing from the store.
                FileVerdict::UnchangedByHash(fresh, object) | FileVerdict::Modified(fresh, object) => {
                    if let Some(mut object) = object {
                        object.store()?;
                    }
                    fresh
                }
            }
        }
        None => build_inventory_item_from_file(&fs_path, &file_name, item_type, anchor)?,
    };

    inventory.add_item(item);

    // The incomplete-load marker for `path` rides this shard's own content-write barrier
    // (below) instead of paying a second, standalone one — this shard write is the first of this
    // load's durable mutations that stages anything, so folding the marker into it is free. See
    // `load_guard_utils`'s doc comment (folding section) for the crash-ordering argument.
    //
    // The parent is registered before its shard is written (see `ShardLedger`). A crash between
    // the two leaves it registered with no shard, which stages nothing.
    let marker_path = load_guard_utils::marker_path();
    let marker_bytes = load_guard_utils::pending_start_marker(path.as_key());
    let extra = marker_bytes.as_deref().map(|bytes| (marker_path.as_path(), bytes));

    publish_shards(&mut ledger, ShardPublish {
        register: BTreeSet::from([parent.as_key().to_string()]),
        extra,
        ..ShardPublish::one(parent.as_key(), inventory)
    })
}

/// Stage the removal of a single file: mark its entry in its parent's inventory as `Deleted`.
/// Staging the removal of a file that is already staged for removal is a no-op that
/// still succeeds.
///
/// # Arguments
/// * `ledger` - The registered set.
/// * `path`   - The path of the file whose removal should be staged.
///
/// # Returns
/// * `Ok(())`      - If the removal was staged successfully.
/// * `Err(String)` - If the file is not in the inventory, or there was an error.
fn stage_removal_for_file(ledger: &mut ShardLedger, path: &WarehousePath) -> Result<(), String> {
    let (parent, file_name) = path.split_parent()?;

    let Some(mut inventory) = ledger.inventory(parent.as_key())? else {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    };

    if !inventory.mark_item_deleted(&file_name) {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    }

    write_shard_mutation(ledger, parent.as_key(), inventory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::inventory::RollupStamp;

    /// Whether `key` is registered, as a fresh read of the metadata finds it.
    fn is_registered(key: &str) -> bool {
        ShardLedger::read().unwrap().holds(key)
    }

    fn item(name: &str, inode: u64, state: InventoryItemState) -> InventoryItem {
        InventoryItem {
            metadata_change_timestamp: 0,
            content_change_timestamp: 0,
            device: 1,
            inode,
            item_type: DirEntryType::Normal,
            user_id: 0,
            group_id: 0,
            file_size: 0,
            hash: "hash".to_string(),
            file_name_length: name.len() as u64,
            state,
            name: name.to_string(),
        }
    }

    #[test]
    fn carry_over_marks_missing_entries_as_deleted() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        old_inventory.add_item(item("gone.txt", 2, InventoryItemState::Normal));

        // The rebuilt inventory only found "kept.txt" on disk.
        let mut new_inventory = Inventory::new();
        new_inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert_eq!(new_inventory.get_items_count(), 2);
        assert!(new_inventory.get_item_by_name("kept.txt").unwrap().state == InventoryItemState::Normal);
        assert!(new_inventory.get_item_by_name("gone.txt").unwrap().state == InventoryItemState::Deleted);
    }

    #[test]
    fn carry_over_keeps_already_staged_removals() {
        let mut old_inventory = Inventory::new();
        old_inventory.add_item(item("removed.txt", 1, InventoryItemState::Deleted));

        let mut new_inventory = Inventory::new();

        carry_over_missing_entries_as_deleted(&old_inventory, &mut new_inventory);

        assert!(new_inventory.get_item_by_name("removed.txt").unwrap().state == InventoryItemState::Deleted);
    }

    /// A fresh warehouse root for one test — mirrors `journal_utils::tests::Scratch`.
    struct Scratch {
        root: std::path::PathBuf,
        _scope: crate::globals::StorageRootScope,
    }

    impl Scratch {
        fn new(name: &str) -> Scratch {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let id = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);

            let root = std::env::temp_dir().join(format!(
                "forklift-inventory-test-{}-{}-{}", name, std::process::id(), id
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(root.join(crate::globals::FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
            let scope = crate::globals::StorageRootScope::enter(&root);

            Scratch { root, _scope: scope }
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// The stat cache's writer and reader must agree on a symlink's identity, or every symlink
    /// misses the cache and rehashes on every walk. Both take it from the link's own `lstat`.
    #[cfg(unix)]
    #[test]
    fn a_symlink_recorded_from_stat_reads_unchanged_against_its_own_lstat() {
        let scratch = Scratch::new("symlink-identity");
        let target = scratch.root.join("target.txt");
        let link = scratch.root.join("link");
        std::fs::write(&target, "content\n").unwrap();
        std::os::unix::fs::symlink("target.txt", &link).unwrap();

        let recorded = build_inventory_item_from_stat(&link, "link", "hash".to_string(),
                                                      DirEntryType::SymbolicLink,
                                                      anchor_proving_all_on(&link)).unwrap();
        let link_metadata = std::fs::symlink_metadata(&link).unwrap();
        assert_eq!(recorded.inode, std::os::unix::fs::MetadataExt::ino(&link_metadata));

        assert!(is_entry_unchanged(&recorded, &link_metadata, DirEntryType::SymbolicLink, &link),
            "a symlink recorded from its own stat must hit the stat cache");
    }

    /// The Windows identity opens the link itself, not its target, so a dangling link has one and
    /// a live link's differs from its target's. Creating a symlink needs developer mode or an
    /// elevated runner; without either the test has nothing to check.
    #[cfg(windows)]
    #[test]
    fn a_symlink_identity_is_its_own_on_windows() {
        let scratch = Scratch::new("symlink-identity");
        let target = scratch.root.join("target.txt");
        let link = scratch.root.join("link");

        if std::os::windows::fs::symlink_file("target.txt", &link).is_err() {
            eprintln!("skipped: this runner cannot create symlinks");
            return;
        }

        let dangling = std::fs::symlink_metadata(&link).unwrap();
        let identity = file_utils::get_device_and_inode(&link, &dangling)
            .expect("a dangling symlink must have an identity");

        std::fs::write(&target, "content\n").unwrap();
        let target_identity = file_utils::get_device_and_inode(&target, &std::fs::metadata(&target).unwrap()).unwrap();

        assert_ne!(identity, target_identity, "a symlink's identity must not be its target's");
        assert_eq!(file_utils::get_device_and_inode(&link, &dangling).unwrap(), identity,
            "a symlink's identity must not change when its target appears");
    }

    /// The alias probe's full id names an entry, not a spelling: two hard links to one file share
    /// it, two files never do, and a symlink has its own, stable while it dangles.
    #[cfg(windows)]
    #[test]
    fn the_full_file_id_tells_entries_apart_on_windows() {
        let scratch = Scratch::new("full-file-id");
        let first = scratch.root.join("first.txt");
        let second = scratch.root.join("second.txt");
        let hard_link = scratch.root.join("hard-link.txt");
        std::fs::write(&first, "one\n").unwrap();
        std::fs::write(&second, "two\n").unwrap();
        std::fs::hard_link(&first, &hard_link).unwrap();

        let first_id = file_utils::get_full_file_id(&first).unwrap();
        assert_eq!(file_utils::get_full_file_id(&hard_link).unwrap(), first_id,
            "two hard links to one file must share an id");
        assert_ne!(file_utils::get_full_file_id(&second).unwrap(), first_id,
            "two files must not share an id");

        let link = scratch.root.join("link");
        if std::os::windows::fs::symlink_file("missing.txt", &link).is_err() {
            eprintln!("skipped the symlink half: this runner cannot create symlinks");
            return;
        }

        let dangling_id = file_utils::get_full_file_id(&link).expect("a dangling symlink must have an id");
        assert_ne!(dangling_id, first_id, "a symlink must have its own id");
        assert_eq!(file_utils::get_full_file_id(&link).unwrap(), dangling_id,
            "a dangling symlink's id must be stable");
    }

    #[test]
    fn prepare_stack_inventory_reports_a_conflict_found_before_a_later_corrupt_shard() {
        // Regression: a single up-front parse pass must not let an unrelated corrupt shard mask
        // a real, actionable conflict discovered earlier in the (sorted) scan. A conflict in an
        // earlier-sorting shard is what the user sees, even when a later shard happens to be
        // corrupt, from both `prepare_stack_inventory` and `has_conflict_entries` (park's check),
        // though both read every shard in parallel.
        let _scratch = Scratch::new("conflict-before-corrupt");

        // "aaa" sorts before "zzz" and has a genuine unresolved conflict.
        let mut conflicted = Inventory::new();
        conflicted.add_item(item("file.txt", 1, InventoryItemState::FirstParentConflict));
        plant_shard(&conflicted, "aaa");

        // "zzz" sorts after "aaa" and is not a valid inventory shard at all.
        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        assert_eq!(has_conflict_entries(), Ok(true),
            "park's check must report the conflict in \"aaa\", not \"zzz\"'s parse error");

        let prepared = prepare_stack_inventory(None)
            .expect("the conflict in \"aaa\" must be reported, not \"zzz\"'s parse error");

        assert!(prepared.has_conflict, "the conflict found while scanning must be recorded");
        assert!(has_conflict_entries_in(&prepared));

        // The scan stopped at "aaa": "zzz" was never reached, so it is absent from the snapshot
        // rather than having failed to parse into it.
        assert!(prepared.shards.contains_key("aaa"));
        assert!(!prepared.shards.contains_key("zzz"));
    }

    #[test]
    fn prepare_stack_inventory_still_surfaces_a_corrupt_shard_when_nothing_is_in_conflict() {
        // The other half of the same fix: when there is no conflict anywhere, a corrupt shard
        // must still fail loudly (never silently skipped) — the short-circuit only changes
        // *priority* between two real problems, it must not hide either one on its own.
        let _scratch = Scratch::new("corrupt-with-no-conflict");

        let mut clean = Inventory::new();
        clean.add_item(item("file.txt", 1, InventoryItemState::Normal));
        plant_shard(&clean, "aaa");

        let corrupt_path = file_utils::get_inventory_data_path_for_key("zzz").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let error = match prepare_stack_inventory(None) {
            Ok(_) => panic!("a corrupt shard must still fail the scan"),
            Err(message) => message,
        };
        assert!(error.contains("zzz"), "the error should name the offending shard, got: {error}");
    }

    #[test]
    fn has_conflict_entries_fails_on_a_corrupt_shard_before_any_conflict() {
        // The parallel read must not let a later conflict mask an earlier corrupt shard: in key
        // order the corrupt "aaa" comes first, so the check fails rather than answering.
        let _scratch = Scratch::new("corrupt-before-conflict");

        let corrupt_path = file_utils::get_inventory_data_path_for_key("aaa").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut conflicted = Inventory::new();
        conflicted.add_item(item("file.txt", 1, InventoryItemState::FirstParentConflict));
        plant_shard(&conflicted, "zzz");

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert("aaa".to_string());
        metadata.insert("zzz".to_string());
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        match has_conflict_entries() {
            Ok(answer) => panic!("a corrupt shard before the conflict must fail the check, got Ok({answer})"),
            Err(message) => assert!(message.contains("aaa"), "the error should name \"aaa\", got: {message}"),
        }
    }

    #[test]
    fn a_refresh_publishes_the_shards_before_a_failing_one_and_none_after() {
        // Every shard is decided in parallel, but they are taken up in metadata order: the corrupt
        // "bbb" ends the pass, so "aaa"'s refresh is published and "ccc"'s is not.
        let _scratch = Scratch::new("refresh-stops-at-failure");

        for key in ["aaa", "ccc"] {
            let mut tracked = Inventory::new();
            tracked.add_item(item("gone-from-disk.txt", 1, InventoryItemState::Normal));
            plant_shard(&tracked, key);
        }

        let corrupt_path = file_utils::get_inventory_data_path_for_key("bbb").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let metadata: BTreeSet<String> = ["aaa", "bbb", "ccc"].iter().map(|key| key.to_string()).collect();
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let error = refresh_tracked_entries().expect_err("the corrupt shard must fail the refresh");
        assert!(error.contains("bbb"), "the error should name \"bbb\", got: {error}");

        let state_of = |key: &str| {
            let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
            let shard = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
            shard.get_item_by_name("gone-from-disk.txt").unwrap().state
        };
        assert!(state_of("aaa") == InventoryItemState::Deleted, "the shard before the failure must be published");
        assert!(state_of("ccc") == InventoryItemState::Normal, "no shard after the failure may be published");
    }

    /// Write a shard straight to `key`'s address, registered or not: a fixture placing one, never
    /// a writer (every writer goes through a `ShardLedger`).
    fn plant_shard(inventory: &Inventory, key: &str) {
        let path = file_utils::get_inventory_data_path_for_key(key).unwrap();
        file_utils::create_folder_if_not_exists(path.parent().unwrap()).unwrap();
        file_utils::write_file_atomically(&path, &InventoryBuilder::build(inventory)).unwrap();
    }

    /// Write a shard directly (bypassing the funnel) with the given rollup hash already
    /// stamped — simulates a shard as it would sit right after a `stack` (see
    /// `cleanup_after_stack_with`), without needing a real tree build.
    fn write_stamped_shard(key: &str, rollup_hash: Option<&str>) {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", 1, InventoryItemState::Normal));
        if let Some(hash) = rollup_hash {
            inventory.stamp_rollup(RollupStamp::trusted(hash.to_string()));
        }
        plant_shard(&inventory, key);
    }

    fn read_rollup(key: &str) -> Option<String> {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        inventory.get_rollup_hash().cloned()
    }

    /// A stamped chain `""` → `a` → `a/b`, plus a stamped sibling `x`, all registered.
    fn stamped_chain_with_sibling() {
        let mut metadata: BTreeSet<String> = BTreeSet::new();

        for key in ["", "a", "a/b", "x"] {
            write_stamped_shard(key, Some("rollup"));
            metadata.insert(key_to_metadata_entry(key));
        }

        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();
    }

    #[test]
    fn a_batch_that_only_refreshes_stat_data_keeps_every_rollup() {
        let _scratch = Scratch::new("batch-carry");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a/b", |inventory| {
            inventory.add_item(item("file.txt", 7, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().unwrap();

        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key("a/b").unwrap();
        let shard = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        assert_eq!(shard.get_item_by_name("file.txt").unwrap().inode, 7, "the stat refresh must be published");

        for key in ["", "a", "a/b", "x"] {
            assert_eq!(read_rollup(key).as_deref(), Some("rollup"),
                "a stat-only refresh must keep the rollup of \"{key}\"");
        }
    }

    #[test]
    fn a_batch_that_changes_content_clears_the_shard_and_its_ancestors_only() {
        let _scratch = Scratch::new("batch-changed");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a/b", |inventory| {
            inventory.add_item(item("file.txt", 1, InventoryItemState::FirstParentConflict));
        }).unwrap();
        batch.publish().unwrap();

        for key in ["", "a", "a/b"] {
            assert_eq!(read_rollup(key), None, "a state change must clear the rollup of \"{key}\"");
        }
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "an unrelated sibling must keep its rollup");
    }

    /// One shard listing `file.txt` as the fixture writes it, with stat data `inode` and `state`,
    /// carrying a rollup no funnel would trust: whatever a producer hands in is never what lands.
    fn forged_shard(inode: u64, state: InventoryItemState) -> Inventory {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", inode, state));
        inventory.stamp_rollup(RollupStamp::trusted("forged".to_string()));
        inventory
    }

    fn publish(writes: Vec<(&str, Inventory)>, removed: &[&str]) -> Result<(), String> {
        let writes = writes.into_iter().map(|(key, inventory)| (key.to_string(), inventory)).collect();
        let removed = removed.iter().map(|key| key.to_string()).collect();

        publish_shards(&mut ShardLedger::read().unwrap(), ShardPublish { removed, ..ShardPublish::of(writes) })
    }

    /// The funnel decides every rollup from the shard on disk: a stat-only rewrite keeps the one on
    /// disk, a change gets none, and neither keeps the one the producer handed in.
    #[test]
    fn publish_shards_decides_each_rollup_from_the_shard_on_disk() {
        let _scratch = Scratch::new("publish-classify");
        stamped_chain_with_sibling();

        publish(vec![
            ("a/b", forged_shard(7, InventoryItemState::Normal)),
            ("x", forged_shard(1, InventoryItemState::FirstParentConflict)),
        ], &[]).unwrap();

        assert_eq!(read_rollup("a/b").as_deref(), Some("rollup"), "a stat-only rewrite keeps the rollup on disk");
        assert_eq!(read_rollup("a").as_deref(), Some("rollup"), "nothing below \"a\" changed content");
        assert_eq!(read_rollup("x"), None, "a content change keeps no rollup");
        assert_eq!(read_rollup(""), None, "the ancestor of a content change keeps no rollup");
        assert_eq!(registered_inode("a/b", "file.txt"), 7, "the stat refresh is published");
    }

    /// A shard classified as stat-only keeps no rollup when it is also the ancestor of a change
    /// decided later in the same call (DESIGN.html §5.0 D item 10, finding #1).
    #[test]
    fn publish_shards_drops_the_rollup_of_a_carried_ancestor_of_a_change() {
        let _scratch = Scratch::new("publish-carried-ancestor");
        stamped_chain_with_sibling();

        publish(vec![
            ("a", forged_shard(7, InventoryItemState::Normal)),
            ("a/b", forged_shard(1, InventoryItemState::FirstParentConflict)),
        ], &[]).unwrap();

        for key in ["", "a", "a/b"] {
            assert_eq!(read_rollup(key), None, "\"{key}\" is above or at the change");
        }
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "an unrelated sibling keeps its rollup");
        assert_eq!(registered_inode("a", "file.txt"), 7, "the carried shard is still published");
    }

    /// A carried ancestor of a change has its rollup cleared on disk before any shard of the call
    /// is written: the shards' own barrier renames in no order, so the ancestor's clear cannot
    /// ride its own rewrite there.
    #[cfg(unix)]
    #[test]
    fn publish_shards_clears_a_carried_ancestor_of_a_change_before_writing_any_shard() {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = Scratch::new("publish-ancestor-first");
        stamped_chain_with_sibling();

        // The change's own shard folder refuses the staged write, so no shard of the call lands.
        let folder = file_utils::get_inventory_data_path_for_key("a/b").unwrap().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();

        let published = publish(vec![
            ("a", forged_shard(7, InventoryItemState::Normal)),
            ("a/b", forged_shard(1, InventoryItemState::FirstParentConflict)),
        ], &[]);

        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(read_rollup("a"), None, "the carried ancestor's clear is durable before the shards' barrier");
        assert_eq!(read_rollup("a/b").as_deref(), Some("rollup"), "the change's shard did not land");
        assert!(published.is_err(), "the change's shard could not be staged");
    }

    /// A removed shard leaves no rollup standing above it, a carried one in the same call included.
    #[test]
    fn publish_shards_clears_every_ancestor_of_a_removed_key() {
        let _scratch = Scratch::new("publish-removed");
        stamped_chain_with_sibling();

        publish(vec![("a", forged_shard(7, InventoryItemState::Normal))], &["a/b"]).unwrap();

        assert_eq!(read_rollup(""), None, "the root is above the removal");
        assert_eq!(read_rollup("a"), None, "a carried shard above the removal keeps no rollup");
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "an unrelated sibling keeps its rollup");
    }

    /// A removed key is unregistered only once every rollup above it is cleared: when the clear
    /// fails, the key stays registered, so the next load still finds it stale and owes the clear.
    #[cfg(unix)]
    #[test]
    fn publish_shards_unregisters_a_removed_key_only_after_its_ancestors_are_cleared() {
        use std::os::unix::fs::PermissionsExt;

        let _scratch = Scratch::new("publish-unregister-after-clear");
        stamped_chain_with_sibling();

        let folder = file_utils::get_inventory_data_path_for_key("").unwrap().parent().unwrap().to_path_buf();
        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o555)).unwrap();

        let published = publish(vec![], &["a/b"]);

        std::fs::set_permissions(&folder, std::fs::Permissions::from_mode(0o755)).unwrap();

        assert!(ShardLedger::read().unwrap().holds("a/b"), "a removal whose ancestor clear failed stays registered");
        assert!(published.is_err(), "the root's clear could not be staged");
    }

    /// A shard on disk that does not parse is an error, never a new shard to overwrite: nothing of
    /// the call is written and no rollup is cleared.
    #[test]
    fn publish_shards_refuses_over_a_shard_that_does_not_parse() {
        let _scratch = Scratch::new("publish-corrupt");
        stamped_chain_with_sibling();
        let corrupt_path = file_utils::get_inventory_data_path_for_key("a/b").unwrap();
        std::fs::write(&corrupt_path, b"not an inventory").unwrap();

        let error = publish(vec![
            ("a/b", forged_shard(7, InventoryItemState::Normal)),
            ("x", forged_shard(1, InventoryItemState::FirstParentConflict)),
        ], &[]).unwrap_err();

        assert!(error.contains("a/b"), "the error names the shard: {error}");
        assert_eq!(std::fs::read(&corrupt_path).unwrap(), b"not an inventory", "the damaged shard is not overwritten");
        assert_eq!(registered_inode("x", "file.txt"), 1, "no other shard of the call is written");
        for key in ["", "a", "x"] {
            assert_eq!(read_rollup(key).as_deref(), Some("rollup"), "\"{key}\" keeps its rollup");
        }
    }

    fn registered_inode(key: &str, name: &str) -> u64 {
        ShardLedger::read().unwrap().inventory(key).unwrap().unwrap().get_item_by_name(name).unwrap().inode
    }

    /// A case-only rename of a directory: the old spelling's entry is removed, then the new
    /// spelling's is written. The two keys get two shards on every volume, each with its own change.
    #[test]
    fn a_batch_keeps_both_changes_to_keys_that_differ_only_in_case() {
        let _scratch = Scratch::new("batch-case-alias");

        let mut setup = ShardMutationBatch::new();
        setup.update("dir", |inventory| {
            inventory.add_item(item("a.txt", 1, InventoryItemState::Normal));
        }).unwrap();
        setup.publish().unwrap();

        assert_ne!(file_utils::get_inventory_data_path_for_key("Dir").unwrap(),
            file_utils::get_inventory_data_path_for_key("dir").unwrap());

        let mut batch = ShardMutationBatch::new();
        batch.update("dir", |inventory| { inventory.mark_item_deleted("a.txt"); }).unwrap();
        batch.update("Dir", |inventory| {
            inventory.add_item(item("a.txt", 2, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().unwrap();

        let item_in = |key: &str| {
            let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key(key).unwrap();
            let shard = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
            shard.get_item_by_name("a.txt").map(|item| (item.state, item.inode))
        };

        assert!(item_in("dir") == Some((InventoryItemState::Deleted, 1)), "the old spelling keeps its removal");
        assert!(item_in("Dir") == Some((InventoryItemState::Normal, 2)), "the new spelling keeps its write");
        assert!(is_registered("dir") && is_registered("Dir"),
            "both spellings are registered");
    }

    /// A rollup is cleared only for the key a change is beneath: `A` is another directory than `a`.
    #[test]
    fn a_batch_keeps_a_carried_rollup_of_a_key_differing_only_in_case_from_a_changed_ancestor() {
        let _scratch = Scratch::new("batch-case-alias-carry");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a", |inventory| {
            inventory.add_item(item("file.txt", 7, InventoryItemState::Normal));
        }).unwrap();
        batch.update("A/b/new", |inventory| {
            inventory.add_item(item("file.txt", 1, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().unwrap();

        assert_eq!(read_rollup("a").as_deref(), Some("rollup"), "a change beneath \"A\" leaves \"a\"'s rollup");
        assert_eq!(read_rollup(""), None, "the shared root is an ancestor of the change");
    }

    #[test]
    fn a_batch_registers_a_shard_it_created() {
        let _scratch = Scratch::new("batch-new-shard");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a/b/new", |inventory| {
            inventory.add_item(item("file.txt", 1, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().unwrap();

        assert!(is_registered("a/b/new"), "a shard the batch created must be registered");
        assert_eq!(read_rollup("a"), None, "a new shard is a content change for its ancestors");
    }

    #[test]
    fn a_batch_refuses_to_publish_over_a_shard_rewritten_since_it_was_read() {
        let _scratch = Scratch::new("batch-guard");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("x", |inventory| {
            inventory.add_item(item("file.txt", 7, InventoryItemState::Normal));
        }).unwrap();
        batch.update("a/b", |inventory| {
            inventory.add_item(item("file.txt", 7, InventoryItemState::Normal));
        }).unwrap();

        // Someone else rewrites a touched shard before the batch publishes.
        ShardLedger::read().unwrap().update_shard("a/b", |inventory| {
            inventory.add_item(item("other.txt", 9, InventoryItemState::Normal));
            Ok(())
        }).unwrap();
        let (_, rewritten) = file_utils::retrieve_inventory_or_none_by_key("a/b").unwrap();
        let (_, sibling_before) = file_utils::retrieve_inventory_or_none_by_key("x").unwrap();

        let error = batch.publish().unwrap_err();
        assert!(error.contains("changed while it was being updated"), "unexpected error: {error}");

        let (_, after) = file_utils::retrieve_inventory_or_none_by_key("a/b").unwrap();
        assert_eq!(after, rewritten, "the other writer's change must not be overwritten");
        let (_, sibling_after) = file_utils::retrieve_inventory_or_none_by_key("x").unwrap();
        assert_eq!(sibling_after, sibling_before, "no shard of a refused batch may be published");
    }

    #[test]
    fn a_batch_publishes_each_shard_at_its_last_change_not_its_first() {
        let _scratch = Scratch::new("batch-anchor");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a/b", |inventory| {
            inventory.add_item(item("first.txt", 1, InventoryItemState::Normal));
        }).unwrap();

        std::thread::sleep(std::time::Duration::from_millis(50));
        let between = std::time::SystemTime::now();

        batch.update("a/b", |inventory| {
            inventory.add_item(item("second.txt", 2, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().unwrap();

        let published_mtime = file_utils::get_symlink_metadata_for_path(
            &file_utils::get_inventory_data_path_for_key("a/b").unwrap()).unwrap().modified().unwrap();
        assert!(published_mtime >= between,
            "a file restored before the shard's last change must be trusted as it is with one \
            update_shard per change: got {:?}, expected no earlier than {:?}", published_mtime, between);
    }

    #[test]
    fn write_shard_mutation_clears_every_ancestor_top_down_but_spares_an_unrelated_sibling() {
        let _scratch = Scratch::new("mutation-clears-ancestors");

        // A chain of previously-stamped shards from root down to a depth-3 directory, plus an
        // unrelated sibling subtree stamped the same way.
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b", "a/b/c", "x"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        // A content-changing mutation three levels deep.
        ShardLedger::read().unwrap().update_shard("a/b/c", |inventory| {
            inventory.add_item(item("new.txt", 99, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        // Every ancestor, root first, is cleared — and the mutated shard itself never keeps a
        // stale rollup (the funnel always writes `None` for the shard it mutates).
        assert_eq!(read_rollup(""), None, "the root's stale rollup must be cleared");
        assert_eq!(read_rollup("a"), None, "an intermediate ancestor's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b"), None, "the immediate parent's stale rollup must be cleared");
        assert_eq!(read_rollup("a/b/c"), None, "the mutated shard itself must not keep a rollup");

        // An unrelated sibling subtree is untouched.
        assert_eq!(read_rollup("x").as_deref(), Some("stale-rollup"),
            "a sibling subtree outside the mutated chain must keep its rollup");
    }

    #[test]
    fn write_shard_mutation_is_a_no_op_on_an_already_clear_ancestor() {
        // A shard whose rollup is already `None` must not be rewritten by ancestor invalidation
        // — a light regression guard for the "no-op, don't rewrite" contract, checked via the
        // shard file's absence of change (it was never given a rollup to lose either way, but
        // this at least exercises the skip path without a real tree build).
        let _scratch = Scratch::new("mutation-noop-ancestor");

        write_stamped_shard("", None);
        write_stamped_shard("a", None);

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        metadata.insert(key_to_metadata_entry("a"));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        ShardLedger::read().unwrap().update_shard("a", |inventory| {
            inventory.add_item(item("new.txt", 1, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        assert_eq!(read_rollup(""), None);
        assert_eq!(read_rollup("a"), None);
    }

    #[test]
    fn remove_inventories_under_clears_ancestor_rollups_conservatively() {
        let _scratch = Scratch::new("narrow-clears-ancestors");

        for key in ["", "a", "a/b"] {
            write_stamped_shard(key, Some("stale-rollup"));
        }

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        for key in ["", "a", "a/b"] {
            metadata.insert(key_to_metadata_entry(key));
        }
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        remove_inventories_under("a/b").unwrap();

        assert_eq!(read_rollup(""), None, "narrow must conservatively clear the root's rollup");
        assert_eq!(read_rollup("a"), None, "narrow must conservatively clear the parent's rollup");
        assert!(!file_utils::get_inventory_data_path_for_key("a/b").unwrap().exists());
    }

    #[test]
    fn the_metadata_is_written_closed_under_ancestors() {
        let _scratch = Scratch::new("metadata-closure");
        write_stamped_shard("a/b", None);

        let registered = |expected: &[&str]| {
            let (_, metadata) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
            let expected: BTreeSet<String> = expected.iter().map(|key| key_to_metadata_entry(key)).collect();
            assert_eq!(metadata.unwrap(), expected);
        };

        // A writer that registers only the directory it wrote still registers every ancestor.
        ShardLedger::read().unwrap().register(&BTreeSet::from(["a/b".to_string()])).unwrap();
        registered(&["", "a", "a/b"]);

        // A remover that drops an ancestor while a descendant stays registered does not unregister it.
        unregister_inventory_keys(&BTreeSet::from(["a".to_string()])).unwrap();
        registered(&["", "a", "a/b"]);
        assert!(is_registered("a"));
        assert!(!is_registered("a/b/c"));
    }

    #[test]
    fn build_inventory_preserves_rollup_across_a_pure_stat_refresh() {
        // The comparison helper that decides whether a rebuilt shard's content is truly
        // unchanged (so its rollup may be carried forward) versus genuinely different (so it
        // must go through the funnel).
        let mut a = Inventory::new();
        a.add_item(item("file.txt", 1, InventoryItemState::Normal));

        let mut b = Inventory::new();
        b.add_item(item("file.txt", 2, InventoryItemState::Normal)); // different inode only

        assert!(inventory_content_matches(&a, &b),
            "a stat-only difference (inode) must not count as a content change");

        let mut c = Inventory::new();
        let mut changed = item("file.txt", 1, InventoryItemState::Normal);
        changed.hash = "different-hash".to_string();
        c.add_item(changed);

        assert!(!inventory_content_matches(&a, &c), "a real hash change must count as a content change");
    }

    /// Set a file's modification time directly, through an open write handle — never a
    /// reopen-by-path (the project's Windows fsync convention).
    /// An anchor that proves every mtime on `path`'s device, as one stamped long after the file was.
    fn anchor_proving_all_on(path: &Path) -> file_utils::StatAnchor {
        let metadata = file_utils::get_symlink_metadata_for_path(path).unwrap();
        file_utils::StatAnchor::At { device: file_utils::get_device_and_inode(path, &metadata).unwrap().0, stamp: u64::MAX }
    }

    fn set_mtime(path: &Path, mtime: std::time::SystemTime) {
        let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
        file.set_modified(mtime).unwrap();
    }

    /// Read the root shard's `a.txt` entry back from disk and ask the stat cache whether it
    /// trusts it against the file as it is now.
    fn root_entry_is_trusted(file: &Path) -> bool {
        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key("").unwrap();
        let inventory = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        let entry = inventory.get_item_by_name("a.txt").unwrap().clone();
        let metadata = file_utils::get_symlink_metadata_for_path(file).unwrap();
        is_entry_unchanged(&entry, &metadata, DirEntryType::Normal, file)
    }

    /// An entry recorded within the same second as its file's write proves nothing, and no later
    /// republish of its shard (a sibling's stage, a rollup stamp) can make it trusted: the proof
    /// lives in the entry, not in the shard's own mtime.
    #[test]
    fn a_racy_entry_stays_untrusted_after_a_sibling_republishes_its_shard() {
        let scratch = Scratch::new("racy-entry-republish");
        let file = scratch.root.join("a.txt");
        std::fs::write(&file, "two").unwrap();

        // The hash is of earlier content, edited within the same second: a racy entry.
        let anchor = file_utils::take_stat_anchor().unwrap();
        let entry = build_inventory_item_from_stat(&file, "a.txt", "hash-of-one".to_string(),
                                                   DirEntryType::Normal, anchor).unwrap();
        assert_eq!(entry.content_change_timestamp, UNPROVEN_MTIME,
            "a stat taken in the same second as the write must record the unproven sentinel");
        ShardLedger::read().unwrap().update_shard("", |inventory| { inventory.add_item(entry); Ok(()) }).unwrap();
        assert!(!root_entry_is_trusted(&file), "a racy entry must not be trusted");

        ShardLedger::read().unwrap().update_shard("", |inventory| { inventory.add_item(item("b.txt", 9, InventoryItemState::Normal)); Ok(()) }).unwrap();
        assert!(!root_entry_is_trusted(&file), "a sibling's republish must not make the racy entry trusted");
    }

    /// The trust rule's boundary, under an injected anchor: an mtime two whole seconds before the
    /// anchor is proven, one second before or later is not.
    #[test]
    fn an_entry_is_proven_only_two_whole_seconds_before_its_anchor() {
        let scratch = Scratch::new("anchor-boundary");
        let file = scratch.root.join("a.txt");
        std::fs::write(&file, "content").unwrap();
        set_mtime(&file, std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));
        let mtime = file_utils::get_content_modification_timestamp_for_file(
            &file_utils::get_symlink_metadata_for_path(&file).unwrap()).unwrap();
        assert_eq!(mtime, 1_000_000);

        let file_utils::StatAnchor::At { device, .. } = anchor_proving_all_on(&file) else { unreachable!() };
        let recorded = |stamp: u64| build_inventory_item_from_stat(
            &file, "a.txt", "hash".to_string(), DirEntryType::Normal,
            file_utils::StatAnchor::At { device, stamp }).unwrap().content_change_timestamp;
        assert_eq!(recorded(mtime + 2), mtime, "two whole seconds before the anchor is proven");
        assert_eq!(recorded(mtime + 1), UNPROVEN_MTIME, "one second before the anchor is not proven");
        assert_eq!(recorded(mtime), UNPROVEN_MTIME, "the anchor's own second is not proven");
        assert_eq!(recorded(0), UNPROVEN_MTIME);
        let unproven = build_inventory_item_from_stat(&file, "a.txt", "hash".to_string(),
                                                      DirEntryType::Normal,
                                                      file_utils::StatAnchor::Unproven).unwrap();
        assert_eq!(unproven.content_change_timestamp, UNPROVEN_MTIME);
    }

    /// A file recorded from a stat is proven only against an anchor taken on its own device: an
    /// anchor from any other volume says nothing about its clock.
    #[test]
    fn an_entry_recorded_from_a_stat_is_unproven_against_another_devices_anchor() {
        let scratch = Scratch::new("anchor-other-device");
        let file = scratch.root.join("a.txt");
        std::fs::write(&file, "content").unwrap();
        set_mtime(&file, std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000));

        let file_utils::StatAnchor::At { device, stamp } = anchor_proving_all_on(&file) else { unreachable!() };
        let recorded = |device| build_inventory_item_from_stat(
            &file, "a.txt", "hash".to_string(), DirEntryType::Normal,
            file_utils::StatAnchor::At { device, stamp }).unwrap().content_change_timestamp;
        assert_eq!(recorded(device.wrapping_add(1)), UNPROVEN_MTIME,
                   "an anchor on another device must not prove the file");
        assert_eq!(recorded(device), 1_000_000, "the file's own device's anchor proves it");
    }

    /// A file whose bytes match its entry but whose executable bit does not is a change where a
    /// stat can see the bit. Where it cannot (Windows, or a volume without the bit), the entry's bit stands: the stat cache
    /// still hits, and nothing would record the file as plain.
    #[test]
    fn a_kind_only_difference_is_a_change_only_where_the_bit_is_observable() {
        let scratch = Scratch::new("kind-only-change");
        let file = scratch.root.join("tool.sh");
        std::fs::write(&file, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let metadata = file_utils::get_symlink_metadata_for_path(&file).unwrap();
        let stat_kind = file_utils::get_type_of_dir_entry(&metadata).unwrap();
        assert_eq!(stat_kind, DirEntryType::Normal);
        let observable = file_utils::executable_bit_is_observable(&metadata).unwrap();
        assert_eq!(observable, cfg!(unix), "the scratch folder is on a volume that keeps the bit where the platform has one");
        let (hashed, _) = build_item_and_object_for_file(&file, "tool.sh", DirEntryType::Normal,
                                                         IngestMode::ComputeOnly,
                                                         file_utils::StatAnchor::Unproven).unwrap();
        let entry_of = |kind: DirEntryType| build_inventory_item_from_stat(
            &file, "tool.sh", hashed.hash.clone(), kind, anchor_proving_all_on(&file)).unwrap();
        let classify = |entry: &InventoryItem| classify_file_against_entry(
            entry, &metadata, stat_kind, &file, "tool.sh", IngestMode::ComputeOnly,
            file_utils::StatAnchor::Unproven).unwrap();

        assert!(matches!(classify(&entry_of(DirEntryType::Normal)), FileVerdict::UnchangedByStat),
            "a plain entry over the same plain file is unchanged");

        let executable = entry_of(DirEntryType::Executable);
        match classify(&executable) {
            FileVerdict::Modified(item, _) if observable => {
                assert_eq!(item.hash, hashed.hash, "a chmod keeps the hash");
                assert_eq!(item.item_type, DirEntryType::Normal, "the rebuilt item takes the file's kind");
            }
            FileVerdict::UnchangedByStat if !observable => {}
            other => panic!("unexpected verdict for a kind-only difference: {}", match other {
                FileVerdict::UnchangedByStat => "unchanged by stat",
                FileVerdict::UnchangedByHash(..) => "unchanged by hash",
                FileVerdict::Modified(..) => "modified",
            }),
        }
        assert_eq!(is_entry_unchanged(&executable, &metadata, stat_kind, &file),
                   !observable);
    }

    /// The bit belongs to the path until a removal is stacked: an entry staged for removal lends
    /// it like a live one, so `remove` then `load` of the same file round-trips everywhere.
    #[test]
    fn an_entry_staged_for_removal_lends_its_bit_like_a_live_one() {
        let live = InventoryItem { item_type: DirEntryType::Executable, ..item("run.sh", 1, InventoryItemState::Normal) };
        let removed = InventoryItem { state: InventoryItemState::Deleted, ..live.clone() };
        let scratch = Scratch::new("removal-lends-bit");
        std::fs::write(scratch.root.join("run.sh"), "#!/bin/sh\n").unwrap();
        let metadata = file_utils::get_symlink_metadata_for_path(&scratch.root.join("run.sh")).unwrap();

        for stat_kind in [DirEntryType::Normal, DirEntryType::Executable, DirEntryType::SymbolicLink] {
            assert_eq!(effective_kind(&removed, &metadata, stat_kind).unwrap(),
                       effective_kind(&live, &metadata, stat_kind).unwrap());
        }
    }

    /// The unproven sentinel never matches a stat, even one whose every other field agrees.
    #[test]
    fn an_unproven_entry_never_reads_unchanged() {
        let scratch = Scratch::new("sentinel-never-matches");
        let file = scratch.root.join("a.txt");
        std::fs::write(&file, "content").unwrap();
        let metadata = file_utils::get_symlink_metadata_for_path(&file).unwrap();
        let mut entry = build_inventory_item_from_stat(&file, "a.txt", "hash".to_string(),
                                                       DirEntryType::Normal,
                                                       anchor_proving_all_on(&file)).unwrap();
        assert!(is_entry_unchanged(&entry, &metadata, DirEntryType::Normal, &file));
        entry.content_change_timestamp = UNPROVEN_MTIME;
        assert!(!is_entry_unchanged(&entry, &metadata, DirEntryType::Normal, &file),
            "the unproven sentinel must never read unchanged");
    }

    #[test]
    fn cleanup_after_stack_stamps_a_stale_rollup() {
        let _scratch = Scratch::new("cleanup-stamp");

        write_stamped_shard("", Some("stale-rollup"));

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory(None).unwrap();

        let mut stamp_hashes: BTreeMap<String, String> = BTreeMap::new();
        stamp_hashes.insert("".to_string(), "fresh-rollup".to_string());

        cleanup_after_stack_with(&prepared, &stamp_hashes).unwrap();

        assert_eq!(read_rollup(""), Some("fresh-rollup".to_string()));
    }

    #[test]
    fn cleanup_after_stack_drops_consumed_deleted_entries() {
        let _scratch = Scratch::new("cleanup-deleted-drop");

        let mut inventory = Inventory::new();
        inventory.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        inventory.add_item(item("removed.txt", 2, InventoryItemState::Deleted));
        plant_shard(&inventory, "");

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry(""));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        let prepared = prepare_stack_inventory(None).unwrap();

        // No stamp target at all — the rewrite is triggered purely by the staged `Deleted`
        // entry that needs dropping, not by a rollup mismatch.
        cleanup_after_stack_with(&prepared, &BTreeMap::new()).unwrap();

        assert!(read_rollup("").is_none());

        let (_, bytes) = file_utils::retrieve_inventory_or_none_by_key("").unwrap();
        let rebuilt = parser::inventory::inventory_parser::parse_inventory(&bytes.unwrap()).unwrap();
        assert!(rebuilt.get_item_by_name("kept.txt").is_some(), "a surviving entry must be kept");
        assert!(rebuilt.get_item_by_name("removed.txt").is_none(), "the Deleted entry must be dropped");
    }

    #[test]
    fn a_corrupt_dirty_shard_during_load_yields_the_resilience_message_not_a_raw_parse_error() {
        // A join-point failure (here, a leftover shard that fails to parse) must not
        // `?`-propagate straight out of `create_inventory_for_directory`: what the walk decided
        // is still registered and published, and the user sees the documented "entries loaded so
        // far were kept, re-run" message a walk failure gives, not a raw parse error.
        let _scratch = Scratch::new("load-corrupt-dirty-shard");

        // A previously-registered directory whose shard is corrupt, with no counterpart left in
        // the working directory — exactly what `load`'s post-walk "leftover dirty entries" pass
        // consults, so this is the shard the join point's dirty-path loop tries (and fails) to
        // parse.
        let corrupt_path = file_utils::get_inventory_data_path_for_key("gone").unwrap();
        std::fs::create_dir_all(corrupt_path.parent().unwrap()).unwrap();
        std::fs::write(&corrupt_path, b"not a valid inventory shard").unwrap();

        let mut metadata: BTreeSet<String> = BTreeSet::new();
        metadata.insert(key_to_metadata_entry("gone"));
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();

        // A real, trackable file in the actual working directory — proves the walk's own
        // successful work is still published despite the dirty-path failure that comes after it.
        std::fs::write(_scratch.root.join("real.txt"), b"hello").unwrap();

        let path = crate::util::path_utils::WarehousePath::from_user_input(".").unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let result = runtime.block_on(create_inventory_for_directory(&path));

        let error = match result {
            Ok(()) => panic!("a corrupt dirty shard must fail the load"),
            Err(message) => message,
        };
        assert!(error.contains("did not complete"),
            "must use the graceful-failure resilience message, not a raw parse error: {error}");
        assert!(error.contains("gone"),
            "the underlying parse error should still name the offending shard: {error}");

        // The walk's own real work is still durable and registered: "entries loaded so far were
        // kept" is a real guarantee, not just wording in the message.
        assert!(file_utils::get_inventory_data_path_for_key("").unwrap().exists(),
            "the root shard this walk produced must still exist on disk");
        assert!(file_utils::get_inventory_data_path_for_key("real.txt").unwrap().parent().is_some());

        let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        let metadata_after = metadata_opt.unwrap();
        assert!(metadata_after.contains(&key_to_metadata_entry("")),
            "the newly discovered root directory must still be registered: {:?}", metadata_after);
    }

    #[test]
    fn a_metadata_entry_holding_a_line_break_reads_back_as_one_entry() {
        let _scratch = Scratch::new("metadata-line-break");

        let path = file_utils::get_inventory_metadata_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let metadata: BTreeSet<String> = ["src/a\nb".to_string(), "c\rd".to_string()].into();
        write_metadata_to_file(&path, &metadata).unwrap();

        let (_, read) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        let expected: BTreeSet<String> =
            [key_to_metadata_entry(""), "src".to_string(), "src/a\nb".to_string(), "c\rd".to_string()].into();
        assert_eq!(read.unwrap(), expected);
    }

    #[test]
    fn a_metadata_file_without_its_format_header_is_refused() {
        let _scratch = Scratch::new("metadata-headerless");

        let path = file_utils::get_inventory_metadata_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"./\nsrc\n").unwrap();

        let error = file_utils::retrieve_inventory_metadata_or_none().unwrap_err();
        assert!(error.contains("has no format header"), "{}", error);
    }

    /// Every shard folder in the store, by path.
    fn stored_shards() -> BTreeSet<std::path::PathBuf> {
        let store = file_utils::get_inventory_store_root().unwrap();
        let mut found = BTreeSet::new();

        for fan_out in std::fs::read_dir(&store).into_iter().flatten() {
            for shard in std::fs::read_dir(fan_out.unwrap().path()).unwrap() {
                found.insert(shard.unwrap().path());
            }
        }

        found
    }

    /// The shard folder of every registered key.
    fn registered_shards() -> BTreeSet<std::path::PathBuf> {
        let (_, metadata) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        metadata.unwrap_or_default().iter()
            .map(|entry| file_utils::get_inventory_folder_for_key(metadata_entry_to_key(entry)).unwrap())
            .collect()
    }

    /// A shard no registered key addresses: written, never registered.
    fn plant_ghost() {
        write_stamped_shard("ghost", None);
        assert!(stored_shards().contains(&file_utils::get_inventory_folder_for_key("ghost").unwrap()));
    }

    /// After a subtree removal, the store holds exactly the registered keys' shards, so a key
    /// registered later never reads a shard left over from before.
    fn assert_only_registered_shards_remain() {
        assert_eq!(stored_shards(), registered_shards());
        assert!(file_utils::retrieve_inventory_or_none_by_key("ghost").unwrap().1.is_none());
    }

    #[test]
    fn replacing_a_subtree_leaves_only_registered_shards_in_the_store() {
        let _scratch = Scratch::new("sweep-replace-subtree");
        stamped_chain_with_sibling();
        plant_ghost();

        let mut replacement = Inventory::new();
        replacement.add_item(item("new.txt", 9, InventoryItemState::Normal));
        ShardLedger::read().unwrap().replace_subtree("a", &BTreeMap::from([("a".to_string(), replacement)])).unwrap();

        assert!(!file_utils::get_inventory_folder_for_key("a/b").unwrap().exists(), "a/b was replaced away");
        assert_only_registered_shards_remain();
    }

    #[test]
    fn removing_inventories_under_a_key_leaves_only_registered_shards_in_the_store() {
        let _scratch = Scratch::new("sweep-remove-under");
        stamped_chain_with_sibling();
        plant_ghost();

        remove_inventories_under("a").unwrap();

        assert!(!file_utils::get_inventory_folder_for_key("a/b").unwrap().exists());
        assert_only_registered_shards_remain();
    }

    #[test]
    fn a_stack_cleanup_that_unregisters_a_key_leaves_only_registered_shards_in_the_store() {
        let _scratch = Scratch::new("sweep-cleanup");

        let mut root = Inventory::new();
        root.add_item(item("kept.txt", 1, InventoryItemState::Normal));
        plant_shard(&root, "");

        let mut emptied = Inventory::new();
        emptied.add_item(item("removed.txt", 2, InventoryItemState::Deleted));
        plant_shard(&emptied, "a");

        let metadata: BTreeSet<String> = ["", "a"].iter().map(|key| key_to_metadata_entry(key)).collect();
        let (metadata_path, _) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        write_metadata_to_file(&metadata_path, &metadata).unwrap();
        plant_ghost();

        let prepared = prepare_stack_inventory(None).unwrap();
        cleanup_after_stack_with(&prepared, &BTreeMap::new()).unwrap();

        assert!(!is_registered("a"), "the emptied directory is unregistered");
        assert_only_registered_shards_remain();
    }

    #[test]
    fn replacing_every_inventory_removes_unregistered_shards_too() {
        let _scratch = Scratch::new("sweep-replace-all");
        stamped_chain_with_sibling();
        plant_ghost();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&BTreeMap::from([("".to_string(), Inventory::new())])).unwrap();

        assert_only_registered_shards_remain();
    }

    /// An unregistered entry the sweep cannot remove: a folder inside a fan-out folder nobody may
    /// write to. Returns the fan-out folder, to make writable again before the scratch is dropped.
    #[cfg(unix)]
    fn plant_unremovable_ghost() -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;

        // Not hex, so no key's shard ever lands in it.
        let fan_out = file_utils::get_inventory_store_root().unwrap().join("zz");
        std::fs::create_dir_all(fan_out.join("ghost")).unwrap();
        std::fs::set_permissions(&fan_out, std::fs::Permissions::from_mode(0o500)).unwrap();

        fan_out
    }

    #[cfg(unix)]
    fn make_writable(folder: &Path) {
        use std::os::unix::fs::PermissionsExt;

        std::fs::set_permissions(folder, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    /// The sweep runs after the replacement has landed, so an entry it cannot remove must not fail
    /// the command or leave the load marker over a subtree that was just rebuilt.
    #[cfg(unix)]
    #[test]
    fn a_sweep_that_cannot_remove_an_entry_still_clears_the_replaced_subtrees_load_marker() {
        let _scratch = Scratch::new("sweep-blocked-replace");
        stamped_chain_with_sibling();

        let batch = file_utils::WriteBatch::new();
        load_guard_utils::stage_start_marker(&batch, "a").unwrap();
        batch.finish().unwrap();
        assert!(load_guard_utils::check_no_incomplete_load().is_err(), "the marker is recorded");

        let fan_out = plant_unremovable_ghost();

        let mut replacement = Inventory::new();
        replacement.add_item(item("new.txt", 9, InventoryItemState::Normal));
        let result = ShardLedger::read().unwrap().replace_subtree("a", &BTreeMap::from([("a".to_string(), replacement)]));

        let ghost_survived = fan_out.join("ghost").exists();
        make_writable(&fan_out);

        result.expect("a sweep failure must not fail a replacement that has landed");
        assert!(load_guard_utils::check_no_incomplete_load().is_ok(), "the rebuilt subtree's marker is cleared");
        assert!(ghost_survived || unsafe { libc::geteuid() } == 0, "the sweep was really blocked");
    }

    #[cfg(unix)]
    #[test]
    fn a_sweep_that_cannot_remove_an_entry_does_not_fail_a_subtree_removal() {
        let _scratch = Scratch::new("sweep-blocked-remove-under");
        stamped_chain_with_sibling();
        let fan_out = plant_unremovable_ghost();

        let result = remove_inventories_under("a");

        let ghost_survived = fan_out.join("ghost").exists();
        make_writable(&fan_out);

        result.expect("a sweep failure must not fail a removal that has landed");
        assert!(!is_registered("a"));
        assert!(ghost_survived || unsafe { libc::geteuid() } == 0, "the sweep was really blocked");
    }

    /// A registered key whose slot holds a file instead of a folder is removed like an unregistered
    /// one, so the damage does not block `narrow` or `restore --staged` over that subtree.
    #[test]
    fn removing_a_registered_key_removes_a_file_in_place_of_its_shard_folder() {
        let _scratch = Scratch::new("remove-registered-file-slot");
        stamped_chain_with_sibling();

        let folder = file_utils::get_inventory_folder_for_key("a/b").unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, b"not a folder").unwrap();

        remove_inventories_under("a").expect("a file in a registered slot is removable");

        assert!(std::fs::symlink_metadata(&folder).is_err(), "the file in the slot is gone");
        assert_only_registered_shards_remain();
    }

    /// Only folders belong at the fan-out level, so a sweep removes a file there whatever its name.
    #[test]
    fn a_sweep_removes_a_file_at_the_fan_out_level() {
        let _scratch = Scratch::new("sweep-fan-out-file");
        stamped_chain_with_sibling();

        let stray = file_utils::get_inventory_store_root().unwrap().join("ff");
        std::fs::write(&stray, b"not a folder").unwrap();

        remove_inventories_under("a").unwrap();

        assert!(std::fs::symlink_metadata(&stray).is_err(), "the file at the fan-out level is gone");
    }

    /// A link at the fan-out level is damage: the sweep removes the link and never enters it, so
    /// nothing outside the store is swept.
    #[cfg(unix)]
    #[test]
    fn a_sweep_removes_a_linked_fan_out_folder_without_entering_it() {
        let scratch = Scratch::new("sweep-fan-out-link");
        stamped_chain_with_sibling();

        let outside = scratch.root.join("outside");
        std::fs::create_dir_all(outside.join("project")).unwrap();
        std::fs::write(outside.join("notes.txt"), b"kept").unwrap();

        let link = file_utils::get_inventory_store_root().unwrap().join("zz");
        std::os::unix::fs::symlink(&outside, &link).unwrap();

        remove_inventories_under("a").unwrap();

        assert!(std::fs::symlink_metadata(&link).is_err(), "the link is gone");
        assert_eq!(std::fs::read(outside.join("notes.txt")).unwrap(), b"kept", "a file behind the link is kept");
        assert!(outside.join("project").is_dir(), "a folder behind the link is kept");
    }

    /// A sweep never lists through a link in place of the store, so nothing it points at is swept.
    #[cfg(unix)]
    #[test]
    fn a_sweep_does_not_enter_a_linked_store() {
        let scratch = Scratch::new("sweep-linked-store");
        stamped_chain_with_sibling();

        let outside = scratch.root.join("outside");
        std::fs::create_dir_all(outside.join("src")).unwrap();
        std::fs::write(outside.join("notes.txt"), b"kept").unwrap();
        std::fs::write(outside.join("src").join("main.rs"), b"kept").unwrap();

        let store_root = file_utils::get_inventory_store_root().unwrap();
        std::fs::remove_dir_all(&store_root).unwrap();
        std::os::unix::fs::symlink(&outside, &store_root).unwrap();

        sweep_unregistered_shards();

        assert_eq!(std::fs::read(outside.join("notes.txt")).unwrap(), b"kept", "a file behind the link is kept");
        assert_eq!(std::fs::read(outside.join("src").join("main.rs")).unwrap(), b"kept",
            "a folder's contents behind the link are kept");
    }

    /// A link in a shard slot is removed itself, on every platform, and what it points at is kept.
    #[test]
    fn removing_a_link_to_a_folder_in_a_shard_slot_keeps_its_target() {
        let scratch = Scratch::new("remove-linked-slot");
        stamped_chain_with_sibling();

        let target = scratch.root.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("kept"), b"kept").unwrap();

        let folder = file_utils::get_inventory_folder_for_key("a/b").unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, &folder).unwrap();
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&target, &folder).unwrap();

        remove_inventories_under("a").expect("a link in a registered slot is removable");

        assert!(std::fs::symlink_metadata(&folder).is_err(), "the link is gone");
        assert_eq!(std::fs::read(target.join("kept")).unwrap(), b"kept", "its target is untouched");
    }

    /// A wholesale reset is the way out of a damaged store, so a file where the store's folder
    /// belongs must not block it.
    #[test]
    fn replacing_every_inventory_removes_a_file_in_place_of_the_store() {
        let _scratch = Scratch::new("reset-store-is-a-file");
        stamped_chain_with_sibling();

        let store_root = file_utils::get_inventory_store_root().unwrap();
        std::fs::remove_dir_all(&store_root).unwrap();
        std::fs::write(&store_root, b"not a folder").unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&BTreeMap::from([("".to_string(), Inventory::new())]))
            .expect("a file in place of the store must not block the reset");

        assert!(std::fs::symlink_metadata(&store_root).unwrap().is_dir());
        assert_only_registered_shards_remain();
    }

    /// Where a shard's folder should be, a file is damage, never a directory with nothing staged.
    #[test]
    fn a_file_in_place_of_a_shard_folder_is_refused_by_the_key_it_belongs_to() {
        let _scratch = Scratch::new("shard-folder-is-a-file");

        let folder = file_utils::get_inventory_folder_for_key("docs").unwrap();
        std::fs::create_dir_all(folder.parent().unwrap()).unwrap();
        std::fs::write(&folder, b"not a folder").unwrap();

        let error = file_utils::retrieve_inventory_or_none_by_key("docs").unwrap_err();
        assert!(error.contains("inventory of folder \"docs\""), "{}", error);
    }

    #[test]
    fn replacing_every_inventory_overwrites_a_damaged_metadata_file_without_reading_it() {
        let _scratch = Scratch::new("metadata-damaged-reset");

        let path = file_utils::get_inventory_metadata_path();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a metadata record").unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&std::collections::BTreeMap::new())
            .expect("a wholesale reset must not depend on the metadata it replaces");

        let (_, read) = file_utils::retrieve_inventory_metadata_or_none().unwrap();
        assert_eq!(read.unwrap(), BTreeSet::new());
    }

    /// A shard as `stamped_chain_with_sibling` writes one — `file.txt` at inode 1 — with `extra`
    /// listed beside it when given, at `inode`, stamped `stamp`.
    fn chain_shard(inode: u64, extra: Option<&str>, stamp: Option<&str>) -> Inventory {
        let mut inventory = Inventory::new();
        inventory.add_item(item("file.txt", inode, InventoryItemState::Normal));
        if let Some(name) = extra {
            inventory.add_item(item(name, 2, InventoryItemState::Normal));
        }
        if let Some(hash) = stamp {
            inventory.stamp_rollup(RollupStamp::trusted(hash.to_string()));
        }
        inventory
    }

    /// The new set for a whole-inventory rewrite, by key.
    fn new_set(shards: Vec<(&str, Inventory)>) -> BTreeMap<String, Inventory> {
        shards.into_iter().map(|(key, inventory)| (key.to_string(), inventory)).collect()
    }

    /// Set the mtime of every one of `keys`' shard files to a fixed time long past, so that any
    /// rewrite shows in [`shard_file_identity`] even where the clock is coarse and there is no inode.
    fn pin_shard_mtimes(keys: &[&str]) {
        for key in keys {
            std::fs::File::options().write(true).open(file_utils::get_inventory_data_path_for_key(key).unwrap()).unwrap()
                .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000)).unwrap();
        }
    }

    /// What any write of `key`'s shard file changes: its inode (a write renames a new file over it)
    /// and its mtime (see [`pin_shard_mtimes`]).
    fn shard_file_identity(key: &str) -> (u64, std::time::SystemTime) {
        let metadata = std::fs::metadata(file_utils::get_inventory_data_path_for_key(key).unwrap()).unwrap();
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&metadata);
        #[cfg(not(unix))]
        let inode = 0;
        (inode, metadata.modified().unwrap())
    }

    /// Every shard in the store belongs to a registered key; a registered key that is only an
    /// ancestor of another may have none.
    fn assert_no_unregistered_shards() {
        let stored = stored_shards();
        assert!(stored.is_subset(&registered_shards()), "unregistered shards remain: {:?}",
            stored.difference(&registered_shards()).collect::<Vec<_>>());
    }

    #[test]
    fn replacing_every_inventory_leaves_a_shard_with_equal_bytes_untouched() {
        let _scratch = Scratch::new("diff-equal-untouched");
        stamped_chain_with_sibling();
        pin_shard_mtimes(&["", "a", "a/b", "x"]);
        let sibling = shard_file_identity("x");
        let ancestor = shard_file_identity("a");

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup2"))),
            ("a", chain_shard(1, None, Some("rollup2"))),
            ("a/b", chain_shard(1, Some("new.txt"), Some("rollup2"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).unwrap();

        assert_eq!(shard_file_identity("x"), sibling, "a shard whose bytes are equal is never rewritten");
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"));
        assert_ne!(shard_file_identity("a"), ancestor, "a shard whose bytes differ is rewritten");
        assert_eq!(registered_names("a/b"), vec!["file.txt".to_string(), "new.txt".to_string()]);
        for key in ["", "a", "a/b"] {
            assert_eq!(read_rollup(key).as_deref(), Some("rollup2"), "\"{key}\" carries its fresh stamp");
        }
    }

    /// A stat refresh changes a shard's bytes but no tree, so it rewrites that shard alone: every
    /// ancestor's stamp still names the same tree, and stands.
    #[test]
    fn a_stat_only_change_rewrites_only_its_own_shard_and_keeps_every_rollup() {
        let _scratch = Scratch::new("diff-stat-only");
        stamped_chain_with_sibling();
        pin_shard_mtimes(&["", "a", "a/b", "x"]);
        let untouched: Vec<_> = ["", "a", "x"].iter().map(|key| (*key, shard_file_identity(key))).collect();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup"))),
            ("a", chain_shard(1, None, Some("rollup"))),
            ("a/b", chain_shard(7, None, Some("rollup"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).unwrap();

        assert_eq!(registered_inode("a/b", "file.txt"), 7, "the stat refresh is written");
        for (key, identity) in untouched {
            assert_eq!(shard_file_identity(key), identity, "\"{key}\" is not rewritten");
        }
        for key in ["", "a", "a/b", "x"] {
            assert_eq!(read_rollup(key).as_deref(), Some("rollup"), "\"{key}\" keeps its rollup");
        }
    }

    /// Phase B's renames are durable only together, so a crash may keep a changed shard and lose
    /// its ancestor's rewrite: every stamp that is going must already be gone from disk before any
    /// of phase B can land. The fault stops the rewrite at the registration between the phases.
    #[test]
    fn a_rewrite_clears_every_stale_rollup_before_any_new_content_can_land() {
        let _scratch = Scratch::new("diff-phase-a");
        stamped_chain_with_sibling();

        crate::util::operation_utils::raise_for_fixture();
        let fault = MetadataWriteFaultGuard::failing();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup2"))),
            ("a", chain_shard(1, None, Some("rollup2"))),
            ("a/b", chain_shard(1, Some("new.txt"), Some("rollup2"))),
            ("a/b/c", chain_shard(1, None, Some("rollup2"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).expect_err("the registration of a/b/c fails");
        drop(fault);

        assert_eq!(registered_names("a/b"), vec!["file.txt".to_string()], "no new content landed");
        for key in ["", "a", "a/b"] {
            assert_eq!(read_rollup(key), None, "the going stamp of \"{key}\" is cleared first");
        }
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "a stamp that stays is never cleared");
    }

    #[test]
    fn a_shard_that_does_not_parse_falls_back_to_rewriting_every_shard() {
        let _scratch = Scratch::new("diff-unparseable");
        stamped_chain_with_sibling();
        pin_shard_mtimes(&["", "a", "a/b", "x"]);
        let ancestor = shard_file_identity("a");
        std::fs::write(file_utils::get_inventory_data_path_for_key("x").unwrap(), b"not a shard").unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup"))),
            ("a", chain_shard(1, None, Some("rollup"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).expect("a shard that does not parse must not block the reset");

        assert_eq!(registered_names("x"), vec!["file.txt".to_string()]);
        assert_ne!(shard_file_identity("a"), ancestor, "the store was rewritten whole");
        assert!(!is_registered("a/b"));
        assert_only_registered_shards_remain();
    }

    #[test]
    fn a_shard_address_that_cannot_be_read_falls_back_to_rewriting_every_shard() {
        let _scratch = Scratch::new("diff-unreadable");
        stamped_chain_with_sibling();
        let folder = file_utils::get_inventory_folder_for_key("x").unwrap();
        std::fs::remove_dir_all(&folder).unwrap();
        std::fs::write(&folder, b"not a folder").unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).expect("a damaged shard address must not block the reset");

        assert_eq!(registered_names("x"), vec!["file.txt".to_string()]);
        assert_only_registered_shards_remain();
    }

    /// With nothing registered, no shard read finds the damage: the store root itself is judged.
    #[test]
    fn a_file_in_place_of_the_store_falls_back_even_with_nothing_registered() {
        let _scratch = Scratch::new("diff-store-is-a-file");
        let store_root = file_utils::get_inventory_store_root().unwrap();
        std::fs::create_dir_all(store_root.parent().unwrap()).unwrap();
        std::fs::write(&store_root, b"not a folder").unwrap();
        write_metadata_to_file(&file_utils::get_inventory_metadata_path(), &BTreeSet::new()).unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![("", chain_shard(1, None, None))]))
            .expect("a file in place of the store must not block the reset");

        assert!(std::fs::symlink_metadata(&store_root).unwrap().is_dir());
        assert_eq!(registered_names(""), vec!["file.txt".to_string()]);
    }

    #[test]
    fn replacing_every_inventory_resets_damaged_metadata_over_a_populated_store() {
        let _scratch = Scratch::new("diff-metadata-damaged");
        stamped_chain_with_sibling();
        std::fs::write(file_utils::get_inventory_metadata_path(), b"not a metadata record").unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![("a", chain_shard(1, None, Some("rollup")))]))
            .expect("damaged metadata must not block the reset");

        assert!(is_registered("a") && is_registered(""));
        assert!(!is_registered("a/b") && !is_registered("x"));
        assert_no_unregistered_shards();
    }

    #[test]
    fn a_rewrite_unregisters_and_sweeps_a_dropped_directory() {
        let _scratch = Scratch::new("diff-dropped");
        stamped_chain_with_sibling();
        pin_shard_mtimes(&["", "a", "a/b", "x"]);
        plant_ghost();
        let sibling = shard_file_identity("x");

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup2"))),
            ("a", chain_shard(1, None, Some("rollup2"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).unwrap();

        assert!(!is_registered("a/b"), "the dropped directory is unregistered");
        assert!(!file_utils::get_inventory_folder_for_key("a/b").unwrap().exists(), "and its shard is gone");
        assert_eq!(shard_file_identity("x"), sibling);
        assert_only_registered_shards_remain();
    }

    /// A registered directory with no shard of its own (it held only subdirectories) is unregistered
    /// when the new set drops it, like any other.
    #[test]
    fn a_rewrite_unregisters_a_dropped_directory_that_had_no_shard() {
        let _scratch = Scratch::new("diff-dropped-no-shard");
        write_stamped_shard("", Some("rollup"));
        write_stamped_shard("a/b/c", Some("rollup"));
        let metadata: BTreeSet<String> = ["", "a", "a/b", "a/b/c"].iter().map(|key| key_to_metadata_entry(key)).collect();
        write_metadata_to_file(&file_utils::get_inventory_metadata_path(), &metadata).unwrap();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![("", chain_shard(1, None, Some("rollup2")))])).unwrap();

        for key in ["a", "a/b", "a/b/c"] {
            assert!(!is_registered(key), "\"{key}\" is dropped");
        }
        assert_no_unregistered_shards();
    }

    /// A key whose own entries alone change lands its new content and new stamp in one rename, so
    /// its old stamp is not cleared first; only the stamps above a change are.
    #[test]
    fn a_rewrite_clears_only_the_stamps_above_a_change() {
        let _scratch = Scratch::new("diff-leaf-stamp");
        stamped_chain_with_sibling();

        crate::util::operation_utils::raise_for_fixture();
        let fault = MetadataWriteFaultGuard::failing();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup2"))),
            ("a", chain_shard(1, None, Some("rollup"))),
            ("a/b", chain_shard(1, None, Some("rollup"))),
            ("x", chain_shard(1, Some("new.txt"), Some("rollup2"))),
            ("y", chain_shard(1, None, Some("rollup2"))),
        ])).expect_err("the registration of y fails");
        drop(fault);

        assert_eq!(read_rollup(""), None, "the root is above the change");
        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "x is the change itself");
    }

    /// A shard folder made for a registered key that had none is synced into its fan-out folder:
    /// registration, which syncs a new key's address, does not run for a key registered already.
    #[test]
    fn a_shard_folder_made_for_a_registered_key_is_synced_into_its_fan_out() {
        let _scratch = Scratch::new("diff-folder-sync");
        write_stamped_shard("", None);
        write_stamped_shard("a/b", None);
        let metadata: BTreeSet<String> = ["", "a", "a/b"].iter().map(|key| key_to_metadata_entry(key)).collect();
        write_metadata_to_file(&file_utils::get_inventory_metadata_path(), &metadata).unwrap();
        let folder = file_utils::get_inventory_folder_for_key("a").unwrap();
        assert!(!folder.exists());

        crate::util::operation_utils::raise_for_fixture();
        let syncs = file_utils::DirSyncFaultGuard::recording();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, None)),
            ("a", chain_shard(1, None, None)),
            ("a/b", chain_shard(1, None, None)),
        ])).unwrap();
        let attempts = file_utils::dir_sync_attempts();
        drop(syncs);

        if cfg!(unix) && file_utils::fsync_enabled() {
            assert!(attempts.contains(&folder.parent().unwrap().to_path_buf()),
                "the fan-out folder holding a's new shard folder is synced: {attempts:?}");
        }
        assert_eq!(registered_names("a"), vec!["file.txt".to_string()]);
    }

    /// A directory the new set keeps only as an ancestor has no shard of its own: whatever it held
    /// before must not still read as staged.
    #[test]
    fn a_kept_directory_with_no_shard_of_its_own_loses_its_old_shard() {
        let _scratch = Scratch::new("diff-ancestor-only");
        stamped_chain_with_sibling();

        crate::util::operation_utils::raise_for_fixture();
        replace_all_inventories(&new_set(vec![
            ("", chain_shard(1, None, Some("rollup2"))),
            ("a/b", chain_shard(1, None, Some("rollup"))),
            ("x", chain_shard(1, None, Some("rollup"))),
        ])).unwrap();

        assert!(is_registered("a"), "the ancestor stays registered");
        assert!(ShardLedger::read().unwrap().retrieve("a").unwrap().is_none(), "with no shard");
        assert_no_unregistered_shards();
    }

    /// The names the registered shard at `key` lists, read through the ledger as every writer reads.
    fn registered_names(key: &str) -> Vec<String> {
        ShardLedger::read().unwrap().inventory(key).unwrap()
            .map(|inventory| inventory.get_items().map(|(name, _)| name.clone()).collect())
            .unwrap_or_default()
    }

    /// An unregistered shard at `key` listing `file.txt`, which no working directory holds.
    fn plant_ghost_at(key: &str) {
        write_stamped_shard(key, None);
        assert!(!is_registered(key), "a ghost is not registered");
    }

    fn add_new_txt(inventory: &mut Inventory) -> Result<(), String> {
        inventory.add_item(item("new.txt", 2, InventoryItemState::Normal));
        Ok(())
    }

    /// A load reads the working directory from the process's current folder, which `cargo test`
    /// sets to this crate's root, while the inventory lands in the scratch storage root; so the
    /// load tests load this crate's own sources.
    const LOADED_FOLDER: &str = "src/enums";
    const LOADED_FILE: &str = "src/enums/object.rs";

    fn load(raw: &str) -> Result<(), String> {
        let path = WarehousePath::from_user_input(raw).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        runtime.block_on(create_inventory_for_directory(&path))
    }

    fn load_file(raw: &str) -> Result<(), String> {
        add_file_to_inventory(&WarehousePath::from_user_input(raw).unwrap())
    }

    fn fan_out_of(key: &str) -> std::path::PathBuf {
        file_utils::get_inventory_store_root().unwrap().join(file_utils::shard_address(key).0)
    }

    /// A key whose fan-out folder satisfies `wanted`.
    fn key_where(wanted: impl Fn(&str) -> bool) -> String {
        (0..).map(|i| format!("k{i}")).find(|key| wanted(key)).unwrap()
    }

    #[test]
    fn a_shard_update_never_adopts_an_unregistered_shard() {
        let _scratch = Scratch::new("adopt-update-shard");
        plant_ghost_at("ghost");

        ShardLedger::read().unwrap().update_shard("ghost", add_new_txt).unwrap();

        assert_eq!(registered_names("ghost"), ["new.txt"]);
    }

    #[test]
    fn a_file_load_never_adopts_an_unregistered_shard() {
        let _scratch = Scratch::new("adopt-file-load");
        plant_ghost_at(LOADED_FOLDER);

        load_file(LOADED_FILE).unwrap();

        assert_eq!(registered_names(LOADED_FOLDER), ["object.rs"]);
    }

    #[test]
    fn a_folder_load_never_adopts_an_unregistered_shard() {
        let _scratch = Scratch::new("adopt-folder-load");
        plant_ghost_at(LOADED_FOLDER);

        load(LOADED_FOLDER).unwrap();

        let names = registered_names(LOADED_FOLDER);
        assert!(names.iter().any(|name| name == "object.rs"), "{names:?}");
        assert!(!names.iter().any(|name| name == "file.txt"), "the unregistered shard's entry was adopted: {names:?}");
    }

    #[test]
    fn a_file_removal_never_adopts_an_unregistered_shard() {
        let _scratch = Scratch::new("adopt-file-removal");
        plant_ghost_at("ghost");

        let error = stage_removal_for_file(&mut ShardLedger::read().unwrap(), &WarehousePath::from_user_input("ghost/file.txt").unwrap()).unwrap_err();

        assert!(error.contains("is not in the inventory"), "{error}");
        assert!(!is_registered("ghost"));
    }

    /// The ancestors a load registers with its folder are new keys too, so a shard left at one is
    /// cleared rather than read.
    #[test]
    fn a_load_never_adopts_an_unregistered_shard_at_an_ancestor_it_registers() {
        let _scratch = Scratch::new("adopt-ancestor");
        plant_ghost_at("src");

        load_file(LOADED_FILE).unwrap();

        assert!(is_registered("src"), "the load registers its folder's ancestors");
        assert_eq!(registered_names("src"), Vec::<String>::new());
    }

    /// Every shard in the store is at a registered key.
    fn assert_no_unregistered_shard() {
        let unregistered: Vec<_> = stored_shards().difference(&registered_shards()).cloned().collect();
        assert!(unregistered.is_empty(), "shards published at unregistered keys: {unregistered:?}");
    }

    #[test]
    fn a_shard_update_whose_registration_fails_publishes_nothing() {
        let _scratch = Scratch::new("escape-update-shard");
        stamped_chain_with_sibling();
        let fault = MetadataWriteFaultGuard::failing();

        let error = ShardLedger::read().unwrap().update_shard("new", add_new_txt).unwrap_err();

        drop(fault);
        assert!(error.contains("injected metadata-write failure"), "{error}");
        assert_no_unregistered_shard();
    }

    #[test]
    fn a_file_load_whose_registration_fails_publishes_nothing() {
        let _scratch = Scratch::new("escape-file-load");
        stamped_chain_with_sibling();
        let fault = MetadataWriteFaultGuard::failing();

        let error = load_file(LOADED_FILE).unwrap_err();

        drop(fault);
        assert!(error.contains("injected metadata-write failure"), "{error}");
        assert_no_unregistered_shard();
    }

    #[test]
    fn a_folder_load_whose_registration_fails_publishes_nothing() {
        let _scratch = Scratch::new("escape-folder-load");
        stamped_chain_with_sibling();
        let fault = MetadataWriteFaultGuard::failing();

        let error = load(LOADED_FOLDER).unwrap_err();

        drop(fault);
        assert!(error.contains("injected metadata-write failure"), "{error}");
        assert_no_unregistered_shard();
    }

    #[test]
    fn a_subtree_replacement_whose_registration_fails_publishes_nothing() {
        let _scratch = Scratch::new("escape-replace-subtree");
        stamped_chain_with_sibling();
        let fault = MetadataWriteFaultGuard::failing();

        let replacement = || {
            let mut inventory = Inventory::new();
            inventory.add_item(item("new.txt", 9, InventoryItemState::Normal));
            inventory
        };
        let shards = BTreeMap::from([("a".to_string(), replacement()), ("a/c".to_string(), replacement())]);
        let error = ShardLedger::read().unwrap().replace_subtree("a", &shards).unwrap_err();

        drop(fault);
        assert!(error.contains("injected metadata-write failure"), "{error}");
        assert_no_unregistered_shard();
    }

    /// A batch reads its baseline and compares at publication through the ledger, so a shard at
    /// an unregistered key is neither adopted nor taken for a rewrite.
    #[test]
    fn a_batch_never_adopts_an_unregistered_shard() {
        let _scratch = Scratch::new("batch-ghost");
        plant_ghost_at("ghost");

        let mut batch = ShardMutationBatch::new();
        batch.update("ghost", |inventory| {
            inventory.add_item(item("new.txt", 2, InventoryItemState::Normal));
        }).unwrap();
        batch.publish().expect("an unregistered shard is not a rewrite since the batch read");

        assert_eq!(registered_names("ghost"), ["new.txt"]);
    }

    #[test]
    fn a_batch_refuses_to_publish_over_a_shard_registered_since_it_was_read() {
        let _scratch = Scratch::new("batch-late-registration");

        let mut batch = ShardMutationBatch::new();
        batch.update("late", |inventory| {
            inventory.add_item(item("new.txt", 2, InventoryItemState::Normal));
        }).unwrap();

        ShardLedger::read().unwrap().update_shard("late", |inventory| {
            inventory.add_item(item("other.txt", 9, InventoryItemState::Normal));
            Ok(())
        }).unwrap();

        let error = batch.publish().unwrap_err();
        assert!(error.contains("changed while it was being updated"), "unexpected error: {error}");
        assert_eq!(registered_names("late"), ["other.txt"], "the other writer's shard must not be overwritten");
    }

    /// A batch whose shard was removed since it read it refuses before it registers anything: the
    /// key another writer dropped is not registered again.
    #[test]
    fn a_batch_refusing_over_a_removed_shard_registers_nothing() {
        let _scratch = Scratch::new("batch-removed-since-read");
        stamped_chain_with_sibling();

        let mut batch = ShardMutationBatch::new();
        batch.update("a/b", |inventory| {
            inventory.add_item(item("new.txt", 2, InventoryItemState::Normal));
        }).unwrap();

        remove_inventories_under("a/b").unwrap();

        let error = batch.publish().unwrap_err();
        assert!(error.contains("changed while it was being updated"), "unexpected error: {error}");
        assert!(!ShardLedger::read().unwrap().holds("a/b"), "the dropped key is not registered again");
    }

    #[test]
    fn the_shard_writers_refuse_an_unregistered_key() {
        let _scratch = Scratch::new("writers-refuse");
        stamped_chain_with_sibling();
        let mut ledger = ShardLedger::read().unwrap();

        let mut inventory = Inventory::new();
        inventory.add_item(item("new.txt", 2, InventoryItemState::Normal));

        let refused = |result: Result<(), String>| {
            let error = result.unwrap_err();
            assert!(error.contains("not registered in the inventory metadata"), "{error}");
        };
        refused(write_shard_mutation(&mut ledger, "stray", inventory));

        assert!(file_utils::retrieve_inventory_or_none_by_key("stray").unwrap().1.is_none(), "nothing was written");
        assert_eq!(read_rollup(""), Some("rollup".to_string()), "a refused write clears no rollup");
    }

    /// Only folders belong at the fan-out level, so registering a key removes a file in its
    /// fan-out folder's place.
    #[test]
    fn registering_a_key_removes_a_file_at_its_fan_out_level() {
        let _scratch = Scratch::new("clear-fan-out-file");
        stamped_chain_with_sibling();

        let key = key_where(|key| !fan_out_of(key).exists());
        std::fs::write(fan_out_of(&key), b"not a folder").unwrap();

        ShardLedger::read().unwrap().update_shard(&key, add_new_txt).unwrap();

        assert_eq!(registered_names(&key), ["new.txt"]);
    }

    /// A link at the fan-out level is removed itself and never entered, so nothing it points at is
    /// cleared or written.
    #[cfg(unix)]
    #[test]
    fn registering_a_key_removes_a_linked_fan_out_folder_without_entering_it() {
        let scratch = Scratch::new("clear-fan-out-link");
        stamped_chain_with_sibling();

        let key = key_where(|key| !fan_out_of(key).exists());
        let slot_name = file_utils::shard_address(&key).1;
        let outside = scratch.root.join("outside");
        std::fs::create_dir_all(outside.join(&slot_name)).unwrap();
        std::fs::write(outside.join(&slot_name).join("kept"), b"kept").unwrap();
        std::fs::write(outside.join("notes.txt"), b"kept").unwrap();
        std::os::unix::fs::symlink(&outside, fan_out_of(&key)).unwrap();

        ShardLedger::read().unwrap().update_shard(&key, add_new_txt).unwrap();

        let fan_out = std::fs::symlink_metadata(fan_out_of(&key)).unwrap();
        assert!(fan_out.is_dir() && !fan_out.is_symlink(), "the link is replaced by a fan-out folder");
        assert_eq!(std::fs::read(outside.join("notes.txt")).unwrap(), b"kept", "a file behind the link is kept");
        assert_eq!(std::fs::read(outside.join(&slot_name).join("kept")).unwrap(), b"kept",
            "a folder behind the link is kept with what it holds");
        assert!(!outside.join(&slot_name).join("data").exists(), "no shard was written behind the link");
        assert_eq!(registered_names(&key), ["new.txt"]);
    }

    #[test]
    fn replacing_every_inventory_holds_the_registered_set_closed_under_ancestors() {
        let _scratch = Scratch::new("replace-ledger");
        stamped_chain_with_sibling();

        let ledger = ShardLedger::replace(&BTreeSet::from(["p/q".to_string()])).unwrap();

        let expected: BTreeSet<String> = ["", "p", "p/q"].iter().map(|key| key.to_string()).collect();
        assert_eq!(ledger.keys().map(|key| key.to_string()).collect::<BTreeSet<_>>(), expected);
        assert_eq!(ShardLedger::read().unwrap().keys().map(|key| key.to_string()).collect::<BTreeSet<_>>(), expected);
    }

    /// Register `key` while the sync of `folder` fails: the registration fails with it, before the
    /// metadata names `key`.
    #[cfg(unix)]
    fn assert_registration_waits_for_the_sync_of(key: &str, folder: &Path) {
        assert!(file_utils::fsync_enabled(), "FORKLIFT_FSYNC turns fsync off for the in-process tests; unset it");
        assert!(folder.is_dir(), "{} exists", folder.display());

        let fault = file_utils::DirSyncFaultGuard::failing_exactly(folder);
        let result = ShardLedger::read().unwrap().update_shard(key, add_new_txt);
        drop(fault);

        assert!(!is_registered(key), "\"{key}\" was registered before {} was synced", folder.display());
        assert!(result.unwrap_err().contains("injected directory-sync failure"));
    }

    #[cfg(unix)]
    #[test]
    fn registering_a_key_syncs_its_fan_out_folder_first() {
        let _scratch = Scratch::new("sync-fan-out");
        stamped_chain_with_sibling();

        let key = key_where(|key| key != "x" && fan_out_of(key) == fan_out_of("x"));

        assert_registration_waits_for_the_sync_of(&key, &fan_out_of(&key));
    }

    /// A remover's unlink is not synced, so the fan-out folder syncs even when its slot is
    /// already empty.
    #[cfg(unix)]
    #[test]
    fn registering_a_key_syncs_its_fan_out_folder_after_a_removal_emptied_its_slot() {
        let _scratch = Scratch::new("sync-emptied-slot");
        stamped_chain_with_sibling();

        let key = key_where(|key| key != "x" && fan_out_of(key) == fan_out_of("x"));
        ShardLedger::read().unwrap().update_shard(&key, add_new_txt).unwrap();
        remove_inventories_under(&key).unwrap();
        assert!(!file_utils::get_inventory_folder_for_key(&key).unwrap().exists());

        assert_registration_waits_for_the_sync_of(&key, &fan_out_of(&key));
    }

    #[cfg(unix)]
    #[test]
    fn registering_a_key_syncs_the_store_when_its_fan_out_folder_is_absent() {
        let _scratch = Scratch::new("sync-store-root");
        stamped_chain_with_sibling();

        let key = key_where(|key| !fan_out_of(key).exists());

        assert_registration_waits_for_the_sync_of(&key, &file_utils::get_inventory_store_root().unwrap());
    }

    #[cfg(unix)]
    #[test]
    fn registering_a_key_syncs_the_inventory_folder() {
        let _scratch = Scratch::new("sync-inventory-root");
        stamped_chain_with_sibling();

        let inventory_root = std::path::PathBuf::from(file_utils::get_path_inventory_root());

        assert_registration_waits_for_the_sync_of("new", &inventory_root);
    }

    /// A link in place of the store is refused before anything behind it is touched, and the way
    /// out the refusal names works.
    #[cfg(unix)]
    #[test]
    fn registering_a_key_refuses_a_linked_store_and_leaves_its_target_alone() {
        let scratch = Scratch::new("linked-store-refused");
        stamped_chain_with_sibling();

        let (fan_out_name, slot_name) = file_utils::shard_address("new");
        let outside = scratch.root.join("outside");
        std::fs::create_dir_all(outside.join(&fan_out_name).join(&slot_name)).unwrap();
        std::fs::write(outside.join(&fan_out_name).join(&slot_name).join("kept"), b"kept").unwrap();

        let store_root = file_utils::get_inventory_store_root().unwrap();
        std::fs::remove_dir_all(&store_root).unwrap();
        std::os::unix::fs::symlink(&outside, &store_root).unwrap();

        let result = ShardLedger::read().unwrap().update_shard("new", add_new_txt);

        assert_eq!(std::fs::read(outside.join(&fan_out_name).join(&slot_name).join("kept")).unwrap(), b"kept",
            "nothing behind the link is cleared");
        assert!(!outside.join(&fan_out_name).join(&slot_name).join("data").exists(), "nothing behind the link is written");
        assert!(!is_registered("new"));
        let error = result.unwrap_err();
        assert!(error.contains("is not a folder") && error.contains("restore --staged ."), "{error}");

        // The way out: remove the link, then reset the staging area to the head (here, empty).
        std::fs::remove_file(&store_root).unwrap();
        ShardLedger::read().unwrap().replace_subtree("", &BTreeMap::from([("".to_string(), Inventory::new())])).unwrap();
        ShardLedger::read().unwrap().update_shard("new", add_new_txt).unwrap();
        assert_eq!(registered_names("new"), ["new.txt"]);
    }

    /// A link anywhere on a registered key's shard address is damage, even with a valid shard
    /// behind it: the read refuses it unfollowed rather than take another folder's file for this
    /// directory's staged state, no write lands behind it, and the way out the refusal names works.
    #[test]
    fn a_link_at_any_level_of_a_shard_address_is_refused_unfollowed() {
        let data = || file_utils::get_inventory_data_path_for_key("a/b").unwrap();

        for level in ["store", "fan-out", "slot", "data"] {
            let scratch = Scratch::new(&format!("linked-{level}"));
            stamped_chain_with_sibling();

            let data_path = data();
            let linked = match level {
                "store" => data_path.ancestors().nth(3).unwrap().to_path_buf(),
                "fan-out" => data_path.ancestors().nth(2).unwrap().to_path_buf(),
                "slot" => data_path.ancestors().nth(1).unwrap().to_path_buf(),
                _ => data_path.clone(),
            };
            let outside = scratch.root.join("outside");
            std::fs::rename(&linked, &outside).unwrap();
            let is_dir = level != "data";
            link_to(&outside, &linked, is_dir);

            let behind = if is_dir { outside.join(data_path.strip_prefix(&linked).unwrap()) } else { outside.clone() };
            let shard_behind = std::fs::read(&behind).unwrap();
            let mut ledger = ShardLedger::read().unwrap();

            let read = ledger.retrieve("a/b");
            let written = write_shard_mutation(&mut ledger, "a/b", Inventory::new());
            let updated = ShardLedger::read().unwrap().update_shard("a/b", add_new_txt);

            assert_eq!(std::fs::read(&behind).unwrap(), shard_behind, "{level}: nothing is written behind the link");
            assert!(written.is_err(), "{level}: a write through the link is refused");
            assert!(updated.is_err(), "{level}: an update through the link is refused");
            let error = read.expect_err(level);
            assert!(error.contains("a link is never followed") && error.contains("restore --staged ."), "{level}: {error}");

            // The way out: remove the link, then reset the staging area to the head (here, empty).
            unlink(&linked, is_dir);
            ShardLedger::read().unwrap().replace_subtree("", &BTreeMap::from([("".to_string(), Inventory::new())])).unwrap();
            ShardLedger::read().unwrap().update_shard("a/b", add_new_txt).unwrap();
            assert_eq!(registered_names("a/b"), ["new.txt"], "{level}");
        }
    }

    /// An inventory root moved behind a link whose target is out of reach is not read as a staging
    /// area with nothing in it, and the refusal sends the user to the target, not to a reset that
    /// would abandon what is staged there.
    #[test]
    fn an_inventory_root_out_of_reach_is_refused_with_its_own_way_out() {
        let scratch = Scratch::new("inventory-root-out-of-reach");
        stamped_chain_with_sibling();

        let inventory_root = std::path::PathBuf::from(file_utils::get_path_inventory_root());
        let moved = scratch.root.join("drive");
        std::fs::rename(&inventory_root, &moved).unwrap();
        link_to(&scratch.root.join("unmounted"), &inventory_root, true);

        let ledger = ShardLedger::read();
        let shard = file_utils::retrieve_inventory_or_none_by_key("a/b");

        for (what, result) in [("ledger", ledger.map(|_| ())), ("shard", shard.map(|_| ()))] {
            let error = result.expect_err(what);
            assert!(error.contains("does not resolve to a folder") && error.contains("make its target available"),
                    "{what}: {error}");
            assert!(!error.contains("a link is never followed"), "{what}: {error}");
        }

        // The way out: bring the target back.
        unlink(&inventory_root, true);
        link_to(&moved, &inventory_root, true);
        assert_eq!(registered_names("a/b"), ["file.txt"]);
    }

    /// The registered set decides what every writer may touch, so it is never read through a link,
    /// and the way out the refusal names works.
    #[test]
    fn a_linked_inventory_metadata_is_refused_unfollowed() {
        let scratch = Scratch::new("linked-metadata");
        stamped_chain_with_sibling();

        let metadata = file_utils::get_inventory_metadata_path();
        let outside = scratch.root.join("other-metadata");
        std::fs::rename(&metadata, &outside).unwrap();
        link_to(&outside, &metadata, false);

        let error = ShardLedger::read().map(|_| ()).expect_err("a linked metadata file");
        assert!(error.contains("is not a file (a link is never followed)") && error.contains("restore --staged ."), "{error}");

        // The way out: remove the link, then reset the staging area to the head (here, empty).
        unlink(&metadata, false);
        ShardLedger::read().unwrap().replace_subtree("", &BTreeMap::from([("".to_string(), Inventory::new())])).unwrap();
        ShardLedger::read().unwrap().update_shard("a/b", add_new_txt).unwrap();
        assert_eq!(registered_names("a/b"), ["new.txt"]);
    }

    /// The format-version gate never opens a linked metadata file, so what is refused is the link,
    /// not the version of whatever it points at. The gate runs once per inventory root, so nothing
    /// may reach a shard path before the link is in place.
    #[test]
    fn the_format_version_gate_never_opens_a_linked_metadata_file() {
        let scratch = Scratch::new("gate-linked-metadata");

        let metadata = file_utils::get_inventory_metadata_path();
        std::fs::create_dir_all(metadata.parent().unwrap()).unwrap();
        let outside = scratch.root.join("other-version-metadata");
        std::fs::write(&outside, "/forklift-inventory-metadata 999\n").unwrap();
        link_to(&outside, &metadata, false);

        let store_root = file_utils::get_inventory_store_root();
        let error = ShardLedger::read().map(|_| ()).expect_err("a linked metadata file");

        assert!(store_root.is_ok(), "{store_root:?}");
        assert!(error.contains("is not a file (a link is never followed)"), "{error}");
    }

    fn link_to(target: &Path, link: &Path, is_dir: bool) {
        #[cfg(unix)]
        {
            let _ = is_dir;
            std::os::unix::fs::symlink(target, link).unwrap();
        }
        #[cfg(windows)]
        if is_dir {
            std::os::windows::fs::symlink_dir(target, link).unwrap();
        } else {
            std::os::windows::fs::symlink_file(target, link).unwrap();
        }
    }

    /// Windows removes a link to a folder as a folder.
    fn unlink(link: &Path, is_dir: bool) {
        if cfg!(windows) && is_dir {
            std::fs::remove_dir(link).unwrap();
        } else {
            std::fs::remove_file(link).unwrap();
        }
    }

    /// A key a subtree replacement republishes is new to the ledger, so its address is cleared and
    /// synced before the metadata names it again, and nothing is published if that fails. The
    /// key's ancestors live in other fan-out folders, so the failing sync is the key's own.
    #[cfg(unix)]
    #[test]
    fn replacing_a_subtree_syncs_each_republished_keys_fan_out_folder_first() {
        let _scratch = Scratch::new("sync-replace-subtree");
        stamped_chain_with_sibling();

        let key = key_where(|key| key != "x" && fan_out_of(key) == fan_out_of("x") && fan_out_of(key) != fan_out_of(""));
        ShardLedger::read().unwrap().update_shard(&key, add_new_txt).unwrap();

        let mut replacement = Inventory::new();
        replacement.add_item(item("replaced.txt", 9, InventoryItemState::Normal));

        let fault = file_utils::DirSyncFaultGuard::failing_exactly(&fan_out_of(&key));
        let result = ShardLedger::read().unwrap().replace_subtree(&key, &BTreeMap::from([(key.clone(), replacement)]));
        drop(fault);

        assert!(file_utils::retrieve_inventory_or_none_by_key(&key).unwrap().1.is_none(),
            "no shard is published before its address is synced");
        assert!(result.unwrap_err().contains("injected directory-sync failure"));
    }

    /// The kind a load, removal or restore picks is read through the ledger too, so a shard at an
    /// unregistered parent cannot make a real folder look like a tracked file.
    #[test]
    fn tracked_kind_never_reads_an_unregistered_parent() {
        let _scratch = Scratch::new("tracked-kind-ghost-parent");

        let mut ghost = Inventory::new();
        ghost.add_item(item("enums", 1, InventoryItemState::Normal));
        plant_shard(&ghost, "src");
        assert!(!is_registered("src"));

        let kind = tracked_kind(&ShardLedger::read().unwrap(), &WarehousePath::from_user_input(LOADED_FOLDER).unwrap()).unwrap();

        assert!(matches!(kind, TrackedKind::UntrackedDirectory), "a folder on disk the ledger does not hold");
    }

    /// Whether a merge may write over a file is decided by whether the inventory tracks it, so a
    /// shard at an unregistered parent cannot make an untracked file look tracked.
    #[test]
    fn a_tracked_file_is_never_read_from_an_unregistered_parent() {
        let _scratch = Scratch::new("tracked-file-ghost-parent");
        stamped_chain_with_sibling();
        plant_ghost_at("ghost");

        let ledger = ShardLedger::read().unwrap();
        let mut check = alias_utils::AliasCheck::new();

        assert!(alias_utils::tracked_file(&ledger, "a/file.txt").unwrap(), "a registered parent's file");
        assert!(!alias_utils::tracked_file(&ledger, "ghost/file.txt").unwrap(), "read through the ledger");
        assert!(!check.tracked_file("ghost/file.txt").unwrap(), "read through the check's ledger");
    }

    /// A key still registered and not removed is not new, so an update never clears its shard.
    #[test]
    fn a_ledger_update_never_clears_a_registered_key_it_keeps() {
        let _scratch = Scratch::new("ledger-update-keeps");
        stamped_chain_with_sibling();

        let mut ledger = ShardLedger::read().unwrap();
        ledger.update(&BTreeSet::from(["a/b".to_string()]), &BTreeSet::from(["a/b".to_string(), "x".to_string()])).unwrap();

        assert_eq!(read_rollup("x").as_deref(), Some("rollup"), "the kept key's shard is untouched");
        assert!(file_utils::retrieve_inventory_or_none_by_key("a/b").unwrap().1.is_none(), "the removed key republished is cleared");
    }
}
