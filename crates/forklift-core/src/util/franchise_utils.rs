//! The record of a clone that has not finished: `.forklift/franchise`, and what reads it.
//!
//! A clone that has claimed its target directory leaves this record standing until it
//! succeeds, so an error, a kill or a power loss part-way through never loses what it fetched.
//! Running the same `clone` again resumes it: the verified object store is kept and the rest
//! of the warehouse is rebuilt (see [`prepare_resume`] for what is kept). Every other command
//! refuses to enter a warehouse that holds the record ([`gate`]), with the code
//! `clone_incomplete`, and names the way out.
//!
//! ## Storage
//!
//! One [`record_utils`] record, `franchise`, in the warehouse's `.forklift` folder, replaced
//! atomically. Each entry is `key=value`; a list repeats its key. A record this build cannot read
//! is never taken for "no clone in progress": the gate refuses, and a resume admits it only as
//! the debris of a run killed before its first write was durable.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use crate::error::{CoreError, RefusalCode};
use crate::globals::{FOLDER_NAME_FORKLIFT_ROOT, FOLDER_NAME_OBJECTS_ROOT};
use crate::util::{file_utils, pack_utils, record_utils, remote_utils, signature_table, taint_utils, warehouse_utils};
use crate::util::bundle_utils::{FILE_NAME_DOWNLOAD, FILE_NAME_LATEST, FOLDER_NAME_BUNDLES};
use crate::util::remote_utils::PartialBundle;
use crate::util::warehouse_utils::Placement;
use crate::util::record_utils::RecordError;

/// The name of the record in the warehouse's `.forklift` folder.
pub const FILE_NAME_FRANCHISE: &str = "franchise";

/// The [`record_utils`] kind and version of the record.
const FRANCHISE_RECORD: &str = "franchise";
const FRANCHISE_VERSION: u32 = 2;

/// The name of the warehouse lock file, which a resume keeps: it is the resume's own.
const FILE_NAME_LOCK: &str = "lock";

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

/// The part of a clone a failure happened in.
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

/// How far the remote's bundle got.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BundleState {
    /// Nothing of it is kept: not downloaded, not resumable, or discarded.
    None,

    /// Part or all of it is downloaded, not yet imported: the download file holds the first
    /// `length` bytes, synced (see [`remote_utils::RemoteClient::download_bundle`]).
    Partial(PartialBundle),

    /// It was imported in full.
    Imported,
}

impl BundleState {
    /// The download this state keeps, if it keeps one.
    pub fn partial(&self) -> Option<&PartialBundle> {
        match self {
            BundleState::Partial(partial) => Some(partial),
            _ => None,
        }
    }
}

/// Whether the bundle went further during an attempt that started at `start` and stands at `now`:
/// it was imported, more of it was recorded under the same tag, or some of it was recorded under
/// another tag.
pub fn bundle_progressed(start: &BundleState, now: &BundleState) -> bool {
    match (start, now) {
        (BundleState::Imported, _) => false,
        (_, BundleState::Imported) => true,
        (BundleState::Partial(start), BundleState::Partial(now)) if start.etag == now.etag => now.length > start.length,
        (_, BundleState::Partial(now)) => now.length > 0,
        (_, BundleState::None) => false,
    }
}

/// What an attempt was asked to clone, beyond the URL: the parts a resume may change.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
    /// The `--pallet` asked for; `None` for the remote's default.
    pub pallet: Option<String>,

    /// The `--only` prefixes, as normalized keys; empty for a full clone.
    pub only: Vec<String>,

    /// Whether `--cache` was asked for.
    pub cache: bool,
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

