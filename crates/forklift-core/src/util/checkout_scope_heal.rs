//! Restoring the scope of the checkout `forklift heal` runs in: the warehouse's own working tree's
//! or a bay's.
//!
//! ## The copy
//!
//! Every write of a checkout's scope puts the same bytes in `scope-copy` beside it first, then in
//! the scope file. A write interrupted between the two leaves the copy naming the scope that write
//! intended, so a stale copy is never wider than the scope file it would replace; it can be
//! narrower, after an interrupted `scope narrow`.
//!
//! ## Usable
//!
//! A scope file, or its copy, is usable when it parses, names a path in a sparse warehouse, and lies
//! within the fetch scope whenever the fetch scope reads. A copy must also pass the stranding guard
//! before heal writes it back: no working file outside it that could never be stacked again (see
//! [`stranded`]).
//!
//! ## What [`repair`] does
//!
//! - A scope file that is missing, does not parse, or names no path where one is required is
//!   written back from a usable copy, or refused with `checkout_scope_damaged` when there is none.
//! - A scope file that reads but lies outside a readable fetch scope is written back from a usable
//!   copy that differs from it. Otherwise it is left to the fetch-scope heal, which refuses: either
//!   file may be the damaged one.
//! - A usable scope file re-syncs a copy that is missing, damaged or different.
//! - A scope file or copy a newer Forklift wrote is never written over.
//!
//! ## The chosen scope
//!
//! [`set_chosen`] is `heal --scope`: the operator names the paths, for a scope file that is not
//! usable. It refuses a scope that is too narrow (the stranding guard) or too wide (a committed file
//! inside it with no working entry, or a subtree the store does not hold). It does not read the
//! fetch scope through the checks scope writes otherwise go through, so it works when the fetch
//! scope is lost too: the fetch-scope heal later in the same run rebuilds it.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::error::{CoreError, RefusalCode};
use crate::model::tree_item::TreeItem;
use crate::util::path_utils::WarehousePath;
use crate::util::scope_utils::{self, PathScope, RawScope, ScopeClass};
use crate::util::warehouse_utils::{self, WarehouseMode};
use crate::util::scope_heal::listed;
use crate::util::{file_utils, object_utils, pallet_utils, taint_utils};
use crate::util::taint_utils::DurablePath;

/// A checkout scope `heal` wrote.
#[cfg_attr(feature = "docgen", derive(schemars::JsonSchema))]
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ScopeRestored {
    /// The checkout: the warehouse's own working tree, or a bay.
    pub checkout: String,

    /// The prefixes of the scope written; empty for a bay that checks out every path.
    pub scope: Vec<String>,

    /// Whether `--scope` chose them, rather than the copy.
    pub chosen: bool,

    /// What the scope file held that could not be used.
    pub was: String,
}

impl ScopeRestored {
    /// The sentence heal prints for it.
    pub fn sentence(&self) -> String {
        let scope = match self.scope.is_empty() {
            true => "every path".to_string(),
            false => listed(&self.scope),
        };

        match self.chosen {
            true => format!(
                "Set the scope of {} to {}, as --scope named, since its scope file {}.",
                self.checkout, scope, self.was.trim_end_matches('.')
            ),
            false => format!(
                "Restored the scope of {} ({}) from its copy, since its scope file {}.",
                self.checkout, scope, self.was.trim_end_matches('.')
            ),
        }
    }
}

/// The checkout heal runs in.
struct Checkout {
    /// Its name in a sentence.
    name: String,

    /// Its scope file, which heal reads and writes back.
    path: DurablePath,

    /// The copy beside it.
    copy: DurablePath,

    /// Which checkout it is, where both are written together.
    place: taint_utils::Checkout,

    /// Whether it is the warehouse's own working tree.
    main_tree: bool,
}

