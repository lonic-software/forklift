use serde::Serialize;
use forklift_core::enums::config_scope::ConfigScope;
use forklift_core::error::{CoreError, RefusalCode};
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
/// * `url`          - The new server; `None` only with `abort`.
/// * `token`        - The new server's bearer token, set as `remote.token` with the URL; without
///                    it the current `remote.token` is used.
/// * `abort`        - Abandon a move that stopped before rewriting instead.
/// * `store_access` - The command's shared store-access hold, so nothing is evicted while the
///                    absent content is counted.
///
/// # Returns
/// * `Ok(())`      - The origin is the new server, or the abort was reported.
/// * `Err(String)` - The warehouse is full, another move is running, the new server cannot be
///   reached, is not trusted or lacks content (`origin_move_incomplete`), or a write failed.
pub async fn move_origin(url: Option<String>,
                         token: Option<String>,
                         abort: bool,
                         store_access: &StoreAccessLock) -> Result<(), String> {
    if !scope_utils::content_may_be_absent()? {
        return Err(
            "This warehouse holds its whole history, so it has no origin to move: it may talk to any \
            remote. Point it at another one with \"forklift config remote.url <url>\".".to_string()
        );
    }

    let move_lock = MoveLock::try_acquire()?.ok_or_else(running_refusal)?;

    if abort {
        return abort_move(&move_lock);
    }

    // One spelling for the intent, the origin record and the config alike, as the client dials it.
    let url = url.ok_or("Name the server to move the origin to, or pass --abort.")?;
    let target = url.trim_end_matches('/').to_string();
    if target.is_empty() {
        return Err("The new origin must be a remote URL.".to_string());
    }

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

            return Err(match origin_move::abandon(&lock, &move_token) {
                Ok(true) => why,
                Ok(false) => format!(
                    "{} The move had started rewriting the origin before this run, so it still stands; \
                    run \"forklift remote move <url>\" again, naming a server that holds this \
                    warehouse's content.",
                    why
                ),
                Err(stuck) => format!("{} The move's record could not be cleared either: {}", why, stuck),
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

/// `remote move --abort`: clear an intent a move left before rewriting, and report one it left
/// while rewriting, which only a completed move clears.
fn abort_move(move_lock: &MoveLock) -> Result<(), String> {
    let lock = RefLock::acquire()?;
    let intent = origin_move::abort(move_lock, &lock)?;

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

/// The `origin_move_pending` refusal for a move or an abort while another move holds the lock.
fn running_refusal() -> String {
    let next_step = "Wait for the other move to finish, then run this again.";

    CoreError::refusal(
        RefusalCode::OriginMovePending,
        format!("Another move of this warehouse's origin is running; nothing was changed. {}", next_step),
        next_step,
    ).into()
}

/// The result of a move of the origin.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Serialize)]
pub(crate) struct RemoteMoveReport {
    /// The origin now, as `remote.url` names it.
    origin: String,

    /// The origin before the move; none when the record was missing.
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

        match &self.previous_origin {
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

/// The result of `remote move --abort`.
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
        crate::docgen::OutputSchema::of::<RemoteMoveReport>(),
        crate::docgen::OutputSchema::of::<RemoteAbortReport>(),
    ]
}
