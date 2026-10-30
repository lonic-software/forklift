use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use serde::Serialize;
use forklift_core::error::{CoreError, RefusalCode};
use forklift_core::util::lock_utils::MirrorLock;
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::{self, RemoteClient};
use forklift_core::util::{
    config_utils, file_utils, graph_utils, merge_utils, mirror_utils, object_utils, pallet_utils,
    scope_utils, take_utils,
};
use forklift_core::util::take_utils::{FetchAt, TakeTrust};
use crate::commands::export_git::{self, Exporter, GitRepo, EXPORT_LAYOUT};
use crate::events;
use crate::output::{self, CommandOutput};

/// How many times one pallet's push is tried while git's branch keeps moving under it.
const PUSH_ATTEMPTS: usize = 3;

/// Where the mirror records each pallet's last export, in its own repository. No fetch writes it.
const EXPORTED_PREFIX: &str = "refs/forklift/exported/";

/// Where the mirror fetches each pallet's git branch to, for comparison only (D3).
const REMOTE_PREFIX: &str = "refs/forklift/remote/";

/// The step every refusal over the mirror's own repository names.
const DELETE_STEP: &str = "Delete .forklift/mirror.git and run \"forklift git mirror\": it exports again from \
    scratch, and git receives nothing it already holds.";

/// The step every git failure that is not a race names.
const GIT_STEP: &str = "Fix the cause git reports, then run \"forklift git mirror\".";

/// Handle the mirror command (§9.8): keep the git remote at `mirror.url` in step with the pallets
/// the Forklift remote holds. Each mirrored pallet's remote head is fetched, exported
/// incrementally into `.forklift/mirror.git` on the commits this warehouse wrote there before, and
/// pushed to the git branch of the same name, never by force.
///
/// Only what the Forklift remote holds is mirrored, never a local head that was not lifted:
/// mirroring unlifted work and then undoing it would leave git ahead of anything Forklift could
/// lift again.
///
/// # Returns
/// * `Ok(())`      - If every mirrored pallet is in step (or was skipped as not mirrorable).
/// * `Err(String)` - If the mirror is not configured, busy or damaged, or a pallet was refused or
///                   its push failed (after the rest went through: `mirror_incomplete`, with the
///                   run's report as its details).
pub async fn handle_command() -> Result<(), String> {
    refuse_partial_view()?;

    let url = mirror_url()?.ok_or(format!(
        "No git mirror is configured. Set one with \"forklift config set {} <url>\".",
        config_utils::KEY_MIRROR_URL
    ))?;
    let client = RemoteClient::from_config()?;

    let Some(_lock) = MirrorLock::try_acquire()? else {
        return Err(busy_message());
    };

    let repo = open_repository().map_err(Problem::into_message)?;
    let mut record = load_record(&repo).map_err(Problem::into_message)?;
    let info = client.fetch_info().await?;

    // The configured set, else every user pallet the remote holds.
    let pallets: Vec<String> = match config_utils::get_effective_value(config_utils::KEY_MIRROR_PALLETS)? {
        Some((value, _)) => config_utils::parse_mirror_pallets(&value)?,
        None => info.pallets.keys()
            .filter(|name| !name.starts_with(pallet_utils::META_QUALIFIER))
            .cloned()
            .collect(),
    };

    let trust = mirror_trust(&client, &info).await?;

    let mut run = Run::start(&repo, &client, trust, url.clone(), false).map_err(Problem::into_message)?;
    let mut report = MirrorReport { url, pallets: Vec::new() };

    for pallet in &pallets {
        let outcome = run.mirror(pallet, info.pallets.get(pallet).map(String::as_str), &mut record).await;
        report.pallets.push(outcome);
    }

    // The record keeps the configured set: a pallet that went through, or was refused or failed
    // after its export, holds the entry its export wrote; one that failed before it keeps its
    // prior entry, so a later run resumes from it. A pallet no longer configured drops out.
    record.retain(|pallet, _| pallets.contains(pallet));
    mirror_utils::write_mirror_roots(&record)?;

    let failed: Vec<&PalletMirror> = report.pallets.iter().filter(|outcome| outcome.status.is_failure()).collect();
    if failed.is_empty() {
        output::emit(&report);
        return Ok(());
    }

    if !output::is_json() {
        report.render_in_step();
    }

    let message = format!(
        "Did not mirror {} of {} pallet(s):\n{}",
        failed.len(),
        report.pallets.len(),
        failed.iter().map(|outcome| format!("  \"{}\": {}", outcome.pallet, outcome.message.as_deref().unwrap_or_default()))
            .collect::<Vec<_>>().join("\n")
    );

    Err(CoreError::refusal_with_details(
        RefusalCode::MirrorIncomplete,
        message,
        "Follow the step each pallet's message names, then run \"forklift git mirror\" again.",
        serde_json::json!(report),
    ).into())
}

