use std::path::{Path, PathBuf};
use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::error::{self, CoreError, RefusalCode};
use forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT;
use forklift_core::model::remote::WarehouseInfo;
use forklift_core::util::franchise_utils::{self, Admission, Attempt, BundleState, Failure, FranchiseRecord, Phase, Request};
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::{self as remote, BatchLaneClosure, BundleDownload, PartialBundle, RemoteClient, TorSettings};
use forklift_core::util::scope_utils::MaterializationScope;
use forklift_core::util::warehouse_utils::PrepareMode;
use forklift_core::util::{
    bundle_utils, config_utils, file_utils, held_heads, lock_utils, object_utils, operation_utils,
    pack_utils, pallet_utils, remote_utils, shift_utils, take_utils, trust_utils,
    warehouse_utils,
};
use forklift_core::util::operation_utils::{Operation, OperationKind};
use forklift_core::util::take_utils::{FetchAt, TakeTrust};
use crate::output::{self, CommandOutput};

/// Handle the franchise command (git's "clone"): open a local franchise of a remote
/// warehouse — prepare a fresh warehouse in the target directory, remember the remote,
/// adopt its trust anchor, download the history (the remote's bundle first, when it has
/// one, then whatever the bundle lacked as loose objects) and materialize the chosen
/// pallet's head in the working directory.
///
/// The remote is contacted before the target directory is touched, and the requested pallet is
/// checked against the handshake's own answer before anything is fetched, so a bad URL, a bad
/// token, an unreachable host, or a `--pallet` typo all leave the filesystem exactly as they found
/// it.
///
/// Once the directory is claimed, it holds a franchise record (`franchise_utils`) until the
/// franchise finishes. A failure after that point (a dropped connection, a full disk, a typo'd
/// `--only` path, a path the working tree cannot hold) or a kill keeps the directory with what
/// was fetched, and every other command refuses to run there (`franchise_incomplete`). Running
/// `franchise` again into the same directory resumes it: the verified object store is kept, the
/// rest is rebuilt, and `--pallet` and `--only` may change. The URL may not, an omitted `--token`
/// reuses the recorded one, and the trust genesis the first attempt bound to stays enforced as if
/// `--expect` were given again.
///
/// # Arguments
/// * `url`       - The remote to franchise.
/// * `directory` - The directory to create the warehouse in (must be empty or absent, or hold an
///                 unfinished franchise of `url`).
/// * `pallet`    - The pallet to check out (default: the remote's default pallet).
/// * `token`     - The bearer token, when the remote requires one (also remembered in
///                 the warehouse configuration).
/// * `only`      - When non-empty, franchise **sparsely**: fetch the full signed history but
///                 only the content under these subtree path(s), leaving the rest sealed by
///                 hash. The remote's whole-store bundle is skipped, and the remote is
///                 recorded as this sparse warehouse's origin (lift refuses to publish
///                 elsewhere).
/// * `cache`     - Franchise as a **cache**: mark the warehouse as one that may lack content
///                 its history reaches. Like a sparse franchise, it skips the bundle and
///                 records its origin. Never combined with `only` (the CLI refuses that).
/// * `expect`    - The genesis the remote's trust anchor must have (an out-of-band pin),
///                 checked against the handshake before anything is downloaded or created.
///
/// # Returns
/// * `Ok(())` - If the franchise is ready to work in.
/// * `Err(String)` - Before the claim, with the target as it was found: `--token` is given but
///   empty, the directory is not empty (or holds a franchise of another remote), the remote is
///   unreachable, its trust anchor is not the expected one, or the pallet does not exist. After
///   it: a `franchise_incomplete` refusal naming the cause, with the directory kept.
pub async fn handle_command(url: &str,
                            directory: &str,
                            pallet: Option<String>,
                            token: Option<String>,
                            only: Vec<String>,
                            cache: bool,
                            expect: Option<String>) -> Result<(), String> {
    // Normalize the sparse scope up front (before any warehouse is created), so a malformed
    // --only refuses cleanly. An empty list is a full (ordinary) franchise.
    let fetch_scope = resolve_fetch_scope(&only)?;
    let sparse = !fetch_scope.is_full();

    let partial = Partial { sparse, cache };

    // Likewise refused before anything is created: an empty token is never what was meant, and
    // left to the configuration write it would fail only after the warehouse existed. The advice
    // is this command's own — there is no entry yet to remove, only an option to leave out.
    if token.as_deref().is_some_and(config_utils::is_blank) {
        return Err(
            "The access token (--token) cannot be empty. Omit it for a remote that needs none."
                .to_string()
        );
    }

    let target = Path::new(directory);

    // The first of two admissions: before the handshake, because an unfinished franchise's record
    // supplies the token and the trust pin the handshake needs. It goes stale during the handshake,
    // so it is decided again under the warehouse lock.
    let admission = franchise_utils::admit(target)?;

    let resumed = match &admission {
        Admission::Fresh => None,
        Admission::Resume(record) => Some(record.clone()),
    };
    let recorded = resumed.clone().flatten();

    if let Some(record) = &recorded {
        check_resume(directory, record, url, expect.as_deref())?;
    }

    let token = token.or_else(|| recorded.as_ref().and_then(|record| record.token.clone()));
    let recorded_pin = recorded.as_ref().and_then(|record| record.genesis.clone());
    let implicit_pin = expect.is_none() && recorded_pin.is_some();
    let pin = expect.or(recorded_pin);

    // Talk to the remote *before* touching the filesystem at all. A bad URL, bad token or
    // unreachable host must leave the target exactly as found — not even an empty directory left
    // behind for a directory that did not exist before.
    //
    // Tor settings come from *global* configuration only — deliberately, never the effective
    // (warehouse-then-global) resolution `RemoteClient::new` would use. There is no "this
    // warehouse" yet to have an opinion here; only the ambient working directory, which may
    // happen to sit inside a *different*, unrelated warehouse. Falling through to `from_config`'s
    // cascade would silently make a brand-new clone's Tor routing depend on whatever unrelated
    // warehouse the caller happened to be standing in — a cwd-dependent, security-relevant
    // surprise.
    let client = RemoteClient::new_with_tor(url, token.clone(), TorSettings::from_global_config()?)?;
    let info = client.fetch_info().await?;

    // The handshake already carries the remote's trust anchor, so a stripped anchor and a pin are
    // checked here — before the bundle download and before the target is touched. A resume
    // enforces the genesis its first attempt bound to as if `--expect` were given again.
    trust_utils::check_handshake_trust(client.url(), &info, None, pin.as_deref()).map_err(|e| {
        if implicit_pin { repin_advice(e, directory) } else { e }
    })?;

    // Resolve the pallet from the handshake alone — no I/O — so a `--pallet` typo is refused
    // immediately, before anything is fetched or mutated. (The `--only` scope cannot be validated
    // this early: it names a subtree of a *tree*, and no tree is available until the pallet's
    // history is fetched — see the check after `fetch_history_at` below.)
    let resolution = resolve_pallet(&info, pallet.clone())?;

    let request = Request { pallet, only: fetch_scope.prefixes().to_vec(), cache };

    // The trust binding: the pin, else the genesis the remote announced. A resume whose record
    // holds none (the remote had no anchor) binds to the one it announces now.
    let genesis = pin.or_else(|| info.trust.as_ref().map(|anchor| anchor.genesis.clone()));

    let record = FranchiseRecord {
        url: url.to_string(),
        token: token.clone(),
        request,
        genesis,
        bundle: BundleState::None,
        attempt: Attempt::Running,
        start_census: 0,
        last_failure: None,
    };

    let original_cwd = std::env::current_dir()
        .map_err(|e| format!("Error while reading the current directory: {}", e))?;
    let absolute_target = original_cwd.join(target);

    // The handshake (and the pallet check) succeeded; only now may the filesystem change. A fresh
    // directory is claimed with its record already in place; a resumed one is already claimed.
    // What the second admission must find: this franchise's remote and trust binding.
    let expected = match resumed {
        None => {
            franchise_utils::claim(target, &record)?;
            Expected::Record(record.genesis.clone())
        }
        Some(Some(previous)) => Expected::Record(previous.genesis),
        Some(None) => Expected::Debris,
    };

    let mut attempt = FranchiseAttempt {
        directory: directory.to_string(),
        forklift: absolute_target.join(FOLDER_NAME_FORKLIFT_ROOT),
        record,
        phase: Phase::Prepare,
        bundle_at_start: BundleState::None,
        finishing: false,
    };

    let outcome = enter_and_franchise(
        &mut attempt, &absolute_target, expected, resolution, fetch_scope, partial, &client, info,
    ).await;

    // Drop any mmap'd pack handles this run loaded (reading a franchised bundle's packs while
    // adopting trust or meta pallets), and get back to where we started.
    pack_utils::invalidate_cache();
    let _ = std::env::set_current_dir(&original_cwd);

    outcome
}

