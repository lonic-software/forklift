//! The shard-write funnels: the only code that writes a shard's content or removes a shard
//! (DESIGN.html §5.0 D item 8).
//!
//! A shard's rollup hash is what lets `stocktake` and `stack` skip reading the subtree beneath it,
//! so every write must keep two rules:
//!
//! * **R1.** A write that changes a shard's effective content (any entry's name, type, hash or
//!   state), or removes a shard, leaves no rollup above it standing: every ancestor's rollup is
//!   durably cleared before the new content (or the removal) lands.
//! * **R2.** A rollup is only ever carried forward from the shard's own bytes over a write that
//!   leaves its effective content alone, or stamped from a tree whose hash names the whole subtree
//!   (a [`RollupStamp`]).
//!
//! The raw writers ([`save_inventory_deferred`], [`ensure_inventory_folder_and_build`],
//! `ShardLedger::shard_path_for_write`, [`remove_shard_folder`]) are private to this module, so the
//! rest of the crate can reach a shard only through a funnel that keeps both rules: a writer outside
//! it cannot forget an ancestor clear, because it has no way to write without one. A producer only
//! says *what* each shard should hold; [`publish_shards`] decides from the bytes on disk whether
//! that is a change, and so which rollups survive.
//!
//! Privacy confines who can stamp a rollup, not whether the stamp is right: a [`RollupStamp`] is
//! minted only by the parser (from bytes already on disk) and `tree_utils::rollup_stamp`, which
//! trusts its caller that the hash names the tree the shard was built from.

use super::*;
use crate::model::inventory::RollupStamp;

/// Serializes every funnel against every other in this process: each holds it from its first read
/// of a shard to its last write or removal, so no two interleave one shard's read and its rewrite
/// (an ancestor clear reading content a concurrent rewrite is about to replace, and writing the
/// stale copy back), and no stamp lands between another funnel's clear and its write.
///
/// Within one command the funnels already run one at a time — `load`'s walk decides concurrently
/// but publishes once, at its single-threaded join point — so the lock is uncontended there, and it
/// is held only across the parse-decide-rewrite of shard files, never across the walk, hashing or
/// object I/O that produces their content. It does not cover a funnel's caller reading a
/// [`ShardLedger`] or a [`PreparedInventory`] before the call: that snapshot is the caller's, and
/// within a command, which holds the warehouse lock, only that command's funnels change what it
/// read.
static SHARD_MUTATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Stage a single shard's rollup clear into `batch`, if it exists and currently has one. A
/// missing shard, or a shard whose rollup is already `None`, is left untouched — not even
/// staged. The batched counterpart of the old per-shard immediate clear (DESIGN.html §5.0 D
/// item 8, stage 2b): a caller invalidating several ancestors stages them all here and calls
/// [`file_utils::WriteBatch::finish`] once, instead of paying a full atomic-write barrier per
/// shard — see [`clear_ancestors_batched`].
///
/// # Returns
/// * `Ok(())`      - The clear was staged (or there was nothing to clear).
/// * `Err(String)` - If the shard could not be read, parsed, or staged.
fn stage_rollup_clear(ledger: &ShardLedger, key: &str, batch: &file_utils::WriteBatch) -> Result<(), String> {
    // Read through the ledger: an unregistered key has no shard to clear, whatever its address
    // holds.
    let Some(bytes) = ledger.retrieve(key)? else {
        return Ok(());
    };

    let shard_path = file_utils::get_inventory_data_path_for_key(key)?;

    let mut inventory = parser::inventory::inventory_parser::parse_inventory(&bytes)
        .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e))?;

    if inventory.get_rollup_hash().is_none() {
        return Ok(());
    }

    inventory.clear_rollup();
    let bytes = ensure_inventory_folder_and_build(&inventory, &shard_path)?;

    batch.stage(&shard_path, &bytes)
}

/// Clear the rollup hash of every existing ancestor shard of `key`, from the root down to (but
/// not including) `key` itself, as one batched durability barrier (DESIGN.html §5.0 D item 8,
/// stage 2b) — every ancestor's clear is staged first, then fsynced, renamed and its directory
/// fsynced together in [`file_utils::WriteBatch::finish`], instead of once per ancestor. For a
/// funnel that removes (rather than rewrites) the shards at and below `key` —
/// [`remove_inventories_under`], [`ShardLedger::replace_subtree`] — which must run it to completion,
/// holding [`SHARD_MUTATION_LOCK`], before anything is removed.
fn clear_ancestors_batched(ledger: &ShardLedger, key: &str) -> Result<(), String> {
    let batch = file_utils::WriteBatch::new();

    for ancestor_key in ancestor_keys_root_first(key) {
        stage_rollup_clear(ledger, &ancestor_key, &batch)?;
    }

    batch.finish()
}