fn checkout() -> Checkout {
    let place = taint_utils::Checkout::active();
    let name = match &place {
        taint_utils::Checkout::Bay(name) => format!("bay \"{}\"", name),
        taint_utils::Checkout::Main => "the warehouse's own working tree".to_string(),
    };

    Checkout {
        name,
        path: DurablePath::scope(&place),
        copy: DurablePath::scope_copy(&place),
        main_tree: place == taint_utils::Checkout::Main,
        place,
    }
}

/// The fetch scope, as the checks here read it.
enum Fetch {
    /// It reads: a sparse warehouse's prefixes, or the full scope of one that is not sparse.
    Reads(PathScope),

    /// A sparse warehouse's fetch scope that cannot be used, so nothing is judged against it.
    Unread,
}

impl Fetch {
    fn read(sparse: bool) -> Result<Fetch, String> {
        if !sparse {
            return Ok(Fetch::Reads(PathScope::full()));
        }

        Ok(match scope_utils::read_scope_raw(&scope_utils::fetch_scope_path())? {
            RawScope::Read(scope) if !scope.is_full() => Fetch::Reads(scope),
            _ => Fetch::Unread,
        })
    }
}

/// Why a scope that parses cannot be used, or `None` when it can.
fn unusable_reason(scope: &PathScope, sparse: bool, fetch: &Fetch) -> Option<String> {
    if sparse && scope.is_full() {
        return Some("names no path, which only a bay of a warehouse that is not sparse may do".to_string());
    }

    match fetch {
        Fetch::Reads(fetch) if !scope.subset_of(fetch) => Some(format!(
            "names {}, outside the fetch scope ({})", listed(scope.prefixes()), listed(fetch.prefixes())
        )),
        _ => None,
    }
}

/// Why a scope file that parses cannot be the scope of a checkout of this warehouse, judged as
/// [`repair`] judges it, or `None` when it can.
pub(crate) fn unusable_scope(scope: &PathScope) -> Result<Option<String>, String> {
    let sparse = warehouse_utils::warehouse_mode()? == WarehouseMode::Sparse;
    Ok(unusable_reason(scope, sparse, &Fetch::read(sparse)?))
}

/// The scope file of the checkout heal runs in, the one [`repair`] mends.
pub(crate) fn active_scope_path() -> PathBuf {
    checkout().path.as_path().to_path_buf()
}

