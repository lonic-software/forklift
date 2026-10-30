use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::Arc;
use chrono::{TimeZone, Utc};
use serde::Serialize;
use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
use forklift_core::error::sanitize;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::blob::Blob;
use forklift_core::model::object::loose_object::LooseObject;
use forklift_core::model::operator::Operator;
use forklift_core::model::parcel::Parcel;
use forklift_core::model::parcel_action::ParcelAction;
use forklift_core::model::tree_item::TreeItem;
use forklift_core::parser::object::tree::tree_parser;
use forklift_core::util::file_utils::BulkStoreSession;
use forklift_core::util::pack_utils::{IngestBase, IngestStored, StoreIngest};
use forklift_core::util::{file_utils, inventory_utils, object_utils, office_utils, operation_utils, pallet_utils, shift_utils};
use forklift_core::util::operation_utils::{Operation, OperationKind};
use crate::events;
use crate::output::{self, CommandOutput};

/// Handle the import-git command (§7.8): one-way migration of a git repository's history
/// into this warehouse. Git commits become parcels, trees become trees, blobs become
/// blobs, and each local branch becomes a pallet — so a project (or an agent moving one)
/// can adopt forklift without leaving its history behind.
///
/// The imported history is **unsigned** (it predates any trust): import into a warehouse
/// where trust is not yet established, then `office enroll` to anchor it as the legacy
/// boundary. Run it from the git repository's directory (`forklift import-git .`), so the
/// checked-out working tree already holds the imported HEAD — except a file git converted on
/// checkout (line endings, `ident`, a smudge filter), which reads as an unstaged change, since
/// the history keeps the bytes git stored and forklift converts nothing. The import warns,
/// naming them, about every file that does not hold its imported content.
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
            Import into a fresh warehouse, then \"forklift office enroll\".".to_string()
        );
    }

    // Validate it is a git repository up front (a clear error beats a cryptic one later).
    git(path, &["rev-parse", "--git-dir"])?;

    let branches = branches(path)?;

    if branches.is_empty() {
        return Err(format!("\"{}\" has no local branches to import.", path));
    }

    // Every branch is mapped and checked before anything is converted or written, so a branch
    // that cannot become a pallet refuses the import with the warehouse untouched.
    let pallets = map_branches(&branches)?;

    let progress = events::progress();
    let mut converter = Converter::new(path, !no_compact)?;

    // `--no-compact` writes straight to the loose store, one file per object — exactly the
    // bulk write burst `BulkStoreSession` batches the publish for, instead of fsyncing and
    // renaming every object as it lands (the loose path's measured cost at import scale).
    // Pack-direct mode does not need it: it already batches many objects into few pack files.
    let bulk_session = no_compact.then(BulkStoreSession::open).transpose()?;

    // Convert every commit oldest-first (so each commit's parents are already mapped),
    // pulling in the trees and blobs each one references on the way.
    progress.status("Converting commits");

    let commits = all_commits(path)?;
    for (done, commit) in commits.iter().enumerate() {
        converter.convert_commit(commit)?;
        progress.fraction(done + 1, commits.len(), || format!("{}/{} commits", done + 1, commits.len()));
    }

    progress.status("Publishing objects");

    // Publish the final pack before anything points at (or reads) the imported objects:
    // the pallet heads below must never reference an object that is not yet visible. The
    // loose store's bulk session is the same barrier for `--no-compact` — every staged loose
    // object must be durable and visible before any ref, pallet head, or inventory entry below
    // can point at it.
    let packed = converter.finish_ingest()?;
    if let Some(session) = bulk_session {
        session.finish()?;
    }

    // Each local branch becomes a pallet, unsigned, so each is written only while no anchor
    // stands: an "office enroll" in another bay during the conversion refuses the import rather
    // than leaving heads outside the boundary it recorded.
    let mut imported: Vec<(String, String)> = Vec::new();
    let unsigned = pallet_utils::RefPreconditions {
        anchor: pallet_utils::AnchorPrecondition::Absent,
        ..pallet_utils::RefPreconditions::NONE
    };

    for ((branch, commit), pallet) in branches.iter().zip(pallets) {
        let head = converter.commits.get(commit).cloned().ok_or(format!(
            "Branch \"{}\" points at a commit that was not imported.", branch
        ))?;

        pallet_utils::update_pallet_head(&pallet, None, &head, &unsigned)?;
        imported.push((branch.clone(), pallet));
    }

    // Check out git's HEAD branch (or the first branch) as the current pallet, and build
    // the inventory from it so a colocated working tree reads clean wherever it holds the tree.
    let default_branch = default_branch(path).ok();
    let current = default_branch.as_ref()
        .and_then(|branch| imported.iter().find(|(name, _)| name == branch))
        .or_else(|| imported.first())
        .map(|(_, pallet)| pallet.clone());

    // A colocated git repo keeps its `.git` folder in the working tree; forklift should never
    // track it. The rule goes into the tracked ignore file, so it is added only when there is a
    // `.git` folder here to ignore. It is written before the operation below is raised: heal
    // finishes an interrupted import by building its inventory, and adds no rule. The history is
    // already imported by now, so a failure to add it is reported as a warning naming the rule,
    // not as a failed import.
    let mut warnings = converter.warnings;
    let git_rule = if forklift_core::globals::warehouse_root().join(".git").exists() {
        match ignore_git_directory() {
            Ok(GitRule::Refused(e)) => {
                // Adding the rule by hand would not help: the file stays refused until that
                // line is fixed, and fixed, it may already cover `.git`.
                // The reason goes last: a regex error runs over several lines.
                warnings.push(format!(
                    "the \".git\" folder is not ignored: \".forkliftignore\" is refused, so it was left \
                    unchanged. Fix the problem below, then add the rule {} if no rule covers \".git\".\n{}",
                    GIT_DIRECTORY_PATTERN, e
                ));
                None
            }
            Ok(rule) => Some(rule),
            Err(e) => {
                warnings.push(format!(
                    "the \".git\" folder is not ignored ({}); add the rule {} to \".forkliftignore\" by hand",
                    e, GIT_DIRECTORY_PATTERN
                ));
                None
            }
        }
    } else {
        None
    };

    let mut unheld: Vec<String> = Vec::new();

    if let Some(pallet) = &current {
        let head = pallet_utils::get_pallet_head(pallet)?;
        let tree = head.as_deref().map(|head| object_utils::load_parcel(head).map(|parcel| parcel.tree_hash)).transpose()?;

        // Raised before the position, which comes before the inventory build, and that build can
        // fail on a file it cannot read: until staging is built, nothing may commit it.
        let position = pallet_utils::BayPosition::new(pallet, head.clone());
        let mut operation = Operation::new(OperationKind::ImportGit, position.clone(), tree.clone(), position.clone());
        operation.target_tree = tree.clone();
        operation_utils::raise(&operation)?;

        pallet_utils::write_position(&position)?;

        if let Some(tree) = tree {
            progress.status("Building the inventory");
            let built = shift_utils::build_inventories_for_tree(&tree, shift_utils::TreeProvenance::Verify)?;
            inventory_utils::replace_all_inventories(&built.shards)?;
            unheld = built.unheld;
        }

        operation_utils::finish()?;
    }

    // A tracked ignore file the rule was just appended to no longer holds its imported content,
    // but that is the import's own edit, which the report names, not one for `restore` to undo.
    // An edit the file held before then goes unnamed too; it still reads as changed, and the
    // report already sends the user to load the file.
    if matches!(git_rule, Some(GitRule::Added)) {
        unheld.retain(|key| key != file_utils::FILENAME_IGNORE);
    }
    if let Some(warning) = unheld_warning(&unheld) {
        warnings.push(warning);
    }

    output::emit(&ImportReport {
        commits: converter.commits.len(),
        trees: converter.trees.len(),
        blobs: converter.blobs.len(),
        pallets: imported.iter().map(|(_, pallet)| pallet.clone()).collect(),
        current,
        ignored_git: matches!(git_rule, Some(GitRule::Added)),
        git_already_ignored: match git_rule {
            Some(GitRule::Present { line, pattern }) => Some(ExistingRule { line, pattern }),
            _ => None,
        },
        compacted: packed,
        warnings,
        unheld,
    });

    Ok(())
}

