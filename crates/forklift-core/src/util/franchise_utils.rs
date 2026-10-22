//! The record of a franchise that has not finished: `.forklift/franchise`, and what reads it.
//!
//! A franchise that has claimed its target directory leaves this record standing until it
//! succeeds, so an error, a kill or a power loss part-way through never loses what it fetched.
//! Running the same `franchise` again resumes it: the verified object store is kept and the rest
//! of the warehouse is rebuilt (see [`prepare_resume`] for what is kept). Every other command
//! refuses to enter a warehouse that holds the record ([`gate`]), with the code
//! `franchise_incomplete`, and names the way out.
//!
//! ## Storage
//!
//! One [`record_utils`] record, `franchise`, in the warehouse's `.forklift` folder, replaced
//! atomically. Each entry is `key=value`; a list repeats its key. A record this build cannot read
//! is never taken for "no franchise in progress": the gate refuses, and a resume admits it only as
//! the debris of a run killed before its first write was durable.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::error::{CoreError, RefusalCode};
use crate::globals::{FOLDER_NAME_FORKLIFT_ROOT, FOLDER_NAME_OBJECTS_ROOT};
use crate::util::{file_utils, record_utils, remote_utils, taint_utils};
use crate::util::record_utils::RecordError;

/// The name of the record in the warehouse's `.forklift` folder.
pub const FILE_NAME_FRANCHISE: &str = "franchise";

/// The [`record_utils`] kind and version of the record.
const FRANCHISE_RECORD: &str = "franchise";
const FRANCHISE_VERSION: u32 = 1;

/// The name of the warehouse lock file, which a resume keeps: it is the resume's own.
const FILE_NAME_LOCK: &str = "lock";

/// The folder of the warehouse's bundles, which a resume keeps only after a finished import.
const FOLDER_NAME_BUNDLES: &str = "bundles";

/// Whether the latest attempt is still running (or was killed) or returned a failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attempt {
    Running,
    Failed,
}

impl Attempt {
    fn as_str(self) -> &'static str {
        match self {
            Attempt::Running => "running",
            Attempt::Failed => "failed",
        }
    }
}

/// What a failed attempt says to do next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Retry {
    /// A transient failure (the network, a timeout, a server error): the same command again.
    Same,

    /// A failure known to repeat: the same command fails the same way, so something must change.
    Change,

    /// Anything else: the same command again, until it repeats without progress (see
    /// [`bound_retry`]).
    Unknown,
}

impl Retry {
    pub fn as_str(self) -> &'static str {
        match self {
            Retry::Same => "same",
            Retry::Change => "change",
            Retry::Unknown => "unknown",
        }
    }

    fn parse(text: &str) -> Option<Retry> {
        [Retry::Same, Retry::Change, Retry::Unknown].into_iter().find(|retry| retry.as_str() == text)
    }
}

/// The part of a franchise a failure happened in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// Clearing what an earlier attempt left, and preparing the warehouse.
    Prepare,

    /// Downloading or importing the remote's bundle.
    Bundle,

    /// Adopting trust and meta pallets, and fetching the pallet's history.
    Fetch,

    /// Writing the working tree.
    Materialize,
}

impl Phase {
    fn as_str(self) -> &'static str {
        match self {
            Phase::Prepare => "prepare",
            Phase::Bundle => "bundle",
            Phase::Fetch => "fetch",
            Phase::Materialize => "materialize",
        }
    }

    fn parse(text: &str) -> Option<Phase> {
        [Phase::Prepare, Phase::Bundle, Phase::Fetch, Phase::Materialize].into_iter().find(|phase| phase.as_str() == text)
    }
}

/// What an attempt was asked to franchise, beyond the URL: the parts a resume may change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The `--pallet` asked for; `None` for the remote's default.
    pub pallet: Option<String>,

    /// The `--only` prefixes, as normalized keys; empty for a full franchise.
    pub only: Vec<String>,
}

/// The last failure an attempt recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// The request of the attempt that failed.
    pub request: Request,

    /// Where it failed.
    pub phase: Phase,

    /// The code the cause would have had on its own (`error` when it has none).
    pub cause_code: String,

    /// What to do next.
    pub retry: Retry,
}

/// The record of an unfinished franchise.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FranchiseRecord {
    /// The remote's URL.
    pub url: String,

    /// The access token, if one was given: a resume without `--token` reuses it.
    pub token: Option<String>,

    /// The current (or last) attempt's request.
    pub request: Request,

    /// The trust genesis this franchise is bound to: the `--expect` pin, else the genesis the
    /// remote announced on first contact. `None` when the remote announced no anchor.
    pub genesis: Option<String>,

    /// Whether the remote's bundle was imported in full.
    pub bundle_imported: bool,

    /// The latest attempt's state.
    pub attempt: Attempt,

    /// The object-store census taken when the latest attempt started (see [`census`]).
    pub start_census: u64,

    /// The last failure recorded, carried over unchanged by an attempt's start.
    pub last_failure: Option<Failure>,
}