/// Write one shard's new content through [`publish_shards`]: see [`ShardPublish::one`].
///
/// # Arguments
/// * `ledger`    - The registered set, which must already hold `key` (see [`ShardLedger`]).
/// * `key`       - The warehouse path key of the shard being written.
/// * `inventory` - The new content. Any rollup it carries is ignored.
///
/// # Returns
/// * `Ok(())`      - The shard is written, and every rollup above it that its content change
///                   made stale is cleared.
/// * `Err(String)` - See [`publish_shards`].
pub(super) fn write_shard_mutation(ledger: &mut ShardLedger, key: &str, inventory: Inventory) -> Result<(), String> {
    publish_shards(ledger, ShardPublish::one(key, inventory))
}

/// What one [`publish_shards`] call publishes.
pub(super) struct ShardPublish<'a> {
    /// Every shard to publish, by key, with its new entries. Any rollup it carries is ignored:
    /// [`publish_shards`] decides every one.
    pub(super) writes: BTreeMap<String, Inventory>,
    /// Keys whose shards are already gone, to unregister once every ancestor's rollup is cleared.
    pub(super) removed: BTreeSet<String>,
    /// Keys to register (with their ancestors) before any shard lands. Every key of `writes` must
    /// be registered already or named here.
    pub(super) register: BTreeSet<String>,
    /// For a producer that read its baselines earlier and must not overwrite a change made since:
    /// every key of `writes` mapped to the bytes it read (`None` for no shard). `None` for a
    /// producer with no such window.
    pub(super) expected: Option<BTreeMap<String, Option<Vec<u8>>>>,
    /// One other, unrelated file published in the same barrier as the shards' content: see
    /// `inventory_utils::add_file_to_inventory`'s incomplete-load marker.
    pub(super) extra: Option<(&'a Path, &'a [u8])>,
}

impl<'a> ShardPublish<'a> {
    /// Publish `writes`, every key of which is registered already.
    pub(super) fn of(writes: BTreeMap<String, Inventory>) -> Self {
        ShardPublish { writes, removed: BTreeSet::new(), register: BTreeSet::new(), expected: None, extra: None }
    }

    /// Publish the one shard `key`, registered already.
    pub(super) fn one(key: &str, inventory: Inventory) -> Self {
        ShardPublish::of(BTreeMap::from([(key.to_string(), inventory)]))
    }
}

