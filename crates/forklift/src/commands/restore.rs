use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::enums::inventory_item_state::InventoryItemState;
use forklift_core::model::inventory::InventoryItem;
use forklift_core::util::path_utils::{self, WarehousePath};
use forklift_core::util::inventory_utils::TrackedKind;
use forklift_core::util::scope_utils;
use forklift_core::util::{
    alias_utils, cache_miss, file_utils, inventory_utils, load_guard_utils, object_utils, shift_utils,
};
use crate::output;

/// Handle the restore command: rewrite the file (or every tracked file of the directory that
/// differs from its entry) in the working directory from the inventory, discarding unstaged
/// changes. Unstaging is `unload`'s.
///
/// Discarding a change is destructive (Rule 5): when the restore would overwrite a file that
/// differs from its entry, it lists those files and refuses (`confirmation_required`) before
/// writing anything, unless `confirm` is given. Writing back a missing file loses nothing, so it
/// needs no confirmation.
///
/// # Arguments
/// * `target`  - The path of the file or directory to restore.
/// * `confirm` - Whether changes the restore would discard may be discarded.
///
/// # Returns
/// * `Ok(())`      - If the restore completed successfully.
/// * `Err(String)` - If there was an error while handling the command.
pub fn handle_command(target: &str, confirm: bool) -> Result<(), String> {
    let path = WarehousePath::from_user_input(target)?;

    // An out-of-scope path is sealed by hash in a scoped bay and was never materialized;
    // restoring it would have nothing to restore from. Refuse cleanly rather than let the
    // walk below silently do the wrong thing.
    crate::commands::scope::ensure_path_in_scope(path.as_key())?;

    restore_worktree(&path, confirm)
}

/// Restore the working directory from the inventory: rewrite the file (or, for a
/// directory, every tracked, non-`Deleted` file below it) from its staged blob, leaving a file
/// that already holds that content untouched (see `restore_entry`).
///
/// # Arguments
/// * `path`    - The path to restore.
/// * `confirm` - Whether changes the restore would discard may be discarded.
///
/// # Returns
/// * `Ok(())`      - If the restore completed.
/// * `Err(String)` - If the path is not in the inventory, the restore would discard a change
///   without `confirm`, or a write failed.
fn restore_worktree(path: &WarehousePath, confirm: bool) -> Result<(), String> {
    // Read once: nothing below writes the metadata before the batch publishes.
    let ledger = inventory_utils::ShardLedger::read()?;

    if path.is_root() {
        return restore_worktree_directory(&ledger, path.as_key(), confirm);
    }

    match inventory_utils::tracked_kind(&ledger, path)? {
        TrackedKind::Directory => return restore_worktree_directory(&ledger, path.as_key(), confirm),
        TrackedKind::UntrackedDirectory => return Err(format!("No inventory found for folder \"{}\".", path.as_key())),
        TrackedKind::File => {}
    }

    // A tracked file replaced by a folder: writing the file back would have to delete the
    // folder and whatever untracked content it holds, so refuse instead.
    if std::fs::symlink_metadata(path.to_fs_path()).is_ok_and(|metadata| metadata.is_dir()) {
        return Err(format!(
            "Cannot restore \"{}\": the tracked file was replaced by a folder. Move or delete the folder first.",
            path.as_key()
        ));
    }

    let (parent, file_name) = path.split_parent()?;

    let Some(inventory) = ledger.inventory(parent.as_key())? else {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    };

    let Some(item) = inventory.get_item_by_name(&file_name) else {
        return Err(format!("\"{}\" is not in the inventory.", path.as_key()));
    };

    if item.state == InventoryItemState::Deleted {
        return Err(format!(
            "The removal of \"{}\" is staged; use \"forklift unload {}\" to unstage it first.",
            path.as_key(),
            path.as_key()
        ));
    }

    // Judged before what is on disk is compared: another spelling's file is not this one's change.
    ensure_restorable(path.as_key())?;
    let anchor = file_utils::take_stat_anchor()?;

    if !confirm && would_discard(path.as_key(), &item, anchor)? {
        return Err(discard_refusal(&[path.as_key().to_string()], path.as_key()));
    }

    // Recording only stat data keeps the rollups (see `ShardMutationBatch`).
    let mut batch = inventory_utils::ShardMutationBatch::over(&ledger);

    let message = match restore_entry(parent.as_key(), &item, true, confirm)? {
        Restored::Untouched => format!("\"{}\" already matches the inventory.", path.as_key()),
        Restored::Restatted(refreshed) => {
            batch.update(parent.as_key(), |inventory| inventory.add_item(refreshed))?;
            format!("\"{}\" already matches the inventory.", path.as_key())
        }
        Restored::Rewritten(refreshed) => {
            batch.update(parent.as_key(), |inventory| inventory.add_item(refreshed))?;
            format!("Restored \"{}\" from the inventory.", path.as_key())
        }
    };

    batch.publish()?;
    output::message(message);

    Ok(())
}

