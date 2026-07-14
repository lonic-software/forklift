use clap::Parser;
use forklift_core::util::{lock_utils, warehouse_utils};
use crate::cli::{Cli, Command, OfficeAction, ParkAction, ProfileAction};
use crate::output::{ErrorCode, ForkliftError, OutputMode};

pub mod cli;
pub mod commands;
#[cfg(feature = "docgen")]
pub mod docgen;
pub mod output;
pub mod pager;
pub mod passphrase;

/// Windows gives a process's main thread a 1MB stack by default (a linker setting); Linux and
/// macOS give it 8MB. clap's derive-generated `Cli::command()` — the whole tree of every
/// subcommand's args and help text, built as one large function in a debug build — is
/// expensive enough on its own to sit close to a 1MB budget once it is called from inside the
/// tokio dispatch machinery rather than at the very top of `main`; `help` calls it a *second*
/// time, from deeper in that same async call chain (to walk down to one subcommand's help),
/// and was measured to tip a 1MB stack over into a genuine overflow. Rather than chase every
/// future frame that might grow this further (more subcommands, longer help text), the real
/// work runs on a dedicated thread with an explicit, generous stack size — the standard fix
/// for this class of problem — so the platform difference in the OS-assigned main-thread
/// stack never matters.
const WORKER_STACK_SIZE: usize = 8 * 1024 * 1024;

/// The process entry point: hands off to a worker thread with an explicit stack size (see
/// [`WORKER_STACK_SIZE`]) and waits for it. `main` itself does no work that could need a large
/// stack, so its own (platform-default) stack size is irrelevant.
fn main() {
    std::thread::Builder::new()
        .name("forklift-main".to_string())
        .stack_size(WORKER_STACK_SIZE)
        .spawn(run)
        .expect("failed to spawn the forklift worker thread")
        .join()
        .expect("the forklift worker thread panicked");
}

/// Build the async runtime and run [`async_main`] on it, on the worker thread `main` spawns.
fn run() {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start the async runtime")
        .block_on(async_main());
}

/// The real entry point: parses arguments, wires up the pager and passphrase provider, and is
/// the top-level error handler for the [`forklift`] function it wraps.
async fn async_main() {
    // Clap owns the argument errors (usage, suggestions, exit code 2); everything past
    // parsing reports through the Err path below with a deterministic exit code (§7.8).
    let cli = Cli::parse();

    output::set_mode(if cli.json { OutputMode::Json } else { OutputMode::Human });

    // A quit pager or a closed `| head` should stop us cleanly, not panic (git's behavior).
    pager::restore_sigpipe();

    // Core delegates protected-key unlocking back to the terminal through this provider.
    passphrase::install_provider();

    // On a terminal, long read-only output (history, diff, …) is piped through a pager so
    // it is scrollable. The command's own writes are unchanged — the pager is wired in under
    // stdout, and only for read-only display commands so a passphrase prompt can never
    // deadlock behind it. Torn down after the command so the shell waits for the user to quit.
    let pager = if cli.command.pages_output() { pager::setup(cli.no_pager) } else { None };
    let result = forklift(cli).await;
    if let Some(pager) = pager {
        pager.close();
    }

    if let Err(error) = result {
        output::report_error(&error);
        std::process::exit(error.code.exit_code());
    }
}

