use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::error::{self, CoreError, RefusalCode};
use forklift_core::util::lock_utils::{MoveLock, RefLock, StoreAccessLock};
use forklift_core::util::origin_move::{self, Phase};
use forklift_core::util::remote_utils::{self, RemoteClient};
use forklift_core::util::{config_utils, origin_check, origin_utils, scope_utils};
use crate::events;
use crate::output::{self, CommandOutput};

/// Handle `remote move` (§7.6.1): point a partial warehouse at a new origin, once the new server is
/// checked to hold everything this warehouse lacks and leans on its origin for.
///
/// The move holds the move lock for its whole run and records its intent first, so nothing fetches
/// or writes what it checks until it ends. It judges the new server's trust as `lower` would,
/// without adopting anything, fetches the parcels and signatures of its history it lacks, and
/// checks that history reaches every absent object ([`origin_check`]). Only then does it rewrite
/// the origin and `remote.url` (and `remote.token`, given one), and clear the intent. A move that
/// fails before rewriting clears its intent; one that stops while rewriting leaves it, and the
/// next move finishes it.
///
/// # Arguments
/// * `url`          - The new server.
/// * `token`        - The new server's bearer token, set as `remote.token` with the URL; without
///                    it the current `remote.token` is used.
/// * `store_access` - The command's shared store-access hold, so nothing is evicted while the
///                    absent content is counted.
///
/// # Returns
/// * `Ok(())`      - The origin is the new server.
/// * `Err(String)` - The warehouse is full, another move is running, the new server cannot be
///   reached, is not trusted or lacks content (`origin_move_incomplete`), or a write failed.
pub async fn move_origin(url: String,
                         token: Option<String>,
                         store_access: &StoreAccessLock) -> Result<(), String> {
    let move_lock = acquire_move_lock()?;

    // One spelling for the intent, the origin record and the config alike, as the client dials it.
    let target = url.trim_end_matches('/').to_string();
    if target.is_empty() {
        return Err("The new origin must be a remote URL.".to_string());
    }

    // A damaged or missing record is what a move repairs, so it reads as no previous origin.
    let previous = origin_utils::read_origin().ok().flatten();
    let (move_token, phase) = {
        let lock = RefLock::acquire()?;
        origin_move::begin(&move_lock, &lock, &target)?
    };

    let moved = check_and_rewrite(&target, token, &move_token, store_access).await;

    let checked = match moved {
        Ok(checked) => checked,
        Err(why) => {
            let lock = RefLock::acquire()?;

            // Appended, not formatted: a refusal keeps its code, next step and details.
            return Err(match origin_move::abandon(&lock, &move_token) {
                Ok(true) => why,
                Ok(false) if phase == Phase::Rewriting => error::append_to_message(
                    why,
                    "The move had started rewriting the origin before this run, so it still stands; \
                    run \"forklift remote move <url>\" again, naming a server that holds this \
                    warehouse's content."
                ),
                Ok(false) => error::append_to_message(
                    why,
                    &format!(
                        "This run had started rewriting the origin, so the move still stands; run \
                        \"forklift remote move {}\" again once that is fixed.",
                        target
                    )
                ),
                Err(stuck) => error::append_to_message(
                    why,
                    &format!("The move's record could not be cleared either: {}", stuck)
                ),
            });
        }
    };

    output::emit(&RemoteMoveReport {
        origin: target,
        previous_origin: previous,
        finished_stopped_move: phase == Phase::Rewriting,
        walked_parcels: checked.walked_parcels,
        fetched_objects: checked.fetched_objects,
        fetched_signatures: checked.fetched_signatures,
        absent_objects: checked.absent_objects,
        read_objects: checked.read_objects,
    });

    Ok(())
}