/// The presence phase for restoring the directory at `key` (see `cache_miss::ensure_files_present`):
/// in a cache warehouse, the content of every tracked file under it, fetched in one batch before
/// any file is written. That is every file, not only those a restore rewrites, which are known
/// only by reading each one: offline, a restore refuses even where every file already matches,
/// rather than rewrite some of them and then refuse.
fn ensure_directory_present(ledger: &inventory_utils::ShardLedger, key: &str) -> Result<(), String> {
    if !scope_utils::is_cache_mode().unwrap_or(false) {
        return Ok(());
    }

    let mut files: Vec<(String, DirEntryType)> = Vec::new();

    for shard_key in ledger.keys() {
        if !path_utils::covers(key, shard_key) {
            continue;
        }

        let Some(inventory) = ledger.inventory(shard_key)? else { continue };

        files.extend(inventory.get_items()
            .filter(|(_, item)| item.state != InventoryItemState::Deleted && item.item_type != DirEntryType::Tree)
            .map(|(_, item)| (item.hash.clone(), item.item_type)));
    }

    cache_miss::ensure_files_present(files.iter().map(|(hash, item_type)| (hash.as_str(), *item_type)))
}

/// Restore every tracked file of a directory (and its subdirectories) from the inventory.
///
/// # Arguments
/// * `ledger` - The registered set.
/// * `key`     - The warehouse path key of the directory.
/// * `confirm` - Whether changes the restore would discard may be discarded.
///
/// # Returns
/// * `Ok(())`      - If the restore completed.
/// * `Err(String)` - If the directory has no inventory, a file was refused, the restore would
///   discard a change without `confirm`, or a write failed.
fn restore_worktree_directory(ledger: &inventory_utils::ShardLedger, key: &str, confirm: bool) -> Result<(), String> {
    // Every file is judged before any is written, so a refusal (a change to discard without
    // --confirm, an alias, a folder where a file was) leaves the whole directory as it was.
    let discards = plan_directory(ledger, key)?;

    if !confirm && !discards.is_empty() {
        return Err(discard_refusal(&discards, &load_guard_utils::display_path(key)));
    }

    let mut restored_any = false;
    let mut restored_count = 0usize;
    let mut matched_count = 0usize;
    let mut batch = inventory_utils::ShardMutationBatch::over(ledger);
    let mut result: Result<(), String> = Ok(());

    ensure_directory_present(ledger, key)?;

    for shard_key in ledger.keys() {
        if !path_utils::covers(key, shard_key) {
            continue;
        }

        restored_any = true;

        let inventory = match ledger.inventory(shard_key) {
            Ok(Some(shard)) => shard,
            Ok(None) => continue,
            Err(e) => { result = Err(e); break; }
        };

        for (_, item) in inventory.get_items() {
            if item.state == InventoryItemState::Deleted {
                continue;
            }

            let recorded = restore_entry(shard_key, item, false, confirm).and_then(|restored| match restored {
                Restored::Untouched => {
                    matched_count += 1;
                    Ok(())
                }
                Restored::Restatted(refreshed) => {
                    matched_count += 1;
                    batch.update(shard_key, |shard| shard.add_item(refreshed))
                }
                Restored::Rewritten(refreshed) => {
                    restored_count += 1;
                    batch.update(shard_key, |shard| shard.add_item(refreshed))
                }
            });

            if let Err(e) = recorded {
                result = Err(e);
                break;
            }
        }

        if result.is_err() {
            break;
        }
    }

    // What was decided before a failure is still published, unless the failure was a publish
    // itself — the failure is reported after, together with the publish's own failure if that
    // failed too. A failed publish may have recorded some, all or none of the batch's entries,
    // though every file restored before it is already on disk.
    match (result, batch.publish()) {
        (Err(first), Err(publish)) => return Err(format!("{}\n{}", first, publish)),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => return Err(e),
        (Ok(()), Ok(())) => {}
    }

    if !restored_any {
        return Err(format!(
            "No inventory found for folder \"{}\".",
            load_guard_utils::display_path(key)
        ));
    }

    if matched_count == 0 {
        output::message(format!("Restored {} file(s) from the inventory.", restored_count));
    } else {
        output::message(format!(
            "Restored {} file(s) from the inventory; {} already matched it.",
            restored_count, matched_count
        ));
    }

    Ok(())
}