/// The record of an unfinished clone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FranchiseRecord {
    /// The remote's URL.
    pub url: String,

    /// The access token, if one was given: a resume without `--token` reuses it.
    pub token: Option<String>,

    /// The current (or last) attempt's request.
    pub request: Request,

    /// The trust genesis this clone is bound to: the `--expect` pin, else the genesis the
    /// remote announced on first contact. `None` when the remote announced no anchor.
    pub genesis: Option<String>,

    /// How far the remote's bundle got.
    pub bundle: BundleState,

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

        match &self.bundle {
            BundleState::None => entries.push("bundle=none".to_string()),
            BundleState::Imported => entries.push("bundle=imported".to_string()),
            BundleState::Partial(partial) => {
                entries.push("bundle=partial".to_string());
                entries.push(format!("bundle.etag={}", partial.etag));
                entries.push(format!("bundle.total={}", partial.total));
                entries.push(format!("bundle.length={}", partial.length));
            }
        }

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
        let mut bundle = None;
        let mut bundle_etag = None;
        let mut bundle_total = None;
        let mut bundle_length = None;
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
                "cache" => request.cache = parse_cache(&value).ok_or_else(|| malformed("cache"))?,
                "genesis" => genesis = Some(value),
                "bundle" => bundle = Some(match value.as_str() {
                    "none" | "partial" | "imported" => value,
                    _ => return Err(malformed("bundle")),
                }),
                "bundle.etag" => bundle_etag = Some(value),
                "bundle.total" => bundle_total = Some(value.parse::<u64>().map_err(|_| malformed("bundle.total"))?),
                "bundle.length" => bundle_length = Some(value.parse::<u64>().map_err(|_| malformed("bundle.length"))?),
                "attempt" => attempt = Some(match value.as_str() {
                    "running" => Attempt::Running,
                    "failed" => Attempt::Failed,
                    _ => return Err(malformed("attempt")),
                }),
                "census" => start_census = Some(value.parse::<u64>().map_err(|_| malformed("census"))?),
                "failure.pallet" => failure_request.pallet = Some(value),
                "failure.only" => failure_request.only.push(value),
                "failure.cache" => failure_request.cache = parse_cache(&value).ok_or_else(|| malformed("failure.cache"))?,
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

        let bundle = match (bundle.as_deref(), bundle_etag, bundle_total, bundle_length) {
            (Some("none"), None, None, None) => BundleState::None,
            (Some("imported"), None, None, None) => BundleState::Imported,
            (Some("partial"), Some(etag), Some(total), Some(length))
                if length <= total && remote_utils::is_resumable_bundle_tag(&etag) => {
                BundleState::Partial(PartialBundle { etag, total, length })
            }
            _ => return Err(malformed("bundle")),
        };

        Ok(FranchiseRecord {
            url: url.ok_or_else(|| malformed("url"))?,
            token,
            request,
            genesis,
            bundle,
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

    if request.cache {
        entries.push(format!("{}cache=true", prefix));
    }
}

/// A recorded `cache` entry: written only when set, so `true` is its one value.
fn parse_cache(value: &str) -> Option<bool> {
    (value == "true").then_some(true)
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
                format!("The clone record \"{}\" {}", path.to_string_lossy(), why)
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
        .map_err(|e| format!("Error while recording the clone in progress: {}", e))
}

/// Remove the record in `forklift`, with any temp file a replacement left behind, and sync the
/// folder: the last step of a clone that finished, so the warehouse is never gated again.
pub fn finish(forklift: &Path) -> Result<(), String> {
    for temp in record_temps(forklift)? {
        file_utils::remove_unfollowed(&temp)?;
    }

    match std::fs::remove_file(record_path(forklift)) {
        Ok(()) => (),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (),
        Err(e) => return Err(format!(
            "Error while clearing the clone record \"{}\": {}", record_path(forklift).to_string_lossy(), e
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

/// Whether `name`, an entry of a clone's target directory, is a `.forklift` folder a
/// clone was still staging when it stopped: the claim builds `.forklift` under a temp name
/// and renames it into place.
pub fn is_staging_leftover(name: &OsStr) -> bool {
    is_temp_of(name, FOLDER_NAME_FORKLIFT_ROOT)
}

/// Claim `target`, which [`admit`] judged [`Admission::Fresh`], for a new clone: create it
/// (and any missing ancestor) if absent, then put its `.forklift` folder in place with `record`
/// already inside. The folder is built under a staging name and renamed into place only if no
/// `.forklift` exists ([`warehouse_utils::place_forklift`]), so the directory is never a warehouse
/// without its record: a kill at any point leaves either no `.forklift` (only a staging leftover,
/// which [`admit`] ignores) or one the gate refuses. Two clones racing for one directory
/// cannot both claim it.
///
/// An ancestor is created with the tolerant `create_dir_all` and never removed: an empty one never
/// blocks a retry.
pub fn claim(target: &Path, record: &FranchiseRecord) -> Result<(), String> {
    if let Some(parent) = target.parent().filter(|parent| !parent.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Error while creating \"{}\": {}", parent.to_string_lossy(), e))?;
    }

    let taken = || format!(
        "\"{}\" was claimed by another clone while this one was connecting; clone into a new \
        or empty directory.", target.to_string_lossy()
    );

    match std::fs::create_dir(target) {
        Ok(()) => (),
        // Absent before the handshake, or found then: either way it must still be fresh now.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => match admit(target)? {
            Admission::Fresh => (),
            Admission::Resume(_) => return Err(taken()),
        },
        Err(e) => return Err(format!("Error while creating \"{}\": {}", target.to_string_lossy(), e)),
    }

    match warehouse_utils::place_forklift(&target.join(FOLDER_NAME_FORKLIFT_ROOT), |staged| write(staged, record))? {
        Placement::Placed => Ok(()),
        Placement::Taken => Err(taken()),
    }
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

/// How a target directory stands before a clone into it.
#[derive(Debug, PartialEq, Eq)]
pub enum Admission {
    /// Absent, or empty but for `.forklift` folders a stopped claim was staging.
    Fresh,

    /// An unfinished clone: its record, or `None` for a run that stopped before its record was
    /// durable (its `.forklift` holds nothing but the lock, record temps and an unreadable record).
    Resume(Option<FranchiseRecord>),
}

/// Judge `target` for a clone: a fresh one, a resume, or a refusal. Read-only.
pub fn admit(target: &Path) -> Result<Admission, String> {
    match std::fs::symlink_metadata(target) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Admission::Fresh),
        Err(e) => return Err(format!("Error while checking \"{}\": {}", target.to_string_lossy(), e)),
        Ok(_) => (),
    }

    if !target.is_dir() {
        return Err(format!(
            "\"{}\" already exists and is not a directory; clone into a new or empty directory.",
            target.to_string_lossy()
        ));
    }

    let names = list_names(target)?;
    let names: Vec<&std::ffi::OsString> = names.iter().filter(|name| !is_staging_leftover(name)).collect();

    if names.is_empty() {
        return Ok(Admission::Fresh);
    }

    let not_empty = || format!(
        "\"{}\" is not empty; clone into a new or empty directory.", target.to_string_lossy()
    );

    if !names.iter().any(|name| name.as_os_str() == FOLDER_NAME_FORKLIFT_ROOT) {
        return Err(not_empty());
    }

    let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);

    // Never judged through a link: a resume deletes inside it.
    if !is_real_dir(&forklift)? {
        return Err(not_empty());
    }

    let only_debris = names.len() == 1 && holds_only_debris(&forklift)?;

    match read(&forklift) {
        Ok(Some(record)) => Ok(Admission::Resume(Some(record))),
        Ok(None) if only_debris => Ok(Admission::Resume(None)),
        Ok(None) => Err(not_empty()),
        Err(MarkerError::Torn(_)) if only_debris => Ok(Admission::Resume(None)),
        Err(e) => Err(format!(
            "\"{}\" holds a clone that did not finish, but its record cannot be read: {} Delete \
            the directory and clone again.",
            target.to_string_lossy(), e.describe(&record_path(&forklift))
        )),
    }
}

