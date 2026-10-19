use chrono::Utc;
use crate::builder::object::loose_object_builder::LooseObjectBuilder;
use crate::enums::parcel_action_type::ParcelActionType;
use crate::model::operator::Operator;
use crate::model::parcel::Parcel;
use crate::model::parcel_action::ParcelAction;
use crate::util::{
    cherry_pick_utils, config_utils, file_utils, inventory_utils, load_guard_utils, merge_utils,
    object_utils, office_utils, pallet_utils, scope_utils, sign_utils, tree_utils,
};
use crate::util::pallet_utils::RefUpdateError;

/// Why [`stack_parcel`] failed: its head write did not go through (which a merge or pick
/// completion has to settle, see `consolidate`'s caller), or anything else.
#[derive(Debug)]
pub enum StackError {
    /// The parcel was built and stored, but the pallet's head write returned `error`: another bay
    /// or process moved the pallet off the base (`Moved`), or the write failed (`Failed`, which
    /// may still have landed — the pallet then holds `parcel`, and the staged state it consumed is
    /// already cleaned up). Nothing else in the bay was touched: the working tree still holds
    /// `tree`, the stacked parcel's tree.
    HeadWrite {
        error: RefUpdateError,
        pallet: String,
        parcel: String,
        tree: String,
    },

    /// Any other failure; nothing was stacked.
    Other(String),
}

impl From<String> for StackError {
    fn from(error: String) -> Self {
        StackError::Other(error)
    }
}

impl From<crate::error::CoreError> for StackError {
    fn from(error: crate::error::CoreError) -> Self {
        StackError::Other(error.into())
    }
}

impl From<StackError> for String {
    fn from(error: StackError) -> Self {
        match error {
            StackError::HeadWrite { error, .. } => error.to_string(),
            StackError::Other(error) => error,
        }
    }
}

/// Resolve the key a parcel must be signed with: `None` while trust is not established,
/// the operator's local active key afterwards (an error when they have none — signing
/// is mandatory once trust exists).
///
/// # Arguments
/// * `operator` - The configured operator.
///
/// # Returns
/// * `Ok(Some(String))` - The signing key id.
/// * `Ok(None)`         - Trust is not established; parcels are not signed yet.
/// * `Err(String)`      - Trust is established but the operator cannot sign.
pub fn resolve_signing_key(operator: &Operator) -> Result<Option<String>, String> {
    if office_utils::read_trust_anchor()?.is_none() {
        return Ok(None);
    }

    let state = office_utils::read_office_state()?;

    if state.find_user(&operator.identifier).is_none() {
        return Err(format!(
            "Trust is established for this warehouse, and \"{}\" is not enrolled: \
            parcels cannot be stacked without a signature. Ask an enrolled operator to \
            admit you (\"office keygen\", then \"office admit\").",
            operator.identifier
        ));
    }

    state.signing_key_of(&operator.identifier)
        .map(|key| Some(key.key_id.clone()))
        .ok_or(format!(
            "No active key of \"{}\" is present on this machine; parcels cannot be \
            stacked without a signature.",
            operator.identifier
        ))
}

