//! Rebuilding a sparse warehouse's lost or damaged fetch scope, for `forklift heal`.
//!
//! ## What the checkouts say
//!
//! Every checkout's scope (the warehouse's own working tree's and each bay's) lies within the fetch
//! scope: a checkout is never given a scope outside it, and `scope prune` never narrows the fetch
//! scope past one. So the union of the checkout scopes is a lower bound on what was fetched. It is
//! exact while the main tree is unnarrowed and nothing was fetched with `scope fetch`; otherwise it
//! is narrower, which loses nothing (nothing frees an object for falling outside the scope except an
//! explicit `scope prune`) but leaves what lies outside it unchecked for loss.
//!
//! ## The automatic rebuild
//!
//! [`repair`] runs when the fetch scope of a sparse warehouse is missing, empty or does not parse.
//! It reads each checkout's scope on its own, leaving out and reporting one that is missing, does
//! not parse or names no path, and checks the history of every readable user ref head at the union.
//! Only when all of it is present does it write the union, under the [`RefLock`], and only if the
//! file is still unreadable then. While it is, every sparse ref write refuses on reading it, so no
//! ref moves between the check and the write.
//!
//! A fetch scope that parses can still be wrong, since the record carries no digest. When a
//! checkout's scope lies outside it, one of the two files is damaged, and heal cannot tell which.
//! It refuses with `fetch_scope_damaged`, offering both ways out.
//!
//! ## The chosen scope
//!
//! [`set_chosen`] is `heal --fetch-scope`: the operator names the scope, which must hold every
//! checkout's, and heal fetches what the history at it lacks before writing it. It runs under the
//! exclusive store-access hold, as `scope prune` does, since it can narrow a scope that reads.

use std::path::PathBuf;

use serde::Serialize;

use crate::error::{CoreError, RefusalCode};
use crate::util::lock_utils::{RefLock, StoreAccessExclusive};
use crate::util::path_utils::WarehousePath;
use crate::util::remote_utils::{RemoteClient, ScopedWalk};
use crate::util::scope_utils::{self, PathScope, RawScope};
use crate::util::warehouse_utils::{self, WarehouseMode};
use crate::util::{audit_utils, bay_utils, pallet_utils, sealed_utils};

/// A fetch scope `heal` wrote.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopeRebuilt {
    /// The prefixes of the fetch scope written.
    pub scope: Vec<String>,

    /// Whether `--fetch-scope` chose them, rather than the checkouts' scopes.
    pub chosen: bool,

    /// What the fetch-scope file held that could not be used.
    pub was: String,

    /// The checkouts whose scopes the rebuilt one is the union of.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub from: Vec<String>,

    /// Checkouts whose scope could not be read, and why; they were left out.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub left_out: Vec<Unread>,

    /// How many objects `--fetch-scope` fetched.
    #[serde(skip_serializing_if = "is_zero")]
    pub fetched_objects: usize,
}

fn is_zero(count: &usize) -> bool {
    *count == 0
}

/// A checkout whose scope could not be used.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Unread {
    /// The checkout.
    pub checkout: String,

    /// Why its scope was not used.
    pub reason: String,
}

/// A checkout's scope that could be read.
#[derive(Clone, Debug, Serialize)]
struct ReadScope {
    checkout: String,
    scope: Vec<String>,
}

/// Everything a `fetch_scope_damaged` refusal reports.
#[derive(Debug, Default, Serialize)]
struct Refused {
    /// The fetch scope, when it parses.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope: Option<Vec<String>>,

    /// Why the fetch scope cannot be used, when it cannot.
    #[serde(skip_serializing_if = "Option::is_none")]
    fetch_scope_unreadable: Option<String>,

    /// The checkouts whose scopes were read.
    read: Vec<ReadScope>,

    /// The checkouts whose scopes could not be.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    unread: Vec<Unread>,

    /// The checkouts whose scopes lie outside the fetch scope.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    outside: Vec<String>,

    /// What the check of the history found missing.
    #[serde(skip_serializing_if = "Option::is_none")]
    missing: Option<String>,
}