/// The one funnel for shard content: every producer that rewrites shards publishes through here —
/// `load`'s walk (`create_inventory_for_directory`'s join point), `park`'s working-directory refresh
/// (`refresh_tracked_entries`), [`ShardMutationBatch::publish`], and every single-shard write
/// through [`write_shard_mutation`] — and this decides every rollup from what is on disk.
///
/// Under [`SHARD_MUTATION_LOCK`], each shard of `writes` is read as it is on disk and classified:
///
/// * **Carry** — its effective content (every entry's name, type, hash and state) is the same as on
///   disk: only stat data changed, so it keeps the rollup the shard on disk carries.
/// * **Change** — anything else, a shard new on disk included: written with no rollup, and every
///   ancestor's rollup is cleared.
///
/// Every ancestor of a key in `removed` is cleared too. A carried shard that is itself the ancestor
/// of a change or a removal in the same call keeps no rollup either, whichever order the producer
/// decided them in (DESIGN.html §5.0 D item 10, finding #1).
///
/// Then, in three steps, each durable before the next starts:
///
/// 1. **Phase A** — every ancestor owed a clear is cleared on disk, as one batched barrier. That
///    includes an ancestor that is itself a shard of `writes`: phase B renames its shards in no
///    order, so an ancestor whose clear waited for phase B could still stand, with its rollup,
///    over a descendant's new content that landed first.
/// 2. **Registration** — `removed` is unregistered and `register` registered, in one metadata
///    write: a removal lands only once every rollup above it is gone, and a shard only ever lands
///    at a registered key.
/// 3. **Phase B** — every shard of `writes`, and `extra`, as a second barrier.
///
/// The phases are never one shared batch: a single `WriteBatch::finish` gives no ordering between
/// the renames inside it, only that all of them are durable before it returns. Any blob a shard of
/// `writes` names must already be durable before this is called (DESIGN.html §5.0 D item 10,
/// finding #7); no phase stages one.
///
/// A refusal — a key of `writes` neither registered nor in `register`, a damaged address of a
/// registered key, a shard changed underneath `expected`, a shard on disk that does not parse —
/// comes before anything is written. A failure past that point leaves nothing of `writes` known to
/// be published: phase B may have written some shards before it failed, so no caller rolls a shard
/// back as if it were never written.
///
/// # Arguments
/// * `ledger`  - The registered set, updated in place by step 2. Within a command, which holds the
///               warehouse lock, only this command's funnels change it.
/// * `publish` - What to publish: see [`ShardPublish`].
///
/// # Returns
/// * `Ok(())`      - Every shard of `writes` is durably published, every ancestor clear owed is
///                   durable, and the metadata registers exactly what `publish` asked.
/// * `Err(String)` - A refusal above, or a failed read, metadata write, or phase.
pub(super) fn publish_shards(ledger: &mut ShardLedger, publish: ShardPublish) -> Result<(), String> {
    let ShardPublish { mut writes, removed, register, expected, extra } = publish;

    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // Every refusal before anything is cleared, so a key this call would refuse costs no rollup. A
    // key about to be registered is judged once registration has cleared its address.
    for key in writes.keys() {
        if ledger.holds(key) || !register.contains(key) {
            ledger.shard_path_for_write(key)?;
        }
    }

    let mut clear_keys: BTreeSet<String> = removed.iter()
        .flat_map(|key| ancestor_keys_root_first(key))
        .collect();

    // Every shard on disk read and parsed across every core; judged below in key order, so the
    // first refusal in key order wins, as a serial loop would.
    let keys: Vec<String> = writes.keys().cloned().collect();
    let read = fanout_utils::fanout_map(&keys, |key| ledger.retrieve(key).map(|bytes| {
        let parsed = bytes.as_ref().map(|bytes| parser::inventory::inventory_parser::parse_inventory(bytes)
            .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e)));
        (bytes, parsed)
    }));

    for ((key, inventory), read) in writes.iter_mut().zip(read) {
        let (bytes, parsed) = read?;

        if let Some(expected) = &expected {
            if expected.get(key) != Some(&bytes) {
                return Err(format!(
                    "The inventory of folder \"{}\" changed while it was being updated, so the \
                    inventory changes this command still held were not recorded.",
                    load_guard_utils::display_path(key)
                ));
            }
        }

        let on_disk = parsed.transpose()?;

        match on_disk.as_ref().and_then(|on_disk| {
            inventory_content_matches(inventory, on_disk).then(|| on_disk.get_rollup_hash())
        }) {
            Some(Some(rollup)) => inventory.stamp_rollup(RollupStamp::trusted(rollup.clone())),
            Some(None) => inventory.clear_rollup(),
            None => {
                inventory.clear_rollup();
                clear_keys.extend(ancestor_keys_root_first(key));
            }
        }
    }

    // A carried shard above a change or a removal decided anywhere in this call.
    for (key, inventory) in writes.iter_mut() {
        if clear_keys.contains(key) {
            inventory.clear_rollup();
        }
    }

    let phase_a_batch = file_utils::WriteBatch::new();

    for key in &clear_keys {
        stage_rollup_clear(ledger, key, &phase_a_batch)?;
    }

    phase_a_batch.finish()?;

    ledger.update(&removed, &register)?;

    let phase_b_batch = file_utils::WriteBatch::new();

    for (key, inventory) in &writes {
        stage_shard_at(&ledger.shard_path_for_write(key)?, inventory, &phase_b_batch)?;
    }

    if let Some((extra_path, extra_content)) = extra {
        if let Some(parent) = extra_path.parent() {
            if !parent.as_os_str().is_empty() {
                file_utils::create_folder_if_not_exists(parent)?;
            }
        }
        phase_b_batch.stage(extra_path, extra_content)?;
    }

    phase_b_batch.finish()
}