/// Whether `forklift` holds nothing but the lock, the record and its temps: the shape a run
/// killed before its record was durable leaves.
fn holds_only_debris(forklift: &Path) -> Result<bool, String> {
    Ok(list_names(forklift)?.iter().all(|name| {
        name == FILE_NAME_LOCK || name == FILE_NAME_FRANCHISE || is_temp_of(name, FILE_NAME_FRANCHISE)
    }))
}

/// Whether `path` is a folder itself, not a link to one. An absent path is not.
fn is_real_dir(path: &Path) -> Result<bool, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(metadata.is_dir()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Error while checking \"{}\": {}", path.to_string_lossy(), e)),
    }
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
///    recorded path is deleted, and each deletion is made durable before step 2 drops the taint
///    records. Without an anchor to verify against (`record` is `None`, or recorded no genesis),
///    every kept signature, sidecar or table, is deleted too, so the resume fetches them again
///    exactly as a fresh clone would.
/// 2. Every entry of `.forklift` but the object store, the record, the held lock and, unless
///    `keep` is [`BundleState::None`], the bundles. Refs above all: the history walk stops at any
///    parcel a local ref holds, and a ref an earlier attempt wrote is complete only at that
///    attempt's scope.
/// 3. Every entry of the bundles but the one file `keep` names: the download, cut back to the
///    length `keep` says was synced (and removed when shorter), or the latest bundle once
///    imported. So a temp a killed download left never lingers.
/// 4. Every entry of the target but `.forklift`: the partial working tree.
///
/// Nothing is followed through a link at any level below the target, which the user named: a
/// `.forklift` that is not a real folder is refused, an object store, fan-out or bundles folder
/// that is not one is removed as itself, and every other link is removed as itself.
///
/// Returns what is kept of the bundle: `keep`, or [`BundleState::None`] when its file is gone, or
/// for an import when the object store was discarded, so nothing an earlier attempt stored is
/// kept.
pub fn prepare_resume(target: &Path, record: Option<&FranchiseRecord>, keep: &BundleState) -> Result<BundleState, String> {
    let forklift = target.join(FOLDER_NAME_FORKLIFT_ROOT);
    let objects = forklift.join(FOLDER_NAME_OBJECTS_ROOT);

    if !is_real_dir(&forklift)? {
        return Err(format!("\"{}\" is not a folder; delete \"{}\" and clone again.",
                           forklift.to_string_lossy(), target.to_string_lossy()));
    }

    let taints = taint_utils::read_taints(&forklift)?;
    let discarded = taints.torn || (std::fs::symlink_metadata(&objects).is_ok() && !is_real_dir(&objects)?);

    if discarded {
        file_utils::remove_unfollowed(&objects)?;
        file_utils::sync_dir(&forklift)?;
    } else {
        let mut touched = std::collections::BTreeSet::new();

        for recorded in &taints.recorded {
            if is_object_store_path(recorded) {
                touched.insert(remove_in_store(&forklift, recorded)?);
            }
        }

        for folder in touched {
            match file_utils::sync_dir(&folder) {
                // A folder an earlier removal in this loop took with it.
                Err(_) if std::fs::symlink_metadata(&folder).is_err() => (),
                synced => synced?,
            }
        }
    }

    if record.is_none_or(|record| record.genesis.is_none()) {
        remove_signature_sidecars(&objects)?;
    }

    let keep = match keep {
        BundleState::Imported if discarded => &BundleState::None,
        keep => keep,
    };

    for name in list_names(&forklift)? {
        let kept = name == FOLDER_NAME_OBJECTS_ROOT
            || name == FILE_NAME_FRANCHISE
            || name == FILE_NAME_LOCK
            || (*keep != BundleState::None && name == FOLDER_NAME_BUNDLES);

        if !kept {
            file_utils::remove_unfollowed(&forklift.join(&name))?;
        }
    }

    let bundle = keep_bundle(&forklift.join(FOLDER_NAME_BUNDLES), keep)?;

    for name in list_names(target)? {
        if name != FOLDER_NAME_FORKLIFT_ROOT {
            file_utils::remove_unfollowed(&target.join(&name))?;
        }
    }

    file_utils::sync_dir(&forklift)?;
    file_utils::sync_dir(target)?;

    Ok(bundle)
}

