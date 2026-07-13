use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use chrono::{TimeZone, Utc};
use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::operator::Operator;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::util::pack_utils::{IngestBase, IngestStored, StoreIngest};
use forklift_core::util::{inventory_utils, object_utils, office_utils, pallet_utils, shift_utils};
use crate::output::{self, CommandOutput};

/// Handle the import-git command (§7.8): one-way migration of a git repository's history
/// into this warehouse. Git commits become parcels, trees become trees, blobs become
/// blobs, and each local branch becomes a pallet — so a project (or an agent moving one)
/// can adopt forklift without leaving its history behind.
///
/// The imported history is **unsigned** (it predates any trust): import into a warehouse
/// where trust is not yet established, then `office enroll` to anchor it as the legacy
/// boundary. Run it from the git repository's directory (`forklift import-git .`), so the
/// checked-out working tree already matches the imported HEAD.
///
/// A large import lands hundreds of thousands of objects at once, which is exactly the case
/// the loose object store is slowest in — writing (and later compacting away) one file per
/// object is the measured wall of a big import. So import writes native packs directly,
/// delta-compressing successive versions of files and directory trees on the way in — the user
/// gets a dense warehouse without the store ever existing in its loose worst case. `--no-compact` opts
/// out and stores loose objects instead.
///
/// # Arguments
/// * `path`       - The path of the git repository to import from.
/// * `no_compact` - Store loose objects instead of packing on the way in.
///
/// # Returns
/// * `Ok(())`      - If the import completed.
/// * `Err(String)` - If `path` is not a git repository, trust is already established, a
///   pallet would collide, or git could not be read.
pub fn handle_command(path: &str, no_compact: bool) -> Result<(), String> {
    // Import builds each pallet's parcels straight from the git tree (bypassing the overlay
    // entirely, unlike `stack`/`park`) and materializes the whole imported HEAD into the
    // working directory — importing into a scoped bay is not a sensible operation (§7.6).
    // Refuse cleanly, like export-git.
    crate::commands::scope::refuse_in_scoped_bay(
        "import-git",
        "Run import-git from a full workspace (or the main tree), not a scoped bay.",
    )?;

    // Imported parcels are unsigned; a trusted warehouse rejects those, so import must
    // come first — enrolling afterwards records the imported heads as the legacy boundary.
    if office_utils::read_trust_anchor()?.is_some() {
        return Err(
            "Trust is already established here; unsigned git history cannot be imported. \
            Import into a fresh warehouse, then \"office enroll\".".to_string()
        );
    }

    // Validate it is a git repository up front (a clear error beats a cryptic one later).
    git(path, &["rev-parse", "--git-dir"])?;

    let branches = branches(path)?;

    if branches.is_empty() {
        return Err(format!("\"{}\" has no local branches to import.", path));
    }

    let mut converter = Converter::new(path, !no_compact)?;

    // Convert every commit oldest-first (so each commit's parents are already mapped),
    // pulling in the trees and blobs each one references on the way.
    for commit in all_commits(path)? {
        converter.convert_commit(&commit)?;
    }

    // Publish the final pack before anything points at (or reads) the imported objects:
    // the pallet heads below must never reference an object that is not yet visible.
    let packed = converter.finish_ingest()?;

    // Each local branch becomes a pallet.
    let mut imported: Vec<(String, String)> = Vec::new();

    for (branch, commit) in &branches {
        let pallet = sanitize_pallet_name(branch);

        if pallet_utils::does_pallet_exist(&pallet) {
            return Err(format!(
                "Pallet \"{}\" already exists; import into a fresh warehouse.", pallet
            ));
        }

        let head = converter.commits.get(commit).cloned().ok_or(format!(
            "Branch \"{}\" points at a commit that was not imported.", branch
        ))?;

        pallet_utils::set_pallet_head(&pallet, &head)?;
        imported.push((branch.clone(), pallet));
    }

    // Check out git's HEAD branch (or the first branch) as the current pallet, and build
    // the inventory from it so a colocated working tree reads clean.
    let default_branch = default_branch(path).ok();
    let current = default_branch.as_ref()
        .and_then(|branch| imported.iter().find(|(name, _)| name == branch))
        .or_else(|| imported.first())
        .map(|(_, pallet)| pallet.clone());

    if let Some(pallet) = &current {
        pallet_utils::set_current_pallet_name(pallet)?;

        if let Some(head) = pallet_utils::get_pallet_head(pallet)? {
            let tree = object_utils::load_parcel(&head)?.tree_hash;
            let shards = shift_utils::build_inventories_for_tree(&tree)?;
            inventory_utils::replace_all_inventories(&shards)?;
        }
    }

    // A colocated git repo keeps its `.git` folder in the working tree; forklift should
    // never track it. Adding the pattern is harmless when the repo lives elsewhere.
    let ignored_git = ignore_git_directory()?;

    output::emit("import-git", &ImportReport {
        commits: converter.commits.len(),
        trees: converter.trees.len(),
        blobs: converter.blobs.len(),
        pallets: imported.iter().map(|(_, pallet)| pallet.clone()).collect(),
        current,
        ignored_git,
        compacted: packed,
        warnings: converter.warnings,
    });

    Ok(())
}