/// How many of the files that do not hold their imported content the warning names.
const UNHELD_NAMED: usize = 5;

/// The warning for the working files that do not hold the content imported for them, which
/// read as unstaged changes from the first stocktake. Forklift converts nothing on checkout, so a
/// file git converted reads as changed although nobody edited it, and saying so here is what keeps
/// that from looking like an unexplained edit. It offers `load` first: a listed file may hold an
/// uncommitted edit, which `restore` would discard.
///
/// # Arguments
/// * `unheld` - The keys of those files, in tree order.
///
/// # Returns
/// * `Some(String)` - The warning, naming the first few files.
/// * `None`         - Every file holds its imported content.
fn unheld_warning(unheld: &[String]) -> Option<String> {
    if unheld.is_empty() {
        return None;
    }

    let mut named = unheld.iter().take(UNHELD_NAMED).map(|key| format!("\"{}\"", key)).collect::<Vec<_>>().join(", ");
    if unheld.len() > UNHELD_NAMED {
        named.push_str(&format!(" and {} more", unheld.len() - UNHELD_NAMED));
    }

    Some(format!(
        "{} file(s) do not hold the content imported for them, so they read as changed: {}. \
        An uncommitted edit or deletion does this, and so does git converting a file as it checks \
        it out (line endings, \"ident\", a filter). \"forklift load\" and stack a path to keep it as it is; \
        for a file you did not edit, \"forklift restore\" writes its imported content back instead, \
        discarding what the file holds.",
        unheld.len(), named
    ))
}

