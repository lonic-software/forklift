use std::path::{Path, PathBuf};
use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::model::remote::WarehouseInfo;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::remote_utils::{RemoteClient, TorSettings};
use forklift_core::util::scope_utils::MaterializationScope;
use forklift_core::util::{
    bundle_utils, config_utils, object_utils, pack_utils, pallet_utils, remote_utils,
    scope_utils, shift_utils, warehouse_utils,
};
use crate::output::{self, CommandOutput};

/// Handle the franchise command (git's "clone"): open a local franchise of a remote
/// warehouse — prepare a fresh warehouse in the target directory, remember the remote,
/// adopt its trust anchor, download the history (the remote's bundle first, when it has
/// one, then whatever the bundle lacked as loose objects) and materialize the chosen
/// pallet's head in the working directory.
///
/// The remote is contacted before the target directory (or the warehouse in it) is touched, and
/// the requested pallet is checked against the handshake's own answer before anything is fetched,
/// so a bad URL, a bad token, an unreachable host, or a `--pallet` typo all leave the filesystem
/// exactly as they found it — nothing to fetch, nothing to clean up. A failure after that point
/// (a dropped connection mid-fetch, a full disk, a typo'd `--only` path, …) is cleaned up before
/// this returns: the directory (or missing ancestor chain) franchise created is removed, or — if
/// it pre-existed empty — just emptied back out, so an immediate retry lands in a directory that
/// is new or empty either way, never permanently wedged.
///
/// # Arguments
/// * `url`       - The remote to franchise.
/// * `directory` - The directory to create the warehouse in (must be empty or absent).
/// * `pallet`    - The pallet to check out (default: the remote's default pallet).
/// * `token`     - The bearer token, when the remote requires one (also remembered in
///                 the warehouse configuration).
/// * `only`      - When non-empty, franchise **sparsely**: fetch the full signed history but
///                 only the content under these subtree path(s), leaving the rest sealed by
///                 hash. The remote's whole-store bundle is skipped, and the remote is
///                 recorded as this sparse warehouse's origin (lift refuses to publish
///                 elsewhere).
///
/// # Returns
/// * `Ok(())` - If the franchise is ready to work in.
/// * `Err(String)` - If the directory is not empty, the remote is unreachable, the pallet does
///   not exist, or a transfer failed. The target is left as it was found (see above) either way.
pub async fn handle_command(url: &str,
                            directory: &str,
                            pallet: Option<String>,
                            token: Option<String>,
                            only: Vec<String>) -> Result<(), String> {
    // Normalize the sparse scope up front (before any warehouse is created), so a malformed
    // --only refuses cleanly. An empty list is a full (ordinary) franchise.
    let fetch_scope = resolve_fetch_scope(&only)?;
    let sparse = !fetch_scope.is_full();

    let target = Path::new(directory);

    if target.exists() {
        let mut entries = std::fs::read_dir(target)
            .map_err(|e| format!("Error while reading \"{}\": {}", directory, e))?;

        if entries.next().is_some() {
            return Err(format!(
                "\"{}\" is not empty; franchise into a new or empty directory.",
                directory
            ));
        }
    }

    // Extending the same principle as the --only normalization above (refuse cleanly before any
    // warehouse is created) to the network: talk to the remote *before* touching the filesystem
    // at all. A bad URL, bad token or unreachable host must leave the target exactly as found —
    // not even an empty directory left behind for a directory that did not exist before.
    //
    // Tor settings come from *global* configuration only — deliberately, never the effective
    // (warehouse-then-global) resolution `RemoteClient::new` would use. There is no "this
    // warehouse" yet to have an opinion here; only the ambient working directory, which may
    // happen to sit inside a *different*, unrelated warehouse. Before this handshake moved
    // earlier, it always ran *inside* the fresh (still-empty) target, whose own warehouse scope
    // had never had anything to set — so global was, in effect, the only scope that could ever
    // apply. Falling through to `from_config`'s cascade now would silently make a brand-new
    // clone's Tor routing depend on whatever unrelated warehouse the caller happened to be
    // standing in — a cwd-dependent, security-relevant surprise this preserves against.
    let client = RemoteClient::new_with_tor(url, token.clone(), TorSettings::from_global_config())?;
    let info = client.fetch_info().await?;

    // Resolve the pallet from the handshake alone — no I/O — so a `--pallet` typo is refused
    // immediately, before anything is fetched or mutated: the same principle once more. (The
    // `--only` scope, by contrast, cannot be validated this early: it names a subtree of a
    // *tree*, and no tree is available until the pallet's history is actually fetched — see the
    // check after `fetch_history_scoped` below. That fetch is unavoidably real I/O even for this
    // validation, though a sparse one stays lean — spine and signatures only, never out-of-scope
    // content — so a typo'd --only wastes far less than a typo'd --pallet against a non-sparse
    // franchise would have, before this change.)
    let resolution = resolve_pallet(&info, pallet)?;

    // The handshake (and the pallet check) succeeded; only now may the filesystem change.
    // `claim_target` is the sole authority on whether franchise itself created the target (or any
    // missing ancestor) — decided atomically, not by a separate `exists()` check racing the
    // multi-second (or, over Tor, longer) gap the handshake just took. See its own doc comment.
    let original_cwd = std::env::current_dir()
        .map_err(|e| format!("Error while reading the current directory: {}", e))?;
    let created_root = claim_target(target)?;
    let absolute_target = original_cwd.join(target);
    let absolute_created_root = created_root.map(|root| original_cwd.join(root));

    let outcome = enter_and_franchise(
        target, directory, url, resolution, token, fetch_scope, sparse, &client, info,
    ).await;

    if let Err(error) = outcome {
        // Drop any mmap'd pack handles this run loaded (reading a franchised bundle's packs
        // while adopting trust or meta pallets) before deleting anything under the target.
        // Originally added on the theory that an open mmap blocks its own pack file's deletion
        // on Windows (PR #84 review finding 1) — tested directly on `windows-latest` and that
        // specific claim did not hold (see `pack_utils::invalidate_cache`'s doc comment and
        // `an_mmapd_pack_does_not_block_its_own_directory_removal` for the confirming probe).
        // Kept anyway as cheap, correctness-neutral insurance, reusing the same drop-before-
        // delete/rename idiom `compact`/`import_transport_packs` already use, rather than
        // asserting a Windows-deletion-blocking mechanism this run no longer relies on.
        pack_utils::invalidate_cache();

        // Get back to where we started before touching the target — it may currently be our
        // cwd, and removing the directory a process is standing in leaves it dangling. Cleanup
        // failing must never shadow the original error: the operator needs to see why franchise
        // failed, not why the cleanup did, so a cleanup error is appended, not substituted.
        let _ = std::env::set_current_dir(&original_cwd);
        return Err(cleanup_after_failure(&absolute_target, absolute_created_root.as_deref(), error));
    }

    Ok(())
}

