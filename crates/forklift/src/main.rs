use std::ffi::OsString;
use clap::error::ErrorKind;
use clap::{CommandFactory, FromArgMatches};
use forklift_core::util::{lock_utils, warehouse_utils};
use forklift_core::util::fanout_utils::block_on_worker_stack;
use crate::cli::{Cli, Command, GitAction, OfficeAction, PalletAction, ParkAction, ProfileAction, RemoteAction, ScopeAction, SelfAction, StoreAction};
use crate::output::{ErrorCode, ForkliftError, OutputMode};

pub mod cache_fetch;
pub mod cli;
pub mod color;
pub mod commands;
#[cfg(feature = "docgen")]
pub mod docgen;
pub mod events;
#[cfg(any(test, feature = "docgen"))]
mod internal_refs;
pub mod output;
pub mod pager;
pub mod passphrase;
pub mod prefix;
mod remote_heads;
#[cfg(test)]
mod surface_check;
#[cfg(test)]
mod test_support;

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
/// stack never matters. The size is core's
/// [`WORKER_STACK_SIZE`](forklift_core::util::fanout_utils::WORKER_STACK_SIZE), which the
/// runtime's threads get too: they read objects, and a delta chain's reconstruction recurses.
///
/// The process entry point: hands off to a worker thread with an explicit stack size and waits
/// for it. `main` itself does no work that could need a large stack, so its own
/// (platform-default) stack size is irrelevant.
fn main() {
    // Before anything else opens a file descriptor: validating an inherited `PULT_EVENTS`
    // number is only sound while nothing of ours could have been given that number.
    events::init();

    block_on_worker_stack("forklift-main", async_main);
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

    // A usage error is reported here, as a document under `--json`; everything past parsing
    // reports through the Err path below with a deterministic exit code (§7.8).
    let cli = parse_or_exit();

    output::set_mode(if cli.json { OutputMode::Json } else { OutputMode::Human });

    // Before the pager below takes over stdout, which would make a terminal look like a pipe.
    color::init(cli.color);

    // A quit pager or a closed `| head` should stop us cleanly, not panic (git's behavior).
    pager::restore_sigpipe();

    // Core delegates protected-key unlocking back to the terminal through this provider.
    passphrase::install_provider();

    // A cache warehouse's reads fetch what it holds by hash from its origin.
    cache_fetch::install();

    // On a terminal, long read-only output (history, diff, …) is piped through a pager so
    // it is scrollable. The command's own writes are unchanged — the pager is wired in under
    // stdout, and only for read-only display commands so a passphrase prompt can never
    // deadlock behind it. Torn down after the command so the shell waits for the user to quit.
    let pager = if cli.command.pages_output() { pager::setup(cli.no_pager) } else { None };
    let result = forklift(cli).await;
    events::finish();
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

    // Undocumented, test-only debug hook, same shape again: lets the pack preload tests prove a
    // `diff` read the pack folder once, and that the read was the preload's, and the lazy index tests
    // prove a command validated only the index buckets it read.
    if std::env::var("FORKLIFT_DEBUG_PACK_COUNTS").is_ok() {
        eprintln!("pack-set-reads: {}", forklift_core::util::pack_utils::pack_set_read_count());
        eprintln!("pack-preloads: {}", forklift_core::util::pack_utils::pack_preload_count());
        eprintln!("pack-buckets-validated: {}", forklift_core::util::pack_utils::pack_buckets_validated());
        eprintln!("pack-full-validations: {}", forklift_core::util::pack_utils::pack_full_validations());
    }

    // Undocumented, test-only debug hook, same shape again: lets the commit-graph tests prove a
    // cold walk rewrote each shard a bounded number of times, not once per parcel.
    if std::env::var("FORKLIFT_DEBUG_GRAPH_WRITES").is_ok() {
        eprintln!("graph-shard-writes: {}", forklift_core::util::graph_utils::shard_write_count());
    }

    if let Err(error) = result {
        // A result that failed to serialize already wrote this process's one document.
        if !output::result_unserializable() {
            output::report_error(&error);
        }
        std::process::exit(error.code.exit_code());
    }

    // The command succeeded but its result did not serialize: the one document written was a
    // failure, and the exit status agrees with it.
    if output::result_unserializable() {
        std::process::exit(ErrorCode::Generic.exit_code());
    }
}