/// Mirror the pallet a lift just landed, at the head it landed (the lift trigger). Runs only when
/// `mirror.url` is set, `mirror.auto` is on and the pallet is in the mirrored set. The outcome is
/// reported in the lift's result and never fails the lift; a pallet left behind is caught up by
/// the next lift or `forklift git mirror`.
///
/// # Returns
/// * `Some(PalletMirror)` - What the mirror did.
/// * `None`               - If no mirror applies to this pallet.
pub(crate) async fn after_lift(client: &RemoteClient, pallet: &str, head: &str) -> Option<PalletMirror> {
    let applies = || -> Result<Option<String>, String> {
        let Some(url) = mirror_url()? else { return Ok(None) };

        if let Some((value, _)) = config_utils::get_effective_value(config_utils::KEY_MIRROR_AUTO)? {
            if !config_utils::parse_switch(config_utils::KEY_MIRROR_AUTO, &value)? {
                return Ok(None);
            }
        }
        if let Some((value, _)) = config_utils::get_effective_value(config_utils::KEY_MIRROR_PALLETS)? {
            if !config_utils::parse_mirror_pallets(&value)?.iter().any(|name| name == pallet) {
                return Ok(None);
            }
        }

        Ok(Some(url))
    };

    let url = match applies() {
        Ok(Some(url)) => url,
        Ok(None) => return None,
        Err(why) => return Some(PalletMirror::new(pallet, MirrorStatus::Failed, why)),
    };

    match scope_utils::is_scoped().and_then(|scoped| Ok(scoped || scope_utils::content_may_be_absent()?)) {
        Ok(false) => {}
        Ok(true) => return Some(PalletMirror::new(pallet, MirrorStatus::Skipped,
            "The mirror needs every file of the pallet, and this bay or warehouse may hold only \
            part of it. Run \"forklift git mirror\" from a full workspace.".to_string())),
        Err(why) => return Some(PalletMirror::new(pallet, MirrorStatus::Failed, why)),
    }

    let lock = match MirrorLock::try_acquire() {
        Ok(Some(lock)) => lock,
        Ok(None) => return Some(PalletMirror::new(pallet, MirrorStatus::Busy, busy_message())),
        Err(why) => return Some(PalletMirror::new(pallet, MirrorStatus::Failed, why)),
    };

    let outcome = async {
        let repo = open_repository()?;
        let mut record = load_record(&repo)?;
        let mut run = Run::start(&repo, client, TakeTrust::local()?, url, true)?;

        Ok::<PalletMirror, Problem>(run.mirror(pallet, Some(head), &mut record).await)
    }.await;
    drop(lock);

    Some(outcome.unwrap_or_else(|problem| problem.into_outcome(pallet)))
}

/// What every mirrored head is verified against before it is exported: git receives only history
/// the anchor vouches for. With an anchor held, the office first fast-forwards to the remote's
/// (verified against it, as a lower would), so a head signed by a newly admitted key verifies. With
/// none held, the remote's own anchor and office — verified, never installed: a mirror adopts no
/// trust.
async fn mirror_trust(client: &RemoteClient, info: &forklift_core::model::remote::WarehouseInfo) -> Result<TakeTrust, String> {
    let prepared = remote_utils::prepare_remote_trust(client, info, None).await?;

    if let Some(first_contact) = prepared.take_trust() {
        return Ok(first_contact);
    }

    prepared.commit()?;

    TakeTrust::local()
}

/// Refuse a view that may hold only part of each tree: a scoped bay, or a partial warehouse (its
/// fetch scope path-pruned, or cloned with `--cache`). An export of a partial tree would be a
/// different commit.
fn refuse_partial_view() -> Result<(), CoreError> {
    crate::commands::scope::refuse_in_scoped_bay(
        "git mirror",
        "Run \"forklift git mirror\" from a full workspace (or the main tree), not a scoped bay.",
    )?;

    if scope_utils::content_may_be_absent()? {
        return Err(CoreError::refusal(
            RefusalCode::SparseWorkspace,
            "\"forklift git mirror\" needs every file of each mirrored pallet, and this warehouse was cloned \
            with --only or --cache, so it may lack some of them.",
            "Run \"forklift git mirror\" from a clone made without --only or --cache.",
        ));
    }

    Ok(())
}

/// The configured `mirror.url`, if any.
fn mirror_url() -> Result<Option<String>, String> {
    // Checked again here: a value written to the file by hand never passed `forklift config`.
    config_utils::get_effective_value(config_utils::KEY_MIRROR_URL)?
        .map(|(url, _)| config_utils::parse_mirror_url(&url))
        .transpose()
}

