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
    // Activate the durability-taint machinery for the rest of this process — unconditionally,
    // before argument dispatch, regardless of which command runs. This is safe to do even for a
    // command that never touches a warehouse (`version`, `help`): every other function this
    // switch affects (`taint_utils::record_taint`/`gate_check`/`read_taints`,
    // `heal_utils::heal_if_tainted`) is itself a no-op until a storage root is actually resolved.
    // The CLI may activate because it also wires the heal below — `forklift-server` does neither
    // (see `taint_utils`'s module doc comment on the all-or-nothing activation rule): activating
    // the taint write/gate without ever healing would be strictly worse than today's baseline.
    forklift_core::util::taint_utils::activate();

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

    // Undocumented, test-only debug hook (mirrors the `FORKLIFT_DISABLE_ROLLUP_SKIP` kill
    // switch): the rollup-skip equivalence tests need to observe, across the subprocess
    // boundary, that a skip actually fired — not just that its output happens to be correct
    // (which the rest of those tests already cover independently).
    if std::env::var("FORKLIFT_DEBUG_ROLLUP_SKIP_COUNT").is_ok() {
        eprintln!("rollup-skip-count: {}", forklift_core::util::inventory_utils::rollup_skip_count());
    }

    // Undocumented, test-only debug hook, same shape as the one above: lets the durability-barrier
    // batching work (DESIGN.html §5.0 D item 10) prove a burst of writes actually collapsed to a
    // constant number of barriers — see `file_utils::barrier_count`'s doc comment.
    if std::env::var("FORKLIFT_DEBUG_BARRIER_COUNT").is_ok() {
        eprintln!("barrier-count: {}", forklift_core::util::file_utils::barrier_count());
    }

    if let Err(error) = result {
        output::report_error(&error);
        std::process::exit(error.code.exit_code());
    }
}

/// The main forklift process: enter the warehouse, take the lock when the command mutates, heal
/// a standing durability taint, then dispatch. Warehouse entry and locking carry classified
/// errors so an agent can branch (§7.8); a command's own error is generic unless it says
/// otherwise.
///
/// The storage-scope entry-heal ([`forklift_core::util::heal_utils::heal_if_tainted`]) runs once
/// here, after [`warehouse_utils::enter_warehouse`] resolves the storage root (bay redirects
/// included) and — for a mutating command — after the warehouse lock is acquired, and before any
/// other line of this process may consult `does_object_exist` or dispatch to a command handler —
/// this is the single chokepoint every `requires_warehouse()` command passes through exactly once
/// per invocation, covering all of them uniformly, including read-only ones, **except the two
/// named by [`Command::bypasses_taint_heal`]** (`heal` and `audit` — see that method's doc comment
/// for why: refusing behind the very chokepoint a command exists to resolve or diagnose would be
/// circular). This is deliberately unlike `load_guard_utils::check_no_incomplete_load`, which is
/// called individually from inside `stack`'s and `park`'s own handlers — a taint can leave a
/// *durable reference* pointing at unproven bytes if left unhealed before *any* command trusts
/// existence, not just the two that durably commit staged inventory, so a single early chokepoint
/// (rather than a per-command guard sprinkled at each write site) is what the trust-gating
/// invariant actually needs.
///
/// **Lock-then-heal for mutating commands (the design memo's §3.3 reorder, closing half of the
/// §1.2 restage-vs-live-writer race).** [`Command::requires_warehouse_lock`] commands acquire
/// [`lock_utils::WarehouseLock`] *before* entry-heal runs, not after: a lock-*waiting* mutating
/// command must never restage a recorded path concurrently with the current lock-holder's own
/// deletions (e.g. a `stack` consuming a staged inventory shard, or a `compact --all` dropping a
/// superseded pack) — after this reorder it can't, because it only reaches entry-heal once it
/// *is* the lock-holder, the same way `heal` itself has always healed under the lock it holds. A
/// read-only command never takes this lock at all, so its entry-heal still runs lock-free exactly
/// as before; what makes *that* half of the race safe is `heal_if_tainted`'s own boundary (I1/I2,
/// `heal_utils`'s module doc comment): it restages only a content-addressed object it can
/// hash-verify, and escalates anything else to `forklift heal` rather than rewriting it lock-free.
/// **Behavioral change:** a tainted-and-locked warehouse now reports `WarehouseLocked` before the
/// `durability_taint` refusal for a mutating command (previously the taint could win that race);
/// a read-only command is unaffected either way, since it never takes the lock.
///
/// # Arguments
/// * `cli` - The parsed command line.
///
/// # Returns
/// * `Ok(())`             - If the process completes successfully.
/// * `Err(ForkliftError)` - A classified failure (code + message + optional next step).
async fn forklift(cli: Cli) -> Result<(), ForkliftError> {
    // Held for the mutating case (see the doc comment above); stays `None` for a read-only
    // command, which never takes the warehouse lock at all.
    let mut _lock: Option<lock_utils::WarehouseLock> = None;

    if cli.command.requires_warehouse() {
        warehouse_utils::enter_warehouse().map_err(|message| ForkliftError::new(
            ErrorCode::NotAWarehouse,
            message,
            "Run \"forklift prepare\" to create a warehouse here, or change into one."
        ))?;

        // Mutating commands hold the warehouse lock for their whole runtime, so two forklift
        // processes can never interleave writes to the staging area or the pallet refs — and
        // (see the doc comment above) so a lock-waiting command's own entry-heal never races the
        // lock-holder's deletions.
        if cli.command.requires_warehouse_lock() {
            _lock = Some(lock_utils::WarehouseLock::acquire().map_err(|message| ForkliftError::new(
                ErrorCode::WarehouseLocked,
                message,
                "Wait for the other forklift process to finish, or clear a stale lock as instructed."
            ))?);
        }

        if !cli.command.bypasses_taint_heal() {
            forklift_core::util::heal_utils::heal_if_tainted()?;
        }
    }

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
        Command::Compact { all, redelta } => commands::compact::handle_command(all, redelta),
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
        Command::Heal => commands::heal::handle_command(),
        Command::History { revision, class, limit, after, oneline } => commands::history::handle_command(revision, class, limit, after, oneline).await,
        Command::Query { revisions, from, class, unsupervised, supervisor, signer, author_after, author_before, merges, no_merges, grep, model, tool, tag, touches, verify: _, recorded, r#where, limit, after, oneline } =>
            commands::query::handle_command(commands::query::QueryArgs {
                revisions, from, class, unsupervised, supervisor, signer, author_after,
                author_before, merges, no_merges, grep, recorded, model, tool, tag, touches,
                r#where, limit, after, oneline,
            }).await,
        Command::ImportGit { path, no_compact } => commands::import_git::handle_command(&path, no_compact),
        Command::ExportGit { path } => commands::export_git::handle_command(&path),
        Command::Lift => commands::lift::handle_command().await,
        Command::Load { path } => commands::load::handle_command(&path).await,
        Command::Lower => commands::lower::handle_command().await,
        Command::Manifest { action } => commands::manifest::handle_command(action),
        Command::Haul { action } => commands::haul::handle_command(action).await,
        Command::Mcp { root } => commands::mcp::handle_command(root),
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