impl FranchiseRecord {
    fn to_bytes(&self) -> Vec<u8> {
        let mut entries: Vec<String> = vec![format!("url={}", self.url)];

        if let Some(token) = &self.token {
            entries.push(format!("token={}", token));
        }

        push_request(&mut entries, "", &self.request);

        if let Some(genesis) = &self.genesis {
            entries.push(format!("genesis={}", genesis));
        }

        entries.push(format!("bundle={}", if self.bundle_imported { "imported" } else { "none" }));
        entries.push(format!("attempt={}", self.attempt.as_str()));
        entries.push(format!("census={}", self.start_census));

        if let Some(failure) = &self.last_failure {
            push_request(&mut entries, "failure.", &failure.request);
            entries.push(format!("failure.phase={}", failure.phase.as_str()));
            entries.push(format!("failure.cause={}", failure.cause_code));
            entries.push(format!("failure.retry={}", failure.retry.as_str()));
        }

        record_utils::to_bytes(FRANCHISE_RECORD, FRANCHISE_VERSION, entries)
    }

    fn from_bytes(bytes: &[u8]) -> Result<FranchiseRecord, MarkerError> {
        let entries = record_utils::from_bytes(bytes, FRANCHISE_RECORD, FRANCHISE_VERSION).map_err(|e| match e {
            RecordError::OtherVersion(_) | RecordError::Unversioned => MarkerError::Foreign(e.describe()),
            _ => MarkerError::Torn(e.describe()),
        })?;

        let malformed = |what: &str| MarkerError::Torn(format!("has a malformed entry ({}).", what));

        let mut url = None;
        let mut token = None;
        let mut request = Request::default();
        let mut genesis = None;
        let mut bundle_imported = None;
        let mut attempt = None;
        let mut start_census = None;
        let mut failure_request = Request::default();
        let mut failure_phase = None;
        let mut failure_cause = None;
        let mut failure_retry = None;

        for entry in &entries {
            let (key, value) = entry.split_once('=').ok_or_else(|| malformed("no \"=\""))?;
            let value = value.to_string();

            match key {
                "url" => url = Some(value),
                "token" => token = Some(value),
                "pallet" => request.pallet = Some(value),
                "only" => request.only.push(value),
                "genesis" => genesis = Some(value),
                "bundle" => bundle_imported = Some(match value.as_str() {
                    "imported" => true,
                    "none" => false,
                    _ => return Err(malformed("bundle")),
                }),
                "attempt" => attempt = Some(match value.as_str() {
                    "running" => Attempt::Running,
                    "failed" => Attempt::Failed,
                    _ => return Err(malformed("attempt")),
                }),
                "census" => start_census = Some(value.parse::<u64>().map_err(|_| malformed("census"))?),
                "failure.pallet" => failure_request.pallet = Some(value),
                "failure.only" => failure_request.only.push(value),
                "failure.phase" => failure_phase = Some(Phase::parse(&value).ok_or_else(|| malformed("failure.phase"))?),
                "failure.cause" => failure_cause = Some(value),
                "failure.retry" => failure_retry = Some(Retry::parse(&value).ok_or_else(|| malformed("failure.retry"))?),
                // An unknown key is a field this build does not know, so the record is not read
                // as if the field were not there.
                other => return Err(MarkerError::Foreign(format!("holds a field this version of Forklift does not know (\"{}\").", other.escape_debug()))),
            }
        }

        let last_failure = match (failure_phase, failure_cause, failure_retry) {
            (Some(phase), Some(cause_code), Some(retry)) => Some(Failure { request: failure_request, phase, cause_code, retry }),
            (None, None, None) if failure_request == Request::default() => None,
            _ => return Err(malformed("failure")),
        };

        Ok(FranchiseRecord {
            url: url.ok_or_else(|| malformed("url"))?,
            token,
            request,
            genesis,
            bundle_imported: bundle_imported.ok_or_else(|| malformed("bundle"))?,
            attempt: attempt.ok_or_else(|| malformed("attempt"))?,
            start_census: start_census.ok_or_else(|| malformed("census"))?,
            last_failure,
        })
    }
}

fn push_request(entries: &mut Vec<String>, prefix: &str, request: &Request) {
    if let Some(pallet) = &request.pallet {
        entries.push(format!("{}pallet={}", prefix, pallet));
    }

    for only in &request.only {
        entries.push(format!("{}only={}", prefix, only));
    }
}

/// Why the record could not be read.
#[derive(Debug, PartialEq, Eq)]
pub enum MarkerError {
    /// Cut short or malformed: a crash with fsync off, or damage. The phrase follows "The record ".
    Torn(String),

    /// Written by another version of Forklift. The phrase follows "The record ".
    Foreign(String),

    /// The file could not be read at all.
    Unreadable(String),
}

impl MarkerError {
    fn describe(&self, path: &Path) -> String {
        match self {
            MarkerError::Torn(why) | MarkerError::Foreign(why) => {
                format!("The franchise record \"{}\" {}", path.to_string_lossy(), why)
            }
            MarkerError::Unreadable(why) => why.clone(),
        }
    }
}

/// The record's path in a warehouse's `.forklift` folder.
pub fn record_path(forklift: &Path) -> PathBuf {
    forklift.join(FILE_NAME_FRANCHISE)
}