fn busy_message() -> String {
    "Another mirror run is using this warehouse's mirror. Run \"forklift git mirror\" again \
    once it finishes.".to_string()
}

/// Why a mirror step stopped.
enum Problem {
    /// The mirror's own repository holds something this build did not write in this layout.
    Damaged(String),
    /// Anything else: a git or remote failure, quoted.
    Failed(String),
}

impl Problem {
    fn into_message(self) -> String {
        match self {
            Problem::Damaged(message) | Problem::Failed(message) => message,
        }
    }

    fn into_outcome(self, pallet: &str) -> PalletMirror {
        match self {
            Problem::Damaged(message) => PalletMirror::new(pallet, MirrorStatus::Damaged, message),
            Problem::Failed(message) => PalletMirror::new(pallet, MirrorStatus::Failed, message),
        }
    }
}

impl From<String> for Problem {
    fn from(message: String) -> Problem {
        Problem::Failed(message)
    }
}

impl From<CoreError> for Problem {
    fn from(error: CoreError) -> Problem {
        Problem::Failed(error.into())
    }
}

fn damaged(what: String) -> Problem {
    Problem::Damaged(format!("The mirror's repository .forklift/mirror.git {}. {}", what, DELETE_STEP))
}

/// Open the mirror's repository, creating it on first use (or when a run stopped before
/// initializing it left only the folder).
fn open_repository() -> Result<GitRepo, Problem> {
    let path = mirror_utils::mirror_repository_path();
    let empty = path.is_dir() && path.read_dir().map(|mut entries| entries.next().is_none()).unwrap_or(false);

    if !path.exists() || empty {
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("Error while creating \"{}\": {}", path.display(), e))?;
        return Ok(GitRepo::create_bare(&path)?);
    }

    // A folder git would not recognize is refused rather than handed to git, which would
    // otherwise search the folders around it for a repository.
    if !path.join("HEAD").is_file() || !path.join("objects").is_dir() || !path.join("refs").is_dir() {
        return Err(damaged("is not a git repository".to_string()));
    }

    Ok(GitRepo::open_bare(&path)?)
}

/// The mirror's exported refs: each pallet name with the commit its last export ended at.
fn exported_refs(repo: &GitRepo) -> Result<Vec<(String, String)>, Problem> {
    let listing = repo.run(&["for-each-ref", "--format=%(objectname) %(refname)", EXPORTED_PREFIX], None)?;

    Ok(listing.lines()
        .filter_map(|line| line.split_once(' '))
        .filter_map(|(commit, name)| name.strip_prefix(EXPORTED_PREFIX).map(|pallet| (pallet.to_string(), commit.to_string())))
        .collect())
}

/// Read the roots record, or rebuild it from the parcels the exported refs name when it cannot be
/// read, so an update never writes over a corrupt record with one entry.
fn load_record(repo: &GitRepo) -> Result<BTreeMap<String, String>, Problem> {
    if let Ok(record) = mirror_utils::read_mirror_roots() {
        return Ok(record);
    }

    let mut record = BTreeMap::new();
    for (pallet, commit) in exported_refs(repo)? {
        let parcel = read_commit(repo, &commit)?.exported_parcel(&commit)?;
        if file_utils::does_object_exist(&parcel)? {
            record.insert(pallet, parcel);
        }
    }
    mirror_utils::write_mirror_roots(&record)?;

    Ok(record)
}

/// A git commit object, read raw.
struct RawCommit {
    tree: String,
    parents: Vec<String>,
    /// The whole object.
    bytes: Vec<u8>,
}

impl RawCommit {
    /// The message's last line, read from the raw object (never through git's trailer parser).
    fn last_line(&self) -> String {
        let message = self.bytes.strip_suffix(b"\n").unwrap_or(&self.bytes);
        let line = message.rsplit(|byte| *byte == b'\n').next().unwrap_or_default();

        String::from_utf8_lossy(line).into_owned()
    }

    /// The parcel and export layout the last line names, if it is a parcel line at all.
    fn parcel_line(&self) -> Option<(String, String)> {
        let rest = self.last_line().strip_prefix("Forklift-Parcel: ")?.to_string();
        let (parcel, layout) = rest.split_once(" export/")?;
        let numbered = !layout.is_empty() && layout.bytes().all(|byte| byte.is_ascii_digit());

        (object_utils::is_object_hash(parcel) && numbered).then(|| (parcel.to_string(), layout.to_string()))
    }

    /// Whether this commit ends in exactly the parcel line this build writes for `parcel`.
    fn names(&self, parcel: &str) -> bool {
        self.bytes.ends_with(format!("\n{}\n", export_git::parcel_line(parcel)).as_bytes())
    }