/// The files under the directory at `key` whose changes restoring it would discard, refusing a
/// file the restore could not write: one aliased by another spelling, one under a non-directory,
/// or one replaced by a folder. Nothing is written, not even fresh stat data.
///
/// # Arguments
/// * `ledger` - The registered set.
/// * `key`    - The warehouse path key of the directory.
///
/// # Returns
/// * `Ok(Vec<String>)` - The warehouse paths whose changes would be discarded.
/// * `Err(String)`     - If a file was refused or could not be read.
fn plan_directory(ledger: &inventory_utils::ShardLedger, key: &str) -> Result<Vec<String>, String> {
    let anchor = file_utils::take_stat_anchor()?;
    let mut discards: Vec<String> = Vec::new();

    for shard_key in ledger.keys() {
        if !path_utils::covers(key, shard_key) {
            continue;
        }

        let Some(inventory) = ledger.inventory(shard_key)? else { continue };

        for (_, item) in inventory.get_items() {
            if item.state == InventoryItemState::Deleted || item.item_type == DirEntryType::Tree {
                continue;
            }

            let file_path = tracked_path(shard_key, &item.name);
            ensure_restorable(&file_path)?;

            if would_discard(&file_path, item, anchor)? {
                discards.push(file_path);
            }
        }
    }

    Ok(discards)
}

/// Whether restoring `item` would discard what is at `file_path`: a file (or link) is there and
/// does not hold the entry's content as a file of its kind. A conflict entry's file is compared
/// too, though the restore rewrites it either way. A missing file loses nothing.
///
/// # Arguments
/// * `file_path` - The warehouse path of the tracked file.
/// * `item`      - Its inventory entry.
/// * `anchor`    - The stat anchor the comparison proves against.
///
/// # Returns
/// * `Ok(bool)`    - Whether a change would be discarded.
/// * `Err(String)` - If a folder replaced the file: restoring it would delete the folder.
fn would_discard(file_path: &str, item: &InventoryItem, anchor: file_utils::StatAnchor) -> Result<bool, String> {
    let Ok(metadata) = std::fs::symlink_metadata(file_path) else {
        return Ok(false);
    };

    if metadata.is_dir() {
        return Err(format!(
            "Cannot restore \"{}\": the tracked file was replaced by a folder. Move or delete the folder first.",
            file_path
        ));
    }

    Ok(matching_content(file_path, item, anchor).is_none())
}

/// The `confirmation_required` refusal for a restore of `target` that would discard the changes
/// to `files`, which its details list.
fn discard_refusal(files: &[String], target: &str) -> String {
    let shown: Vec<String> = files.iter().take(20).map(|file| format!("  {}", file)).collect();
    let more = match files.len() > shown.len() {
        true => format!("\n  ... and {} more", files.len() - shown.len()),
        false => String::new(),
    };
    let separator = if target.starts_with('-') { "-- " } else { "" };

    crate::human!("Restoring \"{}\" would discard the changes to:\n{}{}", target, shown.join("\n"), more);

    CoreError::refusal_with_details(
        RefusalCode::ConfirmationRequired,
        format!("Restoring \"{}\" would discard the changes to {} file(s); nothing was changed.", target, files.len()),
        format!(
            "Keep any change you want first (load it, or copy it aside), then run \"forklift restore --confirm {}{}\" to discard the rest.",
            separator, target
        ),
        serde_json::json!({ "path": target, "discards": files }),
    ).into()
}