/// Read the record in `forklift`, if one stands.
pub fn read(forklift: &Path) -> Result<Option<FranchiseRecord>, MarkerError> {
    let path = record_path(forklift);

    match std::fs::read(&path) {
        Ok(bytes) => FranchiseRecord::from_bytes(&bytes).map(Some),
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => Ok(None),
        Err(e) => Err(MarkerError::Unreadable(format!("Error while reading \"{}\": {}", path.to_string_lossy(), e))),
    }
}

/// Write the record in `forklift`, replacing any: atomically, and synced with its folder.
pub fn write(forklift: &Path, record: &FranchiseRecord) -> Result<(), String> {
    file_utils::replace_file_atomically(&record_path(forklift), &record.to_bytes())
        .map_err(|e| format!("Error while recording the franchise in progress: {}", e))
}

/// Remove the record in `forklift`, with any temp file a replacement left behind, and sync the
/// folder: the last step of a franchise that finished, so the warehouse is never gated again.
pub fn finish(forklift: &Path) -> Result<(), String> {
    for temp in record_temps(forklift)? {
        remove_unfollowed(&temp)?;
    }

    match std::fs::remove_file(record_path(forklift)) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!(
            "Error while clearing the franchise record \"{}\": {}", record_path(forklift).to_string_lossy(), e
        )),
    }

    file_utils::sync_dir(forklift)
}

/// The temp files an interrupted replacement of the record left in `forklift`.
fn record_temps(forklift: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = match std::fs::read_dir(forklift) {
        Ok(entries) => entries,
        Err(e) if matches!(e.kind(), std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory) => return Ok(Vec::new()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", forklift.to_string_lossy(), e)),
    };

    let mut temps = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("Error while reading \"{}\": {}", forklift.to_string_lossy(), e))?;

        if is_temp_of(&entry.file_name(), FILE_NAME_FRANCHISE) {
            temps.push(entry.path());
        }
    }

    Ok(temps)
}

/// Whether `name` is a temp [`file_utils::temp_path_for`] names for `base`.
fn is_temp_of(name: &OsStr, base: &str) -> bool {
    name.to_str().and_then(file_utils::staged_temp_base) == Some(base)
}

/// Whether `name`, an entry of a franchise's target directory, is a `.forklift` folder a
/// franchise was still staging when it stopped: the claim builds `.forklift` under a temp name
/// and renames it into place.
pub fn is_staging_leftover(name: &OsStr) -> bool {
    is_temp_of(name, FOLDER_NAME_FORKLIFT_ROOT)
}

/// Claim `target`, which [`admit`] judged [`Admission::Fresh`], for a new franchise: create it
/// (and any missing ancestor) if absent, then put its `.forklift` folder in place with `record`
/// already inside. The folder is built under a temp name and renamed into place only if no
/// `.forklift` exists, so the directory is never a warehouse without its record: a kill at any
/// point leaves either no `.forklift` (only a staging leftover, which [`admit`] ignores) or one
/// the gate refuses. Two franchises racing for one directory cannot both claim it.
///
/// An ancestor is created with the tolerant `create_dir_all` and never removed: an empty one never
/// blocks a retry.
pub fn claim(target: &Path, record: &FranchiseRecord) -> Result<(), String> {
    if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Error while creating \"{}\": {}", parent.to_string_lossy(), e))?;
    }

    match std::fs::create_dir(target) {
        Ok(()) => (),
        // Absent before the handshake, or found then: either way it must still be fresh now.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match admit(target)? {
            Admission::Fresh => (),
            Admission::Resume(_) => return Err(format!(
                "\"{}\" was claimed by another franchise while this one was connecting; franchise \
                into a new or empty directory.", target.to_string_lossy()
            )),
        },
        Err(e) => return Err(format!("Error while creating \"{}\": {}", target.to_string_lossy(), e)),
    }

    let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
    let staging = file_utils::temp_path_for(&forklift)?;

    std::fs::create_dir(&staging)
        .map_err(|e| format!("Error while creating \"{}\": {}", staging.to_string_lossy(), e))?;

    let placed = write(&staging, record)
        .and_then(|()| match std::fs::symlink_metadata(&forklift) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Ok(_) => Err(format!(
                "\"{}\" was claimed by another franchise while this one was connecting; franchise into \
                a new or empty directory.", target.to_string_lossy()
            )),
            Err(e) => Err(format!("Error while checking \"{}\": {}", forklift.to_string_lossy(), e)),
        })
        .and_then(|()| std::fs::rename(&staging, &forklift).map_err(|e| format!(
            "Error while moving \"{}\" into place: {}", forklift.to_string_lossy(), e
        )));

    if let Err(e) = placed {
        // A real folder this call created, so the recursive removal follows nothing it did not make.
        let _ = std::fs::remove_dir_all(&staging);
        return Err(e);
    }

    file_utils::sync_dir(target)
}

/// Count the final entries of the object store under `forklift`: loose objects, signature
/// sidecars and pack files, never temp files. The difference between two counts is what an
/// attempt stored: nothing in an attempt deletes a final entry but a pack install replacing a
/// stale half-pair under the same names.
pub fn census(forklift: &Path) -> Result<u64, String> {
    fn count(dir: &Path) -> Result<u64, String> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(format!("Error while counting \"{}\": {}", dir.to_string_lossy(), e)),
        };

        let mut total = 0;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Error while counting \"{}\": {}", dir.to_string_lossy(), e))?;
            let file_type = entry.file_type()
                .map_err(|e| format!("Error while counting \"{}\": {}", entry.path().to_string_lossy(), e))?;

            if file_type.is_dir() {
                total += count(&entry.path())?;
            } else if file_type.is_file() && !entry.file_name().to_string_lossy().contains(".tmp") {
                total += 1;
            }
        }

        Ok(total)
    }

    count(&forklift.join(FOLDER_NAME_OBJECTS_ROOT))
}