/// Steps 2 to 5 of a move: judge the new server, check it, and rewrite the origin to it.
async fn check_and_rewrite(target: &str,
                           token: Option<String>,
                           move_token: &origin_move::MoveToken,
                           store_access: &StoreAccessLock) -> Result<origin_check::Checked, String> {
    // Its own client: the origin-checked one refuses while the move stands.
    let dial_token = match &token {
        Some(token) => Some(token.clone()),
        None => config_utils::get_effective_value(config_utils::KEY_REMOTE_TOKEN)?.map(|(value, _)| value),
    };
    let client = RemoteClient::new(target, dial_token)?;
    let info = client.fetch_info().await?;

    remote_utils::check_remote_trust(&client, &info, None)?;

    let checked = origin_check::check_new_origin(&client, &info, store_access, &events::progress()).await?;

    let lock = RefLock::acquire()?;
    origin_move::set_rewriting(&lock, move_token, target)?;
    origin_utils::write_locked(&lock, move_token, target)?;
    config_utils::set_value(config_utils::KEY_REMOTE_URL, target, ConfigScope::Warehouse)?;
    if let Some(token) = &token {
        config_utils::set_value(config_utils::KEY_REMOTE_TOKEN, token, ConfigScope::Warehouse)?;
    }
    origin_move::finish(&lock, move_token)?;

    Ok(checked)
}

/// Handle bare `remote`: report the configured remote, whether this is a partial warehouse and,
/// for one, its origin, and what would stop a client talking to the remote from here (a move of
/// the origin standing, a remote that is not the origin, or a partial warehouse's missing origin
/// record). Reads only; takes no lock.
///
/// # Returns
/// * `Ok(())`      - The report was emitted.
/// * `Err(String)` - The configuration, the fetch state, the origin record or a move's intent
///   could not be read.
pub fn show() -> Result<(), String> {
    let remote = config_utils::get_effective_value(config_utils::KEY_REMOTE_URL)?.map(|(value, _)| value);
    let partial = scope_utils::content_may_be_absent()?;
    let origin = if partial { origin_utils::read_origin()? } else { None };

    // A move of the origin standing stops every client first, remote or none. Past it, what
    // `checked_remote` refuses is what every client dialing the remote would; with no remote
    // there is nothing to dial, and only a partial warehouse's missing origin is still damage.
    let blocked = if let Err(pending) = origin_move::ensure_none_pending() {
        Some(block_of(pending)?)
    } else if remote.is_none() {
        if partial && origin.is_none() { Some(block_of(origin_utils::missing_origin_refusal())?) } else { None }
    } else {
        match origin_utils::checked_remote() {
            Ok(_) => None,
            Err(refused) => Some(block_of(refused)?),
        }
    };

    output::emit(&RemoteReport { remote, partial, origin, blocked });

    Ok(())
}

/// A refusal as what bare `remote` reports; any other error is the show's own failure.
fn block_of(error: CoreError) -> Result<RemoteBlock, String> {
    match error {
        CoreError::Refusal { code, message, next_step, .. } => Ok(RemoteBlock { code: code.as_str(), message, next_step }),
        other => Err(other.into()),
    }
}

/// Handle `remote abort`: clear an intent a move left before rewriting, and refuse over one it
/// left while rewriting, which only a completed move clears.
///
/// # Returns
/// * `Ok(())`      - The intent was cleared, or there was none.
/// * `Err(String)` - The warehouse is full, a move is running or stopped while rewriting
///   (`origin_move_pending`), or the intent could not be cleared.
pub fn abort_move() -> Result<(), String> {
    let move_lock = acquire_move_lock()?;
    let lock = RefLock::acquire()?;
    let intent = origin_move::abort(&move_lock, &lock)?;

    if let Some(intent) = &intent {
        if intent.phase == Phase::Rewriting {
            return Err(origin_move::pending_refusal(intent).into());
        }
    }

    output::emit(&RemoteAbortReport {
        abandoned: intent.and_then(|intent| intent.target),
    });

    Ok(())
}

/// The move lock, for a move or an abort of a partial warehouse's origin. A full warehouse has no
/// origin, and a move already running holds the lock (`origin_move_pending`).
fn acquire_move_lock() -> Result<MoveLock, String> {
    if !scope_utils::content_may_be_absent()? {
        return Err(
            "This warehouse holds its whole history, so it has no origin to move: it may talk to any \
            remote. Point it at another one with \"forklift config set remote.url <url>\".".to_string()
        );
    }

    MoveLock::try_acquire()?.ok_or_else(running_refusal)
}

/// The `origin_move_pending` refusal for a move or an abort while another move holds the lock.
fn running_refusal() -> String {
    let next_step = "Wait for the other move to finish, then run this again.";

    CoreError::refusal(
        RefusalCode::OriginMovePending,
        format!("Another move of this warehouse's origin is running; nothing was changed. {}", next_step),
        next_step,
    ).into()
}