/// What restoring one tracked entry did.
pub(super) enum Restored {
    /// The file already held the staged content and the stat cache proved it: nothing was
    /// written, and the entry needs no change.
    Untouched,

    /// The file already held the staged content, but only its hash proved it: nothing was
    /// written, and the entry is to be recorded with the file's fresh stat data, so the next
    /// comparison trusts the stat cache again.
    Restatted(InventoryItem),

    /// The file was written from its blob; the entry is to be recorded with its new stat data.
    Rewritten(InventoryItem),
}

/// Restore one tracked, non-`Deleted` entry: write its file from the staged blob unless the
/// file already holds that content, so a file that matches keeps its mtime and inode (build
/// tools that watch mtimes see nothing changed). The caller records the returned entry.
///
/// Only a `Normal` entry is ever left unwritten: restoring a conflict entry writes it and
/// records it as `Normal`, which is what resolves the conflict. A file of another on-disk kind
/// (a symlink for a file, an executable for a plain file) is written even when its bytes hash
/// the same.
///
/// # Arguments
/// * `parent_key` - The warehouse path key of the entry's directory.
/// * `item`       - The inventory entry.
/// * `fetch`      - Whether to fetch its content, in a cache warehouse, before a rewrite: false
///                  where the caller's presence phase fetched it already.
/// * `discard`    - Whether a file that differs from the entry may be overwritten. The caller
///                  judged it would not be; without this, one changed since is refused, not lost.
///
/// # Returns
/// * `Ok(Restored)` - What was done.
/// * `Err(String)`  - If the path was refused, a file changed since the caller judged it, the
///                    clock probe could not be stamped, or a write failed.
fn restore_entry(parent_key: &str, item: &InventoryItem, fetch: bool, discard: bool) -> Result<Restored, String> {
    let file_path = tracked_path(parent_key, &item.name);

    ensure_restorable(&file_path)?;

    let anchor = file_utils::take_stat_anchor()?;
    let present = std::fs::symlink_metadata(&file_path).is_ok();
    let matched = if present { matching_content(&file_path, item, anchor) } else { None };

    if item.state == InventoryItemState::Normal {
        if let Some(restored) = matched {
            return Ok(restored);
        }
    }

    // A conflict entry is rewritten even when its file matches, which loses nothing; a file that
    // differs is lost unless the caller allowed it.
    if present && matched.is_none() && !discard {
        return Err(changed_since(&file_path));
    }

    // A directory's restore fetched every file's content already (`ensure_directory_present`); a
    // single file's is fetched only once it is known to need rewriting.
    if fetch {
        cache_miss::ensure_files_present([(item.hash.as_str(), item.item_type)])?;
    }
    shift_utils::write_tracked_file(&file_path, &item.hash, item.item_type, false)?;

    // Anchored and statted right after the write: a file statted later would record an editor's
    // save in between against the staged hash.
    let anchor = file_utils::take_stat_anchor()?;
    let refreshed = inventory_utils::build_inventory_item_from_stat(
        std::path::Path::new(&file_path),
        &item.name,
        item.hash.clone(),
        item.item_type,
        anchor,
    )?;

    Ok(Restored::Rewritten(refreshed))
}

/// The refusal for a file that changed after the restore judged it would discard nothing.
fn changed_since(file_path: &str) -> String {
    format!(
        "\"{}\" changed while it was being restored, so it was left as it is; files before it may \
        already be restored. Run the restore again to see what it would discard.",
        file_path
    )
}