/// Clear the bundles folder but the one file `keep` names (step 3 of [`prepare_resume`]), and
/// return what is kept. A bundles folder that is not a real one is removed as itself.
fn keep_bundle(bundles: &Path, keep: &BundleState) -> Result<BundleState, String> {
    let name = match keep {
        BundleState::None => return Ok(BundleState::None),
        BundleState::Partial(_) => FILE_NAME_DOWNLOAD,
        BundleState::Imported => FILE_NAME_LATEST,
    };

    if !is_real_dir(bundles)? {
        file_utils::remove_unfollowed(bundles)?;
        return Ok(BundleState::None);
    }

    for entry in list_names(bundles)? {
        if entry != name {
            file_utils::remove_unfollowed(&bundles.join(&entry))?;
        }
    }

    let kept = match keep {
        BundleState::Partial(partial) => cut_download(&bundles.join(name), partial)?,
        keep => keep.clone(),
    };

    file_utils::sync_dir(bundles)?;

    Ok(kept)
}

/// Cut the download at `path` back to the length `partial` says was synced: bytes past it never
/// were, so they are not trusted. A download that is not a file, or is shorter, is removed.
fn cut_download(path: &Path, partial: &PartialBundle) -> Result<BundleState, String> {
    if !holds_download(path, partial) {
        file_utils::remove_unfollowed(path)?;
        return Ok(BundleState::None);
    }

    let file = std::fs::OpenOptions::new().write(true).open(path)
        .map_err(|e| format!("Error while opening \"{}\": {}", path.to_string_lossy(), e))?;
    let cut_error = |e: std::io::Error| format!("Error while cutting \"{}\" back: {}", path.to_string_lossy(), e);

    file.set_len(partial.length).map_err(cut_error)?;

    if file_utils::fsync_enabled() {
        file.sync_all().map_err(cut_error)?;
    }

    Ok(BundleState::Partial(partial.clone()))
}

/// Whether `path` is a file, not a link, at least as long as `partial` says was synced to it.
fn holds_download(path: &Path, partial: &PartialBundle) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file() && metadata.len() >= partial.length)
}

/// Remove `recorded`, a path inside the object store relative to `forklift`, judging every level
/// unfollowed: where a folder on the way is not a real one, that entry is removed as itself instead
/// (nothing beneath it is the store's). Returns the folder whose entry was removed.
fn remove_in_store(forklift: &Path, recorded: &Path) -> Result<PathBuf, String> {
    let mut parent = forklift.to_path_buf();
    let mut components = recorded.components().peekable();

    while let Some(component) = components.next() {
        let path = parent.join(component);

        if components.peek().is_none() || !is_real_dir(&path)? {
            file_utils::remove_unfollowed(&path)?;
            return Ok(parent);
        }

        parent = path;
    }

    Ok(parent)
}

/// What `forklift` still holds of the bundle `state` describes, after a clearing that stopped
/// partway: an import needs the object store and the bundles, each a real folder, and a download
/// its file in a real bundles folder, at least as long as recorded.
pub fn held_bundle(forklift: &Path, state: &BundleState) -> BundleState {
    let bundles = forklift.join(FOLDER_NAME_BUNDLES);
    let held = is_real_dir(&bundles).unwrap_or(false) && match state {
        BundleState::None => false,
        BundleState::Partial(partial) => holds_download(&bundles.join(FILE_NAME_DOWNLOAD), partial),
        BundleState::Imported => is_real_dir(&forklift.join(FOLDER_NAME_OBJECTS_ROOT)).unwrap_or(false),
    };

    if held { state.clone() } else { BundleState::None }
}

/// Whether `path`, relative to `.forklift`, names something inside the object store.
fn is_object_store_path(path: &Path) -> bool {
    let mut components = path.components();

    components.next() == Some(Component::Normal(OsStr::new(FOLDER_NAME_OBJECTS_ROOT)))
        && components.clone().next().is_some()
        && components.all(|component| matches!(component, Component::Normal(_)))
}

/// Delete every signature the store holds: each sidecar (`<hash>.sig`) in the loose store's
/// fan-out folders, and each signature table (`pack/<id>.sigs`).
fn remove_signature_sidecars(objects: &Path) -> Result<(), String> {
    let table_suffix = format!(".{}", signature_table::SIGNATURE_TABLE_EXTENSION);

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

        let is_pack_folder = folder.file_name() == OsStr::new(pack_utils::PACK_FOLDER_NAME);

        for file in list_names(&folder.path())? {
            let name = file.to_string_lossy();

            if name.ends_with(".sig") || (is_pack_folder && name.ends_with(&table_suffix)) {
                file_utils::remove_unfollowed(&folder.path().join(file))?;
            }
        }
    }

    Ok(())
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