/// Stack the inventory as a new parcel on the current pallet: build the tree objects,
/// create the parcel (recording the configured operator), advance the pallet head, and
/// clean up the consumed staged state.
///
/// When a consolidation is in progress, the consolidated head becomes the parcel's second
/// parent and the consolidation state is cleared. Stacking refuses to run while any
/// inventory entry is in a conflict state.
///
/// The parcel is stacked on a base, and the head write is a compare-and-set against it, so a
/// pallet another bay or process moved meanwhile is refused rather than overwritten. The base is
/// `expected_base` when given (a merge or pick completing inside the command that built its tree
/// passes the head it built on); otherwise the head an in-progress consolidation or cherry-pick
/// recorded when it started, because that is what the staged merge output was built from;
/// otherwise the pallet's head as read here.
///
/// # Arguments
/// * `description`   - The parcel description.
/// * `expected_base` - The head the staged tree was built on, when the caller knows it.
///
/// # Returns
/// * `Ok((String, String))` - The new parcel's hash and the pallet it was stacked on.
/// * `Err(StackError)`      - If there is nothing to stack, the operator identity is not
///                            configured, conflicts are unresolved, an operation failed, or the
///                            head write did not go through (`StackError::HeadWrite`).
pub async fn stack_parcel(description: Option<String>,
                          expected_base: Option<&str>) -> Result<(String, String), StackError> {
    // The cheapest possible pre-check runs first: a `load` that started but never finished
    // cleanly (crashed mid-walk, or returned an error) leaves a marker behind
    // (`load_guard_utils`), and stacking now could silently commit an incomplete inventory as a
    // durable parcel. Refuses before even resolving the operator identity below. `park`'s push
    // carries the identical check for the identical reason — see its own doc comment.
    load_guard_utils::check_no_incomplete_load()?;

    // The operator identity is resolved before any work happens, so a missing
    // configuration aborts the stack before objects are written.
    let operator = config_utils::get_operator()?;

    // Once trust is established, every parcel must be signed — there is no unsigned
    // escape hatch. The signing key is resolved up front so a missing key aborts the
    // stack before objects are written.
    let signing_key_id = resolve_signing_key(&operator)?;

    // Computed before the inventory is read, because the read skips every subtree whose rollup
    // already matches the head (see `inventory_utils::prepare_stack_inventory`), and the scope
    // gates which keys may be skipped at all.
    let scope = scope_utils::current_scope()?;
    let pallet = pallet_utils::get_current_pallet_name()?;
    let consolidation = merge_utils::read_consolidation_state()?;

    // A cherry-pick in progress (§9.1 #8) completes here, single-parent: the picked
    // parcel's authors are preserved and this operator is recorded as the stacker.
    let cherry_pick = cherry_pick_utils::read_state()?;

    // The base the parcel stacks on, which the head write below expects the pallet still to be at.
    // Merge output staged by an earlier command was built on the head recorded when it started,
    // not on whatever the pallet holds now; stacking it on a head another bay moved would revert
    // that bay's parcel.
    let head = match (expected_base, &consolidation, &cherry_pick) {
        (Some(base), _, _) => Some(base.to_string()),
        (None, Some(state), _) => Some(state.our_head.clone()),
        (None, None, Some(state)) => Some(state.our_head.clone()),
        (None, None, None) => pallet_utils::get_pallet_head(&pallet)?,
    };
    let head_root_hash = match &head {
        Some(head_hash) => Some(object_utils::load_parcel(head_hash)?.tree_hash),
        None => None,
    };

    // Every shard the tree build can need is read and parsed exactly once here, and shared by the
    // conflict check, the tree build and the post-stack cleanup. The read happens before anything
    // is written, so the conflict check below still runs strictly before any warehouse mutation.
    let prepared = std::sync::Arc::new(inventory_utils::prepare_stack_inventory(
        head_root_hash.as_deref().map(|hash| (hash, &scope)),
    )?);

    if inventory_utils::has_conflict_entries_in(&prepared) {
        return Err(StackError::Other(
            "There are unresolved conflicts in the inventory. Resolve them, \"load\" the \
            resolved files, and stack again (see \"stocktake\" for the list).".to_string()
        ));
    }

    // An orphaned skeleton (left behind when a merge is aborted by removing
    // ".forklift/consolidation" directly, instead of resolving it) is harmless on its own — a
    // plain stack never reads it — but stale disk state is untidy and could confuse a future
    // reader. Clean it up opportunistically whenever there is no consolidation in progress to
    // own it; best-effort, since this is hygiene, not correctness, so it must never fail the
    // stack.
    if consolidation.is_none() {
        let _ = merge_utils::OutOfScopeSkeleton::clear();
    }

    // Every tree object built below is staged into `batch`, not fsynced immediately — one
    // durability barrier for the whole burst instead of a per-object fsync pair (§ perf). The
    // tree build runs from `TaskExecutor`'s parallel workers, so this uses `WriteBatch` (its
    // `stage` is safe to share across threads), not `BulkStoreSession` (a documented
    // single-sequential-writer primitive — see its doc comment for why sharing it across
    // parallel writers is unsafe). Nothing may depend on a staged object's durability or
    // visibility until `batch.finish()` below returns `Ok` — in particular the pallet head is
    // only ever set after that, never staged into this same batch (see the comment there).
    let batch = std::sync::Arc::new(file_utils::WriteBatch::new());

    // `track_tree_hashes: true` — the per-key hashes below are read to stamp shards' rollups
    // after a successful stack (DESIGN.html §5.0 D item 10, finding #8; `park`'s own call passes
    // `false`, since it discards the map immediately instead).
    let (partial_root, tree_hashes) = tree_utils::build_tree_from_inventory_deferred(&prepared, &batch, true).await?;
    let partial_root = partial_root
        .ok_or("There is nothing to stack. Use the \"load\" command to stage changes first.".to_string())?;

    // In a scoped (sparse) bay the dock materializes only the in-scope subtree(s), so the freshly
    // built root above is a sparse partial that would drop every out-of-scope sibling. The overlay
    // splices it onto the head's spine — copying out-of-scope siblings verbatim by hash — so the
    // stacked root tree is byte-identical to what a full workspace would produce (§3.2).
    let root_tree = if scope.is_full() {
        partial_root
    } else {
        // A completing merge splices its out-of-scope skeleton into the merge parcel's
        // tree: the out-of-scope siblings theirs changed one-sided, adopted by hash. A plain
        // stack has no skeleton, so the overlay copies every out-of-scope sibling verbatim.
        // `read_required` (not `read`) is deliberate here: a consolidation in progress with no
        // skeleton file is a broken invariant (an interrupted write), not "no resolutions" — see
        // its doc comment.
        let skeleton = match &consolidation {
            Some(_) => merge_utils::OutOfScopeSkeleton::read_required()?,
            None => merge_utils::OutOfScopeSkeleton::default(),
        };

        // Runs sequentially (no TaskExecutor here), but staged into the very same batch as the
        // parallel tree build above so every spine object it writes joins the same one barrier.
        tree_utils::build_scoped_root_tree(
            head_root_hash.as_deref(), &partial_root, &scope, skeleton.entries(), &batch,
        )?
    };

    // A top-level name only this file system tells apart from `.forklift` (`.Forklift` on Linux,
    // `.forklift.` anywhere but Windows) would stack fine and then refuse every checkout on a
    // file system that does not, so refuse it here, where it can still be unloaded. A name this
    // volume resolves to the metadata folder (which a whole-directory load walks into) would stack
    // the folder's own content.
    for (name, _) in root_tree.get_subtrees().chain(root_tree.get_files()) {
        if office_utils::opens_metadata_folder(name)? {
            return Err(StackError::Other(format!(
                "Refusing to stack the top-level entry {:?}: it is, on this or some other file \
                system, the warehouse's metadata folder, so no working directory there could check \
                it out. Rename it, or unload it and add it to .forkliftignore.",
                name
            )));
        }
    }

    // The per-key hashes above come straight from the plain (unspliced) inventory tree build,
    // so they are only trustworthy for a key the splice never touches: the whole warehouse when
    // the bay is fully in scope, or an individually fully-in-scope directory otherwise (the
    // splice only ever rewrites the spine above an in-scope boundary and the root — see
    // `tree_utils::build_scoped_root_tree` — never a key inside one). Anything else (a spine or
    // out-of-scope key) is dropped rather than stamped with a hash the splice may have changed
    // underneath it — cleanup below leaves those unstamped (DESIGN.html §5.0 D item 8).
    let stamp_hashes: std::collections::BTreeMap<String, String> = if scope.is_full() {
        tree_hashes
    } else {
        tree_hashes.into_iter()
            .filter(|(key, _)| scope.classify(key) == scope_utils::ScopeClass::InScope)
            .collect()
    };

    let root_is_empty = root_tree.get_files().len() == 0 && root_tree.get_subtrees().len() == 0;

    if head.is_none() && root_is_empty {
        return Err(StackError::Other(
            "There is nothing to stack. Use the \"load\" command to stage changes first.".to_string()
        ));
    }

    if let Some(head_hash) = &head {
        let head_parcel = object_utils::load_parcel(head_hash)?;

        // A consolidation may legitimately produce the same tree (e.g. "theirs" only
        // re-applied changes we already have) — the merge parcel still has to be
        // recorded, so the no-op check only applies to plain stacks.
        if consolidation.is_none() && head_parcel.tree_hash == root_tree.hash {
            // For a cherry-pick, an unchanged tree means the pick is empty (the head
            // already has the change): clear the pick and say so, rather than recording
            // an empty parcel.
            if cherry_pick.is_some() {
                cherry_pick_utils::clear_state()?;

                return Err(StackError::Other(
                    "The cherry-pick is empty: applying it changes nothing (the head already \
                    has these changes). Nothing was stacked.".to_string()
                ));
            }

            return Err(StackError::Other(format!(
                "Nothing to stack: the inventory matches the head of pallet \"{}\".",
                pallet
            )));
        }
    }

    let mut parents: Vec<String> = head.iter().cloned().collect();

    // A consolidation adds the consolidated head as a second parent; a cherry-pick does
    // not — it re-applies a diff as a single-parent parcel (no merge, no rewrite).
    if let Some(state) = &consolidation {
        parents.push(state.their_head.clone());
    }

    // A cherry-pick's completing `stack` defaults to the pick's stored description.
    let description = description.or_else(|| cherry_pick.as_ref().and_then(|cp| cp.description.clone()));

    // Authorship convention: every parcel records its author(s) as explicit Author
    // actions, even when the author and the stacker are the same operator (like git,
    // which records author == committer on plain commits). A cherry-pick preserves the
    // source parcel's Author actions and adds this operator's Stack action.
    let timestamp = Utc::now();

    let actions = match &cherry_pick {
        Some(state) => {
            let authors = cherry_pick_utils::collect_source_authors(&state.source, &operator)?;

            let mut actions: Vec<ParcelAction> = authors.into_iter()
                .map(|author| ParcelAction {
                    operator: author,
                    action: ParcelActionType::Author,
                    description: None,
                    timestamp,
                })
                .collect();

            actions.push(ParcelAction {
                operator: operator.clone(),
                action: ParcelActionType::Stack,
                description: None,
                timestamp,
            });

            actions
        }
        None => vec![
            ParcelAction {
                operator: operator.clone(),
                action: ParcelActionType::Author,
                description: None,
                timestamp,
            },
            ParcelAction {
                operator: operator.clone(),
                action: ParcelActionType::Stack,
                description: None,
                timestamp,
            },
        ],
    };

    let parcel = Parcel {
        tree_hash: root_tree.hash.clone(),
        parents,
        actions,
        description,
    };

    // The parcel object itself joins the same batch as the tree objects it points to: the head
    // will reference it directly, so it must be just as durable as everything beneath it before
    // anything downstream can see it.
    let mut object = LooseObjectBuilder::build_parcel(&parcel);
    object.store_deferred(&batch)?;

    // The barrier: every tree object and the parcel object staged above become durable and
    // visible at their final content-addressed paths now, all at once, in exchange for far
    // fewer device flushes than fsyncing each individually. This must complete (return `Ok`)
    // before anything below is allowed to reference what it just published — the signature
    // sidecar and the pallet head are both written only from this point on, deliberately kept
    // out of this same batch (see the crash-ordering note on the ref write below).
    batch.finish()?;

    if let Some(key_id) = &signing_key_id {
        let signature = sign_utils::sign_parcel_hash(key_id, &object.hash)?;
        sign_utils::store_parcel_signature(&object.hash, &signature)?;
    }

    // Deliberately its own separate atomic write (unbatched, exactly as every other ref write),
    // not folded into `batch` above: `WriteBatch::finish` offers no
    // atomicity *across* its entries, only that each one individually is never visible before
    // its bytes are durable (see its doc comment). Sequencing the ref write as a distinct step
    // that only starts after `batch.finish()` has already returned `Ok` is what guarantees the
    // head can never become visible pointing at a parcel (or any tree beneath it) that is not
    // yet durable — batching the ref in with the objects it references would trade that
    // guarantee for a few more saved fsyncs, which is not a trade this makes.
    //
    // It is a compare-and-set against the base this parcel was built on, so a move by another bay
    // or process since that read is refused rather than silently replaced.
    match pallet_utils::update_pallet_head(&pallet, head.as_deref(), &object.hash) {
        Ok(()) => {}

        // Another bay already moved the pallet to this very parcel: the move stands.
        Err(RefUpdateError::Moved(moved)) if moved.found.as_deref() == Some(object.hash.as_str()) => {}

        Err(error) => {
            // A failed write can still have landed, so whether the parcel is on the pallet is read
            // back, never inferred from the error. If it is, the staged state it consumed is
            // cleaned up exactly as on success, and the failure is still reported.
            let landed = matches!(error, RefUpdateError::Failed(_))
                && pallet_utils::get_pallet_head(&pallet).ok().flatten().as_deref() == Some(object.hash.as_str());

            if landed {
                if let Err(cleanup) = clean_up_after_stack(&prepared, &stamp_hashes) {
                    return Err(StackError::Other(format!("{} {}", error, cleanup)));
                }
            }

            return Err(StackError::HeadWrite { error, pallet, parcel: object.hash, tree: root_tree.hash });
        }
    }

    clean_up_after_stack(&prepared, &stamp_hashes)?;

    Ok((object.hash, pallet))
}

/// Clean up what a stacked parcel consumed: see the call in [`stack_parcel`].
fn clean_up_after_stack(prepared: &inventory_utils::PreparedInventory,
                        stamp_hashes: &std::collections::BTreeMap<String, String>) -> Result<(), String> {
    // The parcel consumed the staged removals (and the consolidation or cherry-pick, if any).
    // Reuses the same parsed snapshot the conflict check and tree build already read — nothing
    // between then and here changes a shard's content on disk, only the object store, the ref
    // and the signature sidecar (see `cleanup_after_stack_with`'s doc comment). The skipped keys
    // it leaves untouched need no scope filtering of their own (unlike `stamp_hashes` above): a
    // skip root's parent is always fully in scope (`prepare_stack_inventory`'s scoped-bay
    // caveat).
    inventory_utils::cleanup_after_stack_with(prepared, stamp_hashes)?;
    merge_utils::clear_consolidation_state()?;
    merge_utils::OutOfScopeSkeleton::clear()?;
    cherry_pick_utils::clear_state()
}