    /// The parcel this commit was exported from by this build, or the refusal over the mirror's
    /// repository when its last line is not one this build writes.
    fn exported_parcel(&self, commit: &str) -> Result<String, Problem> {
        match self.parcel_line() {
            Some((parcel, _)) if self.names(&parcel) => Ok(parcel),
            Some((_, layout)) if layout != EXPORT_LAYOUT.to_string() => Err(damaged(format!(
                "holds commit {}, written in export layout {}; this build writes export layout {}",
                commit, layout, EXPORT_LAYOUT
            ))),
            Some(_) => Err(damaged(format!(
                "holds commit {}, whose last line is not the parcel line this build writes", commit
            ))),
            None => Err(damaged(format!("holds commit {}, which does not name the parcel it was exported from", commit))),
        }
    }
}

fn read_commit(repo: &GitRepo, commit: &str) -> Result<RawCommit, Problem> {
    let output = repo.output(&["cat-file", "commit", commit])?;
    if !output.status.success() {
        return Err(damaged(format!(
            "cannot read commit {}: {}", commit, String::from_utf8_lossy(&output.stderr).trim()
        )));
    }

    let bytes = output.stdout;
    let header_end = bytes.windows(2).position(|pair| pair == b"\n\n").unwrap_or(bytes.len());
    let headers = String::from_utf8_lossy(&bytes[..header_end]).into_owned();

    let mut tree = String::new();
    let mut parents = Vec::new();
    for line in headers.lines() {
        if let Some(hash) = line.strip_prefix("tree ") {
            tree = hash.to_string();
        } else if let Some(hash) = line.strip_prefix("parent ") {
            parents.push(hash.to_string());
        }
    }

    Ok(RawCommit { tree, parents, bytes })
}

/// Check an exported ref's commit one level down: it has the parcel's distinct parents, each
/// exported as that parent. A tip rewritten to name another parcel of the history fails here,
/// since nothing above the tip says which parcel it must name.
fn check_parents(repo: &GitRepo, commit: &str, raw: &RawCommit, parcel: &str, parents: &[String]) -> Result<(), Problem> {
    let parents = export_git::distinct(parents);
    check_parent_count(commit, raw, parcel, parents.len())?;

    for (parent, parent_commit) in parents.into_iter().zip(&raw.parents) {
        if !read_commit(repo, parent_commit)?.names(parent) {
            return Err(damaged(format!(
                "holds commit {} for parcel {}, but its parent {} does not name parcel {}",
                commit, parcel, parent_commit, parent
            )));
        }
    }

    Ok(())
}

fn check_parent_count(commit: &str, raw: &RawCommit, parcel: &str, distinct: usize) -> Result<(), Problem> {
    if distinct != raw.parents.len() {
        return Err(damaged(format!(
            "holds commit {} with {} parent(s), where parcel {} has {}",
            commit, raw.parents.len(), parcel, distinct
        )));
    }

    Ok(())
}

/// A commit this build exported, which a run may build on: its parcel is in the local store.
#[derive(Clone)]
struct Boundary {
    parcel: String,
    commit: String,
}

/// An exported ref whose parcel is not in the local store yet, with its commit as read at the
/// start of the run.
struct Pending {
    pallet: String,
    boundary: Boundary,
    raw: RawCommit,
}

/// One mirror run over the mirror's repository.
struct Run<'a> {
    repo: &'a GitRepo,
    client: &'a RemoteClient,
    /// What every head is verified against before it is exported.
    trust: TakeTrust,
    url: String,
    /// Where git's network calls run, so a relative `url` is read from the warehouse's root
    /// (the folder holding the shared `.forklift`), from any bay.
    from: PathBuf,
    /// A lift's trigger: git must fail rather than prompt.
    unattended: bool,
    exporter: Exporter<'a>,
    boundaries: Vec<Boundary>,
    /// Exported refs whose parcel is not in the local store yet: each becomes a boundary once a
    /// fetch brings its parcel in.
    pending: Vec<Pending>,
    /// Why an exported ref could not become a boundary, by pallet: reported for that pallet
    /// alone, whichever pallet's fetch brought its parcel in.
    refused: BTreeMap<String, Problem>,
    progress: Progress,
}

/// Where git's branch stands against the new export.
enum Verdict {
    Push,
    UpToDate,
    AlreadyAhead,
    Diverged(String),
    LayoutSkew(String, String),
}