/// The main forklift process: enter the warehouse, take the lock when the command
/// mutates, then dispatch. Warehouse entry and locking carry classified errors so an
/// agent can branch (§7.8); a command's own error is generic unless it says otherwise.
///
/// # Arguments
/// * `cli` - The parsed command line.
///
/// # Returns
/// * `Ok(())`             - If the process completes successfully.
/// * `Err(ForkliftError)` - A classified failure (code + message + optional next step).
async fn forklift(cli: Cli) -> Result<(), ForkliftError> {
    if cli.command.requires_warehouse() {
        warehouse_utils::enter_warehouse().map_err(|message| ForkliftError::new(
            ErrorCode::NotAWarehouse,
            message,
            "Run \"forklift prepare\" to create a warehouse here, or change into one."
        ))?;
    }

    // Mutating commands hold the warehouse lock for their whole runtime, so two forklift
    // processes can never interleave writes to the staging area or the pallet refs.
    let _lock = if cli.command.requires_warehouse_lock() {
        Some(lock_utils::WarehouseLock::acquire().map_err(|message| ForkliftError::new(
            ErrorCode::WarehouseLocked,
            message,
            "Wait for the other forklift process to finish, or clear a stale lock as instructed."
        ))?)
    } else {
        None
    };

    // Snapshot the pre-operation state for journaled commands (§7.8), so `undo` can
    // reverse this operation. Best-effort throughout: a journaling problem must never
    // block or fail a command — undo simply falls back to its classic behavior.
    let journal_pre = cli.command.journal_op()
        .and_then(|op| forklift_core::util::journal_utils::capture(op).ok());

    // A mutating command adds loose objects; captured before `dispatch` consumes `cli`.
    let auto_maintenance = cli.command.triggers_auto_maintenance();

    let result = dispatch(cli).await;

    if result.is_ok() {
        if let Some(pre) = journal_pre {
            let _ = forklift_core::util::journal_utils::push_if_changed(pre);
        }

        // Now that the command has succeeded, keep the object store healthy if it has
        // accumulated enough loose objects or packs to warrant it (git's gc --auto). Runs
        // synchronously under the warehouse lock we still hold — see `maintenance::run_if_due`.
        if auto_maintenance {
            commands::maintenance::run_if_due();
        }
    }

    result.map_err(ForkliftError::from)
}