/// Refuse a resume of the franchise `record` describes that cannot be one: another remote, or an
/// `--expect` pin that is not the genesis the franchise is bound to.
fn check_resume(directory: &str, record: &FranchiseRecord, url: &str, expect: Option<&str>) -> Result<(), String> {
    let command = franchise_utils::resume_command(&record.url, directory, &record.request);

    if !same_remote(&record.url, url) {
        return Err(CoreError::refusal_with_details(
            RefusalCode::FranchiseIncomplete,
            format!(
                "\"{}\" holds a franchise of {} that has not finished, not of {}.", directory, record.url, url
            ),
            format!("Run \"{}\" to resume it, or delete \"{}\" first.", command, directory),
            franchise_details(directory, record),
        ).into());
    }

    match (expect, record.genesis.as_deref()) {
        (Some(expect), Some(genesis)) if expect != genesis => Err(CoreError::refusal_with_details(
            RefusalCode::FranchiseIncomplete,
            format!(
                "\"{}\" holds a franchise bound to trust genesis {}, not the expected {}.",
                directory, genesis, expect
            ),
            format!(
                "Verify out of band which genesis the remote should have. To resume, run \"{}\" \
                without --expect; to start over, delete \"{}\".", command, directory
            ),
            franchise_details(directory, record),
        ).into()),
        _ => Ok(()),
    }
}