impl ShardLedger {
    /// Replace the staging area below the given directory with the given shards: the existing
    /// inventory folders under the key are removed, the given shards are written, and the
    /// metadata file is updated accordingly. Used by `restore --staged` (and `unload`) to reset a
    /// subtree of the inventory to the pallet head.
    ///
    /// The subtree at `key` is now rebuilt wholesale from `shards` — a known-good source — so any
    /// incomplete-load marker `key` covers no longer describes a real gap; see
    /// `load_guard_utils::clear_recorded_under`'s doc comment. A marker only partially overlapping
    /// `key` is left alone (conservative — part of that region may still be genuinely incomplete).
    ///
    /// # Arguments
    /// * `key`    - The warehouse path key of the subtree to replace (`""` for everything).
    /// * `shards` - Warehouse path key → inventory for the new state of the subtree.
    ///
    /// # Returns
    /// * `Ok(())`      - If the subtree was replaced.
    /// * `Err(String)` - If a folder or file operation failed.
    pub fn replace_subtree(&mut self,
                           key: &str,
                           shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
        // The subtree at `key` is about to be replaced wholesale — a content change from any
        // ancestor's point of view — so their rollups (if any) must be cleared before the new
        // content lands, exactly as `publish_shards` orders it. `shards` itself carries
        // correctly-stamped rollups already (the caller builds them from a known tree), so nothing
        // further is needed for `key` and below.
        let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

        clear_ancestors_batched(self, key)?;

        // `path_utils::covers("", _)` is always true, so this one predicate already subsumes the
        // former separate `key.is_empty()` case (drop everything) without a special-cased branch.
        let replaced: BTreeSet<String> = self.keys()
            .filter(|entry_key| path_utils::covers(key, entry_key))
            .map(str::to_string)
            .collect();

        for replaced_key in &replaced {
            remove_shard_folder(replaced_key)?;
        }

        // Unregister what was removed and register what is about to be published, in one metadata
        // write before any new shard lands. A key removed above and republished here is cleared and
        // synced again, so the removal is durable before the key is registered again.
        self.update(&replaced, &shards.keys().cloned().collect())?;

        // One durability barrier for the whole replacement burst instead of one per shard
        // (DESIGN.html §5.0 D item 8, stage 2b) — every shard here carries a caller-computed,
        // already-correct rollup (or none), so these writes have no ordering requirement against
        // each other, only against the ancestor clear above (already durable by the time this runs).
        let batch = file_utils::WriteBatch::new();

        for (shard_key, inventory) in shards {
            save_inventory_deferred(self, shard_key, inventory, &batch)?;
        }

        batch.finish()?;

        load_guard_utils::clear_recorded_under(key);

        sweep_unregistered_shards();

        Ok(())
    }


    /// The shard path of `key`, for a write: refused when `key` is not registered, or when its
    /// address is damaged, so that nothing is written through a link.
    fn shard_path_for_write(&self, key: &str) -> Result<std::path::PathBuf, String> {
        if !self.holds(key) {
            return Err(format!(
                "The inventory of folder \"{}\" was not written: the folder is not registered in the \
                inventory metadata.",
                load_guard_utils::display_path(key)
            ));
        }

        Ok(file_utils::judge_shard_address(key)?.0)
    }
}

/// Replace the whole staging area with the given shards, so that the store holds exactly them and
/// the metadata registers exactly their directories. Used when `shift` repopulates the inventory
/// from the target pallet's tree, `park`'s push resets to the pallet head, a `consolidate`
/// fast-forward repopulates from the target pallet, `git import` checks out its initial branch, and
/// `lower`/`clone` materialize a fetched tree (`shift_utils::materialize_tree`).
///
/// Only the shards whose bytes differ are written (see [`replace_differing_inventories`]): a
/// switch between two trees that share most directories leaves those directories' shards, and the
/// rollups on them, untouched. A store this cannot trust — metadata missing, damaged or
/// unreadable, a store root that is not a folder, a registered shard that cannot be read or does
/// not parse — is wiped and rewritten whole instead ([`replace_every_inventory`]), so damage never
/// blocks the reset.
///
/// The whole staging area is now rebuilt from `shards` — a known-good source — so every previously
/// recorded incomplete-load root is now moot; see `load_guard_utils::clear_all_recorded`'s doc
/// comment.
///
/// Either way, an error or a crash part-way leaves a staging area that is neither the old one nor
/// the new one: some shards already the new content, others still the old, and (after a wipe)
/// registered keys with no shard, which read as empty directories. Committed, that is a tree nobody
/// staged. So this refuses unless the operation it belongs to raised its marker first
/// (`operation_utils`), which keeps every command that could commit refusing until `forklift heal`
/// has finished or undone the operation.
///
/// # Arguments
/// * `shards` - Warehouse path key → inventory, for every tracked directory.
///
/// # Returns
/// * `Ok(())`      - If the staging area was replaced.
/// * `Err(String)` - If no operation marker stands, or a folder or file operation failed.
pub fn replace_all_inventories(shards: &BTreeMap<String, Inventory>) -> Result<(), String> {
    crate::util::operation_utils::require_raised("Rewriting the whole staged inventory")?;

    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    if !replace_differing_inventories(shards)? {
        replace_every_inventory(shards)?;
    }

    // Only once every shard has landed: the markers must not go while registered keys still have
    // no shard.
    load_guard_utils::clear_all_recorded();

    Ok(())
}

