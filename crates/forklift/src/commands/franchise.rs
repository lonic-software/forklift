use std::path::{Path, PathBuf};
use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::error::{self, CoreError, RefusalCode};
use forklift_core::globals::FOLDER_NAME_FORKLIFT_ROOT;
use forklift_core::model::remote::WarehouseInfo;
use forklift_core::util::franchise_utils::{self, Admission, Attempt, Failure, FranchiseRecord, Phase, Request};
use forklift_core::util::path_utils::WarehousePath;
use forklift_core::util::progress_utils::Progress;
use forklift_core::util::remote_utils::{BatchLaneClosure, RemoteClient, TorSettings};
use forklift_core::util::scope_utils::MaterializationScope;
use forklift_core::util::{
    bundle_utils, config_utils, file_utils, lock_utils, object_utils, operation_utils, pack_utils,
    pallet_utils, remote_utils, scope_utils, shift_utils, trust_utils, warehouse_utils,
};
use forklift_core::util::operation_utils::{Operation, OperationKind};
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
                            expect: Option<String>) -> Result<(), String> {
    // Normalize the sparse scope up front (before any warehouse is created), so a malformed
    // --only refuses cleanly. An empty list is a full (ordinary) franchise.
    let fetch_scope = resolve_fetch_scope(&only)?;
    let sparse = !fetch_scope.is_full();

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

    let request = Request { pallet, only: fetch_scope.prefixes().to_vec() };

    // The trust binding: the pin, else the genesis the remote announced. A resume whose record
    // holds none (the remote had no anchor) binds to the one it announces now.
    let genesis = pin.or_else(|| info.trust.as_ref().map(|anchor| anchor.genesis.clone()));

    let record = FranchiseRecord {
        url: url.to_string(),
        token: token.clone(),
        request,
        genesis,
        bundle_imported: false,
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
        bundle_imported_at_start: false,
        finishing: false,
    };

    let outcome = enter_and_franchise(
        &mut attempt, &absolute_target, expected, resolution, fetch_scope, sparse, &client, info,
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

    /// Whether the bundle was imported when this attempt started, so importing it counts as
    /// progress.
    bundle_imported_at_start: bool,

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
            || (self.record.bundle_imported && !self.bundle_imported_at_start);

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
    sparse: bool,
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

    // A finished import is kept only for a full franchise: a sparse one has no bundle.
    let keep_bundles = !sparse && previous.as_ref().is_some_and(|previous| previous.bundle_imported);

    let outcome = match start_attempt(attempt, absolute_target, previous.as_ref(), keep_bundles) {
        Ok(()) => franchise_into_prepared_target(attempt, resolution, fetch_scope, sparse, client, info).await,
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
/// Until the start is recorded, a failure records the earlier attempt's import and census, so a
/// resume that fails while clearing claims nothing it did not keep.
fn start_attempt(attempt: &mut FranchiseAttempt,
                 absolute_target: &Path,
                 previous: Option<&FranchiseRecord>,
                 keep_bundles: bool) -> Result<(), String> {
    if let Some(previous) = previous {
        attempt.record.bundle_imported = previous.bundle_imported;
        attempt.record.start_census = previous.start_census;
        attempt.bundle_imported_at_start = previous.bundle_imported;
    }

    let discarded = franchise_utils::prepare_resume(absolute_target, previous, keep_bundles).map_err(|e| {
        // Clearing stopped partway: an import counts as kept only if its store and bundle still are.
        let kept = keep_bundles && attempt.record.bundle_imported && franchise_utils::holds_import(&attempt.forklift);
        attempt.record.bundle_imported = kept;
        attempt.bundle_imported_at_start = kept;
        e
    })?;

    // A discarded store took the imported objects with it: the bundle is imported again.
    let keep_bundles = keep_bundles && !discarded;

    attempt.record.bundle_imported = keep_bundles;
    attempt.bundle_imported_at_start = keep_bundles;
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
    sparse: bool,
    client: &RemoteClient,
    info: WarehouseInfo,
) -> Result<(), String> {
    let url = attempt.record.url.clone();

    warehouse_utils::prepare_warehouse()?;

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
    // fetched loose by the history walk below. A sparse franchise skips it entirely — the bundle
    // is the whole store, which would defeat the point of fetching only a subtree. A resume whose
    // earlier attempt finished the import keeps it and skips this too; a half download is never
    // kept, so it is downloaded again.
    attempt.phase = Phase::Bundle;

    if !sparse && !attempt.record.bundle_imported {
        let bundle_path = bundle_utils::get_latest_bundle_path();

        if let Some(parent) = bundle_path.parent() {
            file_utils::create_folder_if_not_exists(parent)?;
        }

        if client.fetch_bundle_to(&bundle_path).await? {
            match bundle_utils::import_bundle(&bundle_path) {
                Ok(imported) => {
                    report.bundle_objects = Some(imported.stored_objects);
                    report.bundle_signatures = Some(imported.stored_signatures);
                    report.densify_suggested = imported.installed_packs;

                    attempt.record.bundle_imported = true;
                    franchise_utils::write(&attempt.forklift, &attempt.record)?;
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

    attempt.phase = Phase::Fetch;

    // `None` because the pin was already checked against this same handshake by
    // `check_handshake_trust` in `handle_command`, before the download — not because there is none.
    let trust = remote_utils::adopt_remote_trust(client, &info, None).await?;
    report.adopted_genesis = trust.adopted_genesis;

    // Adopt the meta pallets too (the manifest, …), so a clone carries the post-metadata,
    // not just the working history. A fresh clone has none locally, so nothing diverges.
    // A clone fails loudly on a meta pallet it cannot verify, rather than arriving without it.
    let meta = remote_utils::adopt_meta_pallets(client, &info).await;

    if let Some((pallet, why)) = meta.failed.into_iter().next() {
        return Err(error::prefix_message(&format!("Could not adopt the remote {}", pallet), why));
    }

    report.meta_adopted = meta.adopted;

    let PalletResolution { pallet_name, remote_head } = resolution;

    let Some(remote_head) = remote_head else {
        // Legitimately unborn: `resolve_pallet` already ruled out a typo before any of this
        // warehouse's mutation began, so reaching this branch means either the remote's own
        // default pallet has nothing stacked yet, or an explicitly named pallet does not exist
        // on an otherwise-fresh remote — both fine starting points, not errors.
        pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet_name, None))?;

        // There is no tree to validate --only against (the pallet has nothing stacked on the
        // remote yet); record the requested sparse scope and origin as-is, so a later
        // lower/expand into this pallet, once it is born, stays within it.
        if sparse {
            scope_utils::set_fetch_scope(&fetch_scope)?;
            scope_utils::set_bay_scope(&fetch_scope)?;
            config_utils::set_value(config_utils::KEY_REMOTE_ORIGIN, &url, ConfigScope::Warehouse)?;
        }

        attempt.finishing = true;
        franchise_utils::finish(&attempt.forklift)?;

        report.pallet = pallet_name;
        report.unborn = true;
        emit_report(&mut report, client);

        return Ok(());
    };

    let stats = remote_utils::fetch_history_at(client, &remote_head, &pallet_name, &fetch_scope, &Progress::NONE).await?;

    let tree_hash = object_utils::load_parcel(&remote_head)?.tree_hash;

    // Validate the --only paths against the fetched head BEFORE any of this warehouse's state
    // (the fetch scope, the origin record, the pallet head) is written. The fetched objects are
    // kept, so a resume with the path corrected fetches only what the new scope adds. The spine is
    // fetched, so the walk to each in-scope subtree resolves without touching anything absent.
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

        scope_utils::set_fetch_scope(&fetch_scope)?;
        scope_utils::set_bay_scope(&fetch_scope)?;
        config_utils::set_value(config_utils::KEY_REMOTE_ORIGIN, &url, ConfigScope::Warehouse)?;
    }

    // Raised before the pallet's head is written, and finished once the tree is materialized. The
    // franchise record outlives it: a franchise interrupted anywhere is resumed by `franchise`,
    // which discards the marker with the rest of the attempt.
    let position = pallet_utils::BayPosition::new(&pallet_name, Some(remote_head.clone()));
    operation_utils::raise(&Operation::new(OperationKind::Franchise, position.clone(), None, position))?;

    // A franchise fills a fresh warehouse (a resume discarded every ref), so the pallet is unborn
    // here.
    pallet_utils::update_pallet_head(&pallet_name, None, &remote_head, &pallet_utils::RefPreconditions::NONE)?;
    pallet_utils::write_position(&pallet_utils::BayPosition::new(&pallet_name, Some(remote_head.clone())))?;

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

    /// Why the remote's batch download stopped being used, when it did: everything the command
    /// fetched after that came one object at a time. `degraded` is a remote too slow to start a
    /// batch answer; `endpoint_absent` is one that serves no batch downloads. Absent when the
    /// lane stayed open.
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

        if let Some(closure) = self.batch_lane {
            println!("{}", output::batch_lane_line(closure));
        }
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
            request: Request { pallet: None, only: vec!["src/ap".to_string()] },
            genesis: Some("abc".to_string()),
            bundle_imported: false,
            attempt: Attempt::Failed,
            start_census: 3,
            last_failure: Some(Failure {
                request: Request { pallet: None, only: vec!["src/ap".to_string()] },
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
            bundle_imported_at_start: false,
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
    /// import and census, never a fresh start it did not make.
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

        let previous = FranchiseRecord { bundle_imported: true, start_census: 7, ..record() };
        let recorded = root.join("record");
        std::fs::create_dir_all(recorded.join("objects")).unwrap();
        std::fs::create_dir_all(recorded.join("bundles")).unwrap();

        let mut resumed = attempt(recorded.clone());
        assert!(start_attempt(&mut resumed, &target, Some(&previous), true).is_err());
        assert!(resumed.record.bundle_imported);
        assert!(resumed.bundle_imported_at_start);
        assert_eq!(resumed.record.start_census, 7);

        // A resume that was not keeping the bundle (a sparse one) never claims it.
        let mut sparse = attempt(recorded.clone());
        assert!(start_attempt(&mut sparse, &target, Some(&previous), false).is_err());
        assert!(!sparse.record.bundle_imported && !sparse.bundle_imported_at_start);

        // Nor does one whose clearing already took the bundle away.
        std::fs::remove_dir(recorded.join("bundles")).unwrap();
        let mut cleared = attempt(recorded);
        assert!(start_attempt(&mut cleared, &target, Some(&previous), true).is_err());
        assert!(!cleared.record.bundle_imported && !cleared.bundle_imported_at_start);
    }
}