/// Restore the scope of the checkout heal runs in from its copy, or re-sync the copy from it (see
/// the module doc comment). The bay position must read: the stranding guard judges the working tree
/// against the tree the checkout stands on.
///
/// # Arguments
/// * `notes` - Where a copy that could not be re-synced is noted.
///
/// # Returns
/// * `Ok(Some(ScopeRestored))` - The scope file was written back from the copy.
/// * `Ok(None)`                - Nothing needed restoring, or the case is the fetch-scope heal's.
/// * `Err(CoreError)`          - A `checkout_scope_damaged` refusal, or a file that could not be
///   read or written.
pub fn repair(base: Base, notes: &mut Vec<String>) -> Result<Option<ScopeRestored>, CoreError> {
    let sparse = warehouse_utils::warehouse_mode()? == WarehouseMode::Sparse;
    let checkout = checkout();

    // The main tree of a warehouse that is not sparse records no scope: its reader refuses one.
    if checkout.main_tree && !sparse {
        return Ok(None);
    }

    let fetch = Fetch::read(sparse)?;
    let copy_path = checkout.copy.as_path();

    let was = match scope_utils::read_scope_raw(checkout.path.as_path())? {
        RawScope::Newer(_) => return Ok(None),
        RawScope::Read(scope) => match unusable_reason(&scope, sparse, &fetch) {
            None => {
                resync_copy(&checkout, &scope, notes)?;
                return Ok(None);
            }
            // A scope file that reads but lies outside a fetch scope that reads: either file may be
            // the damaged one. Only a usable copy that says otherwise settles it here.
            Some(reason) if !scope.is_full() => {
                // A copy that does not settle it is noted, so the fetch-scope heal's refusal says why
                // heal here did not restore it.
                return match usable_copy(copy_path, sparse, &fetch)? {
                    Ok((copy, bytes)) if copy != scope => match stranded(&copy, &fetch, base) {
                        Ok(blockers) if blockers.is_empty() => restore(&checkout, &bytes, &copy, &reason).map(Some),
                        Ok(blockers) => {
                            notes.push(format!(
                                "The copy of the scope of {} (\"{}\") names {}, but would leave these files outside \
                                the checkout, where they could never be stacked again, so it was not restored: {}.",
                                checkout.name, copy_path.to_string_lossy(), scope_name(&copy), listed(&blockers)
                            ));
                            Ok(None)
                        }
                        Err(error) => {
                            notes.push(format!(
                                "The copy of the scope of {} (\"{}\") names {}, but whether it would leave a file \
                                outside the checkout could not be told, so it was not restored: {}",
                                checkout.name, copy_path.to_string_lossy(), scope_name(&copy), error
                            ));
                            Ok(None)
                        }
                    },
                    _ => Ok(None),
                };
            }
            Some(reason) => reason,
        },
        RawScope::Missing => "is missing".to_string(),
        RawScope::Damaged(reason) => reason,
    };

    let choose = "Run \"forklift heal --scope <path>...\", naming the paths this checkout held.";

    let (refused_why, remedy) = match usable_copy(copy_path, sparse, &fetch)? {
        Ok((copy, bytes)) => match stranded(&copy, &fetch, base) {
            Ok(blockers) if blockers.is_empty() => return restore(&checkout, &bytes, &copy, &was).map(Some),
            // Files that already lay outside the checkout count too, and stop --scope as well, so
            // moving them is named first.
            Ok(blockers) => (
                format!(
                    "Its copy (\"{}\") names {}, and would leave these files outside the checkout, where they \
                    could never be stacked again: {}.",
                    copy_path.to_string_lossy(), scope_name(&copy), listed(&blockers)
                ),
                "A committed file among them means the copy is narrower than what this checkout held: run \
                \"forklift heal --scope <path>...\", naming the paths it held. If only untracked files stand in \
                the way, move them out of the working tree and run \"forklift heal\" again.".to_string(),
            ),
            Err(error) => (
                format!(
                    "Its copy (\"{}\") names {}, but whether it would leave a file outside the checkout cannot be \
                    told: {}",
                    copy_path.to_string_lossy(), scope_name(&copy), error
                ),
                base_unread_remedy(),
            ),
        },
        Err(why) => (format!("Its copy (\"{}\") {}.", copy_path.to_string_lossy(), why.trim_end_matches('.')), choose.to_string()),
    };

    Err(CoreError::refusal(
        RefusalCode::CheckoutScopeDamaged,
        format!(
            "The scope file of {} (\"{}\") {}, so Forklift cannot tell which paths it checks out. {} Nothing \
            was changed. {}",
            checkout.name, checkout.path.as_path().to_string_lossy(), was.trim_end_matches('.'),
            if refused_why.ends_with('.') { refused_why.clone() } else { format!("{}.", refused_why) }, remedy
        ),
        String::new(),
    ))
}

/// The way out when the tree the checkout stands on cannot be read: no heal judges a scope without
/// it, so only the position and that tree reading again, or a new checkout, ends the refusal.
fn base_unread_remedy() -> String {
    let replace = match crate::globals::active_bay() {
        Some(name) => format!(
            "remove the bay with \"forklift bay remove {}\" and add a new one under another name, copying any \
            uncommitted work across by hand", name
        ),
        None => "clone the warehouse again".to_string(),
    };
    format!(
        "Heal judges a scope against the tree this checkout stands on, which its position names, so it restores \
        none until the position and that tree both read; if the store no longer holds that tree, {}.",
        replace
    )
}