/// One wanted key's shard as [`replace_differing_inventories`] compares it.
struct ShardDiff {
    /// Whether the freshly built shard's bytes differ from the shard on disk (no shard on either
    /// side counts as equal).
    differs: bool,
    /// The rollup stamp of the shard on disk, when it exists and `differs`.
    on_disk_stamp: Option<String>,
    /// The freshly built shard's bytes, when the key has a shard in the new set.
    fresh: Option<Vec<u8>>,
}

/// [`replace_all_inventories`] over a store it can trust: every shard whose bytes would change is
/// written, every other is left alone, and the registered set becomes exactly the new one.
///
/// Every wanted key — each key of `shards` and its ancestors — is read and compared across every
/// core, against the store as it stands before anything is written. A registered key the new set
/// drops is never read: it is unregistered, whatever its shard holds. Then, in three steps, each
/// durable before the next starts:
///
/// 1. **Phase A** — every shard on disk whose stamp is going (the new set carries another stamp,
///    or none, for that key), and which is an ancestor of a key whose shard changes or goes, has
///    its rollup cleared, as one batched barrier. A stamp is a tree hash, so it changes exactly
///    when content under it changes: every stamped ancestor of a content change or a removal is
///    cleared here, before phase B lands that change (R1). A key whose own entries alone change
///    needs no clear, since its new content and new stamp land in one rename; and a shard whose only
///    change is stat data keeps its ancestors' bytes, and so their rollups, untouched: they still
///    name the same tree. A shard this does not rewrite is never cleared.
/// 2. **Registration** — the dropped keys are unregistered and the new ones registered, in one
///    metadata write. A key the new set keeps registered with no shard of its own, whose old shard
///    is on disk, is unregistered and registered again in the same write, which clears and syncs
///    its address.
/// 3. **Phase B** — every shard whose bytes differ, as a second barrier; then every unregistered
///    shard, the dropped keys' included, is swept.
///
/// Phase B lands new stamps in the same barrier as the content under them, so a power loss
/// during it may keep an ancestor's new stamp and lose a descendant's rewrite. That state exists
/// only under the operation marker the caller raised, which every committing command refuses and
/// `forklift heal` settles by rebuilding staging from a tree.
///
/// # Arguments
/// * `shards` - Warehouse path key → inventory, for every tracked directory.
///
/// # Returns
/// * `Ok(true)`    - The staging area holds exactly `shards`.
/// * `Ok(false)`   - The store cannot be trusted, and nothing was written: the caller wipes it.
/// * `Err(String)` - A failed phase or metadata write; the marker the caller holds still stands.
fn replace_differing_inventories(shards: &BTreeMap<String, Inventory>) -> Result<bool, String> {
    let Ok((_, Some(metadata))) = file_utils::retrieve_inventory_metadata_or_none() else {
        return Ok(false);
    };

    // A store root that is anything but a real folder — a link included — is never entered.
    match std::fs::symlink_metadata(file_utils::get_inventory_store_root()?) {
        Ok(store) if store.is_dir() => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        _ => return Ok(false),
    }

    let mut ledger = ShardLedger { entries: metadata };

    let wanted = close_under_ancestors(&shards.keys().map(|key| key_to_metadata_entry(key)).collect());
    let keys: Vec<String> = wanted.iter().map(|entry| metadata_entry_to_key(entry).to_string()).collect();

    // A wanted key not registered yet reads as no shard: its address is cleared when it registers.
    let compared = fanout_utils::fanout_map(&keys, |key| -> Option<ShardDiff> {
        let on_disk = ledger.retrieve(key).ok()?;
        let fresh = shards.get(key).map(InventoryBuilder::build);
        let differs = on_disk != fresh;

        let on_disk_stamp = match on_disk {
            Some(bytes) if differs => parser::inventory::inventory_parser::parse_inventory(&bytes).ok()?
                .get_rollup_hash()
                .cloned(),
            _ => None,
        };

        Some(ShardDiff { differs, on_disk_stamp, fresh })
    });

    let Some(compared) = compared.into_iter().collect::<Option<Vec<ShardDiff>>>() else {
        return Ok(false);
    };

    let mut unregistered: BTreeSet<String> = ledger.entries.difference(&wanted)
        .map(|entry| metadata_entry_to_key(entry).to_string())
        .collect();
    let mut stamps_going: Vec<&str> = Vec::new();
    let mut writes: Vec<(&str, Vec<u8>)> = Vec::new();

    for (key, diff) in keys.iter().zip(compared) {
        if !diff.differs {
            continue;
        }

        if diff.on_disk_stamp.is_some() && diff.on_disk_stamp.as_ref() != shards.get(key).and_then(Inventory::get_rollup_hash) {
            stamps_going.push(key);
        }

        match diff.fresh {
            Some(bytes) => writes.push((key, bytes)),
            None => {
                unregistered.insert(key.clone());
            }
        }
    }

    let changed_ancestors: BTreeSet<String> = writes.iter().map(|(key, _)| *key)
        .chain(unregistered.iter().map(String::as_str))
        .flat_map(ancestor_keys_root_first)
        .collect();

    let phase_a_batch = file_utils::WriteBatch::new();

    for key in stamps_going.into_iter().filter(|key| changed_ancestors.contains(*key)) {
        stage_rollup_clear(&ledger, key, &phase_a_batch)?;
    }

    phase_a_batch.finish()?;

    ledger.update(&unregistered, &shards.keys().cloned().collect())?;

    let phase_b_batch = file_utils::WriteBatch::new();

    for (key, bytes) in &writes {
        let shard_path = ledger.shard_path_for_write(key)?;
        create_shard_folder(&shard_path)?;
        phase_b_batch.stage(&shard_path, bytes)?;
    }

    phase_b_batch.finish()?;

    // The dropped keys' shards: unregistered, so never read again, and cleared before any
    // registration reuses their addresses.
    sweep_unregistered_shards();

    Ok(true)
}