/// Whether two spellings of a remote's URL name the same remote, as the client dials it.
fn same_remote(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

/// The `franchise_incomplete` details of a resume refused before it started.
fn franchise_details(directory: &str, record: &FranchiseRecord) -> serde_json::Value {
    let failure = record.last_failure.as_ref().filter(|_| record.attempt == Attempt::Failed);

    franchise_utils::details(directory, Some(&record.url), failure)
}

/// Point a resume's `trust_changed` refusal at the only way out: the directory is bound to the
/// genesis its first attempt saw, so a remote that now serves another can only be franchised
/// afresh, once that is verified.
fn repin_advice(error: CoreError, directory: &str) -> CoreError {
    match error {
        CoreError::Refusal { code: RefusalCode::TrustChanged, message, details, .. } => CoreError::Refusal {
            code: RefusalCode::TrustChanged,
            message: error::sanitize(&format!(
                "{} \"{}\" is bound to the genesis its first attempt saw.", message, directory
            )),
            next_step: error::sanitize(&format!(
                "Verify out of band which genesis the remote should have; then delete \"{}\" and \
                franchise again.", directory
            )),
            details,
        },
        other => other,
    }
}

/// What the second admission, under the lock, must find.
enum Expected {
    /// A record of this franchise's remote, bound to this genesis.
    Record(Option<String>),

    /// The debris of a run killed before its record was durable.
    Debris,
}

/// The attempt in flight, after the claim: what its failure is recorded with.
struct FranchiseAttempt {
    /// The target as the user named it, for messages and the resume command.
    directory: String,

    /// The target's `.forklift` folder, absolute: the record is written there whatever the
    /// current directory is.
    forklift: PathBuf,

    /// The record this attempt keeps up to date.
    record: FranchiseRecord,

    /// The part of the franchise in progress.
    phase: Phase,

    /// How far the bundle had got when this attempt started, so taking it further counts as
    /// progress.
    bundle_at_start: BundleState,

    /// Whether the franchise has begun removing its record: once the record is gone the warehouse
    /// is complete, and a failure must never write it back.
    finishing: bool,
}

impl FranchiseAttempt {
    /// Record `error` as this attempt's failure and turn it into the `franchise_incomplete`
    /// refusal. The retry advice is bounded against the last recorded failure: an unclassified
    /// failure that repeats without storing anything is reported as `change`. Recording is best
    /// effort, and its failure is appended rather than hiding the cause.
    fn fail(&mut self, error: String) -> String {
        if self.finishing && franchise_utils::read(&self.forklift).is_ok_and(|record| record.is_none()) {
            return error;
        }

        let (cause_code, retry) = franchise_utils::classify_failure(&error);
        let mut failure = Failure { request: self.record.request.clone(), phase: self.phase, cause_code, retry };

        // A census that cannot be taken counts as progress: never told to give up on a guess.
        let progressed = franchise_utils::census(&self.forklift)
            .map_or(true, |census| census > self.record.start_census)
            || franchise_utils::bundle_progressed(&self.bundle_at_start, &self.record.bundle);

        failure.retry = franchise_utils::bound_retry(failure.retry, &failure, self.record.last_failure.as_ref(), progressed);

        self.record.attempt = Attempt::Failed;
        self.record.last_failure = Some(failure.clone());

        let written = franchise_utils::write(&self.forklift, &self.record);
        let refusal = String::from(franchise_utils::failure_refusal(&self.directory, &self.record, &error, &failure));

        match written {
            Ok(()) => refusal,
            Err(e) => error::append_to_message(refusal, &format!("(Recording the failure did not succeed: {})", e)),
        }
    }
}

/// Download the remote's bundle, resuming the download the record keeps, and import it. Returns
/// what the import stored, or `None` when the remote has no bundle this build can use. The
/// download is recorded as it is synced, so a failure leaves the next attempt only the rest to
/// ask for. A downloaded file that fails import is never kept to be imported again: a kept one
/// whose header or digest fails (also a header in a version this build does not read) was not all
/// served under one tag, or a crash damaged it, so it is downloaded once more in full. A bundle
/// downloaded in one piece in a version this build does not read is skipped; any other failure
/// discards the download and fails the attempt.
async fn fetch_and_import_bundle(attempt: &mut FranchiseAttempt,
                                 client: &RemoteClient) -> Result<Option<bundle_utils::ImportStats>, String> {
    let download = bundle_utils::get_bundle_download_path();

    if let Some(parent) = download.parent() {
        file_utils::create_folder_if_not_exists(parent)?;
    }

    let checkpoint_bytes = bundle_checkpoint_bytes();
    let mut kept = attempt.record.bundle.partial().cloned();

    loop {
        let forklift = &attempt.forklift;
        let record = &mut attempt.record;

        // The record in memory is updated too: a failing attempt writes it back from there.
        let mut checkpoint = |partial: Option<&PartialBundle>| {
            record.bundle = partial.map_or(BundleState::None, |partial| BundleState::Partial(partial.clone()));
            franchise_utils::write(forklift, record)
        };

        let resumed = match client.download_bundle(&download, kept.as_ref(), checkpoint_bytes, &mut checkpoint).await? {
            BundleDownload::Absent => return Ok(None),
            BundleDownload::Complete { resumed } => resumed,
        };

        let error = match bundle_utils::import_bundle(&download) {
            Ok(imported) => {
                // Published before it is recorded: a crash in between keeps the stored objects and
                // costs only the download, never the warehouse's own latest bundle.
                bundle_utils::publish_bundle_download()?;
                attempt.record.bundle = BundleState::Imported;
                franchise_utils::write(&attempt.forklift, &attempt.record)?;

                return Ok(Some(imported));
            }
            Err(error) => error,
        };

        discard_bundle_download(attempt).map_err(|e| {
            error::append_to_message(error.clone(), &format!("(Discarding the downloaded bundle did not succeed: {})", e))
        })?;

        if resumed && (bundle_utils::is_bundle_header_error(&error) || bundle_utils::is_bundle_digest_error(&error)) {
            kept = None;
            continue;
        }

        // A future envelope is only an optimization this client cannot use: the verified
        // incremental-object walk goes on without it. Known-format corruption stays fatal.
        return if bundle_utils::is_unsupported_bundle_error(&error) { Ok(None) } else { Err(error) };
    }
}

/// Record that nothing of the bundle is kept, then remove its download: a removal that fails
/// leaves only a file the next attempt clears.
fn discard_bundle_download(attempt: &mut FranchiseAttempt) -> Result<(), String> {
    attempt.record.bundle = BundleState::None;
    franchise_utils::write(&attempt.forklift, &attempt.record)?;
    bundle_utils::discard_bundle_download()
}

/// How often the bundle download is synced and recorded: `FORKLIFT_BUNDLE_CHECKPOINT_BYTES` if
/// set to a positive number (tests cut a download after several checkpoints; not a user-facing
/// knob), else [`remote::BUNDLE_CHECKPOINT_BYTES`].
fn bundle_checkpoint_bytes() -> u64 {
    std::env::var("FORKLIFT_BUNDLE_CHECKPOINT_BYTES").ok()
        .and_then(|value| value.parse().ok())
        .filter(|bytes| *bytes > 0)
        .unwrap_or(remote::BUNDLE_CHECKPOINT_BYTES)
}

/// Switch into the claimed target, take its lock, decide admission again and run the franchise.
/// A failure before the second admission passes is returned as it is: this run never started an
/// attempt, and the record (if any) is another run's. Every later failure is recorded.
#[allow(clippy::too_many_arguments)]
async fn enter_and_franchise(
    attempt: &mut FranchiseAttempt,
    absolute_target: &Path,
    expected: Expected,
    resolution: PalletResolution,
    fetch_scope: MaterializationScope,
    partial: Partial,
    client: &RemoteClient,
    info: WarehouseInfo,
) -> Result<(), String> {
    std::env::set_current_dir(absolute_target)
        .map_err(|e| format!("Error while switching to \"{}\": {}", attempt.directory, e))?;

    // Held until the franchise finishes or its failure is recorded: a resume run meanwhile is
    // refused (the lock is never waited for) rather than clearing an attempt in flight.
    let _lock = lock_utils::WarehouseLock::acquire()?;

    let previous = readmit(&attempt.directory, absolute_target, &attempt.record.url, &expected)?;

    if let Some(previous) = &previous {
        attempt.record.last_failure = previous.last_failure.clone();
    }

    // A finished import or a kept download is kept only for a full franchise: a sparse or cache
    // one has no bundle.
    let keep = match &previous {
        Some(previous) if !partial.any() => previous.bundle.clone(),
        _ => BundleState::None,
    };

    let outcome = match start_attempt(attempt, absolute_target, previous.as_ref(), &keep) {
        Ok(()) => franchise_into_prepared_target(attempt, resolution, fetch_scope, partial, client, info).await,
        Err(e) => Err(e),
    };

    outcome.map_err(|e| attempt.fail(e))
}

/// The second admission, under the lock: the target must still hold this franchise — its remote
/// and its trust binding — and nothing a user could be working in. Returns the record found.
fn readmit(directory: &str, absolute_target: &Path, url: &str, expected: &Expected) -> Result<Option<FranchiseRecord>, String> {
    let changed = || format!(
        "\"{}\" changed while this franchise was connecting to the remote. Run the command again.", directory
    );

    match (franchise_utils::admit(absolute_target)?, expected) {
        (Admission::Resume(Some(record)), Expected::Record(genesis)) if same_remote(&record.url, url) && &record.genesis == genesis => {
            Ok(Some(record))
        }
        (Admission::Resume(None), Expected::Debris) => Ok(None),
        _ => Err(changed()),
    }
}

/// Clear what an earlier attempt left (see `franchise_utils::prepare_resume`) and record this
/// attempt's start: its request, its trust binding, and the census its progress is measured from.
/// Until the start is recorded, a failure records the earlier attempt's bundle and census, so a
/// resume that fails while clearing claims nothing it did not keep.
fn start_attempt(attempt: &mut FranchiseAttempt,
                 absolute_target: &Path,
                 previous: Option<&FranchiseRecord>,
                 keep: &BundleState) -> Result<(), String> {
    if let Some(previous) = previous {
        attempt.record.bundle = keep.clone();
        attempt.record.start_census = previous.start_census;
        attempt.bundle_at_start = keep.clone();
    }

    let kept = franchise_utils::prepare_resume(absolute_target, previous, keep).map_err(|e| {
        // Clearing stopped partway: the bundle counts as kept only if what it needs still is.
        let held = franchise_utils::held_bundle(&attempt.forklift, keep);
        attempt.record.bundle = held.clone();
        attempt.bundle_at_start = held;
        e
    })?;

    attempt.record.bundle = kept.clone();
    attempt.bundle_at_start = kept;
    attempt.record.start_census = franchise_utils::census(&attempt.forklift)?;
    attempt.record.attempt = Attempt::Running;

    franchise_utils::write(&attempt.forklift, &attempt.record)
}

/// Everything that fills the warehouse: run only after the remote handshake and the pallet check
/// have already succeeded (see `handle_command`), with the current directory switched into the
/// target, its lock held and the attempt's start recorded. `attempt.phase` follows the work, so a
/// failure is recorded with the part it happened in.
async fn franchise_into_prepared_target(
    attempt: &mut FranchiseAttempt,
    resolution: PalletResolution,
    fetch_scope: MaterializationScope,
    partial: Partial,
    client: &RemoteClient,
    info: WarehouseInfo,
) -> Result<(), String> {
    let sparse = partial.sparse;
    let url = attempt.record.url.clone();

    // The mode is on disk from here on, a sparse warehouse's scopes with it, so every ref write
    // below is gated by what this warehouse will hold. A resume clears the marker before this, so
    // a recorded mode other than the one asked for is never expected; it is refused all the same.
    let mode = partial.prepare_mode(&fetch_scope);
    warehouse_utils::prepare_warehouse(mode)?;

    if warehouse_utils::warehouse_mode()? != mode.mode() {
        return Err(format!(
            "\"{}\" was already prepared as a different kind of warehouse than this franchise asks for.",
            attempt.directory
        ));
    }

    config_utils::set_value(config_utils::KEY_REMOTE_URL, &url, ConfigScope::Warehouse)?;

    if let Some(token) = &attempt.record.token {
        config_utils::set_value(config_utils::KEY_REMOTE_TOKEN, token, ConfigScope::Warehouse)?;
    }

    let mut report = FranchiseReport {
        remote: url.clone(),
        directory: attempt.directory.clone(),
        bundle_objects: None,
        bundle_signatures: None,
        adopted_genesis: None,
        meta_adopted: Vec::new(),
        pallet: String::new(),
        head: None,
        unborn: false,
        scope: fetch_scope.prefixes().to_vec(),
        fetched_objects: 0,
        densify_suggested: false,
        batch_lane: None,
    };

    // The bundle is a fast start, not a requirement: whatever it lacks (or if there is none) is
    // fetched loose by the history walk below. A sparse or cache franchise skips it entirely —
    // the bundle is the whole store, which would defeat the point of fetching less. A resume whose
    // earlier attempt finished the import keeps it and skips this too, and one that kept part of
    // the download asks only for the rest.
    attempt.phase = Phase::Bundle;

    if !partial.any() && attempt.record.bundle != BundleState::Imported {
        if let Some(imported) = fetch_and_import_bundle(attempt, client).await? {
            report.bundle_objects = Some(imported.stored_objects);
            report.bundle_signatures = Some(imported.stored_signatures);
            report.densify_suggested = imported.installed_packs;
        }
    }

    attempt.phase = Phase::Fetch;

    // `None` because the pin was already checked against this same handshake by
    // `check_handshake_trust` in `handle_command`, before the download — not because there is none.
    let mut trust = remote_utils::prepare_remote_trust(client, &info, None).await?;
    let take_trust = trust.take_trust().unwrap_or(TakeTrust::Untrusted);

    let PalletResolution { pallet_name, remote_head } = resolution;

    // The pallet's history is fetched and verified against the anchor the remote serves before
    // that anchor, its office or a meta pallet is installed, so a refusal leaves no trust behind.
    // At the scope this franchise asks for, which is not on disk yet.
    let fetched = match &remote_head {
        Some(remote_head) => {
            let stats = remote_utils::fetch_history_at(client, remote_head, &pallet_name, &fetch_scope, &Progress::NONE).await?;
            take_utils::verify_take(Some(client), &take_trust, remote_head, &pallet_name, FetchAt::Scope(&fetch_scope)).await?;
            Some(stats)
        }
        // Unborn on the remote: no history to judge.
        None => None,
    };

    // A new warehouse holds no pallet head yet, so this judges nothing; a first-contact install
    // still refuses without it.
    trust.judge_local_heads(client, &remote_head.iter().map(String::as_str).collect::<Vec<_>>()).await?;

    let trust = trust.commit()?;
    report.adopted_genesis = trust.adopted_genesis;

    // Adopt the meta pallets too (the manifest, …), so a clone carries the post-metadata,
    // not just the working history. A fresh clone has none locally, so nothing diverges.
    // A clone fails loudly on a meta pallet it cannot verify, rather than arriving without it.
    let meta = remote_utils::adopt_meta_pallets(client, &info).await;

    if let Some((pallet, why)) = meta.failed.into_iter().next() {
        return Err(error::prefix_message(&format!("Could not adopt the remote {}", pallet), why));
    }

    report.meta_adopted = meta.adopted;

    let (Some(remote_head), Some(stats)) = (remote_head, fetched) else {
        // Legitimately unborn: `resolve_pallet` already ruled out a typo before any of this
        // warehouse's mutation began, so reaching this branch means either the remote's own
        // default pallet has nothing stacked yet, or an explicitly named pallet does not exist
        // on an otherwise-fresh remote — both fine starting points, not errors.
        pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet_name, None))?;

        // There is no tree to validate --only against (the pallet has nothing stacked on the
        // remote yet); the requested sparse scope stands as prepared, so a later lower/expand
        // into this pallet, once it is born, stays within it.
        record_partial(partial, &url)?;

        attempt.finishing = true;
        franchise_utils::finish(&attempt.forklift)?;

        report.pallet = pallet_name;
        report.unborn = true;
        emit_report(&mut report, client);

        return Ok(());
    };

    let tree_hash = object_utils::load_parcel(&remote_head)?.tree_hash;

    // Validate the --only paths against the fetched head before the origin record and the pallet
    // head are written. The scopes are already recorded, but a resume clears them with the marker
    // and prepares again, and the fetched objects are kept, so a resume with the path corrected
    // fetches only what the new scope adds. The spine is fetched, so the walk to each in-scope
    // subtree resolves without touching anything absent.
    if sparse {
        for prefix in fetch_scope.prefixes() {
            if !is_directory_in_tree(&tree_hash, prefix)? {
                return Err(CoreError::refusal(
                    RefusalCode::OnlyPrefixNotFound,
                    format!(
                        "\"{}\" is not a directory in pallet \"{}\" on the remote, so it cannot be a \
                        sparse franchise scope.",
                        prefix, pallet_name
                    ),
                    "Name a directory of the pallet with --only.",
                ).into());
            }
        }
    }

    record_partial(partial, &url)?;

    // Raised before the pallet's head is written, and finished once the tree is materialized. The
    // franchise record outlives it: a franchise interrupted anywhere is resumed by `franchise`,
    // which discards the marker with the rest of the attempt.
    let position = pallet_utils::BayPosition::new(&pallet_name, Some(remote_head.clone()));
    operation_utils::raise(&Operation::new(OperationKind::Franchise, position.clone(), None, position))?;

    // A franchise fills a fresh warehouse (a resume discarded every ref), so the pallet is unborn
    // here. The write stands only on the trust the history was verified against.
    pallet_utils::update_pallet_head(&pallet_name, None, &remote_head, &take_utils::installed(&take_trust)?)?;
    pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet_name, Some(remote_head.clone())))?;

    // The head this warehouse was filled from is one the remote held, so a scope-prune may later
    // ask whether it still does. Best effort: an unrecorded head only keeps history a prune
    // could have freed.
    if let Err(why) = held_heads::record_held_head(&pallet_name, &remote_head) {
        output::warn_held_head_unrecorded(&why);
    }

    // A failure here keeps everything fetched, like any other: a resume redoes the whole
    // materialize, never a part of it. Its dominant causes — a path this filesystem cannot
    // represent, a case collision the source tolerated — are not fixed by resuming unchanged, so
    // the error points at `--only`, which a resume may change without refetching.
    attempt.phase = Phase::Materialize;

    shift_utils::materialize_tree(None, &tree_hash, "Franchising", file_utils::take_stat_anchor()?, &Progress::NONE).map_err(|e| error::append_to_message(
        e,
        "(the signed history was already fetched and verified; only checking out the working \
        tree failed. If the same path keeps failing, resume with --only to route around it)",
    ))?;
    operation_utils::finish()?;
    attempt.finishing = true;
    franchise_utils::finish(&attempt.forklift)?;

    report.pallet = pallet_name;
    report.head = Some(remote_head.clone());
    report.fetched_objects = stats.fetched_objects;
    emit_report(&mut report, client);

    Ok(())
}

