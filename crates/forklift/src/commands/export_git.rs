use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use serde::Serialize;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::util::{object_utils, pallet_utils};
use crate::output::{self, CommandOutput};

/// Handle the export-git command (§7.8): one-way export of this warehouse's history into a
/// new git repository — the escape hatch that makes trying forklift reversible. Parcels
/// become commits, trees become trees, blobs become blobs, and each *user* pallet becomes
/// a branch.
///
/// It is deliberately lossy in this direction: git has no home for forklift's signed
/// office, the `@manifest` (approvals / provenance) or per-parcel signatures, so those are
/// dropped — the guarantee is "your code history is never trapped", not a lossless round
/// trip. Requires `git` on PATH.
///
/// # Arguments
/// * `path` - Where to create the git repository (must be empty or nonexistent).
///
/// # Returns
/// * `Ok(())`      - If the export completed.
/// * `Err(String)` - If `path` is a non-empty/existing repo, there is nothing to export,
///                   or git could not be written.
pub fn handle_command(path: &str) -> Result<(), String> {
    // export-git walks the whole tree of every parcel; a scoped bay must not silently export a
    // truncated view. Refuse cleanly and point at a full workspace.
    crate::commands::scope::refuse_in_scoped_bay(
        "export-git",
        "Run export-git from a full workspace (or the main tree), not a scoped bay.",
    )?;

    let target = Path::new(path);

    if target.join(".git").exists() {
        return Err(format!("\"{}\" is already a git repository; export needs a fresh path.", path));
    }
    if target.is_dir() && target.read_dir().map(|mut d| d.next().is_some()).unwrap_or(false) {
        return Err(format!("\"{}\" exists and is not empty; export needs an empty (or new) path.", path));
    }

    // Only user pallets map to git branches (the office and manifest are forklift-only).
    let pallets: Vec<(String, String)> = pallet_utils::list_pallets()?
        .into_iter()
        .filter_map(|name| pallet_utils::get_pallet_head(&name)
            .ok()
            .flatten()
            .map(|head| (name, head)))
        .collect();

    if pallets.is_empty() {
        return Err("This warehouse has nothing stacked to export.".to_string());
    }

    std::fs::create_dir_all(target)
        .map_err(|e| format!("Error while creating \"{}\": {}", path, e))?;
    git(path, &["init", "-q"])?;

    let mut exporter = Exporter::new(path);

    // Convert every parcel reachable from the branch heads, parents before children so a
    // commit's parents are already written when git-commit-tree needs them.
    let order = topological_order(&pallets.iter().map(|(_, head)| head.clone()).collect::<Vec<_>>())?;
    for parcel in &order {
        exporter.convert_commit(parcel)?;
    }

    // Point each branch at its commit.
    let mut branches: Vec<String> = Vec::new();
    for (name, head) in &pallets {
        let commit = exporter.commits.get(head).cloned()
            .ok_or(format!("pallet \"{}\" head was not exported", name))?;
        git(path, &["update-ref", &format!("refs/heads/{}", name), &commit])?;
        branches.push(name.clone());
    }

    // Check out the current pallet's branch (or the first), materializing a working tree.
    let current = pallet_utils::get_current_pallet_name().ok()
        .filter(|name| branches.iter().any(|branch| branch == name))
        .or_else(|| branches.first().cloned());

    if let Some(branch) = &current {
        git(path, &["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)])?;
        git(path, &["checkout", "-f", "-q", branch])?;
    }

    output::emit("export-git", &ExportReport {
        path: path.to_string(),
        commits: exporter.commits.len(),
        trees: exporter.trees.len(),
        blobs: exporter.blobs.len(),
        branches,
        current,
    });

    Ok(())
}

/// Writes forklift objects into a git repository, memoizing by forklift hash so each object
/// is written once even when branches share history.
struct Exporter<'a> {
    path: &'a str,
    blobs: HashMap<String, String>,
    trees: HashMap<String, String>,
    commits: HashMap<String, String>,
}

impl<'a> Exporter<'a> {
    fn new(path: &'a str) -> Exporter<'a> {
        Exporter { path, blobs: HashMap::new(), trees: HashMap::new(), commits: HashMap::new() }
    }

    /// Write one parcel as a git commit (its parents must already be written).
    fn convert_commit(&mut self, forklift_hash: &str) -> Result<String, String> {
        if let Some(hash) = self.commits.get(forklift_hash) {
            return Ok(hash.clone());
        }

        let parcel = object_utils::load_parcel(forklift_hash)?;
        let tree = self.convert_tree(&parcel.tree_hash)?;

        let mut args: Vec<String> = vec!["commit-tree".to_string(), tree];
        for parent in &parcel.parents {
            let commit = self.commits.get(parent)
                .ok_or("a parent commit was not exported before its child".to_string())?;
            args.push("-p".to_string());
            args.push(commit.clone());
        }
        args.push("-m".to_string());
        args.push(parcel.description.clone().unwrap_or_default());

        // Git's author/committer come from the environment; map them from the parcel's
        // Author and Stack actions so authorship and dates survive.
        let (author, author_time) = action_identity(&parcel, true);
        let (committer, committer_time) = action_identity(&parcel, false);

        let commit = git_env(
            self.path,
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &[
                ("GIT_AUTHOR_NAME", &author.0), ("GIT_AUTHOR_EMAIL", &author.1),
                ("GIT_AUTHOR_DATE", &format!("{} +0000", author_time)),
                ("GIT_COMMITTER_NAME", &committer.0), ("GIT_COMMITTER_EMAIL", &committer.1),
                ("GIT_COMMITTER_DATE", &format!("{} +0000", committer_time)),
            ],
        )?;
        let commit = commit.trim().to_string();

        self.commits.insert(forklift_hash.to_string(), commit.clone());

        Ok(commit)
    }

    /// Write one forklift tree as a git tree (recursively).
    fn convert_tree(&mut self, forklift_hash: &str) -> Result<String, String> {
        if let Some(hash) = self.trees.get(forklift_hash) {
            return Ok(hash.clone());
        }

        let tree = object_utils::load_tree(forklift_hash)?;
        let mut entries = String::new();

        for (name, item) in tree.get_files() {
            let (mode, kind) = match item.item_type {
                DirEntryType::Executable => ("100755", "blob"),
                DirEntryType::SymbolicLink => ("120000", "blob"),
                _ => ("100644", "blob"),
            };
            let blob = self.convert_blob(&item.hash)?;
            entries.push_str(&format!("{} {} {}\t{}\n", mode, kind, blob, name));
        }

        for (name, item) in tree.get_subtrees() {
            let subtree = self.convert_tree(&item.hash)?;
            entries.push_str(&format!("040000 tree {}\t{}\n", subtree, name));
        }

        let git_hash = git_stdin(self.path, &["mktree"], entries.as_bytes())?.trim().to_string();

        self.trees.insert(forklift_hash.to_string(), git_hash.clone());

        Ok(git_hash)
    }

    /// Write one forklift blob as a git blob.
    fn convert_blob(&mut self, forklift_hash: &str) -> Result<String, String> {
        if let Some(hash) = self.blobs.get(forklift_hash) {
            return Ok(hash.clone());
        }

        let content = object_utils::load_blob(forklift_hash)?.content;
        let git_hash = git_stdin(self.path, &["hash-object", "-w", "--stdin"], &content)?.trim().to_string();

        self.blobs.insert(forklift_hash.to_string(), git_hash.clone());

        Ok(git_hash)
    }
}

/// The (name, email) and unix time of a parcel's authoring action (`want_author`) or its
/// stacking action, falling back to the first action of any kind (every parcel has one).
fn action_identity(parcel: &Parcel, want_author: bool) -> ((String, String), i64) {
    let action = parcel.actions.iter()
        .find(|action| matches!(action.action, ParcelActionType::Author) == want_author)
        .or_else(|| parcel.actions.first());

    match action {
        Some(action) => {
            let name = if action.operator.name.is_empty() {
                action.operator.identifier.clone()
            } else {
                action.operator.name.clone()
            };
            ((name, action.operator.identifier.clone()), action.timestamp.timestamp())
        }
        None => (("forklift".to_string(), "forklift@localhost".to_string()), 0),
    }
}

/// Parcels reachable from `heads`, ordered parents-before-children (so a commit is written
/// after its parents). An iterative post-order walk — no recursion depth limit on long
/// linear histories.
fn topological_order(heads: &[String]) -> Result<Vec<String>, String> {
    let mut order: Vec<String> = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut stack: Vec<(String, bool)> = heads.iter().map(|head| (head.clone(), false)).collect();

    while let Some((hash, emit)) = stack.pop() {
        if emit {
            order.push(hash);
            continue;
        }
        if !visited.insert(hash.clone()) {
            continue;
        }

        // Re-push to emit this parcel once its parents have been emitted.
        stack.push((hash.clone(), true));

        for parent in object_utils::load_parcel(&hash)?.parents {
            stack.push((parent, false));
        }
    }

    Ok(order)
}

/// Run `git -C <path> <args>` and return trimmed stdout as a string.
fn git(path: &str, args: &[&str]) -> Result<String, String> {
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

    String::from_utf8(output.stdout)
        .map_err(|_| format!("git {} produced non-UTF-8 output.", args.join(" ")))
}

/// Run a git command with `input` on stdin, returning its stdout.
fn git_stdin(path: &str, args: &[&str], input: &[u8]) -> Result<String, String> {
    let mut child = Command::new("git")
        .arg("-C").arg(path)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run git: {}", e))?;

    child.stdin.take().expect("stdin is piped").write_all(input)
        .map_err(|e| format!("Error while writing to git: {}", e))?;

    let output = child.wait_with_output()
        .map_err(|e| format!("Error while running git: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    String::from_utf8(output.stdout)
        .map_err(|_| format!("git {} produced non-UTF-8 output.", args.join(" ")))
}

/// Run `git -C <path> <args>` with extra environment variables (the author/committer
/// identity for `commit-tree`).
fn git_env(path: &str, args: &[&str], env: &[(&str, &str)]) -> Result<String, String> {
    let mut command = Command::new("git");
    command.arg("-C").arg(path).args(args);

    for (key, value) in env {
        command.env(key, value);
    }

    let output = command.output()
        .map_err(|e| format!("Could not run git: {}", e))?;

    if !output.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }

    String::from_utf8(output.stdout)
        .map_err(|_| format!("git {} produced non-UTF-8 output.", args.join(" ")))
}

/// The result of a git export.
#[derive(Serialize)]
struct ExportReport {
    path: String,
    commits: usize,
    trees: usize,
    blobs: usize,

    /// The branches created (one per user pallet).
    branches: Vec<String>,

    /// The branch checked out (the current pallet).
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,
}

impl CommandOutput for ExportReport {
    fn render_human(&self) {
        println!(
            "Exported {} commit(s) into a git repository at \"{}\" ({} branch(es), {} tree(s), {} blob(s)).",
            self.commits, self.path, self.branches.len(), self.trees, self.blobs
        );

        if let Some(current) = &self.current {
            println!("Checked out \"{}\".", current);
        }

        println!(
            "Forklift-only history (the signed office, the @manifest, and signatures) was \
            not exported — git has no equivalent."
        );
    }
}