/// [`replace_all_inventories`] over a store it cannot trust: the whole store goes, the new set is
/// registered, and only then does any shard land. The old metadata is replaced, not read, so a
/// damaged one cannot block the reset.
fn replace_every_inventory(shards: &BTreeMap<String, Inventory>) -> Result<(), String> {
    let ledger = ShardLedger::replace(&shards.keys().cloned().collect())?;

    // One durability barrier for the whole warehouse instead of one per shard (DESIGN.html
    // §5.0 D item 8, stage 2b) — the whole staging area was just wiped above, so there is no
    // ancestor ordering to preserve here at all, only the fresh content itself.
    let batch = file_utils::WriteBatch::new();

    for (key, inventory) in shards {
        save_inventory_deferred(&ledger, key, inventory, &batch)?;
    }

    batch.finish()
}

/// Clean up the staged state after a parcel was stacked, so the inventory describes exactly the
/// tree that parcel recorded: the parcel consumed every staged removal, so `Deleted` entries are
/// dropped from their shards, and a key with nothing left (no surviving entry of its own and no
/// surviving registered key beneath it) is unregistered and its shard folder removed.
///
/// Whether a key goes is decided from the inventory alone, never from the working directory. A
/// directory deleted from disk without a `load` is still in the parcel just stacked, so its shard
/// stays, and `stocktake` keeps reporting the removal as not yet loaded. Deciding from the disk
/// instead would drop that shard after the parcel recorded its content, and the next `stack`
/// would then commit a removal nobody staged.
///
/// Reuses the [`PreparedInventory`] snapshot `stack` built before writing anything. Nothing
/// between then and this call changes a shard's content on disk (stacking writes only
/// tree/parcel objects, the pallet ref and the signature sidecar before this point), so reusing it
/// produces exactly what a fresh read would.
///
/// After a stack, staged state *is* the new head, so every surviving shard's rollup ends up in
/// one of three states:
///  * **Stamp** — `key` is in `stamp_hashes`: rewrite it with the just-built subtree hash from
///    the same tree build `stack` just ran.
///  * **Untouched** — `key` is in `prepared.skip.pruned` (a skip root or a key beneath one): its
///    on-disk rollup already names the right tree (that is *why* it was skipped), and the funnel
///    guarantees it holds no staged removal, so it is left exactly as it sits, unread.
///  * **Clear** — everything else (a key in neither set: an out-of-scope/spine shard in a
///    scoped bay, or a genuinely empty subtree): stamped `None` instead of guessed.
///
/// # Arguments
/// * `prepared`     - The snapshot from [`prepare_stack_inventory`].
/// * `stamp_hashes` - Warehouse path key → the subtree hash `stack` just built there. Trusted
///                    for every key it names (see `stack_utils::stack_parcel`, which omits any
///                    key a scoped bay's spine splice could have changed).
///
/// # Returns
/// * `Ok(())`      - If the cleanup completed.
/// * `Err(String)` - If a shard could not be written, or a folder could not be removed.
pub fn cleanup_after_stack_with(prepared: &PreparedInventory,
                                stamp_hashes: &BTreeMap<String, String>) -> Result<(), String> {
    let Some(metadata) = &prepared.metadata else {
        return Ok(());
    };

    // Every key rewritten below is one this snapshot read from the metadata.
    let ledger = ShardLedger { entries: metadata.clone() };

    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    // Every shard rewritten below (a stamp or a `Deleted`-entry drop) is staged here and
    // published as one durability barrier at the end, instead of once per shard (DESIGN.html
    // §5.0 D item 8, stage 2b) — these rewrites carry no ordering requirement against each
    // other: a stamp is only ever consulted after `stack`'s ref move is already durable (see
    // `stack_utils::stack_parcel`), so losing some of them to a crash mid-barrier only costs a
    // few lost skips next time, never a wrong one.
    let batch = file_utils::WriteBatch::new();

    // Deepest first, so every key beneath `key` is decided before `key` itself.
    for entry in metadata.iter().rev() {
        let key = metadata_entry_to_key(entry);

        if prepared.skip.pruned.contains(key) {
            continue;
        }

        let inventory = prepared.shards.get(key);
        let has_survivor = inventory.is_some_and(|inventory| inventory.get_items()
            .any(|(_, item)| item.state != InventoryItemState::Deleted));

        if !key.is_empty() && !has_survivor {
            let prefix = format!("{}/", key);
            let has_registered_descendant = metadata.range(prefix.clone()..)
                .take_while(|other| other.starts_with(&prefix))
                .any(|other| !removed_keys.contains(metadata_entry_to_key(other)));

            if !has_registered_descendant {
                // Every key beneath this one is already removed, so this one can go too.
                remove_shard_folder(key)?;

                removed_keys.insert(key.to_string());
                continue;
            }
        }

        let Some(inventory) = inventory else {
            continue;
        };

        let has_staged_removals = inventory.get_items()
            .any(|(_, item)| item.state == InventoryItemState::Deleted);

        let target_rollup = stamp_hashes.get(key).cloned();

        // A rewrite is needed either to drop the now-consumed `Deleted` entries (dropping them
        // never changes the tree `stack` already built, since the tree build itself excludes
        // `Deleted` entries) or, even with nothing to drop, purely to stamp a rollup this shard
        // did not already carry.
        if has_staged_removals || inventory.get_rollup_hash() != target_rollup.as_ref() {
            let mut rebuilt = Inventory::new();

            for (_, item) in inventory.get_items() {
                if item.state != InventoryItemState::Deleted {
                    rebuilt.add_item((**item).clone());
                }
            }

            if let Some(hash) = target_rollup {
                rebuilt.stamp_rollup(RollupStamp::trusted(hash));
            }

            save_inventory_deferred(&ledger, key, &rebuilt, &batch)?;
        }
    }

    batch.finish()?;

    if removed_keys.is_empty() {
        return Ok(());
    }

    unregister_inventory_keys(&removed_keys)?;

    sweep_unregistered_shards();

    Ok(())
}