/// Parse the command line, recording the resolved command for the output envelope. A parse
/// failure never returns: without `--json` clap prints and exits as it always has; with it, the
/// failure becomes the process's one stdout document (see [`report_usage_error`]).
fn parse_or_exit() -> Cli {
    let mut args: Vec<OsString> = std::env::args_os().collect();

    // clap colours its own help and usage errors, so it follows `--color` as the commands do.
    let mut command = Cli::command();
    if let Some(choice) = color::from_args(&args) {
        command = command.color(choice.for_clap());
    }
    command.build();

    // A unique prefix of a command name is that command (`ha rev` is `haul review`), so clap is
    // given the full names. An ambiguous prefix is a usage error like clap's own. `branch NAME`
    // is read before any prefix is expanded: under git's word, the word after it is a name.
    let expanded = match branch_name_error(&command, &args) {
        Some(error) => Err(error),
        None => prefix::expand(&command, &mut args),
    };
    let error = match expanded {
        Err(error) => error,
        Ok(()) => {
            match command.try_get_matches_from_mut(&args) {
                Ok(mut matches) => {
                    output::set_command(matches.subcommand_name());

                    match Cli::from_arg_matches_mut(&mut matches) {
                        Ok(cli) => return cli,
                        Err(error) => error,
                    }
                }
                Err(error) => error,
            }
        }
    };

    // The parse failed, so `cli.json` does not exist: read the flag off the raw arguments. It is
    // global, so it may sit anywhere before a `--` terminator.
    let json = args.iter().skip(1)
        .take_while(|arg| arg.as_os_str() != "--")
        .any(|arg| arg.as_os_str() == "--json");

    if !json {
        error.exit();
    }

    output::set_mode(OutputMode::Json);
    output::set_command(named_command(&args).as_deref());
    report_usage_error(&error);
}

/// Whether `level` declares the option `word` spells: a long name or alias (with or without
/// `=VALUE`), or a cluster of short ones up to the first that takes the rest as its value.
fn knows_option(level: &clap::Command, word: &str) -> bool {
    if let Some(long) = word.strip_prefix("--") {
        let long = long.split_once('=').map_or(long, |(name, _)| name);
        return level.get_arguments().any(|arg| arg.get_long() == Some(long)
            || arg.get_all_aliases().is_some_and(|aliases| aliases.contains(&long)));
    }
    for letter in word[1..].chars() {
        let Some(arg) = level.get_arguments().find(|arg| arg.get_short() == Some(letter)
            || arg.get_all_short_aliases().is_some_and(|aliases| aliases.contains(&letter))) else {
            return false;
        };
        if arg.get_action().takes_values() {
            return true;
        }
    }
    true
}

/// The refusal for git's `branch NAME`. `branch` is a hidden alias of the `pallet` family, kept for
/// git users, but git's `branch NAME` creates a branch without switching to it, and `pallet create`
/// also shifts, so a name after `branch` is not taken as a create: it is refused with the command
/// that creates the pallet. A flag the pallet level lacks (git's `-d`, `-m`) leaves the name to
/// clap, which refuses the flag, and so does a help flag, so the help asked for is printed. Only a pallet subcommand's exact name or alias is a subcommand there;
/// `args` are read before [`prefix::expand`], so `branch cr` is the pallet "cr", not a prefix of
/// `create`. Options are stepped over as the prefix walk steps over them, and the words after `--`
/// are left to clap. `command` must be built.
///
/// # Returns
/// * `Some(clap::Error)` - A usage error on `pallet`, naming `forklift pallet create NAME`.
/// * `None`              - The command is not `branch`, or `branch` is given no name.
fn branch_name_error(command: &clap::Command, args: &[OsString]) -> Option<clap::Error> {
    let mut level = command;
    let mut at_root = true;
    let mut index = 1;
    while index < args.len() {
        let word = args[index].to_str()?;
        if word == "--" {
            return None;
        }
        if word.starts_with('-') && word != "-" {
            // A flag the pallet level does not have is git's (`branch -d NAME`, `branch -m OLD NEW`),
            // which says what the name is for: not a create, so clap refuses the flag instead.
            let asks_for_help = word == "--help" || (!word.starts_with("--") && word.contains('h'));
            if asks_for_help || (!at_root && !knows_option(level, word)) {
                return None;
            }
            index += if prefix::takes_next_word(level, word) { 2 } else { 1 };
            continue;
        }
        if at_root {
            if word != "branch" {
                return None;
            }
            level = command.find_subcommand("pallet").expect("the pallet family exists");
            at_root = false;
            index += 1;
            continue;
        }
        if level.find_subcommand(word).is_some() {
            return None;
        }
        return Some(level.clone().error(ErrorKind::InvalidSubcommand, format!(
            "\"{word}\" is not a pallet subcommand. To create the pallet \"{word}\" and shift to it, run \
            \"forklift pallet create {word}\"."
        )));
    }
    None
}