/// Dispatch a parsed command to its handler. Handlers own presentation and return a
/// plain `Result<(), String>`; the generic error is classified by the caller.
async fn dispatch(cli: Cli) -> Result<(), String> {
    match cli.command {
        Command::Alias { action } => commands::alias::handle_command(action),
        Command::Audit { pallet, full } => commands::audit::handle_command(pallet, full),
        Command::Blame { path, rev } => commands::blame::handle_command(&path, rev).await,
        Command::Config { global, unset, key, value } =>
            commands::config::handle_command(global, unset, key, value),
        Command::Profile { action } => match action {
            Some(ProfileAction::Create { name, display_name, identifier }) =>
                commands::profile::create(&name, display_name, identifier),
            Some(ProfileAction::Use { name }) => commands::profile::use_profile(&name),
            Some(ProfileAction::List) | None => commands::profile::list(),
        },
        Command::Compact { all } => commands::compact::handle_command(all),
        Command::Store => commands::store::handle_command(),
        Command::Conflicts => commands::conflicts::handle_command(),
        Command::Bay { action } => commands::bay::handle_command(action),
        Command::Consolidate { pallet } => commands::consolidate::handle_command(&pallet).await,
        Command::CherryPick { revision, message } => commands::cherry_pick::handle_command(&revision, message).await,
        Command::Deliver { target, message } => commands::deliver::handle_command(&target, message),
        Command::Diff { staged, targets } => commands::diff::handle_command(staged, &targets, cli.verbose).await,
        Command::Help { command } => commands::help::handle_command(&command),
        Command::Expand { paths } => commands::expand::handle_command(paths).await,
        Command::Narrow { paths } => commands::narrow::handle_command(paths).await,
        Command::Franchise { url, directory, pallet, token, only } =>
            commands::franchise::handle_command(&url, &directory, pallet, token, only).await,
        Command::History { revision, class, limit, after, oneline } => commands::history::handle_command(revision, class, limit, after, oneline).await,
        Command::ImportGit { path, no_compact } => commands::import_git::handle_command(&path, no_compact),
        Command::ExportGit { path } => commands::export_git::handle_command(&path),
        Command::Lift => commands::lift::handle_command().await,
        Command::Load { path } => commands::load::handle_command(&path).await,
        Command::Lower => commands::lower::handle_command().await,
        Command::Manifest { action } => commands::manifest::handle_command(action),
        Command::Haul { action } => commands::haul::handle_command(action).await,
        Command::Mcp { root } => commands::mcp::handle_command(root),
        Command::Peer { token, ephemeral, server, tor_control, tor_control_password } =>
            commands::peer::handle_command(token, ephemeral, server, tor_control, tor_control_password).await,
        Command::Office { action } => match action {
            Some(OfficeAction::Enroll { offline, passphrase }) =>
                commands::office::enroll(offline, passphrase).await,
            Some(OfficeAction::Keygen { passphrase }) => commands::office::keygen(passphrase),
            Some(OfficeAction::Admit { operator_id, public_key, pop, role, pallets, agent, bot, service, supervisor }) => {
                let class = if agent {
                    forklift_core::util::office_utils::IdentityClass::Agent
                } else if bot {
                    forklift_core::util::office_utils::IdentityClass::Bot
                } else if service {
                    forklift_core::util::office_utils::IdentityClass::Service
                } else {
                    forklift_core::util::office_utils::IdentityClass::Human
                };

                commands::office::admit(&operator_id, &public_key, &pop, &role, pallets, class, supervisor)
            }
            Some(OfficeAction::Link { public_key, pop }) =>
                commands::office::link(&public_key, &pop),
            Some(OfficeAction::Authorize { operator_id, public_key, pop }) =>
                commands::office::authorize(&operator_id, &public_key, &pop),
            Some(OfficeAction::Role { identifier, role, pallets }) =>
                commands::office::role(&identifier, &role, pallets),
            Some(OfficeAction::Rotate { offline, passphrase }) =>
                commands::office::rotate(offline, passphrase).await,
            Some(OfficeAction::Retire { key_id, compromised, offline }) =>
                commands::office::retire(&key_id, compromised, offline).await,
            Some(OfficeAction::Regenesis { confirm }) => commands::office::regenesis(confirm),
            Some(OfficeAction::AcceptRegenesis { confirm }) =>
                commands::office::accept_regenesis(confirm).await,
            Some(OfficeAction::List) | None => commands::office::list().await,
        },
        Command::Palletize { name, revision, all } => commands::palletize::handle_command(name, revision, all).await,
        Command::Park { action } => match action {
            Some(ParkAction::Pop) => commands::park::pop_parked(),
            Some(ParkAction::List) => commands::park::list_parked(),
            None => commands::park::park_changes().await,
        },
        Command::Peek { inventory, object } => commands::peek::handle_command(inventory, object, cli.verbose),
        Command::Prepare => commands::prepare::handle_command(cli.verbose),
        Command::Remove { path } => commands::remove::handle_command(&path),
        Command::Restore { staged, path } => commands::restore::handle_command(staged, &path),
        Command::Scope { action } => commands::scope::handle_command(action),
        Command::ScopePrune { paths, dry_run } => commands::scope_prune::handle_command(paths, dry_run),
        Command::Shift { pallet } => commands::shift::handle_command(&pallet).await,
        Command::Show { target } => commands::show::handle_command(&target),
        Command::Stack { description } => commands::stack::handle_command(description).await,
        Command::Tag { action } => commands::tag::handle_command(action).await,
        Command::Stocktake { summary } => commands::stocktake::handle_command(summary).await,
        Command::Undo => commands::undo::handle_command().await,
        Command::Unload { path } => commands::unload::handle_command(&path),
        Command::Version => commands::version::handle_command(),
        Command::SelfUpdate { check } => commands::self_update::handle_command(check).await,
        #[cfg(feature = "docgen")]
        Command::Docgen { target } => {
            use crate::cli::DocgenTarget;
            match target {
                DocgenTarget::Errors => {
                    print!("{}", docgen::render_errors());
                    Ok(())
                }
                DocgenTarget::JsonSchemas => docgen::render_json_schemas().map(|out| print!("{}", out)),
            }
        }
    }
}