/// Add a `.git` ignore pattern to `.forkliftignore` if it is not already there, so a
/// colocated git repository's metadata folder is not treated as untracked content.
fn ignore_git_directory() -> Result<bool, String> {
    let path = forklift_core::globals::warehouse_root().join(".forkliftignore");
    let pattern = r"^\.git\/?.*$";

    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    if existing.lines().any(|line| line.trim() == pattern) {
        return Ok(false);
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("# git metadata (added by import-git)\n");
    content.push_str(pattern);
    content.push('\n');

    std::fs::write(&path, content)
        .map_err(|e| format!("Error while updating the ignore file: {}", e))?;

    Ok(true)
}

/// A long-lived `git cat-file --batch` process. Every commit, tree and blob is read through
/// this one streaming pipe instead of a fresh `git` fork+exec per object — the dominant cost
/// of importing a large history (git.git is ~80k commits and several hundred thousand
/// objects, i.e. that many process spawns the old path paid one at a time).
struct GitBatch {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl GitBatch {
    fn new(path: &str) -> Result<GitBatch, String> {
        let mut child = Command::new("git")
            .arg("-C").arg(path)
            .args(["cat-file", "--batch"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("Could not run git (is it installed and on your PATH?): {}", e))?;

        let stdin = child.stdin.take().ok_or("git cat-file: no stdin pipe".to_string())?;
        let stdout = BufReader::new(child.stdout.take().ok_or("git cat-file: no stdout pipe".to_string())?);

        Ok(GitBatch { child, stdin, stdout })
    }

    /// Fetch one object by hash: its git type (`commit`/`tree`/`blob`) and its raw bytes.
    /// The batch protocol answers each request line with a `<sha> <type> <size>\n` header,
    /// then exactly `<size>` content bytes, then a trailing newline.
    fn read(&mut self, hash: &str) -> Result<(String, Vec<u8>), String> {
        self.stdin.write_all(hash.as_bytes())
            .and_then(|_| self.stdin.write_all(b"\n"))
            .and_then(|_| self.stdin.flush())
            .map_err(|e| format!("git cat-file: could not request {}: {}", hash, e))?;

        let mut header = String::new();
        self.stdout.read_line(&mut header)
            .map_err(|e| format!("git cat-file: could not read the header for {}: {}", hash, e))?;
        let header = header.trim_end();

        let mut parts = header.split(' ');
        let _sha = parts.next();
        let kind = parts.next()
            .ok_or_else(|| format!("git cat-file: malformed header \"{}\"", header))?;
        if kind == "missing" {
            return Err(format!("git cat-file: object {} is missing", hash));
        }
        let size: usize = parts.next()
            .and_then(|value| value.parse().ok())
            .ok_or_else(|| format!("git cat-file: no size in header \"{}\"", header))?;

        let mut content = vec![0u8; size];
        self.stdout.read_exact(&mut content)
            .map_err(|e| format!("git cat-file: short read for {}: {}", hash, e))?;

        let mut terminator = [0u8; 1];
        self.stdout.read_exact(&mut terminator)
            .map_err(|e| format!("git cat-file: missing record terminator for {}: {}", hash, e))?;

        Ok((kind.to_string(), content))
    }
}

impl Drop for GitBatch {
    fn drop(&mut self) {
        // The helper is disposable. stdin is still open here, so waiting on the child would
        // block it on EOF — kill it, then reap so we never leave a zombie behind.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Converts git objects to forklift objects, memoizing by git hash so every object is read
/// (and stored) once, even when branches share history.
struct Converter {
    batch: GitBatch,
    oid_len: usize,
    blobs: HashMap<String, String>,
    trees: HashMap<String, String>,
    commits: HashMap<String, String>,
    warnings: Vec<String>,

    /// The pack-direct sink (the default), or `None` for `--no-compact`'s loose store.
    ingest: Option<StoreIngest>,

    /// The newest blob seen at each path, by *git* hash — the delta base for the next version
    /// there, re-readable from the batch pipe when its bytes have left the cache.
    latest_blob_at_path: HashMap<String, String>,

    /// The newest tree seen at each directory path, by *git* hash — the delta base for the
    /// next version there, re-derivable (via `tree_base_for`) when its bytes have left the
    /// cache. A directory usually changes one entry per commit, so successive versions delta
    /// extremely well — this is what `compact`'s size-window fallback catches for the loose
    /// path, done exactly here.
    latest_tree_at_path: HashMap<String, String>,

    /// Each stored blob's delta-chain depth (by forklift hash), so chains stay bounded.
    stored_depth: HashMap<String, u32>,

    /// Recently built object bytes (blobs by git hash, trees by `tree_cache_key` of their
    /// forklift hash — the two namespaces share this map but never collide), so the common
    /// delta case never re-reads or rebuilds its base. Bounded; a miss falls back to the pipe
    /// (blobs) or re-derivation (trees), never to a fatter store.
    base_cache: HashMap<String, Arc<Vec<u8>>>,
    base_cache_bytes: usize,
    base_cache_budget: usize,

    /// Blobs stored as deltas (pack-direct mode only).
    deltas: usize,
}

/// The base-bytes cache budget's default. Delta bases are usually the version converted
/// moments ago, so a modest bound hits nearly always; whole-map eviction on overflow is crude
/// but keeps the import's memory flat on repos with huge blobs. Overridable per-run via
/// `FORKLIFT_IMPORT_CACHE_BYTES` (tests force zero, to exercise the re-read/re-derive fallback
/// on every base; not a user-facing knob).
const BASE_CACHE_BYTES: usize = 256 * 1024 * 1024;

/// The cache budget for this run: `FORKLIFT_IMPORT_CACHE_BYTES` if set and parseable, else the
/// default. Read once at converter construction, not on every lookup.
fn cache_budget() -> usize {
    std::env::var("FORKLIFT_IMPORT_CACHE_BYTES").ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(BASE_CACHE_BYTES)
}

impl Converter {
    fn new(path: &str, pack_direct: bool) -> Result<Converter, String> {
        // Tree objects embed each child's hash as raw bytes, so we need the repo's hash
        // width (20 for sha1, git's default; 32 for a sha256 repo) to parse them.
        let oid_len = match git_str(path, &["rev-parse", "--show-object-format"])?.trim() {
            "sha256" => 32,
            _ => 20,
        };

        Ok(Converter {
            batch: GitBatch::new(path)?,
            oid_len,
            blobs: HashMap::new(),
            trees: HashMap::new(),
            commits: HashMap::new(),
            warnings: Vec::new(),
            ingest: pack_direct.then(StoreIngest::new).transpose()?,
            latest_blob_at_path: HashMap::new(),
            latest_tree_at_path: HashMap::new(),
            stored_depth: HashMap::new(),
            base_cache: HashMap::new(),
            base_cache_bytes: 0,
            base_cache_budget: cache_budget(),
            deltas: 0,
        })
    }

    /// Publish the ingested packs (pack-direct mode) and report what was written. Loose mode
    /// (`--no-compact`) reports nothing — its objects are already individually visible.
    fn finish_ingest(&mut self) -> Result<Option<Packed>, String> {
        let Some(ingest) = self.ingest.take() else {
            return Ok(None);
        };
        let stats = ingest.finish()?;
        Ok(Some(Packed { objects: stats.objects, packs: stats.packs, deltas: stats.deltas }))
    }

    /// Store one non-blob object through the active sink.
    fn store_object(&mut self, object: &mut LooseObject) -> Result<(), String> {
        match &mut self.ingest {
            Some(ingest) => ingest.store(object).map(|_| ()),
            None => object.store().map(|_| ()),
        }
    }

    /// Convert one git commit (its parents must already be converted).
    fn convert_commit(&mut self, git_hash: &str) -> Result<String, String> {
        if let Some(hash) = self.commits.get(git_hash) {
            return Ok(hash.clone());
        }

        let (_, bytes) = self.batch.read(git_hash)?;
        let commit = parse_commit(&String::from_utf8_lossy(&bytes))?;

        let mut parents: Vec<String> = Vec::new();
        for parent in &commit.parents {
            parents.push(self.convert_commit(parent)?);
        }

        let tree_hash = self.convert_tree(&commit.tree, "")?;

        // Git's author/committer split maps onto forklift's Author/Stack actions exactly.
        let parcel = Parcel {
            tree_hash,
            parents,
            actions: vec![
                ParcelAction {
                    operator: commit.author,
                    action: ParcelActionType::Author,
                    description: None,
                    timestamp: Utc.timestamp_opt(commit.author_time, 0).single().unwrap_or_else(Utc::now),
                },
                ParcelAction {
                    operator: commit.committer,
                    action: ParcelActionType::Stack,
                    description: None,
                    timestamp: Utc.timestamp_opt(commit.committer_time, 0).single().unwrap_or_else(Utc::now),
                },
            ],
            description: (!commit.message.is_empty()).then_some(commit.message),
        };

        let mut object = LooseObjectBuilder::build_parcel(&parcel);
        self.store_object(&mut object)?;

        self.commits.insert(git_hash.to_string(), object.hash.clone());

        Ok(object.hash)
    }

    /// Convert one git tree recursively. `path_prefix` is the tree's own path from the root
    /// (empty at the root); it keys the per-path delta chains below. Memoization skips whole
    /// unchanged subtrees, so — like the bundle builder's closure walk — only changed paths
    /// are revisited, and each blob is considered exactly where history changed it.
    fn convert_tree(&mut self, git_hash: &str, path_prefix: &str) -> Result<String, String> {
        if let Some(hash) = self.trees.get(git_hash) {
            // Already imported (an identical directory elsewhere in history) — nothing to
            // store, but it is now the newest version at *this* path, so a later version
            // here deltas against it.
            let hash = hash.clone();
            if self.ingest.is_some() {
                self.latest_tree_at_path.insert(path_prefix.to_string(), git_hash.to_string());
            }
            return Ok(hash);
        }

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

        let (_, bytes) = self.batch.read(git_hash)?;
        for entry in parse_raw_tree(&bytes, self.oid_len)? {
            let path = join_path(path_prefix, &entry.name);
            let child = match entry.mode.as_str() {
                "40000" | "040000" => {
                    let hash = self.convert_tree(&entry.hash, &path)?;
                    TreeItem::new(entry.name, hash, DirEntryType::Tree)
                }
                "160000" => {
                    // A submodule (gitlink) has no content to import — record it and skip.
                    self.warnings.push(format!("skipped submodule \"{}\" (git gitlinks are not imported)", entry.name));
                    continue;
                }
                mode => {
                    let item_type = match mode {
                        "100755" => DirEntryType::Executable,
                        "120000" => DirEntryType::SymbolicLink,
                        _ => DirEntryType::Normal,
                    };
                    let hash = self.convert_blob(&entry.hash, &path)?;
                    TreeItem::new(entry.name, hash, item_type)
                }
            };

            tree.add_child(child);
        }

        let mut object = LooseObjectBuilder::build_tree(&tree);
        self.store_tree(&mut object, path_prefix, git_hash)?;

        self.trees.insert(git_hash.to_string(), object.hash.clone());

        Ok(object.hash)
    }

    /// Store one built tree — in pack-direct mode as a delta against the previous version of
    /// the same directory when that saves space. `git_hash` is this tree's own git hash, so a
    /// later version at `path` can find it via `latest_tree_at_path` (and re-derive it on a
    /// cache miss — an ingested tree is not otherwise re-readable before publication).
    fn store_tree(&mut self, object: &mut LooseObject, path: &str, git_hash: &str) -> Result<(), String> {
        if self.ingest.is_none() {
            object.store()?;
            return Ok(());
        }

        let base = self.tree_base_for(path)?;
        let ingest = self.ingest.as_mut().expect("pack-direct mode was just checked");
        let stored = ingest.store_with_base(object, base.as_ref().map(|(hash, bytes, depth)| {
            IngestBase { hash, bytes, depth: *depth }
        }))?;

        self.record_depth(&object.hash.clone(), &stored);
        self.cache_base(&tree_cache_key(&object.hash), Arc::new(std::mem::take(&mut object.content)));
        self.latest_tree_at_path.insert(path.to_string(), git_hash.to_string());

        Ok(())
    }

    /// Record a just-stored object's delta-chain depth. An object the store *already held* has
    /// an unknown record shape — it may itself be a delta of any depth — so it is recorded as
    /// maxed-out: no later version may extend a chain whose true length nobody here knows
    /// (the read side enforces a hard reconstruction bound, so overshooting it would fail
    /// reads, not just density).
    fn record_depth(&mut self, hash: &str, stored: &IngestStored) {
        let depth = match stored {
            IngestStored::Delta { depth } => {
                self.deltas += 1;
                *depth
            }
            IngestStored::Full => 0,
            IngestStored::AlreadyPresent => u32::MAX,
        };
        self.stored_depth.insert(hash.to_string(), depth);
    }

    /// The delta base for the next version of the directory at `path`: the newest tree seen
    /// there, as `(forklift hash, object bytes, chain depth)`. Bytes come from the bounded
    /// cache, or — after an eviction — are rebuilt from the git tree via `rederive_tree_base`,
    /// so density does not depend on the cache budget (mirroring `delta_base_for` for blobs,
    /// which re-reads its base through the batch pipe instead).
    #[allow(clippy::type_complexity)]
    fn tree_base_for(&mut self, path: &str) -> Result<Option<(String, Arc<Vec<u8>>, u32)>, String> {
        let Some(base_git) = self.latest_tree_at_path.get(path).cloned() else {
            return Ok(None);
        };
        let Some(base_hash) = self.trees.get(&base_git).cloned() else {
            return Ok(None);
        };

        let bytes = match self.base_cache.get(&tree_cache_key(&base_hash)) {
            Some(bytes) => Arc::clone(bytes),
            None => match self.rederive_tree_base(&base_git, &base_hash)? {
                Some(bytes) => bytes,
                // Rebuilding failed or mismatched (see `rederive_tree_base`) — no base rather
                // than a delta the reader could not verify; the object stores full instead.
                None => return Ok(None),
            },
        };

        // Unknown depth (a base this run never chained) is maxed-out, never zero: extending a
        // chain of unknown length could overshoot the read-side reconstruction bound.
        let depth = self.stored_depth.get(&base_hash).copied().unwrap_or(u32::MAX);

        Ok(Some((base_hash, bytes, depth)))
    }

    /// Rebuild a tree's object bytes from its git hash, for when the cache has evicted them.
    /// Every child hash the raw git tree names must already be memoized (it was converted
    /// before it could become anyone's "latest at this path"), so the rebuild is just a
    /// replay of `convert_tree`'s own mapping — deterministic, but the delta format trusts the
    /// base's bytes completely, so verify: compare the rebuilt hash against `expect_fork_hash`
    /// (what was actually stored) before handing back bytes a delta could be built against.
    /// A mismatch, or a child hash this run never converted, is fail-safe: no base, never a
    /// corrupt one.
    fn rederive_tree_base(&mut self, git_hash: &str, expect_fork_hash: &str) -> Result<Option<Arc<Vec<u8>>>, String> {
        let (_, bytes) = self.batch.read(git_hash)?;
        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

        for entry in parse_raw_tree(&bytes, self.oid_len)? {
            let child = match entry.mode.as_str() {
                "40000" | "040000" => {
                    let Some(hash) = self.trees.get(&entry.hash).cloned() else {
                        return Ok(None);
                    };
                    TreeItem::new(entry.name, hash, DirEntryType::Tree)
                }
                // A gitlink was skipped (without storing anything) when this tree was first
                // converted, and warned about then — do not warn about it again here.
                "160000" => continue,
                mode => {
                    let item_type = match mode {
                        "100755" => DirEntryType::Executable,
                        "120000" => DirEntryType::SymbolicLink,
                        _ => DirEntryType::Normal,
                    };
                    let Some(hash) = self.blobs.get(&entry.hash).cloned() else {
                        return Ok(None);
                    };
                    TreeItem::new(entry.name, hash, item_type)
                }
            };

            tree.add_child(child);
        }

        let mut object = LooseObjectBuilder::build_tree(&tree);
        if object.hash != expect_fork_hash {
            return Ok(None);
        }

        let bytes = Arc::new(std::mem::take(&mut object.content));
        self.cache_base(&tree_cache_key(expect_fork_hash), Arc::clone(&bytes));

        Ok(Some(bytes))
    }

    /// Convert one git blob — in pack-direct mode as a delta against the previous version at
    /// the same path when that saves space, exactly like the bundle builder's `emit_blob`.
    fn convert_blob(&mut self, git_hash: &str, path: &str) -> Result<String, String> {
        if let Some(hash) = self.blobs.get(git_hash) {
            // Already imported (the same content elsewhere in history) — nothing to store,
            // but it is now the newest version at *this* path, so a later version here
            // deltas against it.
            let hash = hash.clone();
            if self.ingest.is_some() {
                self.latest_blob_at_path.insert(path.to_string(), git_hash.to_string());
            }
            return Ok(hash);
        }

        let (_, content) = self.batch.read(git_hash)?;
        let blob = Blob { content };
        let mut object = LooseObjectBuilder::build_blob(&blob);

        let stored = if self.ingest.is_some() {
            let base = self.delta_base_for(path)?;
            let ingest = self.ingest.as_mut().expect("pack-direct mode was just checked");
            ingest.store_with_base(&object, base.as_ref().map(|(hash, bytes, depth)| {
                IngestBase { hash, bytes, depth: *depth }
            }))?
        } else {
            object.store()?;
            IngestStored::Full
        };

        if self.ingest.is_some() {
            self.record_depth(&object.hash.clone(), &stored);
            self.cache_base(git_hash, Arc::new(std::mem::take(&mut object.content)));
            self.latest_blob_at_path.insert(path.to_string(), git_hash.to_string());
        }

        self.blobs.insert(git_hash.to_string(), object.hash.clone());

        Ok(object.hash)
    }

    /// The delta base for the next version at `path`: the newest blob seen there, as
    /// `(forklift hash, object bytes, chain depth)`. Bytes come from the bounded cache, or —
    /// after an eviction — are re-read through the already-open batch pipe and rebuilt, so
    /// density never depends on the cache budget.
    #[allow(clippy::type_complexity)]
    fn delta_base_for(&mut self, path: &str) -> Result<Option<(String, Arc<Vec<u8>>, u32)>, String> {
        let Some(base_git) = self.latest_blob_at_path.get(path).cloned() else {
            return Ok(None);
        };
        let Some(base_hash) = self.blobs.get(&base_git).cloned() else {
            return Ok(None);
        };

        let bytes = match self.base_cache.get(&base_git) {
            Some(bytes) => Arc::clone(bytes),
            None => {
                let (_, content) = self.batch.read(&base_git)?;
                let bytes = Arc::new(LooseObjectBuilder::build_blob(&Blob { content }).content);
                self.cache_base(&base_git, Arc::clone(&bytes));
                bytes
            }
        };

        // Unknown depth is maxed-out, never zero (see `tree_base_for`).
        let depth = self.stored_depth.get(&base_hash).copied().unwrap_or(u32::MAX);

        Ok(Some((base_hash, bytes, depth)))
    }

    /// Remember one object's bytes as a potential delta base, keyed by git hash for blobs or
    /// by `tree_cache_key` for trees (the two namespaces share this map but never collide).
    /// Overflow clears the whole cache (the `IncomingVerificationCache` pattern): crude, but
    /// the flat memory bound matters more than the rare re-read/re-derive a clear causes.
    fn cache_base(&mut self, key: &str, bytes: Arc<Vec<u8>>) {
        // A real object's serialized bytes always carry a header, so this cannot fire in
        // practice — but it keeps the budget check below airtight even at a budget of zero.
        if bytes.is_empty() {
            return;
        }
        if bytes.len() > self.base_cache_budget {
            return;
        }
        if self.base_cache_bytes.saturating_add(bytes.len()) > self.base_cache_budget {
            self.base_cache.clear();
            self.base_cache_bytes = 0;
        }
        self.base_cache_bytes += bytes.len();
        // Two different git trees can converge to the same forklift tree (e.g. differing only
        // by a skipped submodule), re-inserting the same key — without subtracting the
        // replaced entry's size, the byte count would drift upward and force premature clears.
        if let Some(replaced) = self.base_cache.insert(key.to_string(), bytes) {
            self.base_cache_bytes = self.base_cache_bytes.saturating_sub(replaced.len());
        }
    }
}

/// The base-cache key for a tree's bytes. Blobs key the shared cache by *git* hash; prefixing
/// tree entries (keyed by forklift hash) keeps the two namespaces from ever colliding.
fn tree_cache_key(fork_hash: &str) -> String {
    format!("t/{}", fork_hash)
}

/// Join a tree path prefix and a child name (the root prefix is empty).
fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{}/{}", prefix, name)
    }
}

/// A parsed git commit.
struct GitCommit {
    tree: String,
    parents: Vec<String>,
    author: Operator,
    author_time: i64,
    committer: Operator,
    committer_time: i64,
    message: String,
}

/// One git tree entry.
struct GitTreeEntry {
    mode: String,
    hash: String,
    name: String,
}

/// Run `git -C <path> <args>` and return its stdout bytes.
fn git(path: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let output = Command::new("git")
        .arg("-C").arg(path)
        .args(args)
        .output()
        .map_err(|e| format!("Could not run git (is it installed and on your PATH?): {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    Ok(output.stdout)
}

/// Run a git command and return its stdout as a string, coercing lossily.
///
/// Git object text — author/committer names, messages, tree entry names — is a byte
/// string with no UTF-8 guarantee: git.git and the Linux kernel both carry commits whose
/// author names are Latin-1 (e.g. "David_Kågedal", byte 0xE5), which strict decoding would
/// reject, aborting the whole import. Forklift's model already stores these fields as UTF-8
/// strings, so coerce lossily (invalid bytes become U+FFFD) rather than failing. The stable
/// identifier — the author's email — is ASCII and survives intact; only non-authoritative
/// display bytes are approximated. Hashes and refnames (the other callers) are ASCII, so
/// lossy decoding is a no-op for them. (Blob *content* never goes through here — it is read
/// as raw bytes by `git()`, so binary files import byte-exact.)
fn git_str(path: &str, args: &[&str]) -> Result<String, String> {
    Ok(String::from_utf8_lossy(&git(path, args)?).into_owned())
}

/// The local branches, as `(branch name, commit hash)`.
fn branches(path: &str) -> Result<Vec<(String, String)>, String> {
    let text = git_str(path, &["for-each-ref", "--format=%(objectname) %(refname:short)", "refs/heads/"])?;

    Ok(text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(commit, branch)| (branch.to_string(), commit.to_string()))
        .collect())
}

/// Git's HEAD branch name (the default checkout).
fn default_branch(path: &str) -> Result<String, String> {
    Ok(git_str(path, &["symbolic-ref", "--short", "HEAD"])?.trim().to_string())
}

/// Every commit reachable from the local branches, oldest first (so parents precede
/// children — the order the converter relies on).
fn all_commits(path: &str) -> Result<Vec<String>, String> {
    let text = git_str(path, &["rev-list", "--topo-order", "--reverse", "--branches"])?;

    Ok(text.lines().map(|line| line.trim().to_string()).filter(|line| !line.is_empty()).collect())
}

/// Parse the output of `git cat-file commit <hash>`.
fn parse_commit(text: &str) -> Result<GitCommit, String> {
    let (header, message) = text.split_once("\n\n").unwrap_or((text, ""));

    let mut tree = String::new();
    let mut parents: Vec<String> = Vec::new();
    let mut author: Option<(Operator, i64)> = None;
    let mut committer: Option<(Operator, i64)> = None;

    for line in header.lines() {
        if let Some(rest) = line.strip_prefix("tree ") {
            tree = rest.trim().to_string();
        } else if let Some(rest) = line.strip_prefix("parent ") {
            parents.push(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("author ") {
            author = Some(parse_identity(rest)?);
        } else if let Some(rest) = line.strip_prefix("committer ") {
            committer = Some(parse_identity(rest)?);
        }
        // Other headers (gpgsig and its continuation lines, encoding, …) are ignored.
    }

    if tree.is_empty() {
        return Err("A git commit has no tree.".to_string());
    }

    let (author, author_time) = author.ok_or("A git commit has no author.".to_string())?;
    let (committer, committer_time) = committer.unwrap_or_else(|| (author.clone(), author_time));

    Ok(GitCommit {
        tree,
        parents,
        author,
        author_time,
        committer,
        committer_time,
        message: message.trim_end_matches('\n').to_string(),
    })
}

/// Parse a git identity line: `Name <email> <unix-seconds> <tz-offset>`.
fn parse_identity(text: &str) -> Result<(Operator, i64), String> {
    let email_start = text.find(" <").ok_or("A git identity has no email.".to_string())?;
    let name = text[..email_start].trim().to_string();

    let after_lt = email_start + 2;
    let email_len = text[after_lt..].find('>').ok_or("A git identity has an unterminated email.".to_string())?;
    let email = text[after_lt..after_lt + email_len].to_string();

    let time = text[after_lt + email_len + 1..]
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or("A git identity has no timestamp.".to_string())?;

    // The email is the stable, pseudonymous id; the name is local display data.
    Ok((Operator { name, identifier: email }, time))
}

/// Parse a raw git tree object. `git cat-file --batch` hands back the object's own bytes
/// (not the text `ls-tree` prints): a packed sequence of `<octal-mode> SP <name> NUL
/// <oid-bytes>`, where the child hash is raw binary (`oid_len` bytes) and must be hex-encoded
/// to match the string form the rest of the importer keys on.
fn parse_raw_tree(bytes: &[u8], oid_len: usize) -> Result<Vec<GitTreeEntry>, String> {
    let mut entries = Vec::new();
    let mut i = 0;

    while i < bytes.len() {
        let space = i + bytes[i..].iter().position(|&b| b == b' ')
            .ok_or("A git tree entry has no mode.".to_string())?;
        let mode = String::from_utf8_lossy(&bytes[i..space]).into_owned();

        let name_start = space + 1;
        let nul = name_start + bytes[name_start..].iter().position(|&b| b == 0)
            .ok_or("A git tree entry has an unterminated name.".to_string())?;
        let name = String::from_utf8_lossy(&bytes[name_start..nul]).into_owned();

        let oid_start = nul + 1;
        let oid_end = oid_start + oid_len;
        if oid_end > bytes.len() {
            return Err("A git tree entry has a truncated hash.".to_string());
        }
        let hash = hex_encode(&bytes[oid_start..oid_end]);

        entries.push(GitTreeEntry { mode, hash, name });
        i = oid_end;
    }

    Ok(entries)
}

/// Lowercase hex, matching git's textual object-id form.
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0f) as usize] as char);
    }
    out
}

/// Turn a git branch name into a valid forklift pallet name, replacing characters forklift
/// does not allow (it keeps `/`, so `feature/x` survives).
fn sanitize_pallet_name(branch: &str) -> String {
    let sanitized: String = branch.chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/') { c } else { '-' })
        .collect();

    // A leading '-' (or an empty result) would be rejected; anchor it.
    if sanitized.is_empty() || sanitized.starts_with('-') {
        format!("branch-{}", sanitized.trim_start_matches('-'))
    } else {
        sanitized
    }
}

/// The result of a git import.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ImportReport {
    commits: usize,
    trees: usize,
    blobs: usize,

    /// The pallets created (one per local branch).
    pallets: Vec<String>,

    /// The pallet checked out (git's HEAD branch).
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,

    /// Whether a `.git` ignore pattern was added to `.forkliftignore`.
    ignored_git: bool,

    /// What the pack-direct import wrote (absent with `--no-compact`). The field keeps its
    /// original name: it reports the same fact — the imported store is packed — that the old
    /// post-import compaction pass did.
    #[serde(skip_serializing_if = "Option::is_none")]
    compacted: Option<Packed>,

    /// Anything skipped or worth flagging (e.g. submodules).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,
}