/// The fetch-scope file, read on its own.
enum FetchFile {
    /// It parses and names a path.
    Readable(PathScope),

    /// It cannot be used, for the reason given.
    Unreadable(String),

    /// A newer Forklift wrote it.
    Newer,
}

fn read_fetch_file() -> Result<FetchFile, String> {
    Ok(match scope_utils::read_scope_raw(&scope_utils::fetch_scope_path())? {
        RawScope::Read(scope) if !scope.is_full() => FetchFile::Readable(scope),
        RawScope::Read(_) => FetchFile::Unreadable("names no path".to_string()),
        RawScope::Missing => FetchFile::Unreadable("is missing".to_string()),
        RawScope::Damaged(reason) => FetchFile::Unreadable(reason),
        RawScope::Newer(_) => FetchFile::Newer,
    })
}

/// Each checkout whose scope was read, with that scope, and each whose was not, with why.
type Checkouts = (Vec<(String, PathScope)>, Vec<Unread>);

/// The checkouts' scopes: those that parse and name a path, and the rest with why. A bays folder
/// that cannot be listed leaves only the working tree, with the bays noted as unread.
fn checkout_scopes() -> Result<Checkouts, String> {
    let mut checkouts: Vec<(String, PathBuf)> = vec![
        ("the warehouse's own working tree".to_string(), scope_utils::main_tree_scope_path()),
    ];
    let mut unread: Vec<Unread> = Vec::new();

    match bay_utils::list_bays() {
        Ok(bays) => checkouts.extend(bays.into_iter()
            .map(|name| (format!("bay \"{}\"", name), scope_utils::named_bay_scope_path(&name)))),
        Err(error) => unread.push(Unread { checkout: "the bays".to_string(), reason: error }),
    }

    let mut read: Vec<(String, PathScope)> = Vec::new();

    for (checkout, path) in checkouts {
        let reason = match scope_utils::read_scope_raw(&path)? {
            RawScope::Read(scope) if !scope.is_full() => {
                read.push((checkout, scope));
                continue;
            }
            RawScope::Read(_) => "it names no path".to_string(),
            RawScope::Missing => "it is missing".to_string(),
            RawScope::Damaged(reason) => format!("it {}", reason.trim_end_matches('.')),
            RawScope::Newer(reason) => format!("it {}", reason.trim_end_matches('.')),
        };
        unread.push(Unread { checkout, reason });
    }

    Ok((read, unread))
}

fn union(scopes: &[(String, PathScope)]) -> PathScope {
    PathScope::from_prefixes(scopes.iter().flat_map(|(_, scope)| scope.prefixes().iter().cloned()))
}

/// Check the history of every readable user ref head at `scope`, each bounded by the heads already
/// checked, leaning on the record of objects held by hash, the applied redactions, the withheld
/// content and the unavailable content only when a walk without them fails. The warehouse is
/// sparse, so it may lack what the sealed record holds.
///
/// # Returns
/// * `Ok(())`      - Every head's history is present at `scope`.
/// * `Err(String)` - What is missing, or why it could not be told.
fn check_history_at(scope: &PathScope) -> Result<(), String> {
    let mut checked: Vec<String> = Vec::new();
    let mut tolerated: Option<sealed_utils::Tolerated> = None;

    for head in pallet_utils::readable_user_heads()? {
        if checked.contains(&head) {
            continue;
        }

        // Each walk keeps its own visited sets, so the retry re-checks what the failed walk marked.
        if let Err(missing) = audit_utils::verify_ref_head_closure(&head, &checked, None, scope, None) {
            if tolerated.is_none() {
                tolerated = Some(sealed_utils::read_tolerated().map_err(|error| format!(
                    "{} and the record of objects held by hash, of the redactions applied here, of \
                    the content withheld here or of the content recorded unavailable, cannot be read: {}",
                    missing.trim_end_matches('.'), error
                ))?);
            }
            let tolerated = tolerated.as_ref().expect("read above");

            if tolerated.is_empty() {
                return Err(missing);
            }

            audit_utils::verify_ref_head_closure(&head, &checked, None, scope, Some(tolerated))?;
        }

        checked.push(head);
    }

    Ok(())
}