/// `Untouched` or `Restatted` when the file at `file_path` already holds `item`'s content as a
/// file of the same kind, `None` when it does not or cannot be compared (it is then written).
/// A `Restatted` entry carries stat data proven against `anchor`, taken before this stat, so an
/// edit made after the anchor leaves it unproven and the next comparison rehashes the file.
pub(super) fn matching_content(file_path: &str, item: &InventoryItem, anchor: file_utils::StatAnchor) -> Option<Restored> {
    let path = std::path::Path::new(file_path);
    let metadata = file_utils::get_symlink_metadata_for_path(path).ok()?;

    if metadata.is_dir() {
        return None;
    }

    let item_type = inventory_utils::effective_kind(item, &metadata, file_utils::get_type_of_dir_entry(&metadata).ok()?).ok()?;

    if item_type.on_disk_kind() != item.item_type.on_disk_kind() {
        return None;
    }

    // A size that differs settles it without reading the file. Not for a symlink, whose stored
    // size is not comparable, nor for an entry with zeroed stat data, which records no size.
    if item_type != DirEntryType::SymbolicLink && item.inode != 0 && metadata.len() != item.file_size {
        return None;
    }

    let verdict = inventory_utils::classify_file_against_entry(
        item, &metadata, item_type, path, &item.name, object_utils::IngestMode::ComputeOnly, anchor
    ).ok()?;

    match verdict {
        inventory_utils::FileVerdict::UnchangedByStat => Some(Restored::Untouched),
        // Only the stat data may change here, never the entry's type.
        inventory_utils::FileVerdict::UnchangedByHash(refreshed, _) if refreshed.item_type == item.item_type => {
            Some(Restored::Restatted(refreshed))
        }
        _ => None,
    }
}

/// The warehouse path of the file `name` in the directory `parent_key`.
pub(super) fn tracked_path(parent_key: &str, name: &str) -> String {
    if parent_key.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", parent_key, name)
    }
}