impl<'a> Run<'a> {
    /// Start a run: every exported ref must end in this build's parcel line (D2), and those whose
    /// parcel is in the local store are the boundaries the export builds on. One that does not is
    /// set aside for its own pallet's outcome.
    fn start(repo: &'a GitRepo, client: &'a RemoteClient, trust: TakeTrust, url: String, unattended: bool) -> Result<Run<'a>, Problem> {
        let mut pending = Vec::new();
        let mut refused = BTreeMap::new();
        for (pallet, commit) in exported_refs(repo)? {
            let read = read_commit(repo, &commit)
                .and_then(|raw| raw.exported_parcel(&commit).map(|parcel| (raw, parcel)));
            match read {
                Ok((raw, parcel)) => pending.push(Pending { pallet, boundary: Boundary { parcel, commit }, raw }),
                Err(problem) => { refused.insert(pallet, problem); }
            }
        }

        let mut run = Run {
            repo,
            client,
            trust,
            url,
            from: mirror_utils::warehouse_root(),
            unattended,
            exporter: Exporter::new(repo),
            boundaries: Vec::new(),
            pending,
            refused,
            progress: events::progress(),
        };
        run.promote();

        Ok(run)
    }

    /// Make a boundary of every pending exported ref whose parcel is now in the local store,
    /// checking its commit's parents first. One that fails the check is set aside for its own
    /// pallet's outcome; the run goes on without it.
    fn promote(&mut self) {
        let mut still_pending = Vec::new();

        for pending in std::mem::take(&mut self.pending) {
            match self.promote_one(&pending) {
                Ok(true) => self.boundaries.push(pending.boundary),
                Ok(false) => still_pending.push(pending),
                Err(problem) => { self.refused.insert(pending.pallet, problem); }
            }
        }

        self.pending = still_pending;
    }

    /// Seed one pending exported ref, if its parcel is in the local store.
    fn promote_one(&mut self, pending: &Pending) -> Result<bool, Problem> {
        let Boundary { parcel, commit } = &pending.boundary;
        if !file_utils::does_object_exist(parcel)? {
            return Ok(false);
        }

        let loaded = object_utils::load_parcel(parcel)?;
        check_parents(self.repo, commit, &pending.raw, parcel, &loaded.parents)?;
        self.exporter.seed(parcel, commit, &pending.raw.tree, &loaded.tree_hash);

        Ok(true)
    }

    /// Mirror one pallet at `remote_head` (its head on the Forklift remote), keeping `record` up to
    /// date: its entry is written once the pallet is exported, before the push.
    async fn mirror(&mut self, pallet: &str, remote_head: Option<&str>, record: &mut BTreeMap<String, String>) -> PalletMirror {
        match self.try_mirror(pallet, remote_head, record).await {
            Ok(outcome) => outcome,
            Err(problem) => problem.into_outcome(pallet),
        }
    }

    async fn try_mirror(&mut self, pallet: &str, remote_head: Option<&str>, record: &mut BTreeMap<String, String>)
        -> Result<PalletMirror, Problem> {
        // Checked before any ref is written: a valid pallet name git refuses as a branch name.
        let branch = format!("refs/heads/{}", pallet);
        if !self.repo.output(&["check-ref-format", &branch])?.status.success() {
            return Ok(PalletMirror::new(pallet, MirrorStatus::Skipped, format!(
                "git does not accept \"{}\" as a branch name.", pallet
            )));
        }

        let Some(head) = remote_head else {
            return Ok(PalletMirror::new(pallet, MirrorStatus::Skipped, format!(
                "The Forklift remote has no pallet \"{}\".", pallet
            )));
        };

        // A tip found damaged at the start is refused before its history is fetched; one the
        // fetch brings in is checked after it.
        if let Some(problem) = self.refused.remove(pallet) {
            return Err(problem);
        }

        let roots: Vec<String> = record.values().cloned().collect();
        remote_utils::fetch_history_within(self.client, head, &roots, &self.progress).await?;
        self.promote();
        if let Some(problem) = self.refused.remove(pallet) {
            return Err(problem);
        }

        // Before anything reaches git: a head the anchor does not vouch for is not exported. The
        // pallet's message carries the refusal's step, which the run's next step points at.
        take_utils::verify_take(Some(self.client), &self.trust, head, pallet, FetchAt::Warehouse).await
            .map_err(|refusal| Problem::Failed(match CoreError::from(refusal) {
                CoreError::Refusal { message, next_step, .. } => format!("{} {}", message, next_step),
                other => other.to_string(),
            }))?;

        let mut git = self.fetch_branch(pallet)?;

        self.progress.status("Exporting to git");
        let exported = self.export(head)?;
        let commit = self.exporter.commit_of(head).expect("the head was exported").to_string();
        self.repo.run(&["update-ref", &format!("{}{}", EXPORTED_PREFIX, pallet), &commit], None)?;

        record.insert(pallet.to_string(), head.to_string());
        mirror_utils::write_mirror_roots(record)?;

        // Classified once more after the last attempt's re-fetch, so a race that ends in step is
        // reported as such.
        for attempt in 0..=PUSH_ATTEMPTS {
            match self.classify(pallet, &commit, git.as_deref()).await? {
                Verdict::UpToDate => return Ok(PalletMirror::in_step(pallet, MirrorStatus::UpToDate, &commit, exported)),
                Verdict::AlreadyAhead => {
                    let tip = git.expect("git is ahead of the export");
                    return Ok(PalletMirror::in_step(pallet, MirrorStatus::AlreadyAhead, &tip, exported));
                }
                Verdict::Diverged(foreign) => return Ok(PalletMirror::new(pallet, MirrorStatus::Diverged, format!(
                    "git's branch \"{}\" holds commit {}, which is not part of this warehouse's export. \
                    Nothing was pushed. Return the branch to an ancestor of {} (move {} to another \
                    branch, for instance), then run \"forklift git mirror\".",
                    pallet, foreign, commit, foreign
                ))),
                Verdict::LayoutSkew(foreign, layout) => return Ok(PalletMirror::new(pallet, MirrorStatus::LayoutSkew, format!(
                    "git's branch \"{}\" holds commit {}, written in export layout {}; this build \
                    writes export layout {}. Nothing was pushed. Mirror this branch from builds that \
                    write one layout, or mirror to a new repository.",
                    pallet, foreign, layout, EXPORT_LAYOUT
                ))),
                Verdict::Push if attempt == PUSH_ATTEMPTS => break,
                Verdict::Push => {}
            }

            let pushed = self.repo.network(
                &["push", "--porcelain", &self.url, &format!("{}{}:{}", EXPORTED_PREFIX, pallet, branch)],
                &self.from,
                self.unattended,
            )?;
            if pushed.status.success() {
                return Ok(PalletMirror::in_step(pallet, MirrorStatus::Pushed, &commit, exported));
            }

            // A rejection reads the same for a lost race and a permanent refusal (a hook's
            // decline), so it is a race only if the branch moved.
            let why = push_failure(&pushed);
            let moved = match self.fetch_branch(pallet) {
                Ok(now) if now != git => now,
                _ => return Ok(PalletMirror::new(pallet, MirrorStatus::Failed, format!("{}\n{}", why, GIT_STEP))),
            };
            git = moved;
        }

        Ok(PalletMirror::new(pallet, MirrorStatus::Raced, format!(
            "git's branch \"{}\" kept moving while the mirror pushed ({} attempts). Run \"forklift \
            git mirror\" again.",
            pallet, PUSH_ATTEMPTS
        )))
    }

    /// Fetch git's branch of `pallet` into the comparison ref, returning its tip (none when git
    /// has no such branch).
    fn fetch_branch(&self, pallet: &str) -> Result<Option<String>, Problem> {
        let local = format!("{}{}", REMOTE_PREFIX, pallet);
        let fetched = self.repo.network(
            &["fetch", "-q", "--no-tags", &self.url, &format!("+refs/heads/{}:{}", pallet, local)],
            &self.from,
            self.unattended,
        )?;

        if fetched.status.success() {
            return Ok(Some(self.repo.run(&["rev-parse", "--verify", "-q", &local], None)?.trim().to_string()));
        }

        // Fetching a branch git does not have fails; tell that apart from a failure to reach git.
        let listed = self.repo.network(&["ls-remote", &self.url, &format!("refs/heads/{}", pallet)], &self.from, self.unattended)?;
        if listed.status.success() && listed.stdout.trim_ascii().is_empty() {
            self.repo.run(&["update-ref", "-d", &local], None)?;
            return Ok(None);
        }

        Err(Problem::Failed(format!(
            "git fetch from the mirror failed: {}\n{}",
            String::from_utf8_lossy(&fetched.stderr).trim(), GIT_STEP
        )))
    }

    /// Export `head` and every parcel it reaches that no boundary already holds, parents first.
    /// Returns how many parcels were written.
    fn export(&mut self, head: &str) -> Result<usize, Problem> {
        let mut written = 0;
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack: Vec<(String, bool)> = vec![(head.to_string(), false)];

        while let Some((hash, emit)) = stack.pop() {
            if emit {
                self.exporter.export_parcel(&hash)?;
                written += 1;
                continue;
            }
            if self.exporter.commit_of(&hash).is_some() || !visited.insert(hash.clone()) {
                continue;
            }

            if let Some(boundary) = self.boundary_holding(&hash)? {
                self.locate(&hash, &boundary)?;
                continue;
            }

            stack.push((hash.clone(), true));
            for parent in object_utils::load_parcel(&hash)?.parents {
                stack.push((parent, false));
            }
        }

        Ok(written)
    }

    /// A boundary whose history holds `parcel`, if any.
    fn boundary_holding(&self, parcel: &str) -> Result<Option<Boundary>, Problem> {
        for boundary in &self.boundaries {
            if merge_utils::is_ancestor(parcel, &boundary.parcel)? {
                return Ok(Some(boundary.clone()));
            }
        }

        Ok(None)
    }

    /// Find the commit `target` was exported as, descending from `boundary`'s tip: the commit of
    /// a parcel's i-th distinct parent is its commit's i-th parent (D2). Every commit on the way is
    /// checked to name its parcel, and seeds the exporter. Parents older than the target cannot
    /// lead to it and are not descended.
    fn locate(&mut self, target: &str, boundary: &Boundary) -> Result<(), Problem> {
        let target_generation = graph_utils::generation(target)?;
        let mut visited: HashSet<String> = HashSet::new();
        let mut stack = vec![(boundary.parcel.clone(), boundary.commit.clone())];

        while let Some((parcel, commit)) = stack.pop() {
            if !visited.insert(parcel.clone()) {
                continue;
            }

            let raw = read_commit(self.repo, &commit)?;
            if !raw.names(&parcel) {
                return Err(match raw.parcel_line() {
                    Some((named, layout)) => damaged(format!(
                        "holds commit {} where parcel {} was exported, but it names parcel {} in export layout {}",
                        commit, parcel, named, layout
                    )),
                    None => damaged(format!(
                        "holds commit {} where parcel {} was exported, but it names no parcel", commit, parcel
                    )),
                });
            }

            let loaded = object_utils::load_parcel(&parcel)?;
            self.exporter.seed(&parcel, &commit, &raw.tree, &loaded.tree_hash);
            if parcel == target {
                return Ok(());
            }

            let parents = export_git::distinct(&loaded.parents);
            check_parent_count(&commit, &raw, &parcel, parents.len())?;

            for (parent, parent_commit) in parents.into_iter().zip(raw.parents) {
                if parent == target || graph_utils::generation(parent)? > target_generation {
                    stack.push((parent.clone(), parent_commit));
                }
            }
        }

        Err(damaged(format!("does not reach the commit of parcel {} from commit {}", target, boundary.commit)))
    }

    /// Compare git's tip `git` with the export's `commit` (D3).
    async fn classify(&mut self, pallet: &str, commit: &str, git: Option<&str>) -> Result<Verdict, Problem> {
        let Some(git) = git else { return Ok(Verdict::Push) };

        if git == commit {
            return Ok(Verdict::UpToDate);
        }
        if self.is_ancestor(git, commit)? {
            return Ok(Verdict::Push);
        }

        if !self.is_ancestor(commit, git)? {
            return Ok(match read_commit(self.repo, git)?.parcel_line() {
                Some((_, layout)) if layout != EXPORT_LAYOUT.to_string() => Verdict::LayoutSkew(git.to_string(), layout),
                _ => Verdict::Diverged(git.to_string()),
            });
        }

        // Git is ahead: acceptable only if every commit past the export is one another warehouse
        // exported in this layout from this pallet's lifted history.
        let range = format!("{}..{}", commit, git);
        for ahead in self.repo.run(&["rev-list", "--reverse", &range], None)?.lines() {
            match read_commit(self.repo, ahead)?.parcel_line() {
                None => return Ok(Verdict::Diverged(ahead.to_string())),
                Some((_, layout)) if layout != EXPORT_LAYOUT.to_string() =>
                    return Ok(Verdict::LayoutSkew(ahead.to_string(), layout)),
                Some(_) => {}
            }
        }

        let Some((parcel, _)) = read_commit(self.repo, git)?.parcel_line() else {
            return Ok(Verdict::Diverged(git.to_string()));
        };

        // The pallet's head now, which may be past the head this run exported.
        let info = self.client.fetch_info().await?;
        let Some(remote_head) = info.pallets.get(pallet) else {
            return Ok(Verdict::Diverged(git.to_string()));
        };
        let roots: Vec<String> = self.boundaries.iter().map(|boundary| boundary.parcel.clone()).collect();
        remote_utils::fetch_history_within(self.client, remote_head, &roots, &self.progress).await?;
        self.promote();

        if file_utils::does_object_exist(&parcel)? && merge_utils::is_ancestor(&parcel, remote_head)? {
            return Ok(Verdict::AlreadyAhead);
        }

        Ok(Verdict::Diverged(git.to_string()))
    }

    /// `git merge-base --is-ancestor`.
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool, Problem> {
        let output = self.repo.output(&["merge-base", "--is-ancestor", ancestor, descendant])?;

        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Problem::Failed(format!(
                "git merge-base failed: {}", String::from_utf8_lossy(&output.stderr).trim()
            ))),
        }
    }
}