/// The `heal --fetch-scope` command that sets `scope`.
fn fetch_scope_command(scope: &PathScope) -> String {
    let mut command = "forklift heal".to_string();

    for prefix in scope.prefixes() {
        command.push_str(" --fetch-scope ");
        command.push_str(&shell_word(prefix));
    }

    command
}

/// `word` as one shell word: bare when it is made only of characters no shell treats specially.
fn shell_word(word: &str) -> String {
    let plain = !word.is_empty() && word.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"/._-+@%:,".contains(&byte));

    if plain {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', "'\\''"))
    }
}

/// The top-level folders of the working tree heal runs in, for a step to offer when no checkout's
/// scope can be read: a sparse checkout materializes each prefix under its first component.
fn working_tree_folders() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(crate::globals::warehouse_root()) else {
        return Vec::new();
    };

    let mut folders: Vec<String> = entries.filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.'))
        .collect();
    folders.sort();
    folders
}

fn describe_read(read: &[(String, PathScope)]) -> Vec<ReadScope> {
    read.iter().map(|(checkout, scope)| ReadScope { checkout: checkout.clone(), scope: scope.prefixes().to_vec() }).collect()
}

fn listed(prefixes: &[String]) -> String {
    prefixes.iter().map(|prefix| format!("\"{}\"", prefix)).collect::<Vec<_>>().join(", ")
}

fn refusal(message: String, next_step: String, refused: &Refused) -> CoreError {
    CoreError::refusal_with_details(
        RefusalCode::FetchScopeDamaged,
        message,
        next_step,
        serde_json::json!(refused),
    )
}

/// The lines naming what was read and what was not.
fn read_lines(refused: &Refused) -> Vec<String> {
    let mut lines = Vec::new();

    for read in &refused.read {
        lines.push(format!("  {} checks out {}.", capitalized(&read.checkout), listed(&read.scope)));
    }
    for unread in &refused.unread {
        lines.push(format!("  {}: its scope was not read, as {}.", capitalized(&unread.checkout), unread.reason));
    }

    lines
}