/// The copy, when it is usable but for the stranding guard, with its bytes; otherwise why not.
fn usable_copy(copy_path: &Path, sparse: bool, fetch: &Fetch) -> Result<Result<(PathScope, Vec<u8>), String>, String> {
    Ok(match scope_utils::read_scope_raw(copy_path)? {
        RawScope::Read(copy) => match unusable_reason(&copy, sparse, fetch) {
            Some(reason) => Err(reason),
            None => {
                let bytes = std::fs::read(copy_path)
                    .map_err(|e| format!("Error while reading the scope's copy \"{}\": {}", copy_path.to_string_lossy(), e))?;
                Ok((copy, bytes))
            }
        },
        RawScope::Missing => Err("is missing".to_string()),
        RawScope::Damaged(reason) => Err(reason),
        RawScope::Newer(reason) => Err(reason),
    })
}

/// Write the scope file back with the copy's bytes, so it reads exactly as the copy does.
fn restore(checkout: &Checkout, bytes: &[u8], scope: &PathScope, was: &str) -> Result<ScopeRestored, CoreError> {
    file_utils::write_file_atomically(&checkout.path, bytes)
        .map_err(|e| format!("Error while restoring the scope file \"{}\": {}", checkout.path.as_path().to_string_lossy(), e))?;

    Ok(ScopeRestored { checkout: checkout.name.clone(), scope: scope.prefixes().to_vec(), chosen: false, was: was.to_string() })
}

/// Rewrite a copy that is missing, damaged or different from a usable scope file. A copy a newer
/// Forklift wrote is left as it is. A failure is a note, not an error: the scope file is sound.
fn resync_copy(checkout: &Checkout, scope: &PathScope, notes: &mut Vec<String>) -> Result<(), String> {
    match scope_utils::read_scope_raw(checkout.copy.as_path()) {
        Ok(RawScope::Read(copy)) if &copy == scope => return Ok(()),
        Ok(RawScope::Newer(_)) => return Ok(()),
        _ => {}
    }

    if let Err(error) = scope_utils::write_scope_copy(&checkout.place, scope) {
        notes.push(format!(
            "The copy of the scope of {} could not be rewritten ({}), so a later loss of its scope file has \
            no copy to restore it from.",
            checkout.name, error.trim_end_matches('.')
        ));
    }

    Ok(())
}

