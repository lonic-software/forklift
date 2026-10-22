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

use super::*;
use crate::model::inventory::RollupStamp;

/// Serializes every funnel's read-decide-write of shards against every other in this process, so
/// no two interleave one shard's read and its rewrite (an ancestor clear reading content a
/// concurrent rewrite is about to replace, and writing the stale copy back).
///
/// Within one command the funnels already run one at a time — `load`'s walk decides concurrently
/// but publishes once, at its single-threaded join point — so the lock is uncontended there, and it
/// is held only across the parse-decide-rewrite of shard files, never across the walk, hashing or
/// object I/O that produces their content.
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
/// fsynced together in [`file_utils::WriteBatch::finish`], instead of once per ancestor. Must
/// run to completion before a caller writes new content at `key` — see [`write_shard_mutation`],
/// the funnel that does both, in the correct order, as two separate barriers (not one shared
/// batch — see its doc comment for why the boundary between them matters).
fn clear_ancestors_batched(ledger: &ShardLedger, key: &str) -> Result<(), String> {
    let batch = file_utils::WriteBatch::new();

    for ancestor_key in ancestor_keys_root_first(key) {
        stage_rollup_clear(ledger, &ancestor_key, &batch)?;
    }

    batch.finish()
}

/// Clear the rollup hash of every existing ancestor shard of `key` — see
/// [`clear_ancestors_batched`] for the batching. For a caller that removes (rather than rewrites)
/// the shard at `key` — [`remove_inventories_under`], [`ShardLedger::replace_subtree`] — which
/// still needs its ancestors invalidated but has no new content of its own to write there.
///
/// # Arguments
/// * `ledger` - The registered set; only a registered ancestor's shard is read.
/// * `key`    - The warehouse path key whose ancestors should be invalidated.
///
/// # Returns
/// * `Ok(())`      - Every existing ancestor shard's rollup is now cleared on disk.
/// * `Err(String)` - If an ancestor shard could not be read, parsed or written.
fn clear_ancestor_rollups(ledger: &ShardLedger, key: &str) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    clear_ancestors_batched(ledger, key)
}

/// Write a shard whose effective content (any entry's name/type/hash/state) just changed: the
/// rollup hash of every ancestor shard is cleared first, top-down (root first, batched into one
/// durability barrier — see [`clear_ancestors_batched`]), then this shard's own rollup is
/// cleared and its new content written as a second, separate barrier.
///
/// A direct write would leave a stale-but-still-matching rollup on an ancestor above the change,
/// silently hiding it from a future rollup-based skip (rule R1 in this module's doc comment). This
/// clears whether or not the content really changed; a writer that may only be refreshing stat
/// data (mtime/ctime/inode), and wants the rollups kept when it is, publishes through
/// [`publish_shards`] instead, which decides that from the bytes on disk.
///
/// Ordering matters for crash safety: nothing above the mutated shard is ever left stale once
/// this shard's write is durable, because every ancestor is cleared (and durable — the ancestor
/// batch's `finish` has already returned `Ok`) first; a crash before this shard's write only
/// costs a few lost skips (ancestors cleared for a mutation that, from disk's perspective, never
/// actually happened yet), never a wrong one. The two phases are deliberately *not* merged into
/// one shared batch: a single `WriteBatch::finish` gives no ordering guarantee between the
/// renames inside it, only that all of them are durable before it returns — merging the two
/// phases could let the mutated shard's rename land (and become visible) before an ancestor's,
/// which is exactly the ordering this function exists to prevent.
///
/// # Arguments
/// * `ledger`    - The registered set, which must already hold `key` (see [`ShardLedger`]).
/// * `key`       - The warehouse path key of the shard being written.
/// * `inventory` - The new content. Its rollup hash is overwritten with `None` here — callers
///                 never need to (and must not) set it themselves.
///
/// # Returns
/// * `Ok(())`      - Every ancestor was invalidated and the shard was written.
/// * `Err(String)` - If `key` is not registered, or an ancestor or this shard could not be read,
///                   parsed or written.
pub(super) fn write_shard_mutation(ledger: &ShardLedger, key: &str, inventory: &mut Inventory) -> Result<(), String> {
    write_shard_mutation_with_extra(ledger, key, inventory, None)
}