/// Which pallet to check out, resolved from the handshake alone (see `resolve_pallet`).
struct PalletResolution {
    pallet_name: String,
    /// `None` when the pallet is unborn on the remote (legitimate — see `resolve_pallet`'s doc).
    remote_head: Option<String>,
}

/// What a franchise leaves out: content outside its `--only` scope, or (with `--cache`) content
/// it may fetch only when read.
#[derive(Clone, Copy)]
struct Partial {
    sparse: bool,
    cache: bool,
}

impl Partial {
    /// Whether the warehouse may lack content its history reaches.
    fn any(self) -> bool {
        self.sparse || self.cache
    }

    /// The mode the warehouse is prepared in. `--only` and `--cache` conflict, so at most one holds.
    fn prepare_mode(self, fetch_scope: &MaterializationScope) -> PrepareMode<'_> {
        if self.sparse {
            PrepareMode::Sparse(fetch_scope)
        } else if self.cache {
            PrepareMode::Cache
        } else {
            PrepareMode::Full
        }
    }
}

/// Record the origin of a partial franchise: the one remote that holds what it lacks. Nothing for
/// a full franchise.
fn record_partial(partial: Partial, url: &str) -> Result<(), String> {
    if partial.any() {
        config_utils::set_value(config_utils::KEY_REMOTE_ORIGIN, url, ConfigScope::Warehouse)?;
    }

    Ok(())
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

/// Emit a successful franchise's report, announcing a first-contact adoption first. Only on
/// success: a franchise that fails afterwards is resumed, and a resume adopts the anchor again.
fn emit_report(report: &mut FranchiseReport, client: &RemoteClient) {
    if let Some(genesis) = &report.adopted_genesis {
        output::warn_trust_adopted(&trust_utils::first_contact(client.url(), genesis));
    }

    report.batch_lane = client.batch_lane_closure();
    output::emit(report);
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

    /// The genesis of the remote's trust anchor, adopted on this first contact (absent when the
    /// remote has none).
    #[serde(skip_serializing_if = "Option::is_none")]
    adopted_genesis: Option<String>,

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

    /// Why the remote's batch download stopped being used, when it did. Absent when it stayed open.
    #[serde(skip_serializing_if = "Option::is_none")]
    batch_lane: Option<BatchLaneClosure>,
}

impl CommandOutput for FranchiseReport {
    fn render_human(&self) {
        if let (Some(objects), Some(signatures)) = (self.bundle_objects, self.bundle_signatures) {
            println!(
                "Imported the remote's bundle: {} object(s) and {} signature(s).",
                objects, signatures
            );
        }

        if let Some(genesis) = &self.adopted_genesis {
            println!(
                "Adopted the remote's trust anchor (genesis {}); every parcel is signed from now on.",
                forklift_core::error::sanitize(genesis)
            );
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

        output::print_batch_lane(self.batch_lane);
    }
}


/// The `--json` `data` schema(s) this command can emit (see `docs/generated/json-schemas.md`).
#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<FranchiseReport>(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use forklift_core::util::franchise_utils::Retry;

    fn record() -> FranchiseRecord {
        FranchiseRecord {
            url: "http://example.com:9418".to_string(),
            token: None,
            request: Request { pallet: None, only: vec!["src/ap".to_string()], cache: false },
            genesis: Some("abc".to_string()),
            bundle: BundleState::None,
            attempt: Attempt::Failed,
            start_census: 3,
            last_failure: Some(Failure {
                request: Request { pallet: None, only: vec!["src/ap".to_string()], cache: false },
                phase: Phase::Fetch,
                cause_code: "only_prefix_not_found".to_string(),
                retry: Retry::Change,
            }),
        }
    }

    fn refusal(result: Result<(), String>) -> (RefusalCode, String, serde_json::Value) {
        match CoreError::from(result.unwrap_err()) {
            CoreError::Refusal { code, message, details, .. } => (code, message, details.unwrap()),
            other => panic!("expected a refusal, got {:?}", other),
        }
    }

    #[test]
    fn a_resume_of_another_remote_or_another_pin_is_refused() {
        assert_eq!(check_resume("d", &record(), "http://example.com:9418/", None), Ok(()));
        assert_eq!(check_resume("d", &record(), "http://example.com:9418", Some("abc")), Ok(()));

        let (code, message, details) = refusal(check_resume("d", &record(), "http://other:1", None));
        assert_eq!(code, RefusalCode::FranchiseIncomplete);
        assert!(message.contains("not of http://other:1"), "{}", message);
        assert_eq!(details["remote"], "http://example.com:9418");
        assert_eq!(details["cause_code"], "only_prefix_not_found");

        let (code, message, _) = refusal(check_resume("d", &record(), "http://example.com:9418", Some("abd")));
        assert_eq!(code, RefusalCode::FranchiseIncomplete);
        assert!(message.contains("bound to trust genesis abc, not the expected abd"), "{}", message);

        // A franchise that recorded no genesis accepts any pin: the pin binds it from now on.
        let unbound = FranchiseRecord { genesis: None, ..record() };
        assert_eq!(check_resume("d", &unbound, "http://example.com:9418", Some("abd")), Ok(()));
    }

    #[test]
    fn a_resume_refused_for_a_changed_anchor_is_told_to_start_over() {
        let changed = CoreError::refusal(RefusalCode::TrustChanged, "The anchor changed.", "Verify it before re-running.");

        match repin_advice(changed, "clone") {
            CoreError::Refusal { code, message, next_step, .. } => {
                assert_eq!(code, RefusalCode::TrustChanged);
                assert!(message.contains("\"clone\" is bound to the genesis its first attempt saw"), "{}", message);
                assert!(next_step.contains("delete \"clone\" and franchise again"), "{}", next_step);
            }
            other => panic!("{:?}", other),
        }

        let other = CoreError::Other("connection refused".to_string());
        assert_eq!(repin_advice(other.clone(), "clone"), other);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("forklift-franchise-cmd-{}-{}", label, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn attempt(forklift: PathBuf) -> FranchiseAttempt {
        FranchiseAttempt {
            directory: "clone".to_string(),
            forklift,
            record: FranchiseRecord { attempt: Attempt::Running, last_failure: None, start_census: 0, ..record() },
            phase: Phase::Prepare,
            bundle_at_start: BundleState::None,
            finishing: false,
        }
    }

    /// Once the franchise has begun removing its record the warehouse is complete: a failure past
    /// that point is reported as it is, and never puts the record back.
    #[test]
    fn a_failure_while_finishing_never_writes_the_record_back() {
        let forklift = temp_dir("finishing").join(".forklift");
        std::fs::create_dir_all(&forklift).unwrap();

        let mut finishing = attempt(forklift.clone());
        finishing.finishing = true;
        assert_eq!(finishing.fail("Error while syncing: EIO".to_string()), "Error while syncing: EIO");
        assert!(matches!(franchise_utils::read(&forklift), Ok(None)));

        // Finishing failed before the record went: the failure is recorded like any other.
        franchise_utils::write(&forklift, &finishing.record).unwrap();
        let refusal = finishing.fail("Error while clearing the franchise record: EACCES".to_string());
        assert!(matches!(CoreError::from(refusal), CoreError::Refusal { code: RefusalCode::FranchiseIncomplete, .. }));
        assert_eq!(franchise_utils::read(&forklift).unwrap().unwrap().attempt, Attempt::Failed);
        std::fs::remove_file(franchise_utils::record_path(&forklift)).unwrap();

        let mut failing = attempt(forklift.clone());
        let refusal = failing.fail("Error while writing: No space left on device".to_string());
        assert!(matches!(CoreError::from(refusal), CoreError::Refusal { code: RefusalCode::FranchiseIncomplete, .. }));
        assert!(franchise_utils::read(&forklift).unwrap().is_some());
    }

    /// A resume that fails while clearing what the earlier attempt left records that attempt's
    /// import or download and census, never a fresh start it did not make.
    #[cfg(unix)]
    #[test]
    fn a_resume_that_fails_while_clearing_keeps_the_earlier_import_and_census() {
        let root = temp_dir("clearing");
        let target = root.join("clone");
        let elsewhere = root.join("elsewhere");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        // A `.forklift` that is a link: the resume refuses to clear through it.
        std::os::unix::fs::symlink(&elsewhere, target.join(".forklift")).unwrap();

        let previous = FranchiseRecord { bundle: BundleState::Imported, start_census: 7, ..record() };
        let recorded = root.join("record");
        std::fs::create_dir_all(recorded.join("objects")).unwrap();
        std::fs::create_dir_all(recorded.join("bundles")).unwrap();

        let mut resumed = attempt(recorded.clone());
        assert!(start_attempt(&mut resumed, &target, Some(&previous), &BundleState::Imported).is_err());
        assert_eq!(resumed.record.bundle, BundleState::Imported);
        assert_eq!(resumed.bundle_at_start, BundleState::Imported);
        assert_eq!(resumed.record.start_census, 7);

        // A resume that was not keeping the bundle (a sparse one) never claims it.
        let mut sparse = attempt(recorded.clone());
        assert!(start_attempt(&mut sparse, &target, Some(&previous), &BundleState::None).is_err());
        assert_eq!((sparse.record.bundle, sparse.bundle_at_start), (BundleState::None, BundleState::None));

        // A kept download counts while its file holds what was recorded, and not once it is shorter.
        let partial = BundleState::Partial(PartialBundle { etag: "\"t\"".to_string(), total: 10, length: 4 });
        let downloading = FranchiseRecord { bundle: partial.clone(), ..previous.clone() };
        std::fs::write(recorded.join("bundles/partial"), b"abcd").unwrap();
        let mut kept = attempt(recorded.clone());
        assert!(start_attempt(&mut kept, &target, Some(&downloading), &partial).is_err());
        assert_eq!((kept.record.bundle, kept.bundle_at_start), (partial.clone(), partial.clone()));

        std::fs::write(recorded.join("bundles/partial"), b"abc").unwrap();
        let mut short = attempt(recorded.clone());
        assert!(start_attempt(&mut short, &target, Some(&downloading), &partial).is_err());
        assert_eq!((short.record.bundle, short.bundle_at_start), (BundleState::None, BundleState::None));

        // Nor does one whose clearing already took the bundle away.
        std::fs::remove_dir_all(recorded.join("bundles")).unwrap();
        let mut cleared = attempt(recorded);
        assert!(start_attempt(&mut cleared, &target, Some(&previous), &BundleState::Imported).is_err());
        assert_eq!((cleared.record.bundle, cleared.bundle_at_start), (BundleState::None, BundleState::None));
    }
}