fn capitalized(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Rebuild a sparse warehouse's lost or damaged fetch scope from its checkouts' scopes, or refuse
/// with `fetch_scope_damaged` when that cannot be done safely (see the module doc comment). In a
/// warehouse that is not sparse, or whose fetch scope a newer Forklift wrote, it does nothing.
///
/// # Returns
/// * `Ok(Some(ScopeRebuilt))` - The fetch scope was rebuilt.
/// * `Ok(None)`               - Nothing needed rebuilding.
/// * `Err(CoreError)`         - A `fetch_scope_damaged` refusal, or a file that could not be
///   read or written.
pub fn repair() -> Result<Option<ScopeRebuilt>, CoreError> {
    if warehouse_utils::warehouse_mode()? != WarehouseMode::Sparse {
        return Ok(None);
    }

    let fetch = read_fetch_file()?;
    let (read, unread) = checkout_scopes()?;

    let was = match fetch {
        FetchFile::Newer => return Ok(None),
        FetchFile::Readable(scope) => {
            let outside: Vec<&(String, PathScope)> = read.iter().filter(|(_, checkout)| !checkout.subset_of(&scope)).collect();

            if outside.is_empty() {
                return Ok(None);
            }

            let mut widened: Vec<String> = scope.prefixes().to_vec();
            widened.extend(outside.iter().flat_map(|(_, checkout)| checkout.prefixes().iter().cloned()));
            let widened = PathScope::from_prefixes(widened);

            let refused = Refused {
                fetch_scope: Some(scope.prefixes().to_vec()),
                read: describe_read(&read),
                unread,
                outside: outside.iter().map(|(checkout, _)| checkout.clone()).collect(),
                ..Refused::default()
            };

            let mut lines = vec![format!(
                "The fetch scope ({}) does not hold the scope of {}, which no write leaves, so one of \
                those files is damaged. Nothing was changed.",
                listed(scope.prefixes()), refused.outside.join(" or "),
            )];
            lines.extend(read_lines(&refused));
            lines.push(
                "If the fetch scope is the damaged one, the next step sets it to hold them, fetching what \
                that lacks. If a checkout's scope is, remove that bay with \"forklift bay remove\" and add \
                it again under another name, or, for the warehouse's own working tree, clone the remote \
                again into a new directory.".to_string(),
            );

            return Err(refusal(lines.join("\n"), fetch_scope_command(&widened), &refused));
        }
        FetchFile::Unreadable(reason) => reason,
    };

    let scope = union(&read);

    if read.is_empty() {
        let folders = working_tree_folders();
        let refused = Refused { fetch_scope_unreadable: Some(was.clone()), unread, ..Refused::default() };

        let mut lines = vec![format!(
            "The fetch scope {} and no checkout's scope could be read to rebuild it from. Nothing was changed.",
            was.trim_end_matches('.'),
        )];
        lines.extend(read_lines(&refused));

        let next_step = match folders.is_empty() {
            false => {
                let offered = PathScope::from_prefixes(folders);
                lines.push(
                    "Name the paths this warehouse fetches with \"forklift heal --fetch-scope <path>\": the next \
                    step offers the folders of this working tree, and heal fetches what the history at them \
                    lacks.".to_string(),
                );
                fetch_scope_command(&offered)
            }
            true => "Name the paths this warehouse fetches with \"forklift heal --fetch-scope <path>\", once for each path.".to_string(),
        };

        return Err(refusal(lines.join("\n"), next_step, &refused));
    }

    if let Err(missing) = check_history_at(&scope) {
        let refused = Refused {
            fetch_scope_unreadable: Some(was.clone()),
            read: describe_read(&read),
            unread,
            missing: Some(missing.clone()),
            ..Refused::default()
        };

        let mut lines = vec![format!(
            "The fetch scope {}. The checkouts' scopes name {}, but the history there is not all \
            present, so it was not rebuilt from them: {} Nothing was changed.",
            was.trim_end_matches('.'), listed(scope.prefixes()),
            if missing.ends_with('.') { missing.clone() } else { format!("{}.", missing) },
        )];
        lines.extend(read_lines(&refused));
        lines.push(
            "The next step sets the fetch scope to those paths, fetching what the history there lacks; \
            another \"--fetch-scope <path>\" names one more.".to_string(),
        );

        return Err(refusal(lines.join("\n"), fetch_scope_command(&scope), &refused));
    }

    let _lock = RefLock::acquire()?;

    // Another heal may have rebuilt it meanwhile; a scope that reads now is not overwritten.
    if !matches!(read_fetch_file()?, FetchFile::Unreadable(_)) {
        return Ok(None);
    }

    scope_utils::set_fetch_scope(&scope)?;

    Ok(Some(ScopeRebuilt {
        scope: scope.prefixes().to_vec(),
        chosen: false,
        was,
        from: read.into_iter().map(|(checkout, _)| checkout).collect(),
        left_out: unread,
        fetched_objects: 0,
    }))
}

/// Set a sparse warehouse's fetch scope to `prefixes`, for `heal --fetch-scope`: refused unless the
/// fetch scope cannot be read or a checkout's scope lies outside it, and unless `prefixes` holds
/// every checkout's scope that can be read. Every readable user ref head's history at the new scope
/// is fetched from the origin and checked before it is written; offline, or when the remote lacks
/// an object too, nothing is written.
///
/// # Arguments
/// * `prefixes`  - The paths, as the user gave them.
/// * `exclusive` - The exclusive store-access hold: no other process uses the store meanwhile.
///
/// # Returns
/// * `Ok(ScopeRebuilt)` - The fetch scope was written.
/// * `Err(CoreError)`   - It was not, and why.
pub async fn set_chosen(prefixes: &[String], _exclusive: &StoreAccessExclusive) -> Result<ScopeRebuilt, CoreError> {
    if warehouse_utils::warehouse_mode()? != WarehouseMode::Sparse {
        return Err("Only a sparse warehouse has a fetch scope, so \"--fetch-scope\" has nothing to set. Nothing was changed.".into());
    }

    let mut keys: Vec<String> = Vec::new();

    for raw in prefixes {
        let path = WarehousePath::from_user_input(raw)?;

        if path.is_root() {
            return Err(
                "A fetch scope cannot be the warehouse root: that is a full warehouse. Name the folders \
                this warehouse fetches. Nothing was changed.".into()
            );
        }

        keys.push(path.as_key().to_string());
    }

    let scope = PathScope::from_prefixes(keys);
    let before = std::fs::read(scope_utils::fetch_scope_path()).ok();
    let (read, unread) = checkout_scopes()?;

    let was = match read_fetch_file()? {
        FetchFile::Unreadable(reason) => reason,
        FetchFile::Newer => return Err(
            "The fetch scope was written by a newer version of Forklift; use that version. Nothing was changed.".into()
        ),
        FetchFile::Readable(current) => {
            if read.iter().all(|(_, checkout)| checkout.subset_of(&current)) {
                return Err(format!(
                    "The fetch scope ({}) can be read and holds every checkout's scope, so heal leaves it \
                    alone. \"forklift scope fetch\" widens it and \"forklift scope prune\" narrows it. \
                    Nothing was changed.",
                    listed(current.prefixes()),
                ).into());
            }
            format!("did not hold every checkout's scope ({})", listed(current.prefixes()))
        }
    };

    let uncovered: Vec<&(String, PathScope)> = read.iter().filter(|(_, checkout)| !checkout.subset_of(&scope)).collect();

    if !uncovered.is_empty() {
        let mut wanted: Vec<String> = scope.prefixes().to_vec();
        wanted.extend(uncovered.iter().flat_map(|(_, checkout)| checkout.prefixes().iter().cloned()));

        return Err(format!(
            "The fetch scope must hold the scope of every checkout, and {} not within {}. \
            Name those paths too, as in \"{}\". Nothing was changed.",
            uncovered.iter().map(|(checkout, scope)| format!("{} checks out {}, which is", checkout, listed(scope.prefixes())))
                .collect::<Vec<_>>().join("; "),
            listed(scope.prefixes()),
            fetch_scope_command(&PathScope::from_prefixes(wanted)),
        ).into());
    }

    let client = RemoteClient::for_fetch_scope_repair()?;

    // The handshake sets the client to leave out the content the remote's host withholds, as
    // `scope fetch` does.
    client.fetch_info().await.map_err(|error| crate::error::append_to_message(error, "Nothing was changed."))?;

    let mut walk = ScopedWalk::new(scope.clone());
    let heads = pallet_utils::readable_user_heads()?;

    for head in &heads {
        walk.walk(&client, head).await.map_err(|error| crate::error::append_to_message(
            crate::error::prefix_message(
                &format!("The history at {} could not be fetched", listed(scope.prefixes())),
                error,
            ),
            "Nothing was changed.",
        ))?;
    }

    check_history_at(&scope).map_err(|missing| format!(
        "The history at {} is not all present, even after fetching from the origin: {} Nothing was changed.",
        listed(scope.prefixes()),
        if missing.ends_with('.') { missing.clone() } else { format!("{}.", missing) },
    ))?;

    let _lock = RefLock::acquire()?;

    // Nothing else uses the store under the exclusive hold, so neither can have changed; were one to,
    // what was walked is no longer what would be written over.
    if std::fs::read(scope_utils::fetch_scope_path()).ok() != before || pallet_utils::readable_user_heads()? != heads {
        return Err("The warehouse changed while heal fetched, so the fetch scope was not written; run it again.".into());
    }

    scope_utils::set_fetch_scope(&scope)?;

    Ok(ScopeRebuilt {
        scope: scope.prefixes().to_vec(),
        chosen: true,
        was,
        from: Vec::new(),
        left_out: unread,
        fetched_objects: walk.fetched_objects(),
    })
}