/// Switch into the already-claimed `target` and run the rest of the franchise. `set_current_dir`'s
/// own failure is routed through this `Result` rather than an early `?` return in
/// `handle_command`, so the caller's cleanup runs unconditionally for *any* failure from this
/// point on, not only the ones inside `franchise_into_prepared_target`.
#[allow(clippy::too_many_arguments)]
async fn enter_and_franchise(
    target: &Path,
    directory: &str,
    url: &str,
    resolution: PalletResolution,
    token: Option<String>,
    fetch_scope: MaterializationScope,
    sparse: bool,
    client: &RemoteClient,
    info: WarehouseInfo,
) -> Result<(), String> {
    std::env::set_current_dir(target)
        .map_err(|e| format!("Error while switching to \"{}\": {}", directory, e))?;

    franchise_into_prepared_target(url, directory, resolution, token, fetch_scope, sparse, client, info).await
}

/// Everything that mutates the warehouse: run only after the remote handshake and the pallet
/// check have already succeeded (see `handle_command`), with the current directory already
/// switched into the target. A failure anywhere in here is handled by the caller, which cleans
/// up whatever this left behind.
#[allow(clippy::too_many_arguments)]
async fn franchise_into_prepared_target(
    url: &str,
    directory: &str,
    resolution: PalletResolution,
    token: Option<String>,
    fetch_scope: MaterializationScope,
    sparse: bool,
    client: &RemoteClient,
    info: WarehouseInfo,
) -> Result<(), String> {
    warehouse_utils::prepare_warehouse()?;

    config_utils::set_value(config_utils::KEY_REMOTE_URL, url, ConfigScope::Warehouse)?;

    if let Some(token) = &token {
        config_utils::set_value(config_utils::KEY_REMOTE_TOKEN, token, ConfigScope::Warehouse)?;
    }

    let mut report = FranchiseReport {
        remote: url.to_string(),
        directory: directory.to_string(),
        bundle_objects: None,
        bundle_signatures: None,
        adopted_anchor: false,
        meta_adopted: Vec::new(),
        pallet: String::new(),
        head: None,
        unborn: false,
        scope: fetch_scope.prefixes().to_vec(),
        fetched_objects: 0,
        densify_suggested: false,
    };

    // The bundle is a fast start, not a requirement: whatever it lacks (or if there is none) is
    // fetched loose by the history walk below. A sparse franchise skips it entirely — the bundle
    // is the whole store, which would defeat the point of fetching only a subtree.
    if !sparse {
        let bundle_path = bundle_utils::get_latest_bundle_path();

        if let Some(parent) = bundle_path.parent() {
            forklift_core::util::file_utils::create_folder_if_not_exists(parent)?;
        }

        if client.fetch_bundle_to(&bundle_path).await? {
            match bundle_utils::import_bundle(&bundle_path) {
                Ok(imported) => {
                    report.bundle_objects = Some(imported.stored_objects);
                    report.bundle_signatures = Some(imported.stored_signatures);
                    report.densify_suggested = imported.installed_packs;
                }
                Err(error) if bundle_utils::is_unsupported_bundle_error(&error) => {
                    // A future envelope is only an optimization this client cannot use. Do not
                    // retain it as this warehouse's own `latest` bundle; continue through the
                    // verified incremental-object walk below. Known-format corruption stays fatal.
                    let _ = std::fs::remove_file(&bundle_path);
                }
                Err(error) => return Err(error),
            }
        }
    }

    let trust = remote_utils::adopt_remote_trust(client, &info).await?;
    report.adopted_anchor = trust.adopted_anchor;

    // Adopt the meta pallets too (the manifest, …), so a clone carries the post-metadata,
    // not just the working history. A fresh clone has none locally, so nothing diverges.
    report.meta_adopted = remote_utils::adopt_meta_pallets(client, &info).await?.adopted;

    let PalletResolution { pallet_name, remote_head } = resolution;

    let Some(remote_head) = remote_head else {
        // Legitimately unborn: `resolve_pallet` already ruled out a typo before any of this
        // warehouse's mutation began, so reaching this branch means either the remote's own
        // default pallet has nothing stacked yet, or an explicitly named pallet does not exist
        // on an otherwise-fresh remote — both fine starting points, not errors.
        pallet_utils::set_current_pallet_name(&pallet_name)?;

        // There is no tree to validate --only against (the pallet has nothing stacked on the
        // remote yet); record the requested sparse scope and origin as-is, so a later
        // lower/expand into this pallet, once it is born, stays within it.
        if sparse {
            scope_utils::set_fetch_scope(&fetch_scope)?;
            scope_utils::set_bay_scope(&fetch_scope)?;
            config_utils::set_value(config_utils::KEY_REMOTE_ORIGIN, url, ConfigScope::Warehouse)?;
        }

        report.pallet = pallet_name;
        report.unborn = true;
        output::emit("franchise", &report);

        return Ok(());
    };

    let stats = remote_utils::fetch_history_scoped(client, &remote_head, &fetch_scope).await?;

    let tree_hash = object_utils::load_parcel(&remote_head)?.tree_hash;

    // Validate the --only paths against the fetched head BEFORE any of this warehouse's state
    // (the fetch scope, the origin record, the pallet head) is written: a typo'd path is
    // rejected here and leaves nothing behind — the fetched objects are harmless orphans in an
    // otherwise-empty warehouse the operator is about to discard — rather than a fresh but
    // scope-inconsistent warehouse they would have to notice and clean up by hand. The spine is
    // fetched, so the walk to each in-scope subtree resolves without touching anything absent.
    if sparse {
        for prefix in fetch_scope.prefixes() {
            if !is_directory_in_tree(&tree_hash, prefix)? {
                return Err(format!(
                    "\"{}\" is not a directory in pallet \"{}\" on the remote, so it cannot be a \
                    sparse franchise scope. Nothing was recorded.",
                    prefix, pallet_name
                ));
            }
        }

        scope_utils::set_fetch_scope(&fetch_scope)?;
        scope_utils::set_bay_scope(&fetch_scope)?;
        config_utils::set_value(config_utils::KEY_REMOTE_ORIGIN, url, ConfigScope::Warehouse)?;
    }

    pallet_utils::set_pallet_head(&pallet_name, &remote_head)?;
    pallet_utils::set_current_pallet_name(&pallet_name)?;

    shift_utils::materialize_tree(None, &tree_hash, "Franchising")?;

    report.pallet = pallet_name;
    report.head = Some(remote_head.clone());
    report.fetched_objects = stats.fetched_objects;
    output::emit("franchise", &report);

    Ok(())
}