/// The packed-store summary (a slim view of `pack_utils::IngestStats`).
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct Packed {
    /// Objects written into packs.
    objects: usize,

    /// Packs written.
    packs: usize,

    /// Of the objects, blobs delta-compressed against the previous version at their path.
    deltas: usize,
}

impl CommandOutput for ImportReport {
    fn render_human(&self) {
        println!(
            "Imported {} commit(s) into {} pallet(s) ({} tree(s), {} blob(s)).",
            self.commits, self.pallets.len(), self.trees, self.blobs
        );

        if let Some(current) = &self.current {
            println!("Checked out \"{}\".", current);
        }

        for warning in &self.warnings {
            println!("  warning: {}", warning);
        }

        if self.ignored_git {
            println!("Added \".git/\" to \".forkliftignore\".");
        }

        if let Some(packed) = &self.compacted {
            if packed.objects > 0 {
                println!(
                    "Packed the imported store: {} object(s) into {} pack(s), {} delta-compressed.",
                    packed.objects, packed.packs, packed.deltas
                );
            }
        }

        println!(
            "The history is unsigned (imported). Run \"office enroll\" to establish trust — \
            it anchors the imported history as the legacy boundary."
        );
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<(&'static str, schemars::Schema)> {
    vec![
        ("ImportReport", schemars::schema_for!(ImportReport)),
    ]
}
