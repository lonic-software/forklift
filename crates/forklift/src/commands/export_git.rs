use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use serde::Serialize;
use forklift_core::enums::dir_entry_type::DirEntryType;
use forklift_core::enums::parcel_action_type::ParcelActionType;
use forklift_core::model::parcel::Parcel;
use forklift_core::error::sanitize;
use forklift_core::util::tag_utils::{self, AttributedTag};
use forklift_core::util::{file_utils, haul_utils, kv_utils, manifest_utils, object_utils, office_utils};
use forklift_core::util::{applied_redactions, bay_utils, pallet_utils, park_utils};
use forklift_core::util::applied_redactions::AppliedSet;
use forklift_core::util::unavailable::{self, UnavailableSet};
use forklift_core::util::pallet_utils::{PalletNamespace, PalletRef};
use forklift_core::util::remote_utils::RemoteClient;
use crate::commands::pallet::{relation_to_remote, RemoteRelation};
use crate::output::{self, CommandOutput};

/// The export layout version: the `export/<N>` every exported commit's parcel line and every
/// exported tag's last line (signer or unsigned) names. It covers every rule that fixes an exported
/// object's bytes: the message shape and its parcel line, the identity mapping, the date form
/// (pre-1970 written as the epoch), the parent rule, the type-to-mode map, chunked-blob assembly,
/// filter-free hashing and the tag object's shape. Two builds on one layout write identical objects
/// for the same parcel or tag, so a change to any of those rules bumps it (the golden tip-hash and
/// tag-object tests fail until it does). Layout 1 holds both tag-line forms: the unsigned one was
/// added before any build exported an unsigned tag, so no object written earlier changed.
pub(crate) const EXPORT_LAYOUT: u32 = 1;

/// Handle the git export command (§7.8): one-way export of this warehouse's history into a
/// new git repository — the escape hatch that makes trying forklift reversible. Parcels
/// become commits, trees become trees, blobs become blobs, each *user* pallet becomes
/// a branch, and each tag on an exported parcel becomes an annotated git tag.
///
/// It is deliberately lossy in this direction: git has no home for forklift's signed
/// office, the `@manifest` (approvals / provenance) or per-parcel and per-tag signatures, so
/// those are dropped — the guarantee is "your code history is never trapped", not a lossless
/// round trip. Requires `git` on PATH.
///
/// The objects are deterministic (§9.8): every commit ends in a line naming its parcel and the
/// export layout, and no git configuration or environment of the user's reaches an object write, so
/// the same history exports to the same commits on any machine that applied the same redactions
/// and lacks the same withheld content.
/// A file whose redaction this warehouse applied is left out of every commit that carried it, never
/// replaced by a placeholder, and the report names it by path and parcel. So is a file whose content
/// this warehouse lacks because the host of a remote withholds it (the unavailable record), named
/// with the host's statement.
///
/// # Arguments
/// * `path` - Where to create the git repository (must be empty or nonexistent).
///
/// # Returns
/// * `Ok(())`      - If the export completed.
/// * `Err(String)` - If `path` is a non-empty/existing repo, there is nothing to export,
///                   or git could not be written.
pub async fn handle_command(path: &str) -> Result<(), String> {
    // git export walks the whole tree of every parcel; a scoped bay must not silently export a
    // truncated view. Refuse cleanly and point at a full workspace.
    crate::commands::scope::refuse_in_scoped_bay(
        "git export",
        "Run \"forklift git export\" from a full workspace (or the main tree), not a scoped bay.",
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

    // Every parcel reachable from the branch heads, parents before children so a commit's parents
    // are already written when git-commit-tree needs them.
    let order = topological_order(&pallets.iter().map(|(_, head)| head.clone()).collect::<Vec<_>>())?;

    // Everything that can fail or wait on the network runs before the repository exists, so a
    // failure leaves no half-written export that a retry at the same path would refuse.
    //
    // Only what this warehouse holds is exported, so ask the remote (if there is one) what it has
    // that the export does not, rather than let someone leaving from a fresh clone lose it unseen.
    let remote = match RemoteClient::is_configured() {
        true => Some(check_remote(&pallets, &order.iter().cloned().collect()).await),
        false => None,
    };
    let local_meta = pallet_utils::list_meta_pallets()?;
    let signed = local_meta.iter().any(|name| name == office_utils::OFFICE_PALLET_NAME);
    let mut not_exported: Vec<String> = local_meta.into_iter()
        .chain(remote.iter().flat_map(|remote| remote.meta.iter().cloned()))
        .filter(|name| name != tag_utils::TAGS_PALLET_NAME)
        .map(|name| PalletRef::meta(&name).to_wire())
        .collect();
    not_exported.sort();
    not_exported.dedup();
    // Parked work is bay-local, and every bay's is left behind.
    let mut parked = 0;
    for dir in bay_utils::all_bay_state_dirs()? {
        parked += park_utils::read_parked_in(&dir)?.len();
    }
    let RemoteCheck { left_behind, unchecked, .. } = remote.unwrap_or_default();
    let applied = applied_redactions::read_applied()?;
    let unavailable = unavailable::read_unavailable()?;

    std::fs::create_dir_all(target)
        .map_err(|e| format!("Error while creating \"{}\": {}", path, e))?;
    let repo = GitRepo::create(target)?;

    let mut exporter = Exporter::new(&repo, applied, unavailable);
    for parcel in &order {
        exporter.export_parcel(parcel)?;
    }

    // Point each branch at its commit. Refs and the checkout write no objects, so they run under
    // the user's own configuration: from here on the repository is theirs.
    let mut branches: Vec<String> = Vec::new();
    for (name, head) in &pallets {
        let commit = exporter.exported.get(head).map(|exported| exported.commit.clone())
            .ok_or(format!("pallet \"{}\" head was not exported", name))?;
        repo.run_as_user(&["update-ref", &format!("refs/heads/{}", name), &commit])?;
        branches.push(name.clone());
    }

    let tags = export_tags(&repo, &exporter)?;

    // Check out the current pallet's branch (or the first), materializing a working tree.
    let current = pallet_utils::read_position().ok().map(|position| position.pallet)
        .filter(|name| branches.iter().any(|branch| branch == name))
        .or_else(|| branches.first().cloned());

    if let Some(branch) = &current {
        repo.run_as_user(&["symbolic-ref", "HEAD", &format!("refs/heads/{}", branch)])?;
        repo.run_as_user(&["checkout", "-f", "-q", branch])?;
    }

    output::emit(&ExportReport {
        path: path.to_string(),
        commits: exporter.exported.len(),
        trees: exporter.trees_written,
        blobs: exporter.blobs_written,
        branches,
        tags: tags.exported,
        skipped_tags: tags.skipped,
        unreadable_tags: tags.unreadable,
        current,
        not_exported,
        redacted: exporter.take_redacted(),
        withheld: exporter.take_withheld(),
        signed,
        parked,
        remote_left_behind: left_behind,
        remote_unchecked: unchecked,
    });

    Ok(())
}

/// What the configured remote holds that an export does not, or why it could not be asked.
#[derive(Default)]
struct RemoteCheck {
    /// The remote's pallets the export lacks as they stand there.
    left_behind: Vec<LeftBehind>,

    /// The bare names of the remote's meta pallets.
    meta: Vec<String>,

    /// Why the remote could not be asked.
    unchecked: Option<String>,
}

/// Ask the configured remote for its pallets and compare them with what an export of `pallets`
/// (name and head), reaching the parcels `exported`, writes: a user pallet is left behind unless a
/// pallet of its name is exported at or ahead of its remote head (so the export has no branch for
/// it as it stands there), and `@tags` when its remote head is not one the local `@tags`
/// holds. Other meta pallets are never exported, so they are only named. A remote that cannot be
/// asked is reported, not fatal.
async fn check_remote(pallets: &[(String, String)], exported: &HashSet<String>) -> RemoteCheck {
    let mut check = RemoteCheck::default();

    let info = match RemoteClient::for_warehouse() {
        Ok(client) => client.fetch_info().await,
        Err(e) => Err(String::from(e)),
    };
    let info = match info {
        Ok(info) => info,
        Err(why) => {
            check.unchecked = Some(why);
            return check;
        }
    };

    for (wire, head) in &info.pallets {
        let lacked = match PalletRef::parse(wire) {
            Ok(reference) if reference.namespace == PalletNamespace::Meta => {
                if reference.name != tag_utils::TAGS_PALLET_NAME {
                    check.meta.push(reference.name);
                    continue;
                }
                // Local tags that cannot be compared count as lacking the remote's: a warning
                // too many, never one too few.
                let local = pallet_utils::get_pallet_head_in(PalletNamespace::Meta, &reference.name);
                !matches!(local.and_then(|local| relation_to_remote(head, local.as_deref())),
                    Ok(RemoteRelation::Same | RemoteRelation::LocalAhead))
            }
            // The branch of that name must stand at or ahead of the remote's head; the head being
            // reachable through another pallet leaves this one behind or apart from it.
            Ok(_) => {
                let local = pallets.iter().find(|(name, _)| name == wire).map(|(_, head)| head.as_str());
                !matches!(relation_to_remote(head, local),
                    Ok(RemoteRelation::Same | RemoteRelation::LocalAhead))
            }
            // A meta pallet this build cannot name is still never exported.
            Err(_) if wire.starts_with(pallet_utils::META_QUALIFIER) => {
                check.meta.push(wire.trim_start_matches(pallet_utils::META_QUALIFIER).to_string());
                continue;
            }
            // A user pallet this build cannot name has a head the export may still lack.
            Err(_) => !exported.contains(head),
        };

        if lacked {
            check.left_behind.push(LeftBehind { name: wire.clone(), head: head.clone() });
        }
    }

    check
}

/// The tags an export wrote, the ones it left out, and why none were read when they could not be.
struct TagExport {
    exported: Vec<String>,
    skipped: Vec<SkippedTag>,
    unreadable: Option<String>,
}

/// A tag that can be written, with the commit it goes over, its tagger, its signer key (`None` for
/// an unsigned tag) and its git ref.
pub(crate) struct WritableTag<'a> {
    pub(crate) attributed: &'a AttributedTag,
    pub(crate) commit: String,
    pub(crate) tagger: &'a str,
    pub(crate) signer: Option<&'a str>,
    pub(crate) refname: String,
}