/// Set the scope of the checkout heal runs in to `paths`, for a scope file that is not usable (see
/// the module doc comment), writing the copy and then the scope file.
///
/// # Returns
/// * `Ok(ScopeRestored)` - The scope was written.
/// * `Err(CoreError)`    - Why it was refused, or a file that could not be read or written.
pub fn set_chosen(paths: &[String], base: Base) -> Result<ScopeRestored, CoreError> {
    let sparse = warehouse_utils::warehouse_mode()? == WarehouseMode::Sparse;
    let checkout = checkout();

    if checkout.main_tree && !sparse {
        return Err(CoreError::from(
            "This warehouse is not sparse, so its own working tree checks out every path and records no \
            scope. --scope was not applied, and nothing was changed.".to_string()
        ));
    }

    let fetch = Fetch::read(sparse)?;

    let was = match scope_utils::read_scope_raw(checkout.path.as_path())? {
        RawScope::Newer(reason) => return Err(CoreError::from(format!(
            "The scope file of {} (\"{}\") {} This version of Forklift never writes over it, so --scope was \
            not applied and nothing was changed.",
            checkout.name, checkout.path.as_path().to_string_lossy(), reason
        ))),
        RawScope::Read(scope) => match unusable_reason(&scope, sparse, &fetch) {
            None => return Err(CoreError::from(format!(
                "The scope file of {} reads: it checks out {}. --scope sets only a scope that is lost or \
                damaged, so nothing was changed.",
                checkout.name, scope_name(&scope)
            ))),
            Some(reason) => reason,
        },
        RawScope::Missing => "is missing".to_string(),
        RawScope::Damaged(reason) => reason,
    };

    let scope = chosen_scope(paths, sparse, &fetch)?;

    // What the checks below read of the base tree, or why it could not be read: a base that does
    // not read is the same coded refusal heal without --scope gives.
    let unread = |error: String| CoreError::refusal(
        RefusalCode::CheckoutScopeDamaged,
        format!(
            "The scope file of {} (\"{}\") {}, and whether the scope {} matches what this checkout holds cannot \
            be told: {} --scope was not applied, and nothing was changed. {}",
            checkout.name, checkout.path.as_path().to_string_lossy(), was.trim_end_matches('.'), scope_name(&scope),
            if error.ends_with('.') { error.clone() } else { format!("{}.", error) }, base_unread_remedy()
        ),
        String::new(),
    );

    let mut base = BaseTree::of(base).map_err(unread)?;
    let (no_entry, not_held) = beyond_checkout(&scope, &mut base).map_err(unread)?;

    if !no_entry.is_empty() || !not_held.is_empty() {
        let mut found = Vec::new();
        if !no_entry.is_empty() {
            found.push(format!("these committed files have no working entry and would read as removed: {}", listed(&no_entry)));
        }
        if !not_held.is_empty() {
            found.push(format!("these folders are not in the store, so their files cannot be checked: {}", listed(&not_held)));
        }
        return Err(CoreError::from(format!(
            "The scope {} reaches beyond what this checkout holds: {}. --scope was not applied, and nothing was \
            changed. Name only the paths this checkout held.",
            scope_name(&scope), found.join("; and ")
        )));
    }

    for prefix in scope.prefixes() {
        if !base.is_folder(prefix).map_err(unread)? {
            return Err(CoreError::from(format!(
                "\"{}\" is not a folder in the tree this checkout stands on, so it cannot be in its scope. \
                --scope was not applied, and nothing was changed.",
                prefix
            )));
        }
    }

    let blockers = stranded_against(&scope, &fetch, &mut base).map_err(unread)?;
    if !blockers.is_empty() {
        return Err(CoreError::from(format!(
            "The scope {} would leave these files outside the checkout, where they could never be stacked \
            again: {}. --scope was not applied, and nothing was changed. Name every path this checkout held, and \
            move any untracked file that already lay outside it out of the working tree first.",
            scope_name(&scope), listed(&blockers)
        )));
    }

    scope_utils::write_checkout_scope(&checkout.place, &scope)?;

    Ok(ScopeRestored { checkout: checkout.name, scope: scope.prefixes().to_vec(), chosen: true, was })
}

/// The scope `--scope` names, checked against the warehouse's mode and a fetch scope that reads.
fn chosen_scope(paths: &[String], sparse: bool, fetch: &Fetch) -> Result<PathScope, String> {
    let mut keys: Vec<String> = Vec::new();

    for raw in paths {
        let path = WarehousePath::from_user_input(raw)?;
        let key = path.as_key().to_string();

        if !path.is_root() {
            if let Fetch::Reads(fetch) = fetch {
                if !fetch.is_full() && fetch.classify(&key) != ScopeClass::InScope {
                    return Err(format!(
                        "\"{}\" is outside the warehouse's fetch scope ({}), so it cannot be in this checkout's \
                        scope. --scope was not applied, and nothing was changed.",
                        raw, listed(fetch.prefixes())
                    ));
                }
            }
        }

        keys.push(key);
    }

    let scope = PathScope::from_prefixes(keys);

    if scope.is_full() && sparse {
        return Err(
            "A scope of the warehouse root is the whole tree, which a checkout of this sparse warehouse cannot \
            hold. --scope was not applied, and nothing was changed.".to_string()
        );
    }

    Ok(scope)
}

/// Where the tree the checkout stands on is read from.
#[derive(Clone, Copy, Debug)]
pub enum Base<'a> {
    /// The bay position's base. Another bay may have moved the pallet since, so the pallet's head
    /// is not it.
    Position,
    /// The parcel the position is about to be set to, in the same heal, while the position is
    /// lost: a full hash.
    Parcel(&'a str),
    /// The position is about to be set unborn, in the same heal, while it is lost: no tree.
    Unborn,
}