/// The canonical name of the top-level command the arguments name, when the parse that would
/// have said so failed. `args` have been through [`prefix::expand`], so a prefix the user typed is
/// a full name here. An option's value given as its own word (`--color never`) is skipped, so the
/// first other argument that is not an option is the command, or an alias of one, or nothing
/// known. A help or version flag before it, alone
/// or in a short-flag cluster (`-vh`), means clap answered at the top level, so no command ran.
fn named_command(args: &[OsString]) -> Option<String> {
    let asks_help_or_version = |arg: &str| match arg.strip_prefix("--") {
        Some(long) => long == "help" || long == "version",
        None => arg.starts_with('-') && arg.contains(['h', 'V']),
    };

    let cli = Cli::command();
    let mut words = args.iter().skip(1)
        .map(|arg| arg.to_string_lossy())
        .take_while(|arg| !asks_help_or_version(arg));
    let token = loop {
        let word = words.next()?;
        if !word.starts_with('-') {
            break word;
        }
        if prefix::takes_next_word(&cli, &word) {
            words.next();
        }
    };

    cli.find_subcommand(token.as_ref()).map(|command| command.get_name().to_string())
}

/// Report a clap parse result as the one `--json` document, then exit. Help and version are
/// results, not failures: a success document carrying clap's text, exit 0. Every other kind is
/// a `usage` failure, exit 2, whose `details.usage` keeps clap's full text (the usage line and
/// any "a similar command exists" tip) that the one-line message leaves out.
fn report_usage_error(error: &clap::Error) -> ! {
    let text = error.render().to_string();

    if matches!(error.kind(), ErrorKind::DisplayHelp | ErrorKind::DisplayVersion) {
        output::message(text.trim_end());
        std::process::exit(0);
    }

    let first = text.lines().next().unwrap_or_default();

    output::report_error(&ForkliftError {
        code: ErrorCode::Usage,
        message: first.strip_prefix("error: ").unwrap_or(first).to_string(),
        next_step: Some(
            "Run \"forklift --help\" for the commands, or \"forklift <command> --help\" for a \
             command's arguments.".to_string()
        ),
        details: Some(serde_json::json!({ "usage": text.trim_end() })),
    });

    std::process::exit(ErrorCode::Usage.exit_code());
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
/// **Lock-then-heal for mutating commands (closing half of the restage-vs-live-writer race — see
/// DESIGN.html §3.1.1 for the full taint-and-heal contract).**
/// [`Command::requires_warehouse_lock`] commands acquire
/// [`lock_utils::WarehouseLock`] *before* entry-heal runs, not after: a lock-*waiting* mutating
/// command must never restage a recorded path concurrently with the current lock-holder's own
/// deletions (e.g. a `stack` consuming a staged inventory shard, or a `store compact --all` dropping a
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

    // The shared store-access hold of a command that writes the shared object store: taken right
    // after the warehouse lock, released before auto-maintenance (see below).
    let mut store_access: Option<lock_utils::StoreAccessLock> = None;

    // Help needs no warehouse, and its one failure (an unknown command name) is a usage error,
    // which `dispatch`'s plain `String` errors cannot carry.
    if let Command::Help { command } = &cli.command {
        return commands::help::handle_command(command);
    }

    if cli.command.requires_warehouse() {
        // Every way of not being in a warehouse is a coded refusal with its own next step; anything
        // else (an unreadable marker or redirect) is a failure no prepare would cure.
        warehouse_utils::enter_warehouse().map_err(ForkliftError::from)?;

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

        // Order this command's whole decide-then-publish window against every operation that
        // deletes objects on a liveness or scope decision in any other process (FORK-150): a
        // dedup that finds an unreachable object present must never publish a reference to it
        // while a full repack, gc or scope prune is about to delete it. Taken after the bay lock
        // (always that order, so no cycle) and before entry-heal, which can restage objects.
        // Waits while a deleter runs: the lock is a kernel lock, so a dead holder releases it.
        if cli.command.holds_store_access() {
            store_access = Some(lock_utils::StoreAccessLock::shared(announce_store_access_wait)?);
        }

        // Before entry-heal: an interrupted operation's staging is `heal`'s to settle, not a
        // command's to build on.
        if cli.command.refused_during_interrupted_operation() {
            forklift_core::util::operation_utils::check_none_standing()?;
        }

        if !cli.command.bypasses_taint_heal() {
            forklift_core::util::heal_utils::heal_if_tainted()?;
        }
    }

    // Snapshot the pre-operation state for journaled commands (§7.8), and record the ref writes
    // this process makes from here on, so `undo` can reverse this operation. Best-effort
    // throughout: a journaling problem must never block or fail a command. A missed entry costs
    // only undo's reach — undo refuses an older entry once the bay no longer matches it — and an
    // unreadable journal is reported by undo itself.
    let journal_pre = cli.command.journal_op()
        .and_then(|op| forklift_core::util::journal_utils::capture().ok().map(|pre| (op, pre)));

    // A mutating command adds loose objects; captured before `dispatch` consumes `cli`.
    let auto_maintenance = cli.command.triggers_auto_maintenance();

    let result = dispatch(cli, store_access.as_ref()).await;

    if let Some((op, pre)) = journal_pre {
        if result.is_ok() {
            let _ = forklift_core::util::journal_utils::push_if_changed(op, pre);
        } else {
            forklift_core::util::journal_utils::discard();
        }
    }

    // Everything this command publishes is published. Release the shared hold before
    // auto-maintenance, whose full repack takes the exclusive one and would otherwise be refused
    // by this very process's own shared hold.
    drop(store_access);

    if result.is_ok() {

        // Now that the command has succeeded, keep the object store healthy if it has
        // accumulated enough loose objects or packs to warrant it (git's gc --auto). Runs
        // synchronously under the warehouse lock we still hold — see `maintenance::run_if_due`.
        if auto_maintenance {
            commands::maintenance::run_if_due();
        }
    }

    result.map_err(ForkliftError::from)
}