/// How a target directory stands before a franchise into it.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Absent, or empty but for `.forklift` folders a stopped claim was staging.
    Fresh,

    /// An unfinished franchise: its record, or `None` for a run that stopped before its record was
    /// durable (its `.forklift` holds nothing but the lock, record temps and an unreadable record).
    Resume(Option<FranchiseRecord>),
}

/// Judge `target` for a franchise: a fresh one, a resume, or a refusal. Read-only.
pub fn admit(target: &Path) -> Result<Admission, String> {
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Admission::Fresh),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", target.to_string_lossy(), e)),
        Ok(_) => (),
    }

    if !target.is_dir() {
        return Err(format!(
            "\"{}\" already exists and is not a directory; franchise into a new or empty directory.",
            target.to_string_lossy()
        ));
    }

    let names = list_names(target)?;
    let names: Vec<&std::ffi::OsString> = names.iter().filter(|name| !is_staging_leftover(name)).collect();

    if names.is_empty() {
        return Ok(Admission::Fresh);
    }

    let not_empty = || format!(
        "\"{}\" is not empty; franchise into a new or empty directory.", target.to_string_lossy()
    );

    if !names.iter().any(|name| name.as_os_str() == FOLDER_NAME_FORKLIFT_ROOT) {
        return Err(not_empty());
    }

    let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
    let only_debris = names.len() == 1 && holds_only_debris(&forklift)?;

    match read(&forklift) {
        Ok(Some(record)) => Ok(Admission::Resume(Some(record))),
        Ok(None) if only_debris => Ok(Admission::Resume(None)),
        Ok(None) => Err(not_empty()),
        Err(MarkerError::Torn(_)) if only_debris => Ok(Admission::Resume(None)),
        Err(e) => Err(format!(
            "\"{}\" holds a franchise that did not finish, but its record cannot be read: {} Delete \
            the directory and franchise again.",
            target.to_string_lossy(), e.describe(&record_path(&forklift))
        )),
    }
}

/// Whether `forklift` holds nothing but the lock, the record and its temps: the shape a run
/// killed before its record was durable leaves.
fn holds_only_debris(forklift: &Path) -> Result<bool, String> {
    if !forklift.is_dir() {
        return Ok(false);
    }

    Ok(list_names(forklift)?.iter().all(|name| {
        name == FILE_NAME_LOCK || name == FILE_NAME_FRANCHISE || is_temp_of(name, FILE_NAME_FRANCHISE)
    }))
}

fn list_names(dir: &Path) -> Result<Vec<std::ffi::OsString>, String> {
    std::fs::read_dir(dir)
        .and_then(|entries| entries.map(|entry| entry.map(|entry| entry.file_name())).collect())
        .map_err(|e| format!("Error while reading \"{}\": {}", dir.to_string_lossy(), e))
}

/// Clear what a resume must not keep, holding the warehouse lock, with the current directory at
/// the target. In order:
///
/// 1. The object store's durability taints: a torn taint discards the whole store, otherwise each
///    recorded path is deleted. Without an anchor to verify against (`record` is `None`, or
///    recorded no genesis), every kept signature sidecar is deleted too, so the resume fetches
///    them again exactly as a fresh franchise would.
/// 2. Every entry of `.forklift` but the object store, the record, the held lock and, when
///    `keep_bundles`, the bundles. Refs above all: the history walk stops at any parcel a local
///    ref holds, and a ref an earlier attempt wrote is complete only at that attempt's scope.
/// 3. Every entry of the target but `.forklift`: the partial working tree.
///
/// Nothing is followed through a link: a link is removed as itself.
pub fn prepare_resume(target: &Path, record: Option<&FranchiseRecord>, keep_bundles: bool) -> Result<(), String> {
    let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
    let objects = forklift.join(FOLDER_NAME_OBJECTS_ROOT);

    let taints = taint_utils::read_taints(&forklift)?;

    if taints.torn {
        remove_unfollowed(&objects)?;
    } else {
        for recorded in &taints.recorded {
            if is_object_store_path(recorded) {
                remove_unfollowed(&forklift.join(recorded))?;
            }
        }
    }

    if record.is_none_or(|record| record.genesis.is_none()) {
        remove_signature_sidecars(&objects)?;
    }

    for name in list_names(&forklift)? {
        let kept = name == FOLDER_NAME_OBJECTS_ROOT
            || name == FILE_NAME_FRANCHISE
            || name == FILE_NAME_LOCK
            || (keep_bundles && name == FOLDER_NAME_BUNDLES);

        if !kept {
            remove_unfollowed(&forklift.join(&name))?;
        }
    }

    for name in list_names(target)? {
        if name != FOLDER_NAME_FORKLIFT_ROOT {
            remove_unfollowed(&target.join(&name))?;
        }
    }

    file_utils::sync_dir(&forklift)?;
    file_utils::sync_dir(target)
}

