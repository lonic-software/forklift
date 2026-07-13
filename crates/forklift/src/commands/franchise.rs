use std::path::Path;
use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::remote_utils::RemoteClient;
use forklift_core::util::scope_utils::MaterializationScope;
use forklift_core::util::{
    bundle_utils, config_utils, object_utils, pallet_utils, remote_utils, scope_utils,
    shift_utils, warehouse_utils,
};
use crate::output::{self, CommandOutput};

/// Handle the franchise command (git's "clone"): open a local franchise of a remote
/// warehouse — prepare a fresh warehouse in the target directory, remember the remote,
/// adopt its trust anchor, download the history (the remote's bundle first, when it has
/// one, then whatever the bundle lacked as loose objects) and materialize the chosen
/// pallet's head in the working directory.
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
/// * `Ok(())`      - If the franchise is ready to work in.
/// * `Err(String)` - If the directory is not empty, the remote is unreachable, or a
///                   transfer failed.
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
    } else {
        std::fs::create_dir_all(target)
            .map_err(|e| format!("Error while creating \"{}\": {}", directory, e))?;
    }

    // Everything from here on happens inside the new warehouse.
    std::env::set_current_dir(target)
        .map_err(|e| format!("Error while switching to \"{}\": {}", directory, e))?;

    warehouse_utils::prepare_warehouse()?;

    config_utils::set_value(config_utils::KEY_REMOTE_URL, url, ConfigScope::Warehouse)?;

    if let Some(token) = &token {
        config_utils::set_value(config_utils::KEY_REMOTE_TOKEN, token, ConfigScope::Warehouse)?;
    }

    let client = RemoteClient::new(url, token)?;
    let info = client.fetch_info().await?;

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

    let trust = remote_utils::adopt_remote_trust(&client, &info).await?;
    report.adopted_anchor = trust.adopted_anchor;

    // Adopt the meta pallets too (the manifest, …), so a clone carries the post-metadata,
    // not just the working history. A fresh clone has none locally, so nothing diverges.
    report.meta_adopted = remote_utils::adopt_meta_pallets(&client, &info).await?.adopted;

    let explicitly_requested = pallet.is_some();

    let pallet_name = match pallet {
        Some(name) => name,
        None => info.default_pallet.clone(),
    };

    let Some(remote_head) = info.pallets.get(&pallet_name) else {
        // A pallet absent from the handshake is unborn on the remote. The remote's own
        // default pallet legitimately starts unborn (a fresh remote, or one whose
        // current pallet has nothing stacked while others do); a pallet the user asked
        // for by name gets typo protection instead, unless nothing is stacked at all.
        // Meta pallets (`@office`, …) are not working pallets — a remote with only those
        // is still fresh, and they never appear in the "it has: …" hint.
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

    let stats = remote_utils::fetch_history_scoped(&client, remote_head, &fetch_scope).await?;

    let tree_hash = object_utils::load_parcel(remote_head)?.tree_hash;

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

    pallet_utils::set_pallet_head(&pallet_name, remote_head)?;
    pallet_utils::set_current_pallet_name(&pallet_name)?;

    shift_utils::materialize_tree(None, &tree_hash, "Franchising")?;

    report.pallet = pallet_name;
    report.head = Some(remote_head.clone());
    report.fetched_objects = stats.fetched_objects;
    output::emit("franchise", &report);

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
#[derive(Serialize)]
struct FranchiseReport {
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