/// The ignore rule for a colocated git repository's metadata folder. It matches `.git`
/// and what lies under it, never a sibling such as `.github/` or `.gitignore`: those are
/// tracked content, and ignoring them makes the next `load .` stage their removal.
const GIT_DIRECTORY_PATTERN: &str = r"^\.git(/.*)?$";

/// What [`ignore_git_directory`] found or did.
enum GitRule {
    /// The rule was appended to the ignore file.
    Added,

    /// A rule already in the file covers `.git`, so nothing was written.
    Present { line: usize, pattern: String },

    /// The file, or its content, is refused as rules, so nothing was written: appending a rule
    /// would leave it refused. Holds the reason.
    Refused(String),
}

/// Add a `.git` ignore pattern to `.forkliftignore` unless a rule there already covers `.git`,
/// so a colocated git repository's metadata folder is not treated as untracked content.
///
/// # Returns
/// * `Ok(GitRule)`  - Whether the rule was added, which line already covers `.git`, or why the
///                    file's content is refused.
/// * `Err(String)`  - The file could not be read or written.
fn ignore_git_directory() -> Result<GitRule, String> {
    let pattern = GIT_DIRECTORY_PATTERN;

    // Only a missing file starts empty: on any other read error, writing would replace the
    // user's rules with this one.
    let existing = match file_utils::read_ignore_file() {
        Ok(existing) => existing.unwrap_or_default(),
        Err(file_utils::IgnoreFileError::Refused(e)) => return Ok(GitRule::Refused(e)),
        Err(file_utils::IgnoreFileError::Unreadable(e)) => return Err(e),
    };

    // Judged as the walks judge it, from the content just read: the rule covering `.git`,
    // however it is spelled, is the one that keeps the walks out of it.
    let rules = match file_utils::ignore_rules_from(Some(&existing)) {
        Ok(rules) => rules,
        Err(e) => return Ok(GitRule::Refused(e)),
    };
    if let Some(rule) = rules.covering_rule(".git") {
        return Ok(GitRule::Present { line: rule.line, pattern: rule.pattern.clone() });
    }

    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str("# git metadata (added by import-git)\n");
    content.push_str(pattern);
    content.push('\n');

    std::fs::write(file_utils::ignore_file_path(), content)
        .map_err(|e| format!("Error while updating the ignore file: {}", e))?;

    Ok(GitRule::Added)
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
        if let Some(hash) = self.trees.get(git_hash).cloned() {
            // Its entries were checked when it was first converted, at whatever depth: the name
            // rule below holds at every depth alike.
            // Already imported (an identical directory elsewhere in history) — nothing to
            // store, but it is now the newest version at *this* path, so a later version
            // here deltas against it.
            if self.ingest.is_some() {
                self.latest_tree_at_path.insert(path_prefix.to_string(), git_hash.to_string());
            }
            return Ok(hash);
        }

        let mut tree = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);

        let (_, bytes) = self.batch.read(git_hash)?;
        // A refused name stops the whole import, so say where it is: the directory, from the root.
        let entries = parse_raw_tree(&bytes, self.oid_len).map_err(|e| format!(
            "{} It is in the directory {:?} of the git history.",
            e, if path_prefix.is_empty() { "/" } else { path_prefix }
        ))?;
        for entry in entries {
            refuse_metadata_alias(git_hash, path_prefix, &entry.name)?;

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
    /// the same path when that saves space, exactly like the bundle builder's blob records.
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
/// Git object text — author/committer names, messages — is a byte
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
///
/// Read as full ref names with the `refs/heads/` prefix taken off here, never as git's short
/// names: git shortens a branch to `heads/<name>` whenever it finds the name ambiguous (a tag of
/// the same name, or on Windows a device name it probes as a file).
fn branches(path: &str) -> Result<Vec<(String, String)>, String> {
    let text = git_str(path, &["for-each-ref", "--format=%(objectname) %(refname)", "refs/heads/"])?;

    text.lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(commit, refname)| Ok((branch_name(refname)?, commit.to_string())))
        .collect()
}