/// Like [`write_shard_mutation`], but when `extra` is `Some((path, content))`, additionally
/// publishes that one other, unrelated file as part of *this shard's own* content-write barrier
/// (the second of the two phases `write_shard_mutation` documents — never the ancestor-clear
/// phase, which must stay a separate, earlier barrier for the ordering reasons its own doc
/// comment gives, and which `extra` has no ordering relationship with at all).
///
/// This exists so a caller with exactly one other small file it needs durable no later than this
/// shard's own write can fold it in instead of paying a second, standalone barrier —
/// `inventory_utils::add_file_to_inventory` folding in `load_guard_utils`'s incomplete-load
/// marker is the motivating (and, so far, only) case: a single-file `load` has no walk-wide blob
/// batch to ride (unlike a directory `load` — see `create_inventory_for_directory`'s doc
/// comment), so this shard's own write is the first durable mutation that can stage anything
/// (a registration before it adds only keys with no shard, which stage nothing). Folding
/// `extra` in here — rather than into the ancestor-clear phase — is what keeps this zero-cost:
/// that phase is a no-op (no barrier at all) whenever `key` has no ancestor with a rollup to
/// clear, which is the common case before a first `stack`; forcing a barrier there just to carry
/// `extra` would reintroduce exactly the extra barrier this exists to avoid.
///
/// # Arguments
/// * `ledger`    - The registered set, which must already hold `key` (see [`ShardLedger`]).
/// * `key`       - The warehouse path key of the shard being written.
/// * `inventory` - The new content (see [`write_shard_mutation`]).
/// * `extra`     - An optional extra `(path, content)` to publish in the same barrier as this
///                 shard's own content. `None` behaves exactly like [`write_shard_mutation`].
///
/// # Returns
/// * `Ok(())`      - Every ancestor was invalidated, and the shard (and `extra`, if given) was
///                   written.
/// * `Err(String)` - If `key` is not registered, or an ancestor, the shard, or `extra` could not
///                   be read, parsed or written.
pub(super) fn write_shard_mutation_with_extra(ledger: &ShardLedger,
                                              key: &str,
                                              inventory: &mut Inventory,
                                              extra: Option<(&Path, &[u8])>) -> Result<(), String> {
    let shard_path = ledger.shard_path_for_write(key)?;

    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    clear_ancestors_batched(ledger, key)?;

    inventory.clear_rollup();

    let bytes = ensure_inventory_folder_and_build(inventory, &shard_path)?;

    let Some((extra_path, extra_content)) = extra else {
        return file_utils::write_file_atomically(&shard_path, &bytes);
    };

    let batch = file_utils::WriteBatch::new();
    batch.stage(&shard_path, &bytes)?;

    if let Some(parent) = extra_path.parent() {
        if !parent.as_os_str().is_empty() {
            file_utils::create_folder_if_not_exists(parent)?;
        }
    }
    batch.stage(extra_path, extra_content)?;

    batch.finish()
}