/// Whether `path`, relative to `.forklift`, names something inside the object store.
fn is_object_store_path(path: &Path) -> bool {
    let mut components = path.components();

    components.next() == Some(Component::Normal(OsStr::new(FOLDER_NAME_OBJECTS_ROOT)))
        && components.clone().next().is_some()
        && components.all(|component| matches!(component, Component::Normal(_)))
}

/// Delete every signature sidecar (`<hash>.sig`) in the loose store's fan-out folders.
fn remove_signature_sidecars(objects: &Path) -> Result<(), String> {
    let folders = match std::fs::read_dir(objects) {
        Ok(folders) => folders,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Error while reading \"{}\": {}", objects.to_string_lossy(), e)),
    };

    for folder in folders {
        let folder = folder.map_err(|e| format!("Error while reading \"{}\": {}", objects.to_string_lossy(), e))?;

        if !folder.file_type().is_ok_and(|file_type| file_type.is_dir()) {
            continue;
        }

        for file in list_names(&folder.path())? {
            if file.to_string_lossy().ends_with(".sig") {
                remove_unfollowed(&folder.path().join(file))?;
            }
        }
    }

    Ok(())
}

/// Remove `path` without following it: a link or a file as itself, a real folder recursively
/// (whose own removal follows no link inside it). An absent path is not a failure.
fn remove_unfollowed(path: &Path) -> Result<(), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
    };

    let removed = if metadata.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        // A link to a folder is removed as a folder entry on Windows.
        std::fs::remove_file(path).or_else(|e| if metadata.is_symlink() { std::fs::remove_dir(path) } else { Err(e) })
    };

    match removed {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("Error while removing \"{}\": {}", path.to_string_lossy(), e)),
    }
}

/// Classify a failure after the claim: its code (`error` when it has none), and what to do next.
/// A code known to repeat is `change`; a failure of the transport, a timeout or a server error is
/// `same`; anything else is `unknown`.
pub fn classify_failure(error: &str) -> (String, Retry) {
    match CoreError::from(error.to_string()) {
        CoreError::Refusal { code, .. } => {
            let retry = match code {
                RefusalCode::RemoteProtocolViolation | RefusalCode::OnlyPrefixNotFound => Retry::Change,
                _ => Retry::Unknown,
            };

            (code.as_str().to_string(), retry)
        }
        CoreError::Other(message) => {
            let retry = if remote_utils::is_transient_failure(&message) { Retry::Same } else { Retry::Unknown };
            ("error".to_string(), retry)
        }
    }
}

/// Bound the retries of an unclassified failure: `unknown` becomes `change` when the attempt made
/// no progress and failed with the same request, in the same phase, with the same code as the
/// last recorded failure. A transient failure is never bounded (a stalled network is retried as
/// is), and an attempt that made progress is never told to give up.
pub fn bound_retry(retry: Retry, failure: &Failure, previous: Option<&Failure>, progressed: bool) -> Retry {
    let repeated = previous.is_some_and(|previous| {
        previous.request == failure.request && previous.phase == failure.phase && previous.cause_code == failure.cause_code
    });

    match retry {
        Retry::Unknown if repeated && !progressed => Retry::Change,
        retry => retry,
    }
}

/// The `franchise` command that resumes a franchise of `url` into `directory` with `request`.
/// Never the token (a resume reuses the recorded one) and never `--expect` (a resume enforces
/// the recorded genesis itself).
pub fn resume_command(url: &str, directory: &str, request: &Request) -> String {
    let mut command = format!("forklift franchise {} {}", quote(url), quote(directory));

    if let Some(pallet) = &request.pallet {
        command.push_str(&format!(" --pallet {}", quote(pallet)));
    }

    for only in &request.only {
        command.push_str(&format!(" --only {}", quote(only)));
    }

    command
}

/// Quote `arg` for a POSIX shell when it holds anything but plain characters.
fn quote(arg: &str) -> String {
    let plain = !arg.is_empty() && arg.chars().all(|c| c.is_ascii_alphanumeric() || "-_./:@%+=,~".contains(c));

    if plain {
        arg.to_string()
    } else {
        format!("'{}'", arg.replace('\'', "'\\''"))
    }
}

/// The next step for a franchise whose last attempt failed with `retry`.
fn failed_next_step(command: &str, directory: &str, retry: Retry) -> String {
    match retry {
        Retry::Same | Retry::Unknown => format!(
            "Run \"{}\" to resume it; what was fetched is kept. Or delete \"{}\".", command, directory
        ),
        Retry::Change => format!(
            "Running it unchanged fails the same way. Run \"{}\" with a different --only or --pallet \
            (what was fetched is kept), or delete \"{}\".", command, directory
        ),
    }
}

/// The `franchise_incomplete` refusal for a franchise's own failure after its claim: `cause` with
/// the directory kept, and the next step `retry` earns.
pub fn failure_refusal(directory: &str, record: &FranchiseRecord, cause: &str, failure: &Failure) -> CoreError {
    let message = match CoreError::from(cause.to_string()) {
        CoreError::Refusal { message, .. } | CoreError::Other(message) => message,
    };

    let command = resume_command(&record.url, directory, &record.request);

    CoreError::refusal_with_details(
        RefusalCode::FranchiseIncomplete,
        format!("{} The franchise did not finish; \"{}\" was kept with what was fetched.", message.trim_end(), directory),
        failed_next_step(&command, directory, failure.retry),
        details(directory, Some(&record.url), Some(failure)),
    )
}