/// Refuse a path whose write would overwrite another file through an alias, or that needs a
/// directory where a file is.
///
/// # Arguments
/// * `file_path` - The warehouse path of the tracked file.
///
/// # Returns
/// * `Ok(())`      - If the path may be written.
/// * `Err(String)` - If the path was refused.
fn ensure_restorable(file_path: &str) -> Result<(), String> {
    // The inventory can hold two spellings of one file on a case-insensitive volume (a
    // case-only directory rename staged under the new name while the old one stays): writing
    // the one that is not on disk would overwrite the other through the alias. A file where the
    // path needs a directory is refused too, rather than failing inside the write.
    match alias_utils::AliasCheck::new().occupancy(file_path)? {
        alias_utils::Occupancy::Alias { occupant, .. } | alias_utils::Occupancy::Unidentified { component: occupant } => {
            Err(format!(
                "Cannot restore \"{}\": on this file system it names the same file as \"{}\", which \
                restoring would overwrite. Stage the one you mean to keep (load or unload the other) \
                first.",
                file_path, occupant
            ))
        }
        alias_utils::Occupancy::NonDirectory { occupant } => {
            Err(format!(
                "Cannot restore \"{}\": \"{}\" is not a directory. Move it aside first.",
                file_path, occupant
            ))
        }
        alias_utils::Occupancy::Vacant | alias_utils::Occupancy::Listed => Ok(()),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{self, Warehouse};

    /// How many times `run` reads the ledger on this thread.
    fn ledger_reads(run: impl FnOnce()) -> u64 {
        let before = inventory_utils::ledger_read_count();
        run();
        inventory_utils::ledger_read_count() - before
    }

    #[test]
    fn a_restore_reads_the_ledger_once_whatever_the_path_names() {
        let warehouse = Warehouse::with_working_tree("restore-ledger-reads");
        forklift_core::util::pallet_utils::write_position(&forklift_core::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        std::fs::create_dir_all(warehouse.root.join("dir/sub")).unwrap();
        std::fs::write(warehouse.root.join("dir/a.txt"), b"a\n").unwrap();
        std::fs::write(warehouse.root.join("dir/sub/b.txt"), b"b\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();

        // One read by the command, and one by its batch's publish, which re-reads under the lock.
        for target in ["dir/a.txt", "dir", "."] {
            std::fs::write(warehouse.root.join("dir/a.txt"), b"changed\n").unwrap();
            assert_eq!(ledger_reads(|| handle_command(target, true).unwrap()), 2, "restore {}", target);
        }

    }

    /// A warehouse holding `dir/a.txt` and `dir/b.txt` loaded, named `name`.
    fn loaded_pair(name: &str) -> Warehouse {
        let warehouse = Warehouse::with_working_tree(name);
        forklift_core::util::pallet_utils::write_position(&forklift_core::util::pallet_utils::BayPosition::new("main", None)).unwrap();
        std::fs::create_dir_all(warehouse.root.join("dir")).unwrap();
        std::fs::write(warehouse.root.join("dir/a.txt"), b"a\n").unwrap();
        std::fs::write(warehouse.root.join("dir/b.txt"), b"b\n").unwrap();
        test_support::block_on(crate::commands::load::handle_command(".")).unwrap();
        warehouse
    }

    #[test]
    fn a_restore_that_would_discard_a_change_lists_it_and_writes_nothing_without_confirm() {
        let warehouse = loaded_pair("restore-needs-confirm");
        std::fs::write(warehouse.root.join("dir/a.txt"), b"edited\n").unwrap();
        std::fs::remove_file(warehouse.root.join("dir/b.txt")).unwrap();

        // The missing file would be written back first, were anything written: nothing is.
        for target in ["dir", "dir/a.txt", "."] {
            let error = handle_command(target, false).unwrap_err();
            assert!(error.contains("dir/a.txt") && !error.contains("dir/b.txt"), "{}", error);
            assert_eq!(std::fs::read(warehouse.root.join("dir/a.txt")).unwrap(), b"edited\n", "restore {}", target);
            assert!(!warehouse.root.join("dir/b.txt").exists(), "restore {}", target);
        }

        handle_command("dir", true).unwrap();
        assert_eq!(std::fs::read(warehouse.root.join("dir/a.txt")).unwrap(), b"a\n");
        assert_eq!(std::fs::read(warehouse.root.join("dir/b.txt")).unwrap(), b"b\n");
    }

    #[test]
    fn a_restore_that_discards_nothing_needs_no_confirm() {
        let warehouse = loaded_pair("restore-no-confirm");
        std::fs::remove_file(warehouse.root.join("dir/a.txt")).unwrap();

        handle_command("dir/a.txt", false).unwrap();
        assert_eq!(std::fs::read(warehouse.root.join("dir/a.txt")).unwrap(), b"a\n");

        std::fs::remove_file(warehouse.root.join("dir/b.txt")).unwrap();
        handle_command("dir", false).unwrap();
        assert_eq!(std::fs::read(warehouse.root.join("dir/b.txt")).unwrap(), b"b\n");
    }

    #[test]
    fn a_folder_where_a_file_was_refuses_the_directory_before_anything_is_written() {
        let warehouse = loaded_pair("restore-folder-first");
        std::fs::write(warehouse.root.join("dir/a.txt"), b"edited\n").unwrap();
        std::fs::remove_file(warehouse.root.join("dir/b.txt")).unwrap();
        std::fs::create_dir(warehouse.root.join("dir/b.txt")).unwrap();

        let error = handle_command("dir", true).unwrap_err();
        assert!(error.contains("replaced by a folder"), "{}", error);
        assert_eq!(std::fs::read(warehouse.root.join("dir/a.txt")).unwrap(), b"edited\n");
    }

    #[test]
    fn a_file_changed_after_it_was_judged_is_refused_not_overwritten() {
        let warehouse = loaded_pair("restore-changed-since");
        std::fs::write(warehouse.root.join("dir/a.txt"), b"edited\n").unwrap();

        let ledger = inventory_utils::ShardLedger::read().unwrap();
        let inventory = ledger.inventory("dir").unwrap().unwrap();
        let item = inventory.get_item_by_name("a.txt").unwrap();

        let error = restore_entry("dir", &item, false, false).err().unwrap();
        assert!(error.contains("changed while it was being restored"), "{}", error);
        assert_eq!(std::fs::read(warehouse.root.join("dir/a.txt")).unwrap(), b"edited\n");
        assert!(matches!(restore_entry("dir", &item, false, true), Ok(Restored::Rewritten(_))));
    }
}