/// The `clone` command that resumes a clone of `url` into `directory` with `request`.
/// Never the token (a resume reuses the recorded one) and never `--expect` (a resume enforces
/// the recorded genesis itself).
pub fn resume_command(url: &str, directory: &str, request: &Request) -> String {
    let mut command = format!("forklift clone {} {}", quote(url), quote(directory));

    if let Some(pallet) = &request.pallet {
        command.push_str(&format!(" --pallet {}", quote(pallet)));
    }

    for only in &request.only {
        command.push_str(&format!(" --only {}", quote(only)));
    }

    if request.cache {
        command.push_str(" --cache");
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

/// The next step for a clone whose last attempt failed with `retry`.
fn failed_next_step(command: &str, directory: &str, retry: Retry) -> String {
    match retry {
        Retry::Same | Retry::Unknown => format!(
            "Run \"{}\" to resume it: what was fetched is kept, and everything else in \"{}\" is \
            replaced. Or delete \"{}\".", command, directory, directory
        ),
        Retry::Change => format!(
            "Running it unchanged fails the same way. Run \"{}\" with a different --only or --pallet \
            (what was fetched is kept, and everything else in \"{}\" is replaced), or delete \"{}\".",
            command, directory, directory
        ),
    }
}

/// The `clone_incomplete` refusal for a clone's own failure after its claim: `cause` with
/// the directory kept, and the next step `retry` earns.
pub fn failure_refusal(directory: &str, record: &FranchiseRecord, cause: &str, failure: &Failure) -> CoreError {
    let message = match CoreError::from(cause.to_string()) {
        CoreError::Refusal { message, .. } | CoreError::Other(message) => message,
    };

    let command = resume_command(&record.url, directory, &record.request);

    CoreError::refusal_with_details(
        RefusalCode::CloneIncomplete,
        format!("{} The clone did not finish; \"{}\" was kept with what was fetched.", message.trim_end(), directory),
        failed_next_step(&command, directory, failure.retry),
        details(directory, Some(&record.url), Some(failure)),
    )
}

/// The `clone_incomplete` details: the directory, the remote when known, and the last
/// failure's code and retry advice when there is one.
pub fn details(directory: &str, remote: Option<&str>, failure: Option<&Failure>) -> serde_json::Value {
    serde_json::json!({
        "directory": directory,
        "remote": remote,
        "cause_code": failure.map(|failure| failure.cause_code.as_str()),
        "retry": failure.map(|failure| failure.retry.as_str()),
    })
}

/// Refuse to enter the warehouse at `root` while it holds an unfinished clone: a record, a
/// record's temp file or an unreadable record. Runs before any lock is taken, for every command,
/// so a command run while a clone is still running is told that, not that the lock is held.
pub fn gate(root: &Path) -> Result<(), CoreError> {
    let forklift = root.join(FOLDER_NAME_FORKLIFT_ROOT);

    let directory = root.to_string_lossy().into_owned();

    // Whether a clone is unfinished cannot be told: refused, never guessed either way.
    let unknowable = |why: String| CoreError::refusal_with_details(
        RefusalCode::CloneIncomplete,
        format!("Whether \"{}\" holds a clone that has not finished cannot be checked: {}", directory, why),
        format!("Make \"{}\" readable and run the command again.", forklift.to_string_lossy()),
        details(&directory, None, None),
    );

    let record = match read(&forklift) {
        Ok(Some(record)) => Some(record),
        Ok(None) if record_temps(&forklift).map_err(unknowable)?.is_empty() => return Ok(()),
        Ok(None) | Err(MarkerError::Torn(_) | MarkerError::Foreign(_)) => None,
        Err(e) => return Err(unknowable(e.describe(&record_path(&forklift)))),
    };
    let lock = forklift.join(FILE_NAME_LOCK);
    let locked = std::fs::symlink_metadata(&lock).is_ok();

    let Some(record) = record else {
        let next_step = if locked {
            format!(
                "A clone may still be running there. If none is, remove the stale lock \"{}\" and run \
                \"forklift clone <url> {}\" with the URL it was started with; or delete \"{}\".",
                lock.to_string_lossy(), quote(&directory), directory
            )
        } else {
            format!(
                "Run \"forklift clone <url> {}\" with the URL it was started with, or delete \"{}\".",
                quote(&directory), directory
            )
        };

        return Err(CoreError::refusal_with_details(
            RefusalCode::CloneIncomplete,
            format!("\"{}\" holds a clone that has not finished.", directory),
            next_step,
            details(&directory, None, None),
        ));
    };

    let command = resume_command(&record.url, &directory, &record.request);
    let failure = record.last_failure.as_ref().filter(|_| record.attempt == Attempt::Failed);

    let message = match failure {
        Some(failure) => format!(
            "\"{}\" holds a clone of {} that has not finished: its last attempt failed ({}).",
            directory, record.url, failure.cause_code
        ),
        None if locked => format!(
            "\"{}\" holds a clone of {} that has not finished: it may still be running.",
            directory, record.url
        ),
        None => format!(
            "\"{}\" holds a clone of {} that has not finished: it was interrupted.", directory, record.url
        ),
    };

    let next_step = if locked {
        format!(
            "A clone may still be running there. If none is, remove the stale lock \"{}\", then run \
            \"{}\" to resume it; or delete \"{}\".",
            lock.to_string_lossy(), command, directory
        )
    } else {
        match failure {
            Some(failure) => failed_next_step(&command, &directory, failure.retry),
            None => failed_next_step(&command, &directory, Retry::Same),
        }
    };

    Err(CoreError::refusal_with_details(
        RefusalCode::CloneIncomplete,
        message,
        next_step,
        details(&directory, Some(&record.url), failure),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-clone-utils-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn record() -> FranchiseRecord {
        FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: Some("se=cret\nline".to_string()),
            request: Request { pallet: Some("main".to_string()), only: vec!["docs".to_string(), "src/a b".to_string()], cache: false },
            genesis: Some("abc".to_string()),
            bundle: BundleState::Imported,
            attempt: Attempt::Failed,
            start_census: 42,
            last_failure: Some(Failure {
                request: Request { pallet: None, only: Vec::new(), cache: true },
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

        let cache = FranchiseRecord { request: Request { cache: true, ..Request::default() }, ..record() };
        assert_eq!(FranchiseRecord::from_bytes(&cache.to_bytes()), Ok(cache));
    }

    /// `cache` is written only when set, so any value but `true` is a torn record, never a
    /// clone without `--cache`.
    ///
    /// Revert: read any `cache` value as false → red.
    #[test]
    fn a_cache_entry_other_than_true_is_torn() {
        let text = String::from_utf8(record().to_bytes()).unwrap();
        assert!(text.contains("failure.cache=true\n"), "{}", text);

        for torn in [text.replacen("failure.cache=true\n", "failure.cache=false\n", 1),
                     text.replacen("failure.cache=true\n", "failure.cache=\n", 1)] {
            assert!(matches!(FranchiseRecord::from_bytes(torn.as_bytes()), Err(MarkerError::Torn(_))), "{}", torn);
        }
    }

    /// Each bundle state round-trips, a kept download with a tag that holds quotes and the
    /// record's own separator among them.
    ///
    /// Revert: write a kept download without its tag, or read it back as `none` → red.
    #[test]
    fn a_record_round_trips_each_bundle_state() {
        let partial = PartialBundle { etag: "\"a=b'c\"".to_string(), total: 1 << 40, length: 7 };

        for bundle in [BundleState::None, BundleState::Imported, BundleState::Partial(partial.clone())] {
            let record = FranchiseRecord { bundle, ..record() };
            assert_eq!(FranchiseRecord::from_bytes(&record.to_bytes()), Ok(record));
        }

        // A kept download is read only whole and consistent: a missing field, a length past the
        // total, or a tag no download is kept under is a torn record, never `none`.
        let text = String::from_utf8(FranchiseRecord { bundle: BundleState::Partial(partial), ..record() }.to_bytes()).unwrap();
        for torn in [
            text.replacen("bundle.length=7\n", "", 1),
            text.replacen("bundle.length=7\n", &format!("bundle.length={}\n", (1u64 << 40) + 1), 1),
            text.replacen("bundle.etag=\"a=b'c\"\n", "bundle.etag=W/\"a\"\n", 1),
            text.replacen("bundle=partial\n", "bundle=none\n", 1),
        ] {
            assert_ne!(torn, text);
            assert!(matches!(FranchiseRecord::from_bytes(torn.as_bytes()), Err(MarkerError::Torn(_))), "{}", torn);
        }
    }

    /// The bundle went further when it was imported, when more of it was recorded under one tag,
    /// or when some of it was recorded under another.
    #[test]
    fn bundle_progress_is_an_import_or_more_bytes_recorded() {
        let at = |etag: &str, length| BundleState::Partial(PartialBundle { etag: etag.to_string(), total: 10, length });

        assert!(bundle_progressed(&BundleState::None, &BundleState::Imported));
        assert!(bundle_progressed(&at("\"a\"", 4), &BundleState::Imported));
        assert!(bundle_progressed(&at("\"a\"", 4), &at("\"a\"", 5)));
        assert!(bundle_progressed(&at("\"a\"", 4), &at("\"b\"", 1)));
        assert!(bundle_progressed(&BundleState::None, &at("\"a\"", 1)));

        assert!(!bundle_progressed(&BundleState::Imported, &BundleState::Imported));
        assert!(!bundle_progressed(&at("\"a\"", 4), &at("\"a\"", 4)));
        assert!(!bundle_progressed(&at("\"a\"", 4), &at("\"b\"", 0)));
        assert!(!bundle_progressed(&BundleState::None, &at("\"a\"", 0)));
        assert!(!bundle_progressed(&at("\"a\"", 4), &BundleState::None));
    }

    #[test]
    fn a_record_this_build_cannot_read_is_never_read_as_none() {
        let bytes = record().to_bytes();

        // Cut short: torn.
        assert!(matches!(FranchiseRecord::from_bytes(&bytes[..bytes.len() - 1]), Err(MarkerError::Torn(_))));

        // Another version, and a field this build does not know: foreign.
        let other = String::from_utf8(bytes.clone()).unwrap().replacen("/forklift-franchise 2", "/forklift-franchise 3", 1);
        assert!(matches!(FranchiseRecord::from_bytes(other.as_bytes()), Err(MarkerError::Foreign(_))));

        // A version-1 record, from before a download was kept, is foreign like any other.
        let first = String::from_utf8(bytes.clone()).unwrap().replacen("/forklift-franchise 2", "/forklift-franchise 1", 1);
        assert_ne!(first.as_bytes(), &bytes[..]);
        assert!(matches!(FranchiseRecord::from_bytes(first.as_bytes()), Err(MarkerError::Foreign(_))));

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
        std::fs::write(record_path(&forklift), b"/forklift-franchise 2\nurl=x").unwrap();
        assert!(admit(&dir).is_err(), "a torn record beside other entries is refused");
        std::fs::remove_file(dir.join("half-written")).unwrap();
        std::fs::write(forklift.join("lock"), b"1").unwrap();
        assert_eq!(admit(&dir), Ok(Admission::Resume(None)));

        // A record from another version is refused, never guessed at.
        std::fs::write(record_path(&forklift), b"/forklift-franchise 9\n").unwrap();
        assert!(admit(&dir).is_err());

        // A finished warehouse is not a clone to resume.
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
        std::fs::create_dir_all(objects.join("pack")).unwrap();
        std::fs::create_dir_all(forklift.join("pallets")).unwrap();
        std::fs::create_dir_all(forklift.join("bundles")).unwrap();
        std::fs::write(objects.join("ab/cdef"), b"x").unwrap();
        std::fs::write(objects.join("ab/cdef.sig"), b"x").unwrap();
        std::fs::write(objects.join("pack/p.pack"), b"x").unwrap();
        std::fs::write(objects.join("pack/t.sigs"), b"x").unwrap();
        std::fs::write(forklift.join("pallets/main"), b"x").unwrap();
        std::fs::write(forklift.join("operation"), b"x").unwrap();
        std::fs::write(forklift.join("lock"), b"1").unwrap();
        std::fs::write(dir.join("half-written"), b"x").unwrap();
        write(&forklift, &record()).unwrap();

        prepare_resume(&dir, Some(&record()), &BundleState::None).unwrap();

        let mut kept = list_names(&forklift).unwrap();
        kept.sort();
        assert_eq!(kept, vec!["franchise", "lock", "objects"]);
        assert_eq!(list_names(&dir).unwrap(), vec![".forklift"]);
        assert!(objects.join("ab/cdef.sig").exists(), "an anchored resume keeps its sidecars");
        assert!(objects.join("pack/t.sigs").exists(), "an anchored resume keeps its signature tables");

        // Unanchored: every signature goes too, sidecars and tables (FORK-333), and a finished
        // bundle is kept when asked.
        std::fs::create_dir_all(forklift.join("bundles")).unwrap();
        let unanchored = FranchiseRecord { genesis: None, ..record() };
        assert_eq!(prepare_resume(&dir, Some(&unanchored), &BundleState::Imported), Ok(BundleState::Imported));
        assert!(!objects.join("ab/cdef.sig").exists());
        assert!(!objects.join("pack/t.sigs").exists(), "an unanchored resume removes the signature tables");
        assert!(objects.join("ab/cdef").exists());
        assert!(objects.join("pack/p.pack").exists());
        assert!(forklift.join("bundles").exists());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// A resume keeps only the bundle file its record names: the download, cut back to the length
    /// recorded as synced, or the imported latest bundle. Anything else in the bundles folder — a
    /// temp a killed download left — goes, and a download shorter than recorded is dropped.
    ///
    /// Revert: keep the bundles folder whole → the stray survives → red.
    #[test]
    fn a_resume_keeps_only_the_bundle_file_its_record_names() {
        let dir = temp_dir("resume-bundle");
        let forklift = dir.join(".forklift");
        let bundles = forklift.join("bundles");
        let partial = BundleState::Partial(PartialBundle { etag: "\"t\"".to_string(), total: 10, length: 4 });
        let populate = |download: &[u8]| {
            std::fs::create_dir_all(&bundles).unwrap();
            std::fs::write(bundles.join("partial"), download).unwrap();
            std::fs::write(bundles.join("latest"), b"latest").unwrap();
            std::fs::write(bundles.join("partial.tmp.1"), b"stray").unwrap();
        };

        populate(b"abcdef");
        assert_eq!(prepare_resume(&dir, Some(&record()), &partial), Ok(partial.clone()));
        assert_eq!(list_names(&bundles).unwrap(), vec!["partial"]);
        assert_eq!(std::fs::read(bundles.join("partial")).unwrap(), b"abcd", "cut back to the synced length");

        populate(b"abc");
        assert_eq!(prepare_resume(&dir, Some(&record()), &partial), Ok(BundleState::None));
        assert!(list_names(&bundles).unwrap().is_empty(), "a download shorter than recorded is dropped");

        populate(b"abcd");
        assert_eq!(prepare_resume(&dir, Some(&record()), &BundleState::Imported), Ok(BundleState::Imported));
        assert_eq!(list_names(&bundles).unwrap(), vec!["latest"]);

        populate(b"abcd");
        assert_eq!(prepare_resume(&dir, Some(&record()), &BundleState::None), Ok(BundleState::None));
        assert!(!bundles.exists());

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
        assert_eq!(refused, RefusalCode::CloneIncomplete);
        assert_eq!(details["cause_code"], "error");
        assert_eq!(details["retry"], "unknown");

        std::fs::write(record_path(&forklift), b"torn").unwrap();
        assert_eq!(code(gate(&dir)).0, RefusalCode::CloneIncomplete);

        std::fs::remove_file(record_path(&forklift)).unwrap();
        std::fs::write(forklift.join("franchise.tmp~1-0"), b"x").unwrap();
        assert_eq!(code(gate(&dir)).0, RefusalCode::CloneIncomplete);

        finish(&forklift).unwrap();
        assert!(gate(&dir).is_ok(), "finishing clears the temps with the record");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn the_resume_command_quotes_what_a_shell_would_split() {
        let request = Request { pallet: Some("it's".to_string()), only: vec!["a b".to_string(), "docs".to_string()], cache: false };
        assert_eq!(
            resume_command("http://h:1", "/tmp/my dir", &request),
            "forklift clone http://h:1 '/tmp/my dir' --pallet 'it'\\''s' --only 'a b' --only docs"
        );

        let request = Request { cache: true, ..Request::default() };
        assert_eq!(resume_command("http://h:1", "d", &request), "forklift clone http://h:1 d --cache");
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
        assert!(second.unwrap_err().contains("claimed by another clone"));
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
    /// The claim's rename never replaces a `.forklift` that appeared after the admission, not even
    /// an empty one, which a plain rename onto a folder would.
    #[test]
    fn the_claim_rename_never_replaces_an_empty_folder() {
        let root = temp_dir("rename-no-replace");
        let (staged, placed) = (root.join("staged"), root.join("placed"));
        std::fs::create_dir_all(staged.join("inside")).unwrap();
        std::fs::create_dir_all(&placed).unwrap();

        let refused = file_utils::rename_no_replace(&staged, &placed).unwrap_err();
        assert_eq!(refused.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(staged.join("inside").is_dir() && list_names(&placed).unwrap().is_empty());

        std::fs::remove_dir(&placed).unwrap();
        file_utils::rename_no_replace(&staged, &placed).unwrap();
        assert!(placed.join("inside").is_dir() && !staged.exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    /// No level below the target is followed: a linked `.forklift` is refused by admission and by
    /// the resume, a linked object store is discarded as itself, and a taint under a linked fan-out
    /// folder removes the link, never what it points at.
    #[cfg(unix)]
    #[test]
    fn a_resume_never_deletes_through_a_link() {
        let root = temp_dir("links");
        let outside = root.join("outside");
        std::fs::create_dir_all(outside.join("ab")).unwrap();
        std::fs::write(outside.join("ab").join("cdef"), b"keep").unwrap();

        // A link to what looks like an unfinished clone's `.forklift`.
        let outside_forklift = root.join("outside-forklift");
        std::fs::create_dir_all(outside_forklift.join("refs")).unwrap();
        write(&outside_forklift, &record()).unwrap();

        let linked = root.join("linked");
        std::fs::create_dir_all(&linked).unwrap();
        std::os::unix::fs::symlink(&outside_forklift, linked.join(FOLDER_NAME_FORKLIFT_ROOT)).unwrap();
        assert!(admit(&linked).is_err());
        assert!(prepare_resume(&linked, Some(&record()), &BundleState::None).is_err());
        assert!(outside_forklift.join("refs").is_dir());

        let store_linked = root.join("store-linked");
        let forklift = store_linked.join(FOLDER_NAME_FORKLIFT_ROOT);
        std::fs::create_dir_all(&forklift).unwrap();
        write(&forklift, &record()).unwrap();
        std::os::unix::fs::symlink(&outside, forklift.join(FOLDER_NAME_OBJECTS_ROOT)).unwrap();
        std::fs::create_dir_all(forklift.join("bundles")).unwrap();
        std::fs::write(forklift.join("bundles/latest"), b"x").unwrap();
        // The store is discarded, so the import it held is not kept.
        assert_eq!(prepare_resume(&store_linked, Some(&record()), &BundleState::Imported), Ok(BundleState::None));
        assert!(std::fs::symlink_metadata(forklift.join(FOLDER_NAME_OBJECTS_ROOT)).is_err());

        let fan_out_linked = root.join("fan-out-linked");
        let objects = fan_out_linked.join(FOLDER_NAME_FORKLIFT_ROOT).join(FOLDER_NAME_OBJECTS_ROOT);
        std::fs::create_dir_all(&objects).unwrap();
        std::os::unix::fs::symlink(outside.join("ab"), objects.join("ab")).unwrap();
        let removed_from = remove_in_store(
            &fan_out_linked.join(FOLDER_NAME_FORKLIFT_ROOT), Path::new("objects/ab/cdef")
        ).unwrap();
        assert_eq!(removed_from, objects);
        assert!(std::fs::symlink_metadata(objects.join("ab")).is_err());

        assert_eq!(std::fs::read(outside.join("ab").join("cdef")).unwrap(), b"keep");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A `.forklift` whose record cannot be read for a reason other than its content is refused
    /// with the clone's code, saying it cannot be checked: never waved through, never told to
    /// prepare a warehouse.
    #[test]
    fn the_gate_refuses_what_it_cannot_check() {
        let root = temp_dir("gate-unreadable");
        std::fs::create_dir_all(root.join(FOLDER_NAME_FORKLIFT_ROOT).join(FILE_NAME_FRANCHISE)).unwrap();

        match gate(&root) {
            Err(CoreError::Refusal { code: RefusalCode::CloneIncomplete, message, .. }) => {
                assert!(message.contains("cannot be checked"), "{}", message);
            }
            other => panic!("{:?}", other),
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