/// Which pallet to check out, resolved from the handshake alone (see `resolve_pallet`).
struct PalletResolution {
    pallet_name: String,
    /// `None` when the pallet is unborn on the remote (legitimate — see `resolve_pallet`'s doc).
    remote_head: Option<String>,
}

/// Resolve which pallet franchise should check out, using only the handshake's own answer
/// (`info.pallets`) — no I/O. Named explicitly and absent from the remote is refused right here,
/// before anything is fetched or mutated, unless the remote is entirely fresh (no working pallets
/// stacked at all), in which case an explicit name still starts unborn, exactly like the default
/// pallet does.
///
/// # Returns
/// * `Ok(PalletResolution)` - The pallet to check out, and its head (`None` if unborn).
/// * `Err(String)`          - The named pallet does not exist and the remote is not fresh.
fn resolve_pallet(info: &WarehouseInfo, pallet: Option<String>) -> Result<PalletResolution, String> {
    let explicitly_requested = pallet.is_some();
    let pallet_name = pallet.unwrap_or_else(|| info.default_pallet.clone());

    if let Some(remote_head) = info.pallets.get(&pallet_name) {
        return Ok(PalletResolution { pallet_name, remote_head: Some(remote_head.clone()) });
    }

    // A pallet absent from the handshake is unborn on the remote. The remote's own default
    // pallet legitimately starts unborn (a fresh remote, or one whose current pallet has
    // nothing stacked while others do); a pallet the user asked for by name gets typo
    // protection instead, unless nothing is stacked at all. Meta pallets (`@office`, …) are
    // not working pallets — a remote with only those is still fresh, and they never appear in
    // the "it has: …" hint.
    let user_pallets: Vec<&String> = info.pallets.keys()
        .filter(|name| !name.starts_with(pallet_utils::META_QUALIFIER))
        .collect();
    let is_fresh = user_pallets.is_empty();

    if explicitly_requested && !is_fresh {
        return Err(format!(
            "The remote has no pallet \"{}\" (it has: {}).",
            pallet_name,
            user_pallets.into_iter().cloned().collect::<Vec<_>>().join(", ")
        ));
    }

    Ok(PalletResolution { pallet_name, remote_head: None })
}