/// What a failed push said: git's own message, with the rejected ref's porcelain line.
fn push_failure(output: &std::process::Output) -> String {
    let rejected: Vec<String> = String::from_utf8_lossy(&output.stdout).lines()
        .filter(|line| line.starts_with('!'))
        .map(str::to_string)
        .collect();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    format!("git push to the mirror failed: {}", [stderr, rejected.join("\n")].iter()
        .filter(|part| !part.is_empty()).cloned().collect::<Vec<_>>().join("\n"))
}

/// What the mirror did with one pallet.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone, Copy, PartialEq, Eq, Debug)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MirrorStatus {
    /// The export was pushed to git.
    Pushed,
    /// Git's branch already held the export.
    UpToDate,
    /// Git's branch was already past the export, at a later lift another warehouse mirrored.
    AlreadyAhead,
    /// The pallet cannot be mirrored (the remote lacks it, or git refuses its name as a branch).
    Skipped,
    /// Another mirror run held the mirror.
    Busy,
    /// Git's branch kept moving while the push was retried.
    Raced,
    /// Git's branch holds a commit that is not part of the export; nothing was pushed.
    Diverged,
    /// Git's branch was written in another export layout; nothing was pushed.
    LayoutSkew,
    /// The mirror's own repository holds something this build did not write.
    Damaged,
    /// A fetch, export or push failed.
    Failed,
}