/// Say why a mutating command is not starting yet: another process holds the store exclusively
/// for maintenance. One line, on stderr so it never mixes into a command's result, and as a
/// status report on the progress-events channel.
fn announce_store_access_wait() {
    const WAITING: &str = "Waiting for object-store maintenance in another process…";

    if !output::is_json() {
        eprintln!("{}", WAITING);
    }

    events::progress().status(WAITING);
}

/// Dispatch a parsed command to its handler. Handlers own presentation and return a
/// plain `Result<(), String>`; the generic error is classified by the caller. `store_access` is
/// the command's shared store-access hold, when it takes one ([`Command::holds_store_access`]).
async fn dispatch(cli: Cli, store_access: Option<&lock_utils::StoreAccessLock>) -> Result<(), String> {
    match cli.command {
        Command::Audit { pallet, full } => commands::audit::handle_command(pallet, full),
        Command::Blame { path, rev } => commands::blame::handle_command(&path, rev).await,
        Command::Config { action } => commands::config::handle_command(action),
        Command::Profile { action } => match action {
            Some(ProfileAction::Create { name, identifier }) =>
                commands::profile::create(&name, identifier),
            Some(ProfileAction::Use { name }) => commands::profile::use_profile(&name),
            Some(ProfileAction::Keygen { name, passphrase }) => commands::profile::keygen(name.as_deref(), passphrase),
            Some(ProfileAction::List) | None => commands::profile::list(),
        },
        Command::Store { action } => match action {
            Some(StoreAction::Status) | None => commands::store::handle_command(),
            Some(StoreAction::Compact { all, redelta }) => commands::compact::handle_command(all, redelta),
            Some(StoreAction::Peek { inventory, object }) => commands::peek::handle_command(inventory, object, cli.verbose),
            Some(StoreAction::Evict { dry_run }) => commands::evict::handle_command(dry_run).await,
        },
        Command::Conflicts => commands::conflicts::handle_command(),
        Command::Bay { action } => commands::bay::handle_command(action),
        Command::Consolidate { pallet } => commands::consolidate::handle_command(&pallet).await,
        Command::Pick { revision, message } => commands::pick::handle_command(&revision, message).await,
        Command::Deliver { target, message } => commands::deliver::handle_command(&target, message),
        Command::Diff { staged, targets } => commands::diff::handle_command(staged, &targets, cli.verbose).await,
        Command::Help { .. } => unreachable!("help is answered in `forklift`, before dispatch"),
        Command::Clone { url, directory, pallet, token, only, cache, expect } =>
            commands::clone::handle_command(&url, &directory, pallet, token, only, cache, expect).await,
        Command::Heal => {
            let store_access = store_access.ok_or("heal ran without its store-access hold.")?;
            commands::heal::handle_command(store_access).await
        }
        Command::History { revision, limit, after, oneline } => commands::history::handle_command(revision, limit, after, oneline).await,
        Command::Query { revisions, from, class, unsupervised, supervisor, signer, author_after, author_before, merges, no_merges, grep, model, tool, tag, touches, verify: _, recorded, r#where, limit, after, oneline } =>
            commands::query::handle_command(commands::query::QueryArgs {
                revisions, from, class, unsupervised, supervisor, signer, author_after,
                author_before, merges, no_merges, grep, recorded, model, tool, tag, touches,
                r#where, limit, after, oneline,
            }).await,
        Command::Git { action } => match action {
            GitAction::Import { path, no_compact } => commands::import_git::handle_command(&path, no_compact),
            GitAction::Export { path } => commands::export_git::handle_command(&path),
            GitAction::Mirror => commands::mirror::handle_command().await,
        },
        Command::Lift { recreate } => commands::lift::handle_command(recreate).await,
        Command::Load { path } => commands::load::handle_command(&path).await,
        Command::Lower { pallet, expect, keep, confirm } => commands::lower::handle_command(pallet, expect, keep, confirm).await,
        Command::Manifest { action } => commands::manifest::handle_command(action),
        Command::Haul { action } => commands::haul::handle_command(action).await,
        Command::Mcp { root } => commands::mcp::handle_command(root),
        Command::Office { action } => match action {
            Some(OfficeAction::Enroll { offline, passphrase }) =>
                commands::office::enroll(offline, passphrase).await,
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
        Command::Pallet { action } => match action {
            Some(PalletAction::List { all, remote: false }) => commands::pallet::list_pallets(all),
            None => commands::pallet::list_pallets(false),
            Some(PalletAction::List { all, remote: true }) => commands::pallet::list_remote_pallets(all).await,
            Some(PalletAction::Create { name, revision }) =>
                commands::pallet::create_pallet(&name, revision.as_deref()).await,
            Some(PalletAction::Delete { name, remote: true, .. }) => commands::pallet::delete_remote_pallet(&name).await,
            Some(PalletAction::Delete { name, force, remote: false }) => commands::pallet::delete_pallet(&name, force).await,
            Some(PalletAction::Restore { name, remote: true }) => commands::pallet::restore_remote_pallet(&name).await,
            Some(PalletAction::Restore { name, remote: false }) => commands::pallet::restore_pallet(&name),
        },
        Command::Park { action } => match action {
            Some(ParkAction::Pop) => commands::park::pop_parked(),
            Some(ParkAction::List) => commands::park::list_parked(),
            None => commands::park::park_changes().await,
        },
        Command::Prepare => commands::prepare::handle_command(cli.verbose),
        Command::Remove { path } => commands::remove::handle_command(&path),
        Command::Restore { path, confirm } => commands::restore::handle_command(&path, confirm),
        Command::Scope { action } => match action {
            Some(ScopeAction::Status) | None => commands::scope::handle_command(),
            Some(ScopeAction::Fetch { paths }) => commands::scope_fetch::handle_command(paths).await,
            Some(ScopeAction::Narrow { paths }) => commands::scope_narrow::handle_command(paths).await,
            Some(ScopeAction::Prune { paths, confirm }) => commands::scope_prune::handle_command(paths, confirm).await,
        },
        Command::Remote { action } => match action {
            None => commands::remote::show(),
            Some(RemoteAction::Move { url, token }) => {
                let store_access = store_access.ok_or("remote move ran without its store-access hold.")?;
                commands::remote::move_origin(url, token, store_access).await
            }
            Some(RemoteAction::Abort) => commands::remote::abort_move(),
        },
        Command::Shift { pallet } => commands::shift::handle_command(&pallet).await,
        Command::Show { target } => commands::show::handle_command(&target),
        Command::Stack { message } => commands::stack::handle_command(message).await,
        Command::Tag { action } => commands::tag::handle_command(action).await,
        Command::Stocktake { summary } => commands::stocktake::handle_command(summary).await,
        Command::Undo { as_name } => commands::undo::handle_command(as_name.as_deref()).await,
        Command::Unload { path } => commands::unload::handle_command(&path),
        Command::Version => commands::version::handle_command(),
        Command::SelfCommand { action } => match action {
            SelfAction::Alias { action } => commands::alias::handle_command(action),
            SelfAction::Schema => commands::self_schema::handle_command(),
            SelfAction::Update { check } => commands::self_update::handle_command(check).await,
        },
        #[cfg(feature = "docgen")]
        Command::Docgen { target } => {
            use crate::cli::DocgenTarget;
            match target {
                DocgenTarget::Errors => docgen::render_errors().map(|out| print!("{}", out)),
                DocgenTarget::JsonSchemas => docgen::render_json_schemas().map(|out| print!("{}", out)),
                DocgenTarget::Commands => docgen::render_commands().map(|out| print!("{}", out)),
                DocgenTarget::McpTools => docgen::render_mcp_tools().map(|out| print!("{}", out)),
            }
        }
    }
}