/// Sort `tags` into the ones that can be written as annotated git tags and the ones left out, each
/// with why: `commit_of` names a tag's commit or why it has none; a signed tag's tagger is the
/// operator `office` binds its signer key to; git must accept the name; and no two names may differ
/// only in case, among these tags and the git tags `held` names (a repository already holding `V1`
/// cannot also keep `v1` on a filesystem that folds case).
///
/// # Returns
/// * `Ok((Vec<WritableTag>, Vec<SkippedTag>))` - The tags to write, and the tags left out.
/// * `Err(String)`                             - If git could not be run.
pub(crate) fn plan_tags<'a>(repo: &GitRepo,
                            tags: &'a [AttributedTag],
                            office: Result<&'a office_utils::OfficeState, &str>,
                            held: &[String],
                            mut commit_of: impl FnMut(&AttributedTag) -> Result<String, String>)
    -> Result<(Vec<WritableTag<'a>>, Vec<SkippedTag>), String> {
    let mut writable: Vec<WritableTag> = Vec::new();
    let mut skipped: Vec<SkippedTag> = Vec::new();

    for attributed in tags {
        let name = &attributed.tag.name;
        let skip = |reason: String| SkippedTag { name: name.clone(), reason };

        let commit = match commit_of(attributed) {
            Ok(commit) => commit,
            Err(reason) => {
                skipped.push(skip(reason));
                continue;
            }
        };

        // A signed tag's tagger is the operator the office binds the signer key to, a binding that
        // never changes (a retired key stays in the office). Read strictly, so a machine whose
        // office cannot be read or lacks the key leaves the tag out instead of writing a different
        // tagger, and a different tag object, than every other machine. An unsigned tag's tagger is
        // the operator it declares, which needs no office.
        let (tagger, signer) = match (&attributed.signer, office) {
            (None, _) => (attributed.tagger.as_str(), None),
            (Some(_), Err(why)) => {
                skipped.push(skip(format!("the office that names its tagger could not be read: {}", why)));
                continue;
            }
            (Some(signer), Ok(office)) => match office.find_key(signer) {
                Some(key) => (key.operator.as_str(), Some(key.key_id.as_str())),
                None => {
                    skipped.push(skip(format!("its signer key {} is not in this warehouse's office", signer)));
                    continue;
                }
            },
        };

        // Forklift's tag names are a superset of git's: `a..b` and `v1.lock` are refused there.
        let refname = format!("refs/tags/{}", name);
        if !repo.output(&["check-ref-format", &refname])?.status.success() {
            skipped.push(skip("git does not accept it as a tag name".to_string()));
            continue;
        }

        writable.push(WritableTag { attributed, commit, tagger, signer, refname });
    }

    // Git keeps a loose ref as a file, so on a filesystem that folds case `V1` would overwrite
    // `v1`. Two such names are both left out, on every filesystem, so the result does not depend
    // on the one it is written to. A held tag of the very same name is that tag, not a collision.
    let mut folded: HashMap<String, HashSet<&str>> = HashMap::new();
    for name in writable.iter().map(|tag| tag.attributed.tag.name.as_str()).chain(held.iter().map(String::as_str)) {
        folded.entry(name.to_ascii_lowercase()).or_default().insert(name);
    }

    let (kept, collided): (Vec<WritableTag>, Vec<WritableTag>) = writable.into_iter()
        .partition(|tag| folded[&tag.attributed.tag.name.to_ascii_lowercase()].len() == 1);
    skipped.extend(collided.into_iter().map(|tag| SkippedTag {
        name: tag.attributed.tag.name.clone(),
        reason: "another tag's name differs from it only in case, which git cannot keep apart on \
            every filesystem".to_string(),
    }));

    Ok((kept, skipped))
}

/// Write every tag at the local `@tags` head whose subject this export wrote as an annotated git
/// tag, and name every other one with the reason it was left out. A tag never brings history of
/// its own: one on a parcel no exported branch holds is left out, not exported with that parcel.
/// Tag refs write no object, so like the branches they are set under the user's configuration.
///
/// The branches are already written when this runs, so nothing about a tag fails the export: tags
/// that cannot be read at all are one warning, and a tag git refuses is left out with git's reason.
///
/// # Returns
/// * `Ok(TagExport)` - The tags exported, the tags left out, and why none were read.
/// * `Err(String)`   - If git could not be run.
fn export_tags(repo: &GitRepo, exporter: &Exporter) -> Result<TagExport, String> {
    let mut export = TagExport { exported: Vec::new(), skipped: Vec::new(), unreadable: None };

    let read = match tag_utils::read_tags() {
        Ok(read) => read,
        Err(why) => {
            export.unreadable = Some(format!("the tags could not be read: {}", why));
            return Ok(export);
        }
    };
    export.skipped = read.unreadable.iter()
        .map(|tag| SkippedTag { name: tag.name.clone(), reason: format!("it cannot be read: {}", tag.reason) })
        .collect();
    if read.tags.is_empty() {
        return Ok(export);
    }

    // The tags that could be written: on an exported parcel, unsigned or by a key the office holds,
    // under a name git accepts.
    let office = office_utils::read_office_state();
    let (writable, skipped) = plan_tags(repo, &read.tags, office.as_ref().map_err(String::as_str), &[], |attributed| {
        exporter.commit_of(&attributed.tag.subject).map(str::to_string)
            .ok_or(format!("its parcel {} is on no exported branch", attributed.tag.subject))
    })?;
    export.skipped.extend(skipped);

    for WritableTag { attributed, commit, tagger, signer, refname } in writable {
        let name = &attributed.tag.name;
        let skip = |reason: String| SkippedTag { name: name.clone(), reason };

        // Git checks a tag object's text (a message holding a NUL, say), and a ref name can still
        // be one the filesystem refuses (`con` on Windows): either leaves that tag out. A ref write
        // can land and still report failure, so the ref read back decides.
        let object = match exporter.write_tag(&commit, tagger, signer, attributed) {
            Ok(object) => object,
            Err(refused) => {
                export.skipped.push(skip(refused));
                continue;
            }
        };
        match repo.run_as_user(&["update-ref", &refname, &object]) {
            Ok(_) => export.exported.push(name.clone()),
            Err(_) if repo.run(&["rev-parse", "--verify", "-q", &refname], None)
                .is_ok_and(|landed| landed.trim() == object) => export.exported.push(name.clone()),
            Err(refused) => export.skipped.push(skip(refused)),
        }
    }

    Ok(export)
}

/// The variables that tell git which repository to act on. A git call into a repository Forklift
/// names by path never inherits them, even under the user's own configuration.
const REPOSITORY_VARIABLES: &[&str] = &[
    "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES", "GIT_COMMON_DIR", "GIT_NAMESPACE", "GIT_CEILING_DIRECTORIES",
];

/// A git repository Forklift created and writes objects into, with a configuration Forklift alone
/// writes (§9.8).
pub(crate) struct GitRepo {
    path: PathBuf,
    /// A bare repository is addressed by `GIT_DIR`, so a damaged one fails instead of git
    /// discovering a repository around it.
    bare: bool,
}

impl GitRepo {
    /// Create a sha1 repository at the existing, empty folder `path`, configured so nothing in it
    /// rewrites what Forklift hands git: on macOS `git init` turns core.precomposeunicode on, which
    /// rewrites an NFD name.
    pub(crate) fn create(path: &Path) -> Result<GitRepo, String> {
        let path = std::path::absolute(path)
            .map_err(|e| format!("Error while resolving \"{}\": {}", path.display(), e))?;
        let repo = GitRepo { path, bare: false };
        repo.run(&["init", "-q", "--object-format=sha1"], None)?;
        repo.run(&["config", "core.precomposeunicode", "false"], None)?;

        Ok(repo)
    }

    /// Create a bare sha1 repository at the existing, empty folder `path`, configured as
    /// [`GitRepo::create`] configures one.
    pub(crate) fn create_bare(path: &Path) -> Result<GitRepo, String> {
        let repo = GitRepo::open_bare(path)?;
        repo.run(&["init", "-q", "--bare", "--object-format=sha1"], None)?;
        repo.run(&["config", "core.precomposeunicode", "false"], None)?;

        Ok(repo)
    }

    /// The bare repository at `path`, which [`GitRepo::create_bare`] created.
    pub(crate) fn open_bare(path: &Path) -> Result<GitRepo, String> {
        let path = std::path::absolute(path)
            .map_err(|e| format!("Error while resolving \"{}\": {}", path.display(), e))?;

        Ok(GitRepo { path, bare: true })
    }

    /// Run `git <args>` in this repository under the isolated configuration every object write
    /// needs: no system config, an empty global config and no inherited `GIT_*` variable (the
    /// caller adds identity and dates through `env`). The user's configuration reached object ids
    /// through at least four channels (a commit encoding, CRLF filters, the default object format,
    /// unicode precomposition), so it is shut out as a class rather than key by key.
    ///
    /// # Returns
    /// * `Ok(String)`  - Git's stdout.
    /// * `Err(String)` - If git could not be run or failed; the message quotes git.
    fn run_isolated(&self, args: &[&str], env: &[(&str, &str)], input: Option<&[u8]>) -> Result<String, String> {
        let mut command = self.isolated_command();
        for (key, value) in env {
            command.env(key, value);
        }

        run_git(command, args, input)
    }

    pub(crate) fn run(&self, args: &[&str], input: Option<&[u8]>) -> Result<String, String> {
        self.run_isolated(args, &[], input)
    }