/// What bare `remote` reports.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemoteReport {
    /// The configured `remote.url`; none when no remote is configured.
    remote: Option<String>,

    /// Whether this is a partial warehouse (cloned with `--only` or `--cache`), which talks
    /// only to its origin.
    partial: bool,

    /// A partial warehouse's origin, the one server proved to hold what it lacks; none for a full
    /// warehouse, which may talk to any remote, and for a partial one whose record is missing.
    origin: Option<String>,

    /// The refusal a command that dials the remote would meet from here; none when nothing stops
    /// it. With no remote configured, only a move of the origin standing or a partial warehouse's
    /// missing origin record is reported.
    blocked: Option<RemoteBlock>,
}

/// A refusal that stops clients talking to the remote.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemoteBlock {
    /// The stable refusal code, such as `non_origin_remote` or `origin_move_pending`.
    code: &'static str,

    /// The refusal's explanation.
    message: String,

    /// The step that clears it.
    next_step: String,
}

impl CommandOutput for RemoteReport {
    fn render_human(&self) {
        match &self.remote {
            Some(remote) => println!("Remote: {}", remote),
            None => println!(
                "No remote is configured. Set one with \"forklift config set {} <url>\".",
                config_utils::KEY_REMOTE_URL
            ),
        }

        if !self.partial {
            println!("This warehouse holds its whole history, so it has no origin and may talk to any remote.");
        } else if let Some(origin) = &self.origin {
            println!("Origin: {} (this partial warehouse fetches what it lacks from there alone)", origin);
        }

        if let Some(blocked) = &self.blocked {
            println!("{}", blocked.message);
        }
    }
}

/// The result of a move of the origin.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemoteMoveReport {
    /// The origin now, as `remote.url` names it.
    origin: String,

    /// The origin before this run; none when the record was missing or could not be read, which a
    /// move repairs. A run finishing a stopped move may find it already rewritten to the new origin.
    previous_origin: Option<String>,

    /// Whether this run finished a move that had stopped while rewriting the origin.
    finished_stopped_move: bool,

    /// Parcels of the new server's history walked to fetch what was missing here.
    walked_parcels: usize,

    /// Parcels fetched from the new server, and stored; no content is fetched.
    fetched_objects: usize,

    /// Signatures fetched with them.
    fetched_signatures: usize,

    /// Objects this warehouse lacks and leans on its origin for, each found in the new server's
    /// history.
    absent_objects: usize,

    /// Trees and recipes read from the new server, and not stored, to find them.
    read_objects: usize,
}

impl CommandOutput for RemoteMoveReport {
    fn render_human(&self) {
        if self.finished_stopped_move {
            println!("Finished the move of this warehouse's origin that had stopped.");
        }

        match self.previous_origin.as_ref().filter(|previous| !origin_utils::same_remote(previous, &self.origin)) {
            Some(previous) => println!("The origin moved from \"{}\" to \"{}\".", previous, self.origin),
            None => println!("The origin is now \"{}\".", self.origin),
        }

        println!(
            "It holds the {} object(s) this warehouse lacks; {} parcel(s) and {} signature(s) of its \
            history were fetched.",
            self.absent_objects, self.fetched_objects, self.fetched_signatures
        );
    }
}

/// The result of `remote abort`.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemoteAbortReport {
    /// The server the abandoned move was going to; none when no move had stopped.
    abandoned: Option<String>,
}

impl CommandOutput for RemoteAbortReport {
    fn render_human(&self) {
        match &self.abandoned {
            Some(target) => println!(
                "Abandoned the move of this warehouse's origin to \"{}\"; the origin is where it was.",
                target
            ),
            None => println!("No move of this warehouse's origin had stopped; nothing was changed."),
        }
    }
}

#[cfg(feature = "docgen")]
pub(crate) fn __docgen_schemas() -> Vec<crate::docgen::OutputSchema> {
    vec![
        crate::docgen::OutputSchema::of::<RemoteReport>(),
        crate::docgen::OutputSchema::of::<RemoteMoveReport>(),
        crate::docgen::OutputSchema::of::<RemoteAbortReport>(),
    ]
}