/// The tree the checkout stands on, read as [`Base`] says.
struct BaseTree {
    root: Option<String>,
    trees: HashMap<String, Option<TreeItem>>,
}

/// Whether a path is a committed file of the base tree.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Committed {
    Yes,
    No,
    /// A tree on its way is not in the store, so it cannot be told.
    Unknown,
}

impl BaseTree {
    fn of(base: Base) -> Result<BaseTree, String> {
        let parcel = match base {
            Base::Position => pallet_utils::read_position()?.base,
            Base::Parcel(parcel) => Some(parcel.to_string()),
            Base::Unborn => None,
        };
        let root = match parcel {
            Some(parcel) => Some(object_utils::load_parcel(&parcel)?.tree_hash),
            None => None,
        };

        Ok(BaseTree { root, trees: HashMap::new() })
    }

    /// The tree `hash`, or `None` when the store does not hold it.
    fn tree(&mut self, hash: &str) -> Result<Option<TreeItem>, String> {
        if let Some(tree) = self.trees.get(hash) {
            return Ok(tree.clone());
        }

        let tree = match file_utils::does_object_exist(hash)? {
            true => Some(object_utils::load_tree(hash)?),
            false => None,
        };
        self.trees.insert(hash.to_string(), tree.clone());
        Ok(tree)
    }

    /// The tree at the folder `key`: `Ok(Some(Some(tree)))` when held, `Ok(Some(None))` when a tree
    /// on the way is not in the store, `Ok(None)` when the base has no folder there.
    fn folder(&mut self, key: &str) -> Result<Option<Option<TreeItem>>, String> {
        let Some(root) = self.root.clone() else {
            return Ok(None);
        };
        let Some(mut current) = self.tree(&root)? else {
            return Ok(Some(None));
        };

        if key.is_empty() {
            return Ok(Some(Some(current)));
        }

        for component in key.split('/') {
            let Some(hash) = current.get_subtree(component).map(|item| item.hash.clone()) else {
                return Ok(None);
            };
            match self.tree(&hash)? {
                Some(tree) => current = tree,
                None => return Ok(Some(None)),
            }
        }

        Ok(Some(Some(current)))
    }

    fn is_folder(&mut self, key: &str) -> Result<bool, String> {
        Ok(self.folder(key)?.is_some())
    }

    fn committed_file(&mut self, key: &str) -> Result<Committed, String> {
        let (parent, name) = match key.rsplit_once('/') {
            Some((parent, name)) => (parent, name),
            None => ("", key),
        };

        Ok(match self.folder(parent)? {
            None => Committed::No,
            Some(None) => Committed::Unknown,
            Some(Some(tree)) => match tree.get_file(name) {
                Some(_) => Committed::Yes,
                None => Committed::No,
            },
        })
    }
}

/// The paths where `scope` reaches beyond what the checkout holds: committed files it holds in
/// scope with no working entry, and folders it holds in scope or walks through that the store does
/// not hold.
fn beyond_checkout(scope: &PathScope, base: &mut BaseTree) -> Result<(Vec<String>, Vec<String>), String> {
    let mut no_entry = Vec::new();
    let mut not_held = Vec::new();

    let Some(root) = base.root.clone() else {
        return Ok((no_entry, not_held));
    };

    let mut pending: Vec<(String, String)> = vec![(String::new(), root)];

    while let Some((key, hash)) = pending.pop() {
        let Some(tree) = base.tree(&hash)? else {
            not_held.push(if key.is_empty() { ".".to_string() } else { key });
            continue;
        };

        for (name, _) in tree.get_files() {
            let file = join_key(&key, name);
            if scope.classify(&file) == ScopeClass::InScope && !entry_exists(&file)? {
                no_entry.push(file);
            }
        }

        for (name, item) in tree.get_subtrees() {
            let folder = join_key(&key, name);
            if scope.classify(&folder) != ScopeClass::OutOfScope {
                pending.push((folder, item.hash.clone()));
            }
        }
    }

    no_entry.sort();
    not_held.sort();
    Ok((no_entry, not_held))
}