    /// Run `git <args>` under the isolated configuration and return its raw output whatever its
    /// exit status, for a call whose status is an answer (`merge-base --is-ancestor`) or whose
    /// output is not text (`cat-file` of a commit someone else wrote).
    ///
    /// # Returns
    /// * `Ok(Output)`  - Git's status, stdout and stderr.
    /// * `Err(String)` - If git could not be run.
    pub(crate) fn output(&self, args: &[&str]) -> Result<std::process::Output, String> {
        git_output(self.isolated_command(), args, None)
    }

    /// Run `git <args>` in this repository under the user's own configuration, for a call that
    /// writes no object.
    fn run_as_user(&self, args: &[&str]) -> Result<String, String> {
        run_git(self.command(|key| REPOSITORY_VARIABLES.contains(&key)), args, None)
    }

    /// Run a `git fetch` or `git push` against another repository under the user's own
    /// configuration, so credential helpers keep working, and return its raw output whatever its
    /// exit status. Git runs in `from`, so a relative URL is read from there, and runs no hook:
    /// the user's own hooks are for their repositories, not this one. Stdin is always null. When
    /// `unattended`, git must not prompt either: a lift holds its locks while it waits, so a prompt
    /// git or Git Credential Manager would show fails at once instead (a helper's own window, or an
    /// ssh passphrase prompt, is not covered).
    ///
    /// # Returns
    /// * `Ok(Output)`  - Git's status, stdout and stderr.
    /// * `Err(String)` - If git could not be run.
    pub(crate) fn network(&self, args: &[&str], from: &Path, unattended: bool) -> Result<std::process::Output, String> {
        // No `-C`, which would read a relative URL from the repository: GIT_DIR finds it instead.
        let mut command = self.stripped_command(|key| REPOSITORY_VARIABLES.contains(&key));
        command.current_dir(from).env("GIT_DIR", self.git_dir());
        let mut no_hooks = std::ffi::OsString::from("core.hooksPath=");
        no_hooks.push(self.git_dir().join("forklift-no-hooks"));
        command.arg("-c").arg(no_hooks);
        if unattended {
            command.env("GIT_TERMINAL_PROMPT", "0").env("GCM_INTERACTIVE", "never");
        }

        git_output(command, args, None)
    }

    /// `git -C <path>` under the isolated configuration (see [`GitRepo::run_isolated`]).
    fn isolated_command(&self) -> Command {
        let mut command = self.command(|key| key.starts_with("GIT_"));
        // A missing global config file also reads as empty, so this holds on a git that does not
        // map /dev/null. A git before 2.32 ignores GIT_CONFIG_GLOBAL and reads the global config
        // from the home folder, so that points at a folder that does not exist.
        let no_home = self.git_dir().join("forklift-no-home");
        command.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("HOME", &no_home).env("XDG_CONFIG_HOME", &no_home);

        command
    }

    /// The repository's git folder.
    pub(crate) fn git_dir(&self) -> PathBuf {
        if self.bare { self.path.clone() } else { self.path.join(".git") }
    }

    /// `git -C <path>`, with every inherited variable `strip` names removed.
    fn command(&self, strip: impl Fn(&str) -> bool) -> Command {
        let mut command = self.stripped_command(strip);
        command.arg("-C").arg(&self.path);
        if self.bare {
            command.env("GIT_DIR", &self.path);
        }

        command
    }

    /// `git`, with every inherited variable `strip` names removed.
    fn stripped_command(&self, strip: impl Fn(&str) -> bool) -> Command {
        let mut command = Command::new("git");

        // Windows treats variable names case-insensitively, so compare uppercased.
        for (key, _) in std::env::vars_os() {
            if strip(&key.to_string_lossy().to_ascii_uppercase()) {
                command.env_remove(&key);
            }
        }

        command
    }
}

/// One parcel's exported commit, with the trees a child converts against (D4).
#[derive(Clone)]
struct ExportedCommit {
    commit: String,
    git_tree: String,
    forklift_tree: String,
}

/// Writes forklift parcels into a git repository as commits. Each object is written once per run:
/// whatever this run already converted, at any path, is reused by its forklift hash. Beyond that,
/// a parcel's tree is converted against its first parent's, whose git hashes are read back from
/// the repository, so a parcel built on a commit an earlier run wrote also costs git processes in
/// proportion to its changed paths, not to its tree's size.
///
/// A file whose content is an applied redaction's hash is left out of the git tree, and so is one
/// whose content the unavailable record lists and the store lacks (withheld by the host of a
/// remote); a folder that held nothing else is left out with them. Whatever this run converts it
/// converts that way, but a seeded commit may have been written before the redaction applied, so a
/// tree of one is reused only when nothing under it is redacted; while the redaction record is
/// non-empty, that check reads every tree under such a reused commit once per run, so the cost is
/// no longer only the changed paths. A seeded commit may also have been written while content was
/// absent, which the unavailable record cannot say once the bytes are back, so a mirror keeps its
/// own log of the git trees it wrote leaving withheld content out ([`Self::keep_withheld_log`]),
/// and a logged tree is never reused.
pub(crate) struct Exporter<'a> {
    repo: &'a GitRepo,
    exported: HashMap<String, ExportedCommit>,
    blobs: HashMap<String, String>,
    trees: HashMap<String, String>,
    trees_written: usize,
    blobs_written: usize,
    /// The redactions applied here, whose files every commit leaves out.
    applied: AppliedSet,
    /// The content recorded unavailable here: a file of it the store lacks is left out.
    unavailable: UnavailableSet,
    /// The files each converted forklift tree left out, by path under it, content hash and why. A
    /// tree that left nothing out has no entry.
    omitted_in: HashMap<String, Vec<(String, String, LeftOut)>>,
    /// The git trees written leaving withheld content out, by this run or a logged earlier one,
    /// when this exporter keeps the log; none of them is reused for a forklift tree.
    withheld_trees: Option<HashSet<String>>,
    /// The trees in `withheld_trees` written since the log was last appended to, which must reach
    /// it before a commit names them.
    withheld_unlogged: Vec<String>,
    /// Whether a flush of the log failed in this run, leaving it in doubt: no later commit in the
    /// run is written, since any of them may reuse a tree noted but not logged.
    withheld_log_failed: bool,
    /// The forklift trees that held entries, every one of which was left out.
    emptied: HashSet<String>,
    /// Whether nothing under a forklift tree is redacted, for the trees asked about.
    clean: HashMap<String, bool>,
    /// The files left out of the commits written since the last [`Self::take_redacted`] for a
    /// redaction, by path and content hash, each with the parcels whose commits left it out, in
    /// the order written.
    redacted: BTreeMap<(String, String), Vec<String>>,
    /// As `redacted`, for content withheld by the host of a remote, since the last
    /// [`Self::take_withheld`], each with the host's statement when it was left out.
    withheld: BTreeMap<(String, String), (String, Vec<String>)>,
}

/// Why an export left a file out.
#[derive(Clone, PartialEq, Eq, Debug)]
enum LeftOut {
    /// This warehouse applied a redaction of its content.
    Redacted,
    /// This warehouse lacks its content, which the host of a remote withholds on this statement.
    Withheld(String),
}

/// The log, in a mirror's git folder, of the git trees its runs wrote leaving withheld content out:
/// one hash per line.
const WITHHELD_TREES_LOG: &str = "forklift-withheld-trees";