/// Git's HEAD branch name (the default checkout), read like [`branches`].
fn default_branch(path: &str) -> Result<String, String> {
    branch_name(git_str(path, &["symbolic-ref", "HEAD"])?.trim())
}

/// A branch's name from its full ref name.
fn branch_name(refname: &str) -> Result<String, String> {
    refname.strip_prefix("refs/heads/")
        .map(str::to_string)
        .ok_or(format!("Git reported \"{}\", which is not a local branch.", refname))
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

    let after_lt = email_start + 2;
    let email_len = text[after_lt..].find('>').ok_or("A git identity has an unterminated email.".to_string())?;
    let email = text[after_lt..after_lt + email_len].to_string();

    let time = text[after_lt + email_len + 1..]
        .split_whitespace()
        .next()
        .and_then(|value| value.parse::<i64>().ok())
        .ok_or("A git identity has no timestamp.".to_string())?;

    // The email becomes the operator id. Forklift records no names (the chain is pseudonymous),
    // so the git author name is not kept.
    Ok((Operator { identifier: email }, time))
}

/// Refuse `name` as an entry of the git tree `git_hash`, at `path_prefix` from the root, if some
/// file system opens it as `.forklift`, at any depth: at the top, a parcel carrying it could never
/// be checked out; below it, checking it out would plant another working tree, which every command
/// run inside that directory would take for its warehouse. The name rule alone, unlike `stack`,
/// because the tree is git's and no entry of it is on this volume.
fn refuse_metadata_alias(git_hash: &str, path_prefix: &str, name: &str) -> Result<(), String> {
    if office_utils::is_metadata_folder_name(name) {
        return Err(if path_prefix.is_empty() {
            format!(
                "The git tree {} has the top-level entry {:?}, which some file systems open as the \
                warehouse's metadata folder, so it cannot be imported.",
                git_hash, name
            )
        } else {
            format!(
                "The git tree {} at {:?} has the entry {:?}, which some file systems open as a \
                warehouse's metadata folder, so it cannot be imported: checking it out would plant \
                another working tree there.",
                git_hash, path_prefix, name
            )
        });
    }

    Ok(())
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
        // A name becomes a working-directory path on checkout, so unlike display text it is not
        // coerced: a lossy name would check out under a path the source never had, and two names
        // could collapse into one entry. Refuse it, and anything the tree parser would refuse.
        let name = String::from_utf8(bytes[name_start..nul].to_vec()).map_err(|e| format!(
            "A git tree entry name is not UTF-8 ({:?}), so it cannot be imported.",
            String::from_utf8_lossy(e.as_bytes())
        ))?;
        tree_parser::check_entry_name(&name)?;

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

/// Map every branch to its pallet name (see [`sanitize_pallet_name`]), in branch order, and refuse
/// the import naming the branch or branches when a mapped name is not a valid pallet name, when
/// two branches map to one name, or when the name is already a pallet in this warehouse.
fn map_branches(branches: &[(String, String)]) -> Result<Vec<String>, String> {
    let pallets: Vec<String> = branches.iter().map(|(branch, _)| sanitize_pallet_name(branch)).collect();

    for ((branch, _), pallet) in branches.iter().zip(&pallets) {
        if let Err(e) = pallet_utils::validate_pallet_name(pallet) {
            return Err(format!(
                "Branch \"{}\" cannot be imported: it maps to {}. Rename the branch in git and import again.",
                branch, e
            ));
        }
    }

    let mut by_pallet: HashMap<&str, Vec<&str>> = HashMap::new();
    for ((branch, _), pallet) in branches.iter().zip(&pallets) {
        by_pallet.entry(pallet.as_str()).or_default().push(branch.as_str());
    }

    for pallet in &pallets {
        let sharing = &by_pallet[pallet.as_str()];

        if sharing.len() > 1 {
            let names: Vec<String> = sharing.iter().map(|branch| format!("\"{}\"", branch)).collect();
            return Err(format!(
                "Branches {} all map to pallet \"{}\"; rename all but one in git and import again.",
                names.join(", "), pallet
            ));
        }
    }

    // A pallet is a ref file and a "/" nests it in folders, so no name can also be a folder of
    // another: git keeps `Feature` and `feature/x` apart on a case-sensitive disk, but mapped they
    // are `feature` and `feature/x`.
    for ((branch, _), pallet) in branches.iter().zip(&pallets) {
        for folder in folders_of(pallet) {
            if let Some(owners) = by_pallet.get(folder) {
                return Err(format!(
                    "Branches \"{}\" and \"{}\" map to pallets \"{}\" and \"{}\", and a pallet name \
                    cannot also be a folder of another; rename one in git and import again.",
                    owners[0], branch, folder, pallet
                ));
            }
        }
    }

    for ((branch, _), pallet) in branches.iter().zip(&pallets) {
        if pallet_utils::does_pallet_exist(pallet) {
            return Err(format!(
                "Branch \"{}\" maps to pallet \"{}\", which already exists in this warehouse. Import \
                into a warehouse without that pallet, or rename the branch in git.",
                branch, pallet
            ));
        }
    }

    for existing in pallet_utils::list_pallets()? {
        let nested = folders_of(&existing)
            .find_map(|folder| by_pallet.get(folder).map(|owners| (owners[0], folder.to_string())))
            .or_else(|| branches.iter().zip(&pallets).find_map(|((branch, _), pallet)| {
                folders_of(pallet).any(|folder| folder == existing).then(|| (branch.as_str(), pallet.clone()))
            }));

        if let Some((branch, pallet)) = nested {
            return Err(format!(
                "Branch \"{}\" maps to pallet \"{}\", which nests with the pallet \"{}\" this \
                warehouse already has (a pallet name cannot also be a folder of another). Import into a \
                warehouse without that pallet, or rename the branch in git.",
                branch, pallet, existing
            ));
        }
    }

    Ok(pallets)
}

/// Every folder a pallet name nests in, outermost first: `a/b/c` is in `a` and `a/b`.
fn folders_of(pallet: &str) -> impl Iterator<Item = &str> {
    pallet.match_indices('/').map(move |(at, _)| &pallet[..at])
}

/// Turn a git branch name into a pallet name: lowercased, with the characters forklift does not
/// allow replaced (it keeps `/`, so `feature/x` survives). The result may still be refused (a
/// component ending in `.`, or a device name such as `con`); [`map_branches`] checks it.
fn sanitize_pallet_name(branch: &str) -> String {
    let sanitized: String = branch.chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| if c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-' | '/') { c } else { '-' })
        .collect();

    // A component starting with '-' (or an empty result) would be refused; anchor it, in every
    // component alike, so `@wip` and `feature/@wip` map the same way.
    sanitized.split('/')
        .map(|component| if component.is_empty() || component.starts_with('-') {
            format!("branch-{}", component.trim_start_matches('-'))
        } else {
            component.to_string()
        })
        .collect::<Vec<_>>()
        .join("/")
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

    /// The rule already in `.forkliftignore` that covers `.git`, when one did and so none was
    /// added.
    #[serde(skip_serializing_if = "Option::is_none")]
    git_already_ignored: Option<ExistingRule>,

    /// What the pack-direct import wrote (absent with `--no-compact`). The field keeps its
    /// original name: it reports the same fact — the imported store is packed — that the old
    /// post-import compaction pass did.
    #[serde(skip_serializing_if = "Option::is_none")]
    compacted: Option<Packed>,

    /// Anything skipped or worth flagging (e.g. submodules).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    warnings: Vec<String>,

    /// Every checked-out file that does not hold the content imported for it (an uncommitted
    /// edit or deletion, or a file git converted as it checked it out), in tree order. Each reads
    /// as an unstaged change. The warning names only the first few. A tracked `.forkliftignore`
    /// the import appended the `.git` rule to is not listed, although it reads as changed, even
    /// when it also held an edit of its own before the import.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unheld: Vec<String>,
}

/// A rule of the ignore file, by where it stands.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ExistingRule {
    /// The line number, counting from 1.
    line: usize,

    /// The pattern, as written on that line.
    pattern: String,
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

        // A warning may run over several lines (a refused ignore file's reason does); each one
        // after the first stays inside the warning, indented under its text. A warning can name
        // an entry of the imported repository, so each line is sanitized.
        for warning in &self.warnings {
            let mut lines = warning.lines();
            println!("  warning: {}", sanitize(lines.next().unwrap_or_default()));
            for line in lines {
                println!("           {}", sanitize(line));
            }
        }

        if self.ignored_git {
            println!("Added \".git/\" to \".forkliftignore\". Load and stack it to keep the rule in the history.");
        }

        if let Some(rule) = &self.git_already_ignored {
            // The pattern is tracked content, so it may have come from a remote.
            println!(
                "\".git/\" is already ignored by line {} of \".forkliftignore\": {}",
                rule.line, sanitize(&rule.pattern)
            );
        }

        if let Some(packed) = &self.compacted {
            if packed.objects > 0 {
                println!(
                    "Packed the imported store: {} object(s) into {} pack(s), {} delta-compressed.",
                    packed.objects, packed.packs, packed.deltas
                );
                println!("{}", output::DENSIFY_TIP);
            }
        }

        println!(
            "The history is unsigned (imported). Run \"forklift office enroll\" to establish trust — \
            it anchors the imported history as the legacy boundary."
        );
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ImportReport>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One raw git tree entry with a 20-byte object id.
    fn raw_entry(mode: &str, name: &[u8]) -> Vec<u8> {
        let mut bytes = format!("{} ", mode).into_bytes();
        bytes.extend_from_slice(name);
        bytes.push(0);
        bytes.extend_from_slice(&[0xab; 20]);
        bytes
    }

    #[test]
    fn a_git_tree_name_that_is_not_one_path_component_is_refused() {
        for name in [&b".."[..], b".", b"a/b"] {
            let error = parse_raw_tree(&raw_entry("100644", name), 20).err().expect("must be refused");
            assert!(error.contains(&format!("{:?}", String::from_utf8_lossy(name))), "{}", error);
        }
    }

    #[test]
    fn a_git_tree_name_that_is_not_utf8_is_refused_not_coerced() {
        let error = parse_raw_tree(&raw_entry("100644", b"caf\xe9.txt"), 20).err().expect("must be refused");
        assert!(error.contains("not UTF-8"), "{}", error);
    }

    #[test]
    fn an_ordinary_git_tree_name_is_parsed() {
        let entries = parse_raw_tree(&raw_entry("100644", "café.txt".as_bytes()), 20).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "café.txt");
    }
}