impl MirrorStatus {
    /// Whether this outcome leaves the pallet out of step for a reason a run must report as a
    /// failure (a skip is not one).
    fn is_failure(self) -> bool {
        !matches!(self, MirrorStatus::Pushed | MirrorStatus::UpToDate | MirrorStatus::AlreadyAhead | MirrorStatus::Skipped)
    }
}

/// One pallet's mirror outcome.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Debug)]
pub(crate) struct PalletMirror {
    pallet: String,
    status: MirrorStatus,

    /// The git commit the branch is at, when it is in step with the export.
    #[serde(skip_serializing_if = "Option::is_none")]
    commit: Option<String>,

    /// The parcels this run wrote as new git commits.
    exported: usize,

    /// Why the pallet was skipped or not mirrored, with the step that clears it.
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl PalletMirror {
    fn new(pallet: &str, status: MirrorStatus, message: String) -> PalletMirror {
        PalletMirror { pallet: pallet.to_string(), status, commit: None, exported: 0, message: Some(message) }
    }

    fn in_step(pallet: &str, status: MirrorStatus, commit: &str, exported: usize) -> PalletMirror {
        PalletMirror { pallet: pallet.to_string(), status, commit: Some(commit.to_string()), exported, message: None }
    }

    /// The outcome as one line of human output.
    pub(crate) fn render_line(&self) {
        let commit = self.commit.as_deref().unwrap_or_default();
        let message = self.message.as_deref().unwrap_or_default();

        match self.status {
            MirrorStatus::Pushed => println!(
                "Mirrored \"{}\" to git at {} ({} commit(s) exported).", self.pallet, commit, self.exported
            ),
            MirrorStatus::UpToDate => println!("Git already has \"{}\" at {}.", self.pallet, commit),
            MirrorStatus::AlreadyAhead => println!(
                "Git is already ahead on \"{}\", at {}: another warehouse mirrored a later lift.",
                self.pallet, commit
            ),
            MirrorStatus::Skipped => println!("Skipped \"{}\": {}", self.pallet, message),
            _ => println!("Did not mirror \"{}\": {}", self.pallet, message),
        }
    }
}

/// The result of a mirror run.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct MirrorReport {
    /// The git remote mirrored to (`mirror.url`).
    url: String,

    /// Each mirrored pallet's outcome.
    pallets: Vec<PalletMirror>,
}

impl MirrorReport {
    /// The pallets in step or skipped, and what the mirror never carries.
    fn render_in_step(&self) {
        for outcome in self.pallets.iter().filter(|outcome| !outcome.status.is_failure()) {
            outcome.render_line();
        }

        println!("{}", NOT_MIRRORED);
    }
}

/// What git has no home for, so no run mirrors it.
pub(crate) const NOT_MIRRORED: &str = "Tags, the signed office, the @manifest and signatures are not \
    mirrored: git has no equivalent.";

impl CommandOutput for MirrorReport {
    fn render_human(&self) {
        if self.pallets.is_empty() {
            println!("The Forklift remote holds no pallet to mirror.");
        }

        self.render_in_step();
    }
}

/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<MirrorReport>(),
    ]
}