/// Remove every inventory shard at or under a warehouse path prefix, dropping those keys from
/// the metadata too. Unlike [`stage_removal_for_directory`], this leaves no `Deleted` record —
/// the entries vanish, as if the subtree had never been inventoried. Used by `narrow` when a
/// subtree leaves the checkout's materialization scope: it should stop being reported entirely,
/// not appear as a staged removal to be committed.
///
/// # Arguments
/// * `prefix` - The warehouse path key of the subtree leaving scope (never the root).
///
/// # Returns
/// * `Ok(())`      - If the shards under the prefix were removed.
/// * `Err(String)` - If a shard folder or the metadata could not be updated.
pub fn remove_inventories_under(prefix: &str) -> Result<(), String> {
    let (_, metadata_opt) = file_utils::retrieve_inventory_metadata_or_none()?;

    let Some(metadata) = metadata_opt else {
        return Ok(());
    };

    let ledger = ShardLedger { entries: metadata };

    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // Conservative: the removed shards themselves have no content left to invalidate (their
    // files are about to be deleted below), but every ancestor above `prefix` may have had a
    // rollup describing a subtree that included them — clear those before anything is removed.
    clear_ancestors_batched(&ledger, prefix)?;

    let mut removed_keys: BTreeSet<String> = BTreeSet::new();

    for key in ledger.keys() {
        // The prefix directory itself, or a directory strictly under it.
        let under = key == prefix
            || (key.len() > prefix.len()
                && key.as_bytes()[prefix.len()] == b'/'
                && key.starts_with(prefix));

        if !under {
            continue;
        }

        remove_shard_folder(key)?;

        removed_keys.insert(key.to_string());
    }

    unregister_inventory_keys(&removed_keys)?;

    sweep_unregistered_shards();

    Ok(())
}