impl<'a> Exporter<'a> {
    pub(crate) fn new(repo: &'a GitRepo, applied: AppliedSet, unavailable: UnavailableSet) -> Exporter<'a> {
        Exporter {
            repo,
            exported: HashMap::new(),
            blobs: HashMap::new(),
            trees: HashMap::new(),
            trees_written: 0,
            blobs_written: 0,
            applied,
            unavailable,
            omitted_in: HashMap::new(),
            withheld_trees: None,
            withheld_unlogged: Vec::new(),
            withheld_log_failed: false,
            emptied: HashSet::new(),
            clean: HashMap::new(),
            redacted: BTreeMap::new(),
            withheld: BTreeMap::new(),
        }
    }

    /// The files left out of the commits written since the last call for a redaction, sorted by
    /// path.
    pub(crate) fn take_redacted(&mut self) -> Vec<RedactedFile> {
        std::mem::take(&mut self.redacted).into_iter()
            .map(|((path, hash), parcels)| RedactedFile { path, hash, parcels })
            .collect()
    }

    /// The files left out of the commits written since the last call because this warehouse lacks
    /// content the host of a remote withholds, sorted by path, each with the host's statement.
    pub(crate) fn take_withheld(&mut self) -> Vec<WithheldFile> {
        std::mem::take(&mut self.withheld).into_iter()
            .map(|((path, hash), (statement, parcels))| WithheldFile { path, hash, statement, parcels })
            .collect()
    }

    /// Read the unavailable record again, after a fetch that may have added to it, so a file whose
    /// content it now lists is left out rather than read. No tree converted or reused before
    /// depends on the record: a converted one read every file it kept, and a reused one is judged
    /// by the redaction record and the withheld-trees log alone.
    pub(crate) fn reread_unavailable(&mut self) -> Result<(), String> {
        self.unavailable = unavailable::read_unavailable()?;

        Ok(())
    }

    /// Keep the log of the git trees written leaving withheld content out, in the repository's
    /// git folder: read what earlier runs logged, and log each such tree this run writes before it
    /// is used. For a repository whose commits later runs build on (a mirror), so a tree written
    /// while content was absent is never reused once the bytes are back, which no record held
    /// here can tell.
    pub(crate) fn keep_withheld_log(&mut self) -> Result<(), String> {
        let path = self.repo.git_dir().join(WITHHELD_TREES_LOG);
        let logged = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(format!("Could not read {}: {}", path.display(), error)),
        };
        // Text after the last newline is a torn append, from a run stopped before any commit named
        // its tree: no tree, cut off so the next append starts a fresh line. Any whole line
        // unreadable is refused rather than skipped, since a tree it named would then be reused.
        let kept = logged.iter().rposition(|byte| *byte == b'\n').map_or(0, |end| end + 1);
        let whole = &logged[..kept.saturating_sub(1)];
        if kept < logged.len() {
            let fail = |error: std::io::Error| format!("Could not write {}: {}", path.display(), error);
            let log = std::fs::OpenOptions::new().write(true).open(&path).map_err(fail)?;
            log.set_len(kept as u64).map_err(fail)?;
            if file_utils::fsync_enabled() {
                log.sync_all().map_err(fail)?;
            }
        }
        let mut trees = HashSet::new();
        for line in whole.split(|byte| *byte == b'\n').filter(|_| kept > 0) {
            // A git object id: SHA-1 or SHA-256, as git prints it.
            let hash = std::str::from_utf8(line).ok()
                .filter(|hash| matches!(hash.len(), 40 | 64) && hash.bytes().all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')));
            let Some(hash) = hash else {
                return Err(format!("{} holds a line this build cannot read: \"{}\"",
                    path.display(), sanitize(&String::from_utf8_lossy(line))));
            };
            trees.insert(hash.to_string());
        }
        self.withheld_trees = Some(trees);

        Ok(())
    }

    /// Note `git_tree` as written leaving withheld content out, when the log is kept; it reaches
    /// the log at the next [`Self::flush_withheld_log`].
    fn log_withheld_tree(&mut self, git_tree: &str) {
        if let Some(trees) = self.withheld_trees.as_mut() {
            if trees.insert(git_tree.to_string()) {
                self.withheld_unlogged.push(git_tree.to_string());
            }
        }
    }

    /// Append the trees noted since the last flush to the log, durably: called before a commit
    /// that may name them is written.
    fn flush_withheld_log(&mut self) -> Result<(), String> {
        if self.withheld_unlogged.is_empty() {
            return Ok(());
        }
        if self.withheld_log_failed {
            return Err(format!(
                "Writing {} failed earlier in this run, so no further commit is written; run \"forklift git mirror\" again once it can be written",
                self.repo.git_dir().join(WITHHELD_TREES_LOG).display()));
        }
        let flushed = self.append_withheld_log();
        self.withheld_log_failed = flushed.is_err();

        flushed
    }

    /// Append the noted trees to the log and make it durable, its folder too when it is new.
    fn append_withheld_log(&mut self) -> Result<(), String> {
        let dir = self.repo.git_dir();
        let path = dir.join(WITHHELD_TREES_LOG);
        let fail = |error: std::io::Error| format!("Could not write {}: {}", path.display(), error);
        let created = !path.exists();
        // A write handle, not an append-only one, which Windows refuses to flush.
        let mut log = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(&path).map_err(fail)?;
        let end = std::io::Seek::seek(&mut log, std::io::SeekFrom::End(0)).map_err(fail)?;
        let lines: String = self.withheld_unlogged.iter().map(|tree| format!("{}\n", tree)).collect();
        let written = std::io::Write::write_all(&mut log, lines.as_bytes())
            .and_then(|()| if file_utils::fsync_enabled() { log.sync_all() } else { Ok(()) });
        if let Err(error) = written {
            // Cut a partial batch back off; a tail this cannot cut is a torn append the next run's
            // read cuts, since no later flush in this run appends after it.
            let _ = log.set_len(end);
            return Err(fail(error));
        }
        if created {
            file_utils::sync_dir(&dir)?;
        }
        self.withheld_unlogged.clear();

        Ok(())
    }

    /// Whether nothing under the forklift tree `forklift_hash` is a file an applied redaction
    /// names, so a git tree written for it before the redaction applied holds no copy.
    fn is_clean(&mut self, forklift_hash: &str) -> Result<bool, String> {
        if self.applied.is_empty() {
            return Ok(true);
        }
        if let Some(clean) = self.clean.get(forklift_hash) {
            return Ok(*clean);
        }

        let tree = object_utils::load_tree(forklift_hash)?;
        let mut clean = !tree.get_files()
            .any(|(_, item)| self.applied.contains(&item.hash));
        for (_, item) in tree.get_subtrees() {
            if !clean {
                break;
            }
            clean = self.is_clean(&item.hash)?;
        }

        self.clean.insert(forklift_hash.to_string(), clean);
        Ok(clean)
    }

    /// Record that `parcel` is already in the repository as `commit`, whose git tree is
    /// `git_tree`: a commit this build wrote in this layout, which a child is built on (D2).
    pub(crate) fn seed(&mut self, parcel: &str, commit: &str, git_tree: &str, forklift_tree: &str) {
        self.exported.insert(parcel.to_string(), ExportedCommit {
            commit: commit.to_string(),
            git_tree: git_tree.to_string(),
            forklift_tree: forklift_tree.to_string(),
        });
    }

    /// The commit `parcel` was written or seeded as in this run, if it was.
    pub(crate) fn commit_of(&self, parcel: &str) -> Option<&str> {
        self.exported.get(parcel).map(|exported| exported.commit.as_str())
    }

    /// Write `attributed` as an annotated tag object over `commit` (a commit this run wrote) by
    /// `tagger` (the operator the office binds its signer key to, or the one an unsigned tag
    /// declares) under `signer` (that key's id; `None` for an unsigned tag), returning the tag
    /// object's hash. Written under the isolated configuration, like every object.
    pub(crate) fn write_tag(&self,
                            commit: &str,
                            tagger: &str,
                            signer: Option<&str>,
                            attributed: &AttributedTag) -> Result<String, String> {
        Ok(self.repo.run(&["mktag"], Some(tag_object(commit, tagger, signer, attributed).as_bytes()))?.trim().to_string())
    }

    /// Write one parcel as a git commit (its parents must already be written).
    pub(crate) fn export_parcel(&mut self, forklift_hash: &str) -> Result<String, String> {
        if let Some(exported) = self.exported.get(forklift_hash) {
            return Ok(exported.commit.clone());
        }

        let parcel = object_utils::load_parcel(forklift_hash)?;

        // Git drops a repeated -p, so the parent list goes in with repeats removed and the first
        // occurrence kept: the i-th distinct parent's commit is then the commit's i-th parent.
        let mut parents: Vec<&ExportedCommit> = Vec::new();
        for parent in distinct(&parcel.parents) {
            parents.push(self.exported.get(parent)
                .ok_or("a parent commit was not exported before its child".to_string())?);
        }
        let parent_commits: Vec<String> = parents.iter().map(|parent| parent.commit.clone()).collect();
        let base = parents.first().map(|parent| (parent.forklift_tree.clone(), parent.git_tree.clone()));

        let tree = self.convert_tree(&parcel.tree_hash, base.as_ref().map(|(forklift, git)| (forklift.as_str(), git.as_str())))?;
        self.flush_withheld_log()?;
        let mut args = vec!["commit-tree".to_string(), tree.clone()];
        for commit in parent_commits {
            args.extend(["-p".to_string(), commit]);
        }

        // Git's author/committer come from the environment; map them from the parcel's
        // Author and Stack actions so authorship and dates survive.
        let (author, author_time) = action_identity(&parcel, true);
        let (committer, committer_time) = action_identity(&parcel, false);
        let (author_date, committer_date) = (git_date(author_time), git_date(committer_time));

        // The message goes on stdin: git rewrites an argument (core.precomposeunicode), never stdin.
        let commit = self.repo.run_isolated(
            &args.iter().map(String::as_str).collect::<Vec<_>>(),
            &[
                ("GIT_AUTHOR_NAME", &author.0), ("GIT_AUTHOR_EMAIL", &author.1),
                ("GIT_AUTHOR_DATE", &author_date),
                ("GIT_COMMITTER_NAME", &committer.0), ("GIT_COMMITTER_EMAIL", &committer.1),
                ("GIT_COMMITTER_DATE", &committer_date),
            ],
            Some(commit_message(&parcel, forklift_hash).as_bytes()),
        )?.trim().to_string();

        // Reported once the commit that left them out is written.
        if let Some(omitted) = self.omitted_in.get(&parcel.tree_hash) {
            for (path, hash, why) in omitted {
                let key = (path.clone(), hash.clone());
                match why {
                    LeftOut::Redacted => self.redacted.entry(key).or_default(),
                    LeftOut::Withheld(statement) => &mut self.withheld.entry(key)
                        .or_insert_with(|| (statement.clone(), Vec::new())).1,
                }.push(forklift_hash.to_string());
            }
        }
        self.exported.insert(forklift_hash.to_string(), ExportedCommit {
            commit: commit.clone(),
            git_tree: tree,
            forklift_tree: parcel.tree_hash,
        });

        Ok(commit)
    }

    /// Write one forklift tree as a git tree (recursively), against `base`: the forklift tree at the
    /// same path in the first parent, with the git tree it was exported as. An entry this run has
    /// not converted yet, whose forklift hash matches the base's entry of the same name and kind,
    /// reuses that entry's git hash. Only the hash: the mode is always recomputed from the entry's
    /// own type. A file an applied redaction names, or one whose content the store lacks and the
    /// unavailable record lists, is left out, never reused, and a base tree equal to this one is
    /// reused only when nothing under it is redacted and the withheld-trees log does not name it.
    fn convert_tree(&mut self, forklift_hash: &str, base: Option<(&str, &str)>) -> Result<String, String> {
        if let Some(hash) = self.trees.get(forklift_hash) {
            return Ok(hash.clone());
        }
        if let Some((base_forklift, base_git)) = base {
            let logged = self.withheld_trees.as_ref().is_some_and(|trees| trees.contains(base_git));
            if base_forklift == forklift_hash && !logged && self.is_clean(forklift_hash)? {
                self.trees.insert(forklift_hash.to_string(), base_git.to_string());
                return Ok(base_git.to_string());
            }
        }

        let tree = object_utils::load_tree(forklift_hash)?;
        let base = match base {
            Some((base_forklift, base_git)) => Some((object_utils::load_tree(base_forklift)?, base_git)),
            None => None,
        };
        // The base's git entries, listed only once an entry needs one.
        let mut base_entries: Option<HashMap<String, (String, String)>> = None;

        let mut entries = String::new();
        let mut omitted: Vec<(String, String, LeftOut)> = Vec::new();

        for (name, item) in tree.get_files() {
            // Left out, never replaced: placeholder content would read as a real version of it. A
            // redaction outranks a withholding, as it does on every read.
            if self.applied.contains(&item.hash) {
                omitted.push((name.to_string(), item.hash.clone(), LeftOut::Redacted));
                continue;
            }
            if self.unavailable.lacks(&item.hash)? {
                let statement = self.unavailable.statement(&item.hash)
                    .ok_or(format!("The unavailable record lacks {} but holds no statement for it", item.hash))?;
                omitted.push((name.to_string(), item.hash.clone(), LeftOut::Withheld(statement.to_string())));
                continue;
            }

            let mode = match item.item_type {
                DirEntryType::Executable | DirEntryType::ExecutableChunked => "100755",
                DirEntryType::SymbolicLink => "120000",
                _ => "100644",
            };

            let mut blob = self.blobs.get(&item.hash).cloned();
            if let Some((base_tree, base_git)) = &base {
                if blob.is_none() && base_tree.get_file(name).is_some_and(|base_item| base_item.hash == item.hash) {
                    blob = self.base_entry(&mut base_entries, base_git, name, "blob")?;
                }
            }
            let blob = match blob {
                Some(blob) => blob,
                // A chunked file's hash is a recipe: stream-assemble it into the git blob rather
                // than loading it as a blob (which would fail) or reading it whole into memory.
                None if item.item_type.is_chunked() => self.convert_chunked_blob(&item.hash)?,
                None => self.convert_blob(&item.hash)?,
            };
            self.blobs.insert(item.hash.clone(), blob.clone());
            entries.push_str(&format!("{} blob {}\t{}\0", mode, blob, name));
        }

        for (name, item) in tree.get_subtrees() {
            // Converted against the base's subtree of the same name, which returns its git hash
            // outright when the two are equal.
            let mut base_subtree = None;
            if let Some((base_tree, base_git)) = &base {
                if let Some(base_item) = base_tree.get_subtree(name).filter(|_| !self.trees.contains_key(&item.hash)) {
                    base_subtree = self.base_entry(&mut base_entries, base_git, name, "tree")?
                        .map(|git_hash| (base_item.hash.clone(), git_hash));
                }
            }
            let subtree = self.convert_tree(&item.hash, base_subtree.as_ref().map(|(forklift, git)| (forklift.as_str(), git.as_str())))?;
            if let Some(under) = self.omitted_in.get(&item.hash) {
                omitted.extend(under.iter().map(|(path, hash, why)| (format!("{}/{}", name, path), hash.clone(), why.clone())));
            }
            // A folder that held only files left out is left out with them.
            if !self.emptied.contains(&item.hash) {
                entries.push_str(&format!("040000 tree {}\t{}\0", subtree, name));
            }
        }
        if entries.is_empty() && !omitted.is_empty() {
            self.emptied.insert(forklift_hash.to_string());
        }
        let withheld = omitted.iter().any(|(_, _, why)| matches!(why, LeftOut::Withheld(_)));

        if !omitted.is_empty() {
            self.omitted_in.insert(forklift_hash.to_string(), omitted);
        }

        // NUL-terminated (`-z`): a name may hold a newline or tab, and without `-z` mktree also
        // C-unquotes a name that begins with `"`, writing a different name than the tree holds.
        // A name never holds NUL (the tree parser refuses one), so NUL frames every entry.
        let git_hash = self.repo.run(&["mktree", "-z"], Some(entries.as_bytes()))?.trim().to_string();
        // Logged before any commit names it (see export_parcel), so no later run reuses it.
        if withheld {
            self.log_withheld_tree(&git_hash);
        }
        self.trees_written += 1;
        self.trees.insert(forklift_hash.to_string(), git_hash.clone());

        Ok(git_hash)
    }

    /// The git hash of the entry `name` of git kind `kind` in the git tree `base_git`, listing that
    /// tree into `entries` on first use.
    fn base_entry(&self, entries: &mut Option<HashMap<String, (String, String)>>, base_git: &str, name: &str, kind: &str)
        -> Result<Option<String>, String> {
        if entries.is_none() {
            *entries = Some(self.list_tree(base_git)?);
        }

        Ok(entries.as_ref().and_then(|entries| entries.get(name))
            .filter(|(entry_kind, _)| entry_kind == kind)
            .map(|(_, git_hash)| git_hash.clone()))
    }

    /// The entries of the git tree `git_hash`: name to (kind, hash). Read with `-z`, since plain
    /// ls-tree C-quotes an awkward name.
    fn list_tree(&self, git_hash: &str) -> Result<HashMap<String, (String, String)>, String> {
        let listing = self.repo.run(&["ls-tree", "-z", git_hash], None)?;
        let mut entries = HashMap::new();

        for entry in listing.split_terminator('\0') {
            // "<mode> SP <kind> SP <hash> TAB <name>"
            let parsed = entry.split_once('\t').and_then(|(info, name)| {
                let mut fields = info.split(' ');
                match (fields.next(), fields.next(), fields.next()) {
                    (Some(_), Some(kind), Some(hash)) => Some((name, kind, hash)),
                    _ => None,
                }
            });
            let (name, kind, hash) = parsed.ok_or(format!("git ls-tree printed an entry this build cannot read: \"{}\"", entry))?;
            entries.insert(name.to_string(), (kind.to_string(), hash.to_string()));
        }

        Ok(entries)
    }

    /// Write one forklift blob as a git blob. Hashing from stdin applies no filter.
    fn convert_blob(&mut self, forklift_hash: &str) -> Result<String, String> {
        let content = object_utils::load_blob(forklift_hash)?.content;
        let git_hash = self.repo.run(&["hash-object", "-w", "--stdin"], Some(&content))?.trim().to_string();
        self.blobs_written += 1;

        Ok(git_hash)
    }

    /// Write one chunked forklift file as a git blob by stream-assembling its recipe to a temp
    /// file (bounded to one chunk in memory at a time, integrity-verified), then hashing that file
    /// into the git object store. The assembled bytes round-trip exactly: `--no-filters`, since a
    /// path, unlike stdin, picks up the repository's attributes and end-of-line conversion.
    fn convert_chunked_blob(&mut self, recipe_hash: &str) -> Result<String, String> {
        let (temp_path, file) = create_export_temp(recipe_hash, &TEMP_COUNTER)?;

        let assemble = || -> Result<(), String> {
            let mut writer = std::io::BufWriter::new(file);
            object_utils::assemble_chunked_file(recipe_hash, &mut writer)?;
            std::io::Write::flush(&mut writer)
                .map_err(|e| format!("Error while flushing a temp file for git export: {}", e))
        };

        if let Err(e) = assemble() {
            let _ = std::fs::remove_file(&temp_path);
            return Err(e);
        }

        let temp_str = temp_path.to_string_lossy().into_owned();
        let git_hash = self.repo.run(&["hash-object", "-w", "--no-filters", "--", &temp_str], None)
            .map(|h| h.trim().to_string());
        let _ = std::fs::remove_file(&temp_path);
        let git_hash = git_hash?;
        self.blobs_written += 1;

        Ok(git_hash)
    }
}

/// Numbers [`create_export_temp`]'s files.
static TEMP_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Create a fresh temp file to assemble the chunked blob `recipe_hash` into for `git hash-object`,
/// its name numbered from `counter` as well as by the pid, so no two calls in one process share it.
/// The file is created new, never opened by name: a shared, world-writable temp folder is no place
/// to follow or truncate a planted symlink. A name already taken, by such a plant or by a killed
/// earlier process with this one's id, is stepped past.
fn create_export_temp(recipe_hash: &str, counter: &std::sync::atomic::AtomicU64)
    -> Result<(std::path::PathBuf, std::fs::File), String> {
    let candidates = file_utils::pid_numbered(counter, |pid, n| {
        std::env::temp_dir().join(format!("forklift-export-{}-{}-{}.tmp", pid, n, recipe_hash))
    });

    file_utils::create_first_new_file(candidates)
        .map_err(|e| format!("Error while creating a temp file for git export: {}", e))
}

/// The (name, email) and unix time of a parcel's authoring action (`want_author`) or its
/// stacking action, falling back to the first action of any kind (every parcel has one), as the
/// [`git_identity`] of its operator.
fn action_identity(parcel: &Parcel, want_author: bool) -> ((String, String), i64) {
    let action = parcel.actions.iter()
        .find(|action| matches!(action.action, ParcelActionType::Author) == want_author)
        .or_else(|| parcel.actions.first());

    match action {
        Some(action) => (git_identity(&action.operator.identifier), action.timestamp.timestamp()),
        None => (("forklift".to_string(), "forklift@localhost".to_string()), 0),
    }
}

/// The (name, email) git writes for an operator id. Forklift records no names, so both are the id,
/// cleaned the way git cleans an identity so git writes it as given. An id that cleans to nothing,
/// which git refuses as a name, is named "forklift".
fn git_identity(identifier: &str) -> (String, String) {
    let email = without_crud(identifier);
    let name = if email.is_empty() { "forklift".to_string() } else { email.clone() };

    (name, email)
}

/// `value` as git writes it into an identity (ident.c): trimmed of "crud" (control characters,
/// space and `,:;<>"\'`) at both ends, with every newline, `<` and `>` inside dropped.
fn without_crud(value: &str) -> String {
    let crud = |c: char| c <= ' ' || matches!(c, ',' | ':' | ';' | '<' | '>' | '"' | '\\' | '\'');

    value.trim_matches(crud).chars().filter(|c| !matches!(c, '\n' | '<' | '>')).collect()
}

/// A git date for unix time `seconds`. The `@` form, since git reads a bare number before March
/// 1973 as something other than a unix time and refuses it. Git has no date before 1970, so an
/// earlier one is written as the epoch.
fn git_date(seconds: i64) -> String {
    format!("@{} +0000", seconds.max(0))
}

/// The last line of every exported commit's message: the parcel it was exported from, and the
/// export layout it was written in.
pub(crate) fn parcel_line(parcel_hash: &str) -> String {
    format!("Forklift-Parcel: {} export/{}", parcel_hash, EXPORT_LAYOUT)
}

/// The commit message for `parcel`: its description without trailing newlines, a blank line, and
/// the parcel line; only the parcel line when the description is empty.
fn commit_message(parcel: &Parcel, parcel_hash: &str) -> String {
    match parcel.description.as_deref().map(|d| d.trim_end_matches('\n')).filter(|d| !d.is_empty()) {
        Some(description) => format!("{}\n\n{}\n", description, parcel_line(parcel_hash)),
        None => format!("{}\n", parcel_line(parcel_hash)),
    }
}

/// The last line of every exported tag's message: the key the tag was signed with, which git has no
/// place for, or that it was written without one, and the export layout it was written in. An
/// unsigned tag's line names no key, so its tagger reads as declared, never as vouched for.
fn tag_line(signer: Option<&str>) -> String {
    match signer {
        Some(signer) => format!("Forklift-Tag-Signer: {} export/{}", signer, EXPORT_LAYOUT),
        None => format!("Forklift-Tag-Unsigned: export/{}", EXPORT_LAYOUT),
    }
}

/// The annotated tag object for `attributed` over `commit`, signed by key `signer` (`None` when it
/// was written without a key). The tagger is
/// `tagger`'s identity, as a commit's author is; the date is the tag's own; the message is the
/// tag's, without trailing newlines, a blank line, and the tag line (only the tag line when the
/// message is empty).
fn tag_object(commit: &str, tagger: &str, signer: Option<&str>, attributed: &AttributedTag) -> String {
    let tag = &attributed.tag;
    let (name, email) = git_identity(tagger);

    let message = match tag.message.trim_end_matches('\n') {
        "" => format!("{}\n", tag_line(signer)),
        message => format!("{}\n\n{}\n", message, tag_line(signer)),
    };

    format!(
        "object {}\ntype commit\ntag {}\ntagger {} <{}> {} +0000\n\n{}",
        commit, tag.name, name, email, tag.tagged_at.max(0), message
    )
}

/// `items` with repeats removed, first occurrence kept, in order.
pub(crate) fn distinct(items: &[String]) -> Vec<&String> {
    let mut seen = HashSet::new();
    items.iter().filter(|item| seen.insert(*item)).collect()
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

/// Run `command` (`git -C <path>`) with `args`, `input` on stdin (or none at all, so git never
/// waits on the terminal), returning its stdout.
fn run_git(command: Command, args: &[&str], input: Option<&[u8]>) -> Result<String, String> {
    let output = git_output(command, args, input)?;

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

/// Run `command` with `args` and `input` as [`run_git`] does, returning its output whatever its
/// exit status.
fn git_output(mut command: Command, args: &[&str], input: Option<&[u8]>) -> Result<std::process::Output, String> {
    let mut child = command
        .args(args)
        .stdin(if input.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("Could not run git (is it installed and on your PATH?): {}", e))?;

    if let Some(input) = input {
        child.stdin.take().expect("stdin is piped").write_all(input)
            .map_err(|e| format!("Error while writing to git: {}", e))?;
    }

    child.wait_with_output()
        .map_err(|e| format!("Error while running git: {}", e))
}

/// The result of a git export.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct ExportReport {
    path: String,
    commits: usize,

    /// The git trees written. A tree unchanged from its parcel's first parent is reused, not
    /// written again.
    trees: usize,

    /// The git blobs written. A file unchanged from its parcel's first parent is reused, not
    /// written again.
    blobs: usize,

    /// The branches created (one per user pallet).
    branches: Vec<String>,

    /// The tags exported as annotated git tags.
    tags: Vec<String>,

    /// The tags left out, each with why.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skipped_tags: Vec<SkippedTag>,

    /// Why no tag was exported, when the tags could not be read.
    #[serde(skip_serializing_if = "Option::is_none")]
    unreadable_tags: Option<String>,

    /// The branch checked out (the current pallet).
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,

    /// The meta pallets git has no home for, so not exported, in their qualified form (`@haul`):
    /// this warehouse's and, when it was asked, the remote's. `@tags` is exported, so never here.
    not_exported: Vec<String>,

    /// The files left out because this warehouse applied a redaction of their content: each by
    /// path, with the parcels whose commits carry no version of it in its place.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    redacted: Vec<RedactedFile>,

    /// The files left out because this warehouse lacks their content, which the host of a remote
    /// withholds (the unavailable record): each by path, with the host's statement and the parcels
    /// whose commits carry no version of it in its place.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    withheld: Vec<WithheldFile>,

    /// Whether this warehouse has an office, so its parcels and tags carry signatures the export
    /// drops.
    #[serde(skip)]
    signed: bool,

    /// How many parcels are parked (`park`), across every bay. Parked work is on no pallet, so it is
    /// not exported.
    #[serde(skip_serializing_if = "is_zero")]
    parked: usize,

    /// The configured remote's pallets this export lacks as they stand there: a user pallet with no
    /// branch here, or whose remote head is not an exported parcel, or `@tags` when the remote's is
    /// not one held here. Empty when no remote is configured or it could not be asked.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    remote_left_behind: Vec<LeftBehind>,

    /// Why the configured remote could not be asked which pallets it holds.
    #[serde(skip_serializing_if = "Option::is_none")]
    remote_unchecked: Option<String>,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// A pallet of the remote that an export lacks as it stands there.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Clone)]
pub(crate) struct LeftBehind {
    /// The pallet's name, qualified for a meta pallet (`@tags`).
    name: String,

    /// Its head on the remote.
    head: String,
}

/// A file an export or a mirror left out of the commits it wrote, because this warehouse applied a
/// redaction of its content.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Debug)]
pub(crate) struct RedactedFile {
    /// The file's path in the parcels' trees, `/`-separated.
    path: String,

    /// Its content's hash: the redaction's.
    hash: String,

    /// The parcels whose commits left it out, in the order they were written.
    parcels: Vec<String>,
}

/// Print each of `redacted` as a line of human output.
pub(crate) fn render_redacted(redacted: &[RedactedFile]) {
    for line in redacted_lines(redacted) {
        println!("  {line}");
    }
}

/// One line per file an export left out because its redaction is applied here, unindented.
pub(crate) fn redacted_lines(redacted: &[RedactedFile]) -> Vec<String> {
    redacted.iter()
        .map(|file| format!(
            "Left out \"{}\" ({}) of {} commit(s): its redaction is applied here.",
            sanitize(&file.path), file.hash, file.parcels.len()
        ))
        .collect()
}

/// A file an export or a mirror left out of the commits it wrote, because this warehouse lacks its
/// content, which the host of a remote withholds.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize, Debug)]
pub(crate) struct WithheldFile {
    /// The file's path in the parcels' trees, `/`-separated.
    path: String,

    /// Its content's hash.
    hash: String,

    /// The statement of the host that withholds it, as this warehouse recorded it.
    statement: String,

    /// The parcels whose commits left it out, in the order they were written.
    parcels: Vec<String>,
}

/// Print each of `withheld` as a line of human output.
pub(crate) fn render_withheld(withheld: &[WithheldFile]) {
    for line in withheld_lines(withheld) {
        println!("  {line}");
    }
}

/// One line per file an export left out because this warehouse lacks its withheld content,
/// unindented. The statement is the host's text.
pub(crate) fn withheld_lines(withheld: &[WithheldFile]) -> Vec<String> {
    withheld.iter()
        .map(|file| format!(
            "Left out \"{}\" ({}) of {} commit(s): this warehouse does not hold its content, which the \
            host of a remote withholds. The host's statement: {}",
            sanitize(&file.path), file.hash, file.parcels.len(), sanitize(&file.statement)
        ))
        .collect()
}

/// How an export's human output names a meta pallet it did not export.
fn describe_not_exported(name: &str) -> String {
    let bare = name.trim_start_matches(pallet_utils::META_QUALIFIER);
    match bare {
        office_utils::OFFICE_PALLET_NAME => "the signed office (@office)".to_string(),
        manifest_utils::MANIFEST_PALLET_NAME => "@manifest (approvals and provenance)".to_string(),
        haul_utils::HAUL_PALLET_NAME => "@haul (hauls and their reviews)".to_string(),
        kv_utils::KV_PALLET_NAME => "@kv (custom metadata)".to_string(),
        _ => name.to_string(),
    }
}

impl CommandOutput for ExportReport {
    fn render_human(&self) {
        println!(
            "Exported {} commit(s) into a git repository at \"{}\" ({} branch(es), {} tree(s), {} blob(s)).",
            self.commits, self.path, self.branches.len(), self.trees, self.blobs
        );

        if !self.tags.is_empty() {
            println!("Exported {} tag(s) as annotated git tags.", self.tags.len());
        }

        // An unreadable tag's reason quotes the cell, which is another operator's text.
        for skipped in &self.skipped_tags {
            println!("  warning: tag \"{}\" was not exported: {}.", skipped.name, sanitize(&skipped.reason));
        }
        if let Some(why) = &self.unreadable_tags {
            println!("  warning: no tag was exported: {}.", sanitize(why));
        }

        if let Some(current) = &self.current {
            println!("Checked out \"{}\".", current);
        }

        render_redacted(&self.redacted);
        render_withheld(&self.withheld);

        // A remote's pallet names are its own text.
        for left in &self.remote_left_behind {
            match PalletRef::parse(&left.name) {
                Ok(reference) if reference == PalletRef::meta(tag_utils::TAGS_PALLET_NAME) => println!(
                    "  warning: the remote's tags include some this export does not; a \"forklift lower\" \
                    of any pallet brings them here."
                ),
                Ok(_) => println!(
                    "  warning: the export has no branch for the remote's pallet \"{0}\" as it stands \
                    there; \"forklift lower {0}\" brings it here.",
                    sanitize(&left.name)
                ),
                Err(_) => println!(
                    "  warning: the export lacks the remote's pallet \"{}\", whose name this build \
                    cannot read.",
                    sanitize(&left.name)
                ),
            }
        }
        if !self.remote_left_behind.is_empty() {
            println!("  Then export again, into a new path.");
        }
        if let Some(why) = &self.remote_unchecked {
            println!(
                "  warning: could not ask the remote which pallets it holds, so this export may lack \
                some: {}. \"forklift pallet list --remote\" lists them.",
                sanitize(why)
            );
        }

        if self.parked > 0 {
            println!(
                "  warning: {} parked parcel(s) were not exported. \"forklift park pop\", in a bay \
                that parked one, brings back its latest, to stack before exporting again.",
                self.parked
            );
        }

        // Only a warehouse with an office signs anything; a remote's office signed nothing here.
        let mut dropped: Vec<String> = self.not_exported.iter().map(|name| describe_not_exported(name)).collect();
        if self.signed {
            dropped.push("the signatures on parcels and tags".to_string());
        }
        if !dropped.is_empty() {
            println!("Not exported, as git has no equivalent: {}.", dropped.join(", "));
        }
    }
}

/// A tag an export or a mirror left out.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct SkippedTag {
    /// The tag's name.
    pub(crate) name: String,

    /// Why it was left out.
    pub(crate) reason: String,
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<ExportReport>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use forklift_core::builder::object::loose_object_builder::LooseObjectBuilder;
    use forklift_core::model::blob::Blob;
    use forklift_core::model::chunk::Chunk;
    use forklift_core::model::operator::Operator;
    use forklift_core::model::parcel_action::ParcelAction;
    use forklift_core::model::recipe::{Recipe, RecipeChunk};
    use forklift_core::model::tree_item::TreeItem;
    use crate::test_support::Warehouse;

    /// A file of a fixture tree: its name, type and content (a chunked type is stored as a recipe
    /// over `chunks`).
    enum Entry<'a> {
        File(&'a str, DirEntryType, &'a [u8]),
        Chunked(&'a str, DirEntryType, &'a [&'a [u8]]),
        Dir(&'a str, Vec<Entry<'a>>),
    }

    fn store_blob(content: &[u8]) -> String {
        let mut blob = LooseObjectBuilder::build_blob(&Blob { content: content.to_vec() });
        blob.store().unwrap();
        blob.hash
    }

    fn store_recipe(chunks: &[&[u8]]) -> String {
        let mut recipe_chunks = Vec::new();
        for chunk in chunks {
            let mut object = LooseObjectBuilder::build_chunk(&Chunk { content: chunk.to_vec() });
            object.store().unwrap();
            recipe_chunks.push(RecipeChunk { hash: object.hash, size: chunk.len() as u64 });
        }
        let whole = chunks.concat();
        let mut recipe = LooseObjectBuilder::build_recipe(&Recipe {
            content_hash: object_utils::hash_object_bytes(&whole),
            total_size: whole.len() as u64,
            chunks: recipe_chunks,
        });
        recipe.store().unwrap();
        recipe.hash
    }

    fn store_tree(entries: &[Entry]) -> String {
        let mut root = TreeItem::new(String::new(), String::new(), DirEntryType::Tree);
        for entry in entries {
            let child = match entry {
                Entry::File(name, kind, content) => TreeItem::new(name.to_string(), store_blob(content), *kind),
                Entry::Chunked(name, kind, chunks) => TreeItem::new(name.to_string(), store_recipe(chunks), *kind),
                Entry::Dir(name, entries) => TreeItem::new(name.to_string(), store_tree(entries), DirEntryType::Tree),
            };
            root.add_child(child);
        }
        let mut tree = LooseObjectBuilder::build_tree(&root);
        tree.store().unwrap();
        tree.hash
    }

    fn action(action: ParcelActionType, operator: &str, seconds: i64) -> ParcelAction {
        ParcelAction {
            operator: Operator { identifier: operator.to_string() },
            action,
            description: None,
            timestamp: DateTime::<Utc>::from_timestamp(seconds, 0).unwrap(),
        }
    }

    fn store_parcel(tree: &str, parents: &[&str], actions: Vec<ParcelAction>, description: Option<&str>) -> String {
        let mut parcel = LooseObjectBuilder::build_parcel(&Parcel {
            tree_hash: tree.to_string(),
            parents: parents.iter().map(|parent| parent.to_string()).collect(),
            actions,
            description: description.map(str::to_string),
        });
        parcel.store().unwrap();
        parcel.hash
    }

    /// A fresh repository for `warehouse` to export into.
    fn create_repo(warehouse: &Warehouse) -> GitRepo {
        let path = warehouse.root.join("exported");
        std::fs::create_dir_all(&path).unwrap();
        GitRepo::create(&path).unwrap()
    }

    /// Export `head`'s history into `repo`, returning its commit.
    fn export(exporter: &mut Exporter, head: &str) -> String {
        for parcel in topological_order(&[head.to_string()]).unwrap() {
            exporter.export_parcel(&parcel).unwrap();
        }
        exporter.exported[head].commit.clone()
    }

    fn git_bytes(repo: &GitRepo, args: &[&str]) -> Vec<u8> {
        let output = repo.command(|key| key.starts_with("GIT_")).args(args).output().unwrap();
        assert!(output.status.success(), "git {:?}: {}", args, String::from_utf8_lossy(&output.stderr));
        output.stdout
    }

    /// Test 11 (layout pin): a fixture exercising every rule the export layout covers exports to
    /// a fixed tip hash. A change to the mode map, chunk assembly, message shape, identity
    /// mapping, date form or parent rule moves it, and must bump [`EXPORT_LAYOUT`] (and this
    /// literal) with it.
    ///
    /// The fixture holds a chunked file, an executable, a symlink, an empty file and nested
    /// folders; a parcel dated 1970-01-02, which git refuses in the bare date form (test 19); an
    /// author who is not the stacker; an operator whose name git would strip to nothing; an empty
    /// description; and a merge whose parent list repeats a parent. Git itself drops a repeated
    /// -p, so removing the dedup alone leaves this green; what the pin holds is which occurrence
    /// is kept, and so the parent order.
    #[test]
    fn a_fixed_history_exports_to_a_fixed_tip() {
        let warehouse = Warehouse::storage_only("export-layout-pin");

        let first = store_tree(&[
            Entry::File("README", DirEntryType::Normal, b"hello\n"),
            Entry::File("run.sh", DirEntryType::Executable, b"#!/bin/sh\necho hi\n"),
            Entry::File("link", DirEntryType::SymbolicLink, b"README"),
            Entry::File("empty", DirEntryType::Normal, b""),
            Entry::Chunked("big.bin", DirEntryType::NormalChunked, &[b"chunk one\r\n", b"chunk two\r\n"]),
            Entry::Dir("a", vec![Entry::Dir("b", vec![Entry::File("c.txt", DirEntryType::Normal, b"deep\n")])]),
        ]);
        let second = store_tree(&[
            Entry::File("README", DirEntryType::Normal, b"hello\n"),
            Entry::File("run.sh", DirEntryType::Normal, b"#!/bin/sh\necho hi\n"),
            Entry::File("link", DirEntryType::SymbolicLink, b"README"),
            Entry::File("empty", DirEntryType::Normal, b""),
            Entry::Chunked("big.bin", DirEntryType::ExecutableChunked, &[b"chunk one\r\n", b"chunk two\r\n"]),
            Entry::Dir("a", vec![Entry::Dir("b", vec![Entry::File("c.txt", DirEntryType::Normal, b"deeper\n")])]),
        ]);
        let third = store_tree(&[Entry::File("README", DirEntryType::Normal, b"merged\n")]);

        let root = store_parcel(&first, &[], vec![
            action(ParcelActionType::Author, "alice@example.com", 1_000_000_000),
            action(ParcelActionType::Stack, "bob@example.com", 1_000_000_100),
        ], Some("first\n\nwith a body\n"));
        let crud = store_parcel(&second, &[&root], vec![
            action(ParcelActionType::Author, " <;> ", 86_400),
        ], None);
        let merge = store_parcel(&third, &[&crud, &root, &crud], vec![
            action(ParcelActionType::Stack, "carol@example.com", 1_700_000_000),
        ], Some("merge"));

        let repo = create_repo(&warehouse);
        let mut exporter = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        let tip = export(&mut exporter, &merge);

        let commit = |hash: &str| String::from_utf8(git_bytes(&repo, &["cat-file", "commit", hash])).unwrap();
        let merge_commit = commit(&tip);
        let parents: Vec<&str> = merge_commit.lines().filter_map(|line| line.strip_prefix("parent ")).collect();
        assert_eq!(parents, [exporter.exported[&crud].commit.as_str(), exporter.exported[&root].commit.as_str()],
            "repeats removed, first occurrence kept: {}", merge_commit);
        assert!(merge_commit.ends_with(&format!("merge\n\nForklift-Parcel: {} export/1\n", merge)), "{}", merge_commit);

        let crud_commit = commit(&exporter.exported[&crud].commit);
        assert!(crud_commit.contains("\nauthor forklift <> 86400 +0000\n"), "{}", crud_commit);
        assert!(crud_commit.ends_with(&format!("\n\nForklift-Parcel: {} export/1\n", crud)), "{}", crud_commit);

        let root_commit = commit(&exporter.exported[&root].commit);
        assert!(root_commit.contains("\nauthor alice@example.com <alice@example.com> 1000000000 +0000\n"), "{}", root_commit);
        assert!(root_commit.contains("\ncommitter bob@example.com <bob@example.com> 1000000100 +0000\n"), "{}", root_commit);

        git_bytes(&repo, &["fsck", "--strict"]);
        assert_eq!(tip, "66677f9fc675b41891d4f168fb280606e0aac682", "the export layout's pinned tip moved");
    }

    /// Test 15 (reuse): a parcel changing one file deep in a tree, exported on a commit an
    /// earlier run wrote, writes only that file's blob and the trees on its path. Within one run, a
    /// subtree that moved is written again only where its own path changed.
    #[test]
    fn a_one_file_change_writes_only_its_path() {
        let warehouse = Warehouse::storage_only("export-reuse");

        let names: Vec<String> = (0..4).map(|n| format!("file-{}", n)).collect();
        let tree = |changed: &[u8], right_name: &str| {
            let files = |dir: &str| -> Vec<(String, Vec<u8>)> {
                names.iter().map(|name| (name.clone(), format!("{}/{}\n", dir, name).into_bytes())).collect()
            };
            let (left, right) = (files("left"), files("right"));
            let mut left_entries: Vec<Entry> = left.iter().map(|(name, content)| Entry::File(name, DirEntryType::Normal, content)).collect();
            left_entries.push(Entry::File("changed", DirEntryType::Normal, changed));
            let right_entries: Vec<Entry> = right.iter().map(|(name, content)| Entry::File(name, DirEntryType::Normal, content)).collect();
            store_tree(&[Entry::Dir("left", vec![Entry::Dir("inner", left_entries)]), Entry::Dir(right_name, right_entries)])
        };

        let first = store_parcel(&tree(b"one\n", "right"), &[], vec![action(ParcelActionType::Stack, "op", 1_000)], Some("one"));
        let second = store_parcel(&tree(b"two\n", "right"), &[&first], vec![action(ParcelActionType::Stack, "op", 2_000)], Some("two"));
        let moved = store_parcel(&tree(b"two\n", "moved"), &[&second], vec![action(ParcelActionType::Stack, "op", 3_000)], Some("moved"));

        let repo = create_repo(&warehouse);
        let mut earlier = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        export(&mut earlier, &first);
        assert_eq!((earlier.trees_written, earlier.blobs_written), (4, 9));

        // A later run knows only the commit it builds on, as the mirror does.
        let mut later = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        later.exported.insert(first.clone(), earlier.exported[&first].clone());
        export(&mut later, &second);
        assert_eq!((later.trees_written, later.blobs_written), (3, 1),
            "the changed blob and the root, left and left/inner trees");

        export(&mut later, &moved);
        assert_eq!((later.trees_written, later.blobs_written), (4, 1), "a rename writes only the root again");
    }

    /// Test 1's filter half: a chunked file is hashed byte for byte even in a repository whose own
    /// attributes and config would convert its line endings, as hashing it by path otherwise does.
    #[test]
    fn a_chunked_file_is_hashed_without_filters() {
        let warehouse = Warehouse::storage_only("export-no-filters");
        let content: &[&[u8]] = &[b"one\r\n", b"two\r\n"];
        let tree = store_tree(&[Entry::Chunked("crlf.txt", DirEntryType::NormalChunked, content)]);
        let parcel = store_parcel(&tree, &[], vec![action(ParcelActionType::Stack, "op", 1_000)], Some("crlf"));

        let repo = create_repo(&warehouse);
        let git_dir = warehouse.root.join("exported").join(".git");
        std::fs::create_dir_all(git_dir.join("info")).unwrap();
        std::fs::write(git_dir.join("info").join("attributes"), "* text\n").unwrap();
        git_bytes(&repo, &["config", "core.autocrlf", "true"]);

        let mut exporter = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        let commit = export(&mut exporter, &parcel);

        assert_eq!(git_bytes(&repo, &["cat-file", "blob", &format!("{}:crlf.txt", commit)]), content.concat());
    }

    /// Test 1's message half: an NFD description reaches the commit byte for byte. On macOS, where
    /// git precomposes arguments, a message passed with -m comes back NFC.
    #[test]
    fn an_nfd_description_is_kept_byte_for_byte() {
        let warehouse = Warehouse::storage_only("export-nfd");
        let description = "Cafe\u{301}";
        let tree = store_tree(&[Entry::File("README", DirEntryType::Normal, b"x\n")]);
        let parcel = store_parcel(&tree, &[], vec![action(ParcelActionType::Stack, "op", 1_000)], Some(description));

        let repo = create_repo(&warehouse);
        let mut exporter = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        let commit = export(&mut exporter, &parcel);

        let raw = git_bytes(&repo, &["cat-file", "commit", &commit]);
        let message = String::from_utf8(raw).unwrap();
        assert!(message.contains(&format!("\n\n{}\n\nForklift-Parcel: ", description)), "{:?}", message);
    }

    #[test]
    fn git_s_identity_cleaning_is_matched() {
        assert_eq!(without_crud("  Ada <x> Lovelace, "), "Ada x Lovelace");
        assert_eq!(without_crud(" <;> "), "");
        assert_eq!(without_crud("..."), "...");
        assert_eq!(without_crud("a\nb"), "ab");
    }

    #[test]
    fn a_date_before_1970_is_written_as_the_epoch() {
        assert_eq!(git_date(-5), "@0 +0000");
        assert_eq!(git_date(86_400), "@86400 +0000");
    }

    /// A killed earlier process with this one's id can leave its temp file at the very name the
    /// next export draws. The export steps past it rather than failing, and leaves it alone.
    #[test]
    fn a_dead_process_s_temp_file_at_the_next_name_is_stepped_past() {
        let recipe = format!("litter-recipe-{}", std::process::id());
        // A counter of the test's own: other tests exporting would move the next name past the litter.
        let counter = std::sync::atomic::AtomicU64::new(0);
        let litter: Vec<_> = (0..8)
            .map(|n| std::env::temp_dir().join(format!("forklift-export-{}-{}-{}.tmp", std::process::id(), n, recipe)))
            .collect();
        for file in &litter {
            std::fs::write(file, b"litter").unwrap();
        }

        let created = create_export_temp(&recipe, &counter);
        let contents: Vec<_> = litter.iter().map(|file| std::fs::read(file).unwrap()).collect();
        for file in &litter {
            let _ = std::fs::remove_file(file);
        }

        let (path, _file) = created.expect("litter must not fail the export");
        let _ = std::fs::remove_file(&path);
        assert!(!litter.contains(&path), "a taken name was reused: {}", path.display());
        assert!(contents.iter().all(|content| content == b"litter"), "litter is left as it was");
    }

    /// The tag object is fixed text: a change to its shape changes every exported tag's hash, which
    /// bumps the export layout. The tagger is cleaned as an author is, and falls back to
    /// "forklift"; an unsigned tag's last line names no key.
    #[test]
    fn a_tag_object_is_written_in_a_fixed_shape() {
        use forklift_core::util::tag_utils::Tag;

        let tag = |message: &str, tagged_at: i64| AttributedTag {
            tag: Tag { name: "v1.0".to_string(), subject: "p".to_string(), message: message.to_string(), tagged_at },
            tagger: "ignored".to_string(),
            signer: Some("k1".to_string()),
        };
        let commit = "c".repeat(40);

        assert_eq!(
            tag_object(&commit, "ada@example", Some("k1"), &tag("the release\n\n", 1_700_000_000)),
            format!(
                "object {}\ntype commit\ntag v1.0\ntagger ada@example <ada@example> 1700000000 +0000\n\n\
                the release\n\nForklift-Tag-Signer: k1 export/1\n",
                commit
            )
        );
        assert_eq!(
            tag_object(&commit, "<>", Some("k1"), &tag("", -5)),
            format!("object {}\ntype commit\ntag v1.0\ntagger forklift <> 0 +0000\n\nForklift-Tag-Signer: k1 export/1\n", commit)
        );
        assert_eq!(
            tag_object(&commit, "ada@example", None, &tag("the release", 1_700_000_000)),
            format!(
                "object {}\ntype commit\ntag v1.0\ntagger ada@example <ada@example> 1700000000 +0000\n\n\
                the release\n\nForklift-Tag-Unsigned: export/1\n",
                commit
            )
        );
    }

    /// The withheld-trees log: a torn last append is no tree and is cut off, so a tree logged after
    /// it is read back whole; a whole line naming no tree is refused, never skipped.
    #[test]
    fn the_withheld_trees_log_cuts_a_torn_append_and_refuses_an_unreadable_line() {
        let warehouse = Warehouse::storage_only("export-withheld-log");
        let repo = create_repo(&warehouse);
        let log = repo.git_dir().join(WITHHELD_TREES_LOG);
        let (first, second) = ("a".repeat(40), "b".repeat(64));
        std::fs::write(&log, format!("{}\n{}", first, &second[..10])).unwrap();

        let mut exporter = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        exporter.keep_withheld_log().unwrap();
        assert_eq!(exporter.withheld_trees, Some(HashSet::from([first.clone()])));
        exporter.log_withheld_tree(&second);
        exporter.flush_withheld_log().unwrap();
        assert_eq!(std::fs::read_to_string(&log).unwrap(), format!("{}\n{}\n", first, second));

        let mut again = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        again.keep_withheld_log().unwrap();
        assert_eq!(again.withheld_trees, Some(HashSet::from([first.clone(), second.clone()])));

        std::fs::write(&log, format!("{}\nnot a tree\n", first)).unwrap();
        let mut refused = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        assert!(refused.keep_withheld_log().unwrap_err().contains("not a tree"));

        std::fs::write(&log, [first.as_bytes(), b"\n\xff\n"].concat()).unwrap();
        let mut garbled = Exporter::new(&repo, AppliedSet::default(), UnavailableSet::default());
        assert!(garbled.keep_withheld_log().unwrap_err().contains("cannot read"));
    }
}