/// Publish many shards' new content at once, deciding every rollup from what is on disk: for the
/// producers that rebuild shards in bulk — `load`'s walk (`create_inventory_for_directory`'s join
/// point), `park`'s working-directory refresh (`refresh_tracked_entries`) and
/// [`ShardMutationBatch::publish`].
///
/// Each shard of `writes` is read as it is on disk, under [`SHARD_MUTATION_LOCK`], and classified:
///
/// * **Carry** — its effective content (every entry's name, type, hash and state) is the same as on
///   disk: only stat data changed, so it keeps the rollup the shard on disk carries.
/// * **Change** — anything else, a shard new on disk included: written with no rollup, and every
///   ancestor's rollup is cleared.
///
/// Every ancestor of a key in `removed` (shards already gone, such as a directory `load` found
/// removed) is cleared too. A carried shard that is itself the ancestor of a change or a removal in
/// the same call keeps no rollup either, whichever order the producer decided them in (DESIGN.html
/// §5.0 D item 10, finding #1).
///
/// Then every other ancestor to clear is cleared as one batched barrier (phase A), and only once
/// that is durable is every shard of `writes` published as a second barrier (phase B). The two are
/// never one shared batch — see [`write_shard_mutation`]'s doc comment for why the boundary between
/// "an ancestor's clear is durable" and "the mutated shard's content is durable" matters. Any blob a
/// shard of `writes` names must already be durable before this is called (DESIGN.html §5.0 D item
/// 10, finding #7); neither phase stages one.
///
/// A failure leaves nothing of `writes` known to be published: phase A failing skips phase B, and
/// phase B may have written some shards before it failed. No caller registers a key on the
/// strength of this call, and none rolls a shard back as if it were never written.
///
/// # Arguments
/// * `ledger`   - The registered set, holding every key of `writes` (phase B refuses one it does
///                not hold).
/// * `writes`   - Every shard to publish, by key, with its new entries. Any rollup it carries is
///                ignored: only the classification above decides one.
/// * `removed`  - Keys whose shards are gone, whose ancestors' rollups must not stand.
/// * `expected` - For a producer that read its baselines earlier and must not overwrite a change
///                made since: every key of `writes` mapped to the bytes it read (`None` for no
///                shard). Any key whose shard no longer holds exactly those bytes refuses the whole
///                call before anything is written. `None` for a producer with no such window.
///
/// # Returns
/// * `Ok(())`      - Every shard of `writes` is durably published and every ancestor clear owed is
///                   durable too.
/// * `Err(String)` - A key of `writes` is not registered or its address is damaged (refused before
///                   anything is written), a shard changed underneath `expected`, a shard on disk
///                   could not be read or parsed (never treated as a new one), or phase A or B
///                   failed.
pub(super) fn publish_shards(ledger: &ShardLedger,
                             mut writes: BTreeMap<String, Inventory>,
                             removed: &BTreeSet<String>,
                             expected: Option<&BTreeMap<String, Option<Vec<u8>>>>) -> Result<(), String> {
    let _guard = SHARD_MUTATION_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());

    // Every address judged before anything is cleared, so a key this call would refuse costs no
    // rollup.
    let paths: Vec<std::path::PathBuf> = writes.keys()
        .map(|key| ledger.shard_path_for_write(key))
        .collect::<Result<_, _>>()?;

    let mut clear_keys: BTreeSet<String> = removed.iter()
        .flat_map(|key| ancestor_keys_root_first(key))
        .collect();

    for (key, inventory) in writes.iter_mut() {
        let bytes = ledger.retrieve(key)?;

        if let Some(expected) = expected {
            if expected.get(key) != Some(&bytes) {
                return Err(format!(
                    "The inventory of folder \"{}\" changed while it was being updated, so the \
                    inventory changes this command still held were not recorded.",
                    load_guard_utils::display_path(key)
                ));
            }
        }

        let on_disk = bytes
            .map(|bytes| parser::inventory::inventory_parser::parse_inventory(&bytes)
                .map_err(|e| format!("Error while parsing the inventory of folder \"{}\": {}", key, e)))
            .transpose()?;

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

    for key in clear_keys.iter().filter(|key| !writes.contains_key(key.as_str())) {
        stage_rollup_clear(ledger, key, &phase_a_batch)?;
    }

    phase_a_batch.finish()?;

    let phase_b_batch = file_utils::WriteBatch::new();

    for (inventory, path) in writes.values().zip(&paths) {
        stage_shard_at(path, inventory, &phase_b_batch)?;
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
        // content lands, exactly as `write_shard_mutation` orders it. `shards` itself carries
        // correctly-stamped rollups already (the caller builds them from a known tree), so nothing
        // further is needed for `key` and below.
        clear_ancestor_rollups(self, key)?;

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

/// Replace the whole staging area with the given shards: the existing inventory folders
/// are removed, the given shards are written, and the metadata file is rewritten to list
/// exactly their directories. Used when `shift` repopulates the inventory from the target
/// pallet's tree, `park`'s push resets to the pallet head, a `consolidate` fast-forward
/// repopulates from the target pallet, `import-git` checks out its initial branch, and
/// `lower`/`franchise` materialize a fetched tree (`shift_utils::materialize_tree`).
///
/// The whole staging area is now rebuilt wholesale from `shards` — a known-good source — so
/// every previously recorded incomplete-load root is now moot; see
/// `load_guard_utils::clear_all_recorded`'s doc comment.
///
/// # Arguments
/// * `shards` - Warehouse path key → inventory, for every tracked directory.
///
/// # Returns
/// * `Ok(())`      - If the staging area was replaced.
/// * `Err(String)` - If a folder or file operation failed.
pub fn replace_all_inventories(shards: &std::collections::BTreeMap<String, Inventory>) -> Result<(), String> {
    // The whole store goes, not the root key's shard, and the new set is registered before any
    // shard lands. The old metadata is replaced, not read, so a damaged one cannot block the reset.
    let ledger = ShardLedger::replace(&shards.keys().cloned().collect())?;

    // One durability barrier for the whole warehouse instead of one per shard (DESIGN.html
    // §5.0 D item 8, stage 2b) — the whole staging area was just wiped above, so there is no
    // ancestor ordering to preserve here at all, only the fresh content itself.
    let batch = file_utils::WriteBatch::new();

    for (key, inventory) in shards {
        save_inventory_deferred(&ledger, key, inventory, &batch)?;
    }

    batch.finish()?;

    // Only once every shard has landed: the markers must not go while registered keys still have
    // no shard.
    load_guard_utils::clear_all_recorded();

    Ok(())
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

    // Conservative: the removed shards themselves have no content left to invalidate (their
    // files are about to be deleted below), but every ancestor above `prefix` may have had a
    // rollup describing a subtree that included them — clear those before anything is removed.
    clear_ancestor_rollups(&ledger, prefix)?;

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
    let mut parent_path = std::path::PathBuf::from(inventory_path);
    parent_path.pop();

    file_utils::create_folder_if_not_exists(&parent_path)?;

    Ok(InventoryBuilder::build(inventory))
}