fn details(directory: &str, remote: Option<&str>, failure: Option<&Failure>) -> serde_json::Value {
    serde_json::json!({
        "directory": directory,
        "remote": remote,
        "cause_code": failure.map(|failure| failure.cause_code.as_str()),
        "retry": failure.map(|failure| failure.retry.as_str()),
    })
}

/// Refuse to enter the warehouse at `root` while it holds an unfinished franchise: a record, a
/// record's temp file or an unreadable record. Runs before any lock is taken, for every command,
/// so a command run while a franchise is still running is told that, not that the lock is held.
pub fn gate(root: &Path) -> Result<(), CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    let record = match read(&forklift) {
        Ok(Some(record)) => Some(record),
        Ok(None) if record_temps(&forklift).map_err(CoreError::Other)?.is_empty() => return Ok(()),
        Ok(None) | Err(_) => None,
    };

    let directory = root.to_string_lossy().into_owned();
    let lock = forklift.join(FILE_NAME_LOCK);
    let locked = std::fs::symlink_metadata(&lock).is_ok();

    let Some(record) = record else {
        let next_step = if locked {
            format!(
                "A franchise may still be running there. If none is, remove the stale lock \"{}\" and run \
                \"forklift franchise <url> {}\" with the URL it was started with; or delete \"{}\".",
                lock.to_string_lossy(), quote(&directory), directory
            )
        } else {
            format!(
                "Run \"forklift franchise <url> {}\" with the URL it was started with, or delete \"{}\".",
                quote(&directory), directory
            )
        };

        return Err(CoreError::refusal_with_details(
            RefusalCode::FranchiseIncomplete,
            format!("\"{}\" holds a franchise that has not finished.", directory),
            next_step,
            details(&directory, None, None),
        ));
    };

    let command = resume_command(&record.url, &directory, &record.request);
    let failure = record.last_failure.as_ref().filter(|_| record.attempt == Attempt::Failed);

    let message = match failure {
        Some(failure) => format!(
            "\"{}\" holds a franchise of {} that has not finished: its last attempt failed ({}).",
            directory, record.url, failure.cause_code
        ),
        None if locked => format!(
            "\"{}\" holds a franchise of {} that has not finished: it may still be running.",
            directory, record.url
        ),
        None => format!(
            "\"{}\" holds a franchise of {} that has not finished: it was interrupted.", directory, record.url
        ),
    };

    let next_step = if locked {
        format!(
            "A franchise may still be running there. If none is, remove the stale lock \"{}\", then run \
            \"{}\" to resume it; or delete \"{}\".",
            lock.to_string_lossy(), command, directory
        )
    } else {
        match failure {
            Some(failure) => failed_next_step(&command, &directory, failure.retry),
            None => format!("Run \"{}\" to resume it; what was fetched is kept. Or delete \"{}\".", command, directory),
        }
    };

    Err(CoreError::refusal_with_details(
        RefusalCode::FranchiseIncomplete,
        message,
        next_step,
        details(&directory, Some(&record.url), failure),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-franchise-utils-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record() -> FranchiseRecord {
        FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: Some("se=cret\nline".to_string()),
            request: Request { pallet: Some("main".to_string()), only: vec!["docs".to_string(), "src/a b".to_string()] },
            genesis: Some("abc".to_string()),
            bundle_imported: true,
            attempt: Attempt::Failed,
            start_census: 42,
            last_failure: Some(Failure {
                request: Request { pallet: None, only: vec!["docs".to_string()] },
                phase: Phase::Fetch,
                cause_code: "error".to_string(),
                retry: Retry::Unknown,
            }),
        }
    }

    #[test]
    fn a_record_round_trips() {
        let full = record();
        assert_eq!(FranchiseRecord::from_bytes(&full.to_bytes()), Ok(full));

        let bare = FranchiseRecord {
            token: None, genesis: None, request: Request::default(), last_failure: None, ..record()
        };
        assert_eq!(FranchiseRecord::from_bytes(&bare.to_bytes()), Ok(bare));
    }

    #[test]
    fn a_record_this_build_cannot_read_is_never_read_as_none() {
        let bytes = record().to_bytes();

        // Cut short: torn.
        assert!(matches!(FranchiseRecord::from_bytes(&bytes[..bytes.len() - 1]), Err(MarkerError::Torn(_))));

        // Another version, and a field this build does not know: foreign.
        let other = String::from_utf8(bytes.clone()).unwrap().replacen("/forklift-franchise 1", "/forklift-franchise 2", 1);
        assert!(matches!(FranchiseRecord::from_bytes(other.as_bytes()), Err(MarkerError::Foreign(_))));

        let extra = format!("{}later=1\n", String::from_utf8(bytes).unwrap());
        assert!(matches!(FranchiseRecord::from_bytes(extra.as_bytes()), Err(MarkerError::Foreign(_))));
    }

    #[test]
    fn an_unknown_failure_flips_to_change_only_when_it_repeats_without_progress() {
        let failure = record().last_failure.unwrap();

        assert_eq!(bound_retry(Retry::Unknown, &failure, None, false), Retry::Unknown);
        assert_eq!(bound_retry(Retry::Unknown, &failure, Some(&failure), true), Retry::Unknown);
        assert_eq!(bound_retry(Retry::Unknown, &failure, Some(&failure), false), Retry::Change);
        assert_eq!(bound_retry(Retry::Same, &failure, Some(&failure), false), Retry::Same);

        // Another phase, request or code is not a repeat.
        let elsewhere = Failure { phase: Phase::Materialize, ..failure.clone() };
        assert_eq!(bound_retry(Retry::Unknown, &failure, Some(&elsewhere), false), Retry::Unknown);
        let narrower = Failure { request: Request::default(), ..failure.clone() };
        assert_eq!(bound_retry(Retry::Unknown, &failure, Some(&narrower), false), Retry::Unknown);
        let coded = Failure { cause_code: "trust_changed".to_string(), ..failure.clone() };
        assert_eq!(bound_retry(Retry::Unknown, &failure, Some(&coded), false), Retry::Unknown);
    }

    #[test]
    fn failures_classify_by_code_and_by_transport() {
        let violation = String::from(CoreError::refusal(RefusalCode::RemoteProtocolViolation, "bad", "report"));
        assert_eq!(classify_failure(&violation), ("remote_protocol_violation".to_string(), Retry::Change));

        let trust = String::from(CoreError::refusal(RefusalCode::TrustChanged, "changed", "verify"));
        assert_eq!(classify_failure(&trust), ("trust_changed".to_string(), Retry::Unknown));

        assert_eq!(classify_failure("Timed out while fetching objects: the remote did not respond.").1, Retry::Same);
        assert_eq!(classify_failure("Error while writing the bundle file: No space left on device").1, Retry::Unknown);
    }

    #[test]
    fn the_census_counts_final_entries_only() {
        let dir = temp_dir("census");
        let objects = dir.join("objects");
        std::fs::create_dir_all(objects.join("ab")).unwrap();
        std::fs::create_dir_all(objects.join("pack")).unwrap();
        std::fs::write(objects.join("ab/cdef"), b"x").unwrap();
        std::fs::write(objects.join("ab/cdef.sig"), b"x").unwrap();
        std::fs::write(objects.join("ab/cdef.tmp~1-0"), b"x").unwrap();
        std::fs::write(objects.join("ab/0123.stream.tmp"), b"x").unwrap();
        std::fs::write(objects.join("pack/p.pack"), b"x").unwrap();

        assert_eq!(census(&dir), Ok(3));
        assert_eq!(census(&dir.join("absent")), Ok(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn admission_tells_fresh_resume_and_refusal_apart() {
        let dir = temp_dir("admit");

        assert_eq!(admit(&dir.join("absent")), Ok(Admission::Fresh));
        assert_eq!(admit(&dir), Ok(Admission::Fresh));

        // A staging leftover alone is still fresh.
        std::fs::create_dir(dir.join(".forklift.tmp~1-0")).unwrap();
        assert_eq!(admit(&dir), Ok(Admission::Fresh));

        // A readable record resumes, beside a partial working tree.
        let forklift = dir.join(".forklift");
        std::fs::create_dir(&forklift).unwrap();
        std::fs::write(dir.join("half-written"), b"x").unwrap();
        write(&forklift, &record()).unwrap();
        assert_eq!(admit(&dir), Ok(Admission::Resume(Some(record()))));

        // A torn record is debris only while nothing else stands.
        std::fs::write(record_path(&forklift), b"/forklift-franchise 1\nurl=x").unwrap();
        assert!(admit(&dir).is_err(), "a torn record beside other entries is refused");
        std::fs::remove_file(dir.join("half-written")).unwrap();
        std::fs::write(forklift.join("lock"), b"1").unwrap();
        assert_eq!(admit(&dir), Ok(Admission::Resume(None)));

        // A record from another version is refused, never guessed at.
        std::fs::write(record_path(&forklift), b"/forklift-franchise 9\n").unwrap();
        assert!(admit(&dir).is_err());

        // A finished warehouse is not a franchise to resume.
        std::fs::remove_file(record_path(&forklift)).unwrap();
        std::fs::create_dir(forklift.join("pallets")).unwrap();
        assert!(admit(&dir).unwrap_err().contains("is not empty"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_resume_keeps_the_store_and_the_record_and_clears_the_rest() {
        let dir = temp_dir("resume");
        let forklift = dir.join(".forklift");
        let objects = forklift.join("objects");
        std::fs::create_dir_all(objects.join("ab")).unwrap();
        std::fs::create_dir_all(forklift.join("pallets")).unwrap();
        std::fs::create_dir_all(forklift.join("bundles")).unwrap();
        std::fs::write(objects.join("ab/cdef"), b"x").unwrap();
        std::fs::write(objects.join("ab/cdef.sig"), b"x").unwrap();
        std::fs::write(forklift.join("pallets/main"), b"x").unwrap();
        std::fs::write(forklift.join("operation"), b"x").unwrap();
        std::fs::write(forklift.join("lock"), b"1").unwrap();
        std::fs::write(dir.join("half-written"), b"x").unwrap();
        write(&forklift, &record()).unwrap();

        prepare_resume(&dir, Some(&record()), false).unwrap();

        let mut kept = list_names(&forklift).unwrap();
        kept.sort();
        assert_eq!(kept, vec!["franchise", "lock", "objects"]);
        assert_eq!(list_names(&dir).unwrap(), vec![".forklift"]);
        assert!(objects.join("ab/cdef.sig").exists(), "an anchored resume keeps its sidecars");

        // Unanchored: the sidecars go too, and a finished bundle is kept when asked.
        std::fs::create_dir_all(forklift.join("bundles")).unwrap();
        let unanchored = FranchiseRecord { genesis: None, ..record() };
        prepare_resume(&dir, Some(&unanchored), true).unwrap();
        assert!(!objects.join("ab/cdef.sig").exists());
        assert!(objects.join("ab/cdef").exists());
        assert!(forklift.join("bundles").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_object_store_paths_are_taint_deletions() {
        assert!(is_object_store_path(Path::new("objects/ab/cdef")));
        assert!(!is_object_store_path(Path::new("objects")));
        assert!(!is_object_store_path(Path::new("objects/../config")));
        assert!(!is_object_store_path(Path::new("pallets/main")));
        assert!(!is_object_store_path(Path::new("/objects/ab")));
    }

    #[test]
    fn the_gate_refuses_a_record_a_temp_and_an_unreadable_record() {
        let dir = temp_dir("gate");
        let forklift = dir.join(".forklift");
        std::fs::create_dir(&forklift).unwrap();

        assert!(gate(&dir).is_ok(), "a warehouse without a record is entered");

        let code = |result: Result<(), CoreError>| match result {
            Err(CoreError::Refusal { code, details, .. }) => (code, details.unwrap()),
            other => panic!("expected a refusal, got {:?}", other),
        };

        write(&forklift, &record()).unwrap();
        let (refused, details) = code(gate(&dir));
        assert_eq!(refused, RefusalCode::FranchiseIncomplete);
        assert_eq!(details["cause_code"], "error");
        assert_eq!(details["retry"], "unknown");

        std::fs::write(record_path(&forklift), b"torn").unwrap();
        assert_eq!(code(gate(&dir)).0, RefusalCode::FranchiseIncomplete);

        std::fs::remove_file(record_path(&forklift)).unwrap();
        std::fs::write(forklift.join("franchise.tmp~1-0"), b"x").unwrap();
        assert_eq!(code(gate(&dir)).0, RefusalCode::FranchiseIncomplete);

        finish(&forklift).unwrap();
        assert!(gate(&dir).is_ok(), "finishing clears the temps with the record");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_resume_command_quotes_what_a_shell_would_split() {
        let request = Request { pallet: Some("it's".to_string()), only: vec!["a b".to_string(), "docs".to_string()] };
        assert_eq!(
            resume_command("http://h:1", "/tmp/my dir", &request),
            "forklift franchise http://h:1 '/tmp/my dir' --pallet 'it'\\''s' --only 'a b' --only docs"
        );
    }

    #[test]
    fn a_claim_puts_the_record_in_place_and_a_second_claim_is_refused() {
        let root = temp_dir("claim");

        // A fresh nested target: ancestors and the target are created, the record is inside.
        let target = root.join("a/b/c");
        claim(&target, &record()).unwrap();
        assert_eq!(read(&target.join(FOLDER_NAME_FORKLIFT_ROOT)), Ok(Some(record())));
        assert_eq!(list_names(&target).unwrap(), vec![std::ffi::OsString::from(FOLDER_NAME_FORKLIFT_ROOT)],
            "nothing but the claimed folder: the staging name is gone");

        let second = claim(&target, &record());
        assert!(second.unwrap_err().contains("claimed by another franchise"));
        assert_eq!(read(&target.join(FOLDER_NAME_FORKLIFT_ROOT)), Ok(Some(record())), "the winner's record stands");

        // A pre-existing empty target, and one holding a stopped claim's staging folder.
        let empty = root.join("empty");
        std::fs::create_dir_all(empty.join(".forklift.tmp~1-0")).unwrap();
        claim(&empty, &record()).unwrap();
        assert!(read(&empty.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap().is_some());

        // Anything else in it is never claimed over.
        let busy = root.join("busy");
        std::fs::create_dir_all(&busy).unwrap();
        std::fs::write(busy.join("notes.txt"), "mine").unwrap();
        assert!(claim(&busy, &record()).unwrap_err().contains("is not empty"));
        assert!(!busy.join(FOLDER_NAME_FORKLIFT_ROOT).exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_claim_never_takes_a_target_that_holds_a_forklift_folder() {
        // An empty `.forklift` is a stopped run's debris: a resume's, never a fresh claim's.
        let root = temp_dir("claim-race");
        let target = root.join("t");
        std::fs::create_dir_all(target.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();

        assert!(claim(&target, &record()).is_err());
        assert_eq!(read(&target.join(FOLDER_NAME_FORKLIFT_ROOT)), Ok(None), "the existing folder is untouched");
        assert!(list_names(&target).unwrap().iter().all(|name| !is_staging_leftover(name)),
            "a refused claim removes its staging folder");

        let _ = std::fs::remove_dir_all(&root);
    }
}