/// Atomically claim `target` (and any missing ancestor directories) for this franchise, closing
/// the TOCTOU window a separate `exists()` check followed by `create_dir_all` would leave open
/// across the handshake's network round trip: checking "does it exist" and then creating it are
/// two different moments, and the remote handshake now sits in between them — seconds, not
/// microseconds; longer still over Tor. `std::fs::create_dir` (never `create_dir_all`) is used
/// one path component at a time instead, so each directory's creation is individually atomic —
/// its own result alone proves whether *this call* created it, with no separate read required.
///
/// # Returns
/// * `Ok(Some(root))` - The topmost directory this call created. Everything under it — every
///   other ancestor and `target` itself — was created in this same loop, so removing `root`
///   (recursively) removes exactly and only what franchise itself created here; nothing else
///   could have raced in between two creations both made by this call.
/// * `Ok(None)` - `target` already existed by the time this call reached it (whether from before
///   the handshake, or from something else during it). Its emptiness is re-verified right here,
///   immediately before anything is written into it — the only earlier check (before the
///   handshake) is now stale.
/// * `Err(String)` - A directory could not be created (a real error, not "already exists"), or
///   `target` exists but is no longer empty. Nothing this call itself created is left behind on
///   the first kind of error: it undoes its own partial progress before returning, so the caller
///   never has to reason about a half-claimed chain.
fn claim_target(target: &Path) -> Result<Option<PathBuf>, String> {
    let mut created_root: Option<PathBuf> = None;
    let mut current = PathBuf::new();

    for component in target.components() {
        current.push(component);

        match std::fs::create_dir(&current) {
            Ok(()) => {
                if created_root.is_none() {
                    created_root = Some(current.clone());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => {
                if let Some(root) = &created_root {
                    let _ = std::fs::remove_dir_all(root);
                }
                return Err(format!("Error while creating \"{}\": {}", current.display(), e));
            }
        }
    }

    if created_root.is_none() {
        let mut entries = std::fs::read_dir(target)
            .map_err(|e| format!("Error while reading \"{}\": {}", target.display(), e))?;

        if entries.next().is_some() {
            return Err(format!(
                "\"{}\" is not empty; franchise into a new or empty directory.",
                target.display()
            ));
        }
    }

    Ok(created_root)
}

/// Undo whatever franchise created before it failed, so the directory is retryable rather than
/// permanently wedged — the defect this closes: a franchise that failed partway through used to
/// leave `.forklift`/`.forkliftignore` behind, and a subsequent, correct attempt then refused to
/// enter a directory that was no longer empty.
///
/// `created_root` is `claim_target`'s own record of what it created — never re-inferred here:
/// * `Some(root)` - franchise created `root` (the target itself, or a missing ancestor of it);
///   removing it recursively removes exactly what franchise created.
/// * `None` - the target pre-existed (confirmed empty by `claim_target`); only its contents are
///   removed, leaving the empty directory behind.
///
/// Cleanup failing must never mask the original error — the operator needs to see why franchise
/// failed, not why the cleanup did — so on a cleanup error this appends the leftover path to the
/// original message rather than replacing it.
fn cleanup_after_failure(target: &Path, created_root: Option<&Path>, original_error: String) -> String {
    let cleanup_result = match created_root {
        Some(root) => std::fs::remove_dir_all(root),
        None => remove_directory_contents(target),
    };

    match cleanup_result {
        Ok(()) => original_error,
        Err(cleanup_error) => format!(
            "{} (additionally, cleaning up \"{}\" after the failure did not succeed: {}; remove it \
            by hand before retrying)",
            original_error, created_root.unwrap_or(target).display(), cleanup_error
        ),
    }
}

/// Remove every entry directly under `target`, leaving `target` itself in place. Used to restore
/// a pre-existing (empty) directory that franchise dirtied before failing. `DirEntry::file_type`
/// does not follow symlinks, so a symlink entry is unlinked itself (`remove_file`) rather than
/// having its target recursively removed.
fn remove_directory_contents(target: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(target)? {
        let entry = entry?;
        let path = entry.path();

        if entry.file_type()?.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }

    Ok(())
}

/// Normalize the `--only` paths into a fetch [`MaterializationScope`], validating each is a
/// well-formed, non-root warehouse path. An empty list is the full (ordinary) franchise scope.
/// The paths cannot be checked against the remote's tree yet (there is no local head), so the
/// directory check happens after the fetch.
fn resolve_fetch_scope(only: &[String]) -> Result<MaterializationScope, String> {
    if only.is_empty() {
        return Ok(MaterializationScope::full());
    }

    let mut keys: Vec<String> = Vec::new();

    for raw in only {
        let path = WarehousePath::from_user_input(raw)?;

        if path.is_root() {
            return Err(
                "A franchise scope of the warehouse root is the whole tree — franchise without \
                --only instead.".to_string()
            );
        }

        keys.push(path.as_key().to_string());
    }

    Ok(MaterializationScope::from_prefixes(keys))
}

/// Whether a warehouse path resolves to a subtree (directory) in the given root tree. Used after
/// a sparse fetch to reject an --only path that names nothing (or a file) in the head.
fn is_directory_in_tree(root_tree_hash: &str, key: &str) -> Result<bool, String> {
    let mut current = object_utils::load_tree(root_tree_hash)?;

    for component in key.split('/') {
        let subtree = current.get_subtrees()
            .find(|(name, _)| name.as_str() == component)
            .map(|(_, item)| item.hash.clone());

        match subtree {
            Some(hash) => current = object_utils::load_tree(&hash)?,
            None => return Ok(false),
        }
    }

    Ok(true)
}

/// The result of a franchise: what was imported and which pallet was checked out.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct FranchiseReport {
    remote: String,
    directory: String,

    /// Objects imported from the remote's bundle, when it had one.
    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_objects: Option<usize>,

    #[serde(skip_serializing_if = "Option::is_none")]
    bundle_signatures: Option<usize>,

    /// Whether the remote's trust anchor was adopted.
    adopted_anchor: bool,

    /// The meta pallets (e.g. `@manifest`) adopted from the remote.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    meta_adopted: Vec<String>,

    /// The pallet checked out.
    pallet: String,

    /// Its head (`null` when the pallet is unborn on the remote).
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,

    /// Whether the checked-out pallet started unborn.
    unborn: bool,

    /// The sparse fetch scope, when this was a sparse (`--only`) franchise (empty otherwise).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    scope: Vec<String>,

    /// Loose objects fetched for the materialized pallet.
    fetched_objects: usize,

    /// Whether the adopted bundle installed native packs straight from the remote's own bulk
    /// ingest — the densify suggestion, human-output only (not part of the JSON contract).
    #[serde(skip)]
    densify_suggested: bool,
}

impl CommandOutput for FranchiseReport {
    fn render_human(&self) {
        if let (Some(objects), Some(signatures)) = (self.bundle_objects, self.bundle_signatures) {
            println!(
                "Imported the remote's bundle: {} object(s) and {} signature(s).",
                objects, signatures
            );
        }

        if self.adopted_anchor {
            println!("Adopted the remote's trust anchor; every parcel is signed from now on.");
        }

        if self.densify_suggested {
            println!("{}", output::DENSIFY_TIP);
        }

        for pallet in &self.meta_adopted {
            println!("Adopted the {} pallet from the remote.", pallet);
        }

        if self.unborn {
            println!(
                "Franchised {} into \"{}\": the remote has nothing stacked on \"{}\" yet; \
                the pallet starts unborn.",
                self.remote, self.directory, self.pallet
            );
        } else {
            println!(
                "Franchised {} into \"{}\": pallet \"{}\" at {} ({} object(s) fetched loose).",
                self.remote, self.directory, self.pallet,
                self.head.as_deref().unwrap_or(""), self.fetched_objects
            );
        }

        if !self.scope.is_empty() {
            println!(
                "Sparse: fetched and materialized only {}. The rest of the tree is sealed by \
                hash — widen with \"expand\".",
                self.scope.join(", ")
            );
        }
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("FranchiseReport", schemars::schema_for!(FranchiseReport)),
    ]
}