/// The working-tree files `scope` would strand (see [`stranded_against`]), judged against the tree
/// the checkout stands on.
fn stranded(scope: &PathScope, fetch: &Fetch, base: Base) -> Result<Vec<String>, String> {
    let mut base = BaseTree::of(base)?;
    stranded_against(scope, fetch, &mut base)
}

/// The stranding guard: the working-tree files `scope` classifies out of scope that could never be
/// stacked again under it, since every stack carries the committed version of an out-of-scope path
/// forward by hash. Two kinds count: a committed file of the base tree, anywhere (and one whose
/// tree the store does not hold, which cannot be told apart from it); and an untracked file no
/// rule ignores, under a fetch-scope prefix when the fetch scope reads, or anywhere when it does
/// not. Folders, ignored files, the `.forklift` state, another working tree inside this one and the
/// files every file browser leaves behind do not count. Only folders the scope walks through or
/// leaves out are read: an in-scope folder holds nothing it would strand.
fn stranded_against(scope: &PathScope, fetch: &Fetch, base: &mut BaseTree) -> Result<Vec<String>, String> {
    let rules = file_utils::get_ignore_rules()?;
    let mut found = Vec::new();
    let mut pending: Vec<(String, file_utils::RuleContext)> = vec![(String::new(), file_utils::RuleContext::above_root())];

    while let Some((key, parent_context)) = pending.pop() {
        let fs_path = if key.is_empty() { PathBuf::from(".") } else { PathBuf::from(&key) };
        let entries = file_utils::read_directory_entries(&fs_path)?;
        let context = rules.enter(&parent_context, &key, || file_utils::read_gitignore_in(&fs_path, &entries))?;

        for entry in &entries {
            let name = file_utils::get_name_for_file_or_directory(entry)?;
            let entry_key = join_key(&key, &name);
            let is_directory = entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false);

            if is_directory {
                if scope.classify(&entry_key) == ScopeClass::InScope || warehouse_utils::is_working_tree_root(&entry.path())? {
                    continue;
                }
                // An ignored folder is skipped whole unless the base holds it: its committed files
                // count whatever the rules say.
                if !base.is_folder(&entry_key)? && rules.excludes(&entry_key, true, false, &context) {
                    continue;
                }
                pending.push((entry_key, context.clone()));
                continue;
            }

            if scope.classify(&entry_key) != ScopeClass::OutOfScope
                || pallet_utils::OS_LITTER_FILE_NAMES.contains(&name.as_str()) {
                continue;
            }

            let counts = match base.committed_file(&entry_key)? {
                Committed::Yes | Committed::Unknown => true,
                Committed::No => {
                    !rules.excludes(&entry_key, false, false, &context)
                        && match fetch {
                            Fetch::Reads(fetch) => fetch.is_full() || fetch.classify(&entry_key) == ScopeClass::InScope,
                            Fetch::Unread => true,
                        }
                }
            };

            if counts {
                found.push(entry_key);
            }
        }
    }

    found.sort();
    Ok(found)
}

/// Whether the working tree has an entry at `key`: a file, a link or a folder.
fn entry_exists(key: &str) -> Result<bool, String> {
    match std::fs::symlink_metadata(key) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("Error while reading \"{}\": {}", key, e)),
    }
}

fn join_key(key: &str, name: &str) -> String {
    match key.is_empty() {
        true => name.to_string(),
        false => format!("{}/{}", key, name),
    }
}

fn scope_name(scope: &PathScope) -> String {
    match scope.is_full() {
        true => "every path".to_string(),
        false => listed(scope.prefixes()),
    }
}