/// Remove the shard folder of `key`, if there is one, or whatever non-folder occupies its slot.
/// Folders are flat, so this removes no other key's shard.
fn remove_shard_folder(key: &str) -> Result<(), String> {
    let folder = file_utils::get_inventory_folder_for_key(key)?;

    remove_store_entry(&folder).map_err(|e| format!(
        "Error while removing the inventory of folder \"{}\": {}", load_guard_utils::display_path(key), e
    ))
}

/// Stage a shard write into `batch` instead of writing (and fsyncing) it immediately, for a funnel
/// writing several shards as one logical operation ([`cleanup_after_stack_with`]'s post-stack
/// stamping, a materializer's wholesale shard replacement, [`publish_shards`]). Batches the fsync and
/// rename of every staged shard into one durability barrier — see [`file_utils::WriteBatch`] —
/// instead of paying a full write-fsync-rename-directory-fsync cycle per shard: on a synthetic
/// 300-directory corpus this was the entire gap between stage 1 (per-shard atomic writes) and
/// pre-stage-1 `stack` timings (DESIGN.html §5.0 D item 8, stage 2b measurement).
///
/// The caller must call [`file_utils::WriteBatch::finish`] once every shard for this operation
/// has been staged — nothing staged here is visible or durable before that returns `Ok`.
/// `key` must already be registered in `ledger` (see [`ShardLedger`]).
fn save_inventory_deferred(ledger: &ShardLedger,
                           key: &str,
                           inventory: &Inventory,
                           batch: &file_utils::WriteBatch) -> Result<(), String> {
    stage_shard_at(&ledger.shard_path_for_write(key)?, inventory, batch)
}

/// Stage `inventory` into `batch` at `inventory_path`, a path `ShardLedger::shard_path_for_write`
/// already judged.
fn stage_shard_at(inventory_path: &Path, inventory: &Inventory, batch: &file_utils::WriteBatch) -> Result<(), String> {
    let bytes = ensure_inventory_folder_and_build(inventory, inventory_path)?;

    batch.stage(inventory_path, &bytes)
}

/// Make sure a shard's parent folder exists, and serialize its bytes.
fn ensure_inventory_folder_and_build(inventory: &Inventory, inventory_path: &Path) -> Result<Vec<u8>, String> {
    create_shard_folder(inventory_path)?;

    Ok(InventoryBuilder::build(inventory))
}

/// Make sure the folder of the shard at `inventory_path` exists. A folder created here is made
/// durable with every folder above it up to the inventory root, so a shard renamed into it by the
/// batch that follows cannot vanish with its folder in a power loss; that batch's own device flush
/// covers these syncs.
fn create_shard_folder(inventory_path: &Path) -> Result<(), String> {
    let Some(shard_folder) = inventory_path.parent() else {
        return Ok(());
    };

    if file_utils::create_folder_if_not_exists(shard_folder)? {
        let inventory_root = std::path::PathBuf::from(file_utils::get_path_inventory_root());
        let created_in: BTreeSet<std::path::PathBuf> = shard_folder.ancestors().skip(1)
            .take_while(|folder| folder.starts_with(&inventory_root))
            .map(Path::to_path_buf)
            .collect();
        file_utils::sync_folders_before_flushed_write(&created_in)?;
    }

    Ok(())
}
